//! The `session.summary` ↔ execution trace join (Issue #8908).
//!
//! The transcript-ingest pass reads Claude Code transcripts on its own
//! schedule and knows nothing about which daemon execution a session belongs
//! to; a transcript records no environment, so the `LOOM_TRACEPARENT` its
//! process inherited is gone. This module leaves the pass one small,
//! local-only index to join on.
//!
//! **Write side.** When a traced sweep is dispatched, [`open`] writes
//! `.loom/logs/trace-joins/<trace-id>-<span-id>.json` = `{issue, context, started_at}`
//! (the execution's persisted root context). At the terminal transition,
//! [`close`] stamps `ended_at`. Entries are pruned [`RETAIN_CLOSED_HOURS`]
//! after they close, or [`RETAIN_OPEN_HOURS`] after they open.
//!
//! **Read side.** [`context_for_session`] takes a transcript's `cwd`, its
//! attributed issue (the `/loom:<role> <N>` head) and its first timestamp.
//! It resolves the workspace (the `cwd` itself, or the part before
//! `/.loom/worktrees/`) and returns a context only when **exactly one** entry
//! names that issue and has a window containing the session's start (with
//! [`START_SLACK_SECS`] of clock slack). No issue, no entry, or an ambiguous
//! match returns `None`: the log stays unjoined and is never guessed. A
//! subagent transcript whose head names no issue is therefore unjoined.
//!
//! The index lives beside `trace-context/` rather than inside it, because
//! every `.json`/`.jsonl` there is an execution context or a span journal.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::telemetry::trace::store::TraceStore;
use crate::telemetry::trace::TraceContext;

/// Directory, relative to a workspace root.
pub const JOIN_DIR: &str = ".loom/logs/trace-joins";
/// A closed entry is kept this long, for late ingest passes.
pub const RETAIN_CLOSED_HOURS: i64 = 24;
/// An entry that never closed (daemon crash) is kept this long.
pub const RETAIN_OPEN_HOURS: i64 = 7 * 24;
/// A session may start this much before its execution's recorded start.
pub const START_SLACK_SECS: i64 = 120;
/// Most entries read per lookup. `pub(crate)` so a test can construct the
/// exact >[`MAX_ENTRIES`] fixture that hits the cap (issue #9013 item 2)
/// without duplicating the constant.
pub(crate) const MAX_ENTRIES: usize = 1024;

/// One execution's join entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinEntry {
    pub issue: u32,
    pub context: TraceContext,
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
}

impl JoinEntry {
    fn covers(&self, at: DateTime<Utc>) -> bool {
        at >= self.started_at - Duration::seconds(START_SLACK_SECS)
            && self.ended_at.is_none_or(|end| at <= end)
    }

    fn expired(&self, now: DateTime<Utc>) -> bool {
        match self.ended_at {
            Some(end) => now - end > Duration::hours(RETAIN_CLOSED_HOURS),
            None => now - self.started_at > Duration::hours(RETAIN_OPEN_HOURS),
        }
    }
}

/// Keyed by span as well as trace: every sweep of one issue shares its story
/// trace id (#9037), so a trace id alone would let a retry overwrite, and the
/// earlier sweep's [`close`] then end, the retry's entry.
fn entry_path(root: &Path, context: &TraceContext) -> PathBuf {
    root.join(JOIN_DIR).join(format!(
        "{}-{}.json",
        context.trace_id.as_str(),
        context.span_id.as_str()
    ))
}

fn write(root: &Path, entry: &JoinEntry) -> anyhow::Result<()> {
    let dir = root.join(JOIN_DIR);
    std::fs::create_dir_all(&dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
    serde_json::to_writer(&mut tmp, entry)?;
    tmp.persist(entry_path(root, &entry.context))
        .map_err(|e| e.error)?;
    Ok(())
}

/// The entry for `execution`, from its persisted trace context.
fn saved_context(root: &Path, execution: &str) -> Option<TraceContext> {
    let store = TraceStore::new(root);
    TraceStore::load(&store.path(root, execution))
        .ok()
        .map(|saved| saved.context)
}

/// Open `execution`'s join entry for `issue`. A no-op when tracing is off or
/// the execution has no persisted trace context. Best-effort.
pub fn open(root: &Path, execution: &str, issue: u32) {
    if !super::super::tracing::enabled(root) {
        return;
    }
    if let Err(error) = open_at(root, execution, issue, Utc::now()) {
        log::warn!("observability: session trace join not recorded: {error}");
    }
}

/// [`open`] without the enablement check.
pub fn open_at(root: &Path, execution: &str, issue: u32, now: DateTime<Utc>) -> anyhow::Result<()> {
    let Some(context) = saved_context(root, execution) else {
        return Ok(());
    };
    write(
        root,
        &JoinEntry {
            issue,
            context,
            started_at: now,
            ended_at: None,
        },
    )
}

/// Close `execution`'s join entry at `ended_at`. A no-op when none is open.
pub fn close(root: &Path, execution: &str, ended_at: DateTime<Utc>) {
    let Some(context) = saved_context(root, execution) else {
        return;
    };
    let path = entry_path(root, &context);
    // An execution in flight across the upgrade opened its entry under the
    // pre-#9038 `<trace-id>.json` name; close (and rename) that one instead.
    let legacy = root
        .join(JOIN_DIR)
        .join(format!("{}.json", context.trace_id.as_str()));
    let Some((found, mut entry)) = [path.clone(), legacy].into_iter().find_map(|candidate| {
        std::fs::read(&candidate)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<JoinEntry>(&bytes).ok())
            .filter(|entry| entry.context == context)
            .map(|entry| (candidate, entry))
    }) else {
        return;
    };
    entry.ended_at = Some(ended_at.max(entry.started_at));
    if let Err(error) = write(root, &entry) {
        log::warn!("observability: session trace join not closed: {error}");
    } else if found != path {
        let _ = std::fs::remove_file(found);
    }
}

/// Close every join entry still open under `root` (issue #9013 item 3): with
/// no live process yet started since a restart, an entry with no `ended_at`
/// can only be orphaned — the daemon crashed, or exited, before the
/// terminal transition that would have called [`close`] ran. Left alone it
/// would keep matching every session of its issue for the full
/// [`RETAIN_OPEN_HOURS`] window, turning re-dispatches of that issue
/// ambiguous (and therefore unjoined — the read side already fails safe)
/// for up to 7 days. Stamping `ended_at = now` here bounds that instead to
/// [`RETAIN_CLOSED_HOURS`] from restart, same as an execution that closed
/// normally. Returns the number of entries closed.
pub fn close_orphaned_entries_at(root: &Path, now: DateTime<Utc>) -> usize {
    let Ok(dir) = std::fs::read_dir(root.join(JOIN_DIR)) else {
        return 0;
    };
    let mut closed = 0usize;
    for item in dir.flatten() {
        let path = item.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Some(mut entry) = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<JoinEntry>(&bytes).ok())
        else {
            continue;
        };
        if entry.ended_at.is_some() {
            continue;
        }
        entry.ended_at = Some(now.max(entry.started_at));
        // Mirror `close`'s legacy-name handling: writing always lands at the
        // current trace-id/span-id path, so a pre-#9038 file gets renamed
        // (removed after the write succeeds) rather than left as a stale
        // duplicate beside the freshly-closed one.
        let canonical = entry_path(root, &entry.context);
        if write(root, &entry).is_err() {
            continue;
        }
        if canonical != path {
            let _ = std::fs::remove_file(&path);
        }
        closed += 1;
    }
    closed
}

/// [`close_orphaned_entries_at`] at the current time (the restart pass).
pub fn close_orphaned_entries(root: &Path) -> usize {
    close_orphaned_entries_at(root, Utc::now())
}

/// The workspace root a transcript's `cwd` belongs to: the part before
/// `/.loom/worktrees/` for an issue worktree, else `cwd` itself.
#[must_use]
pub fn workspace_of(cwd: &Path) -> PathBuf {
    let text = cwd.to_string_lossy();
    match text.find("/.loom/worktrees/") {
        Some(at) => PathBuf::from(&text[..at]),
        None => cwd.to_path_buf(),
    }
}

/// Every entry under `root`, pruning expired ones as a side effect, plus
/// whether the [`MAX_ENTRIES`] cap was hit (issue #9013 item 2). Capped means
/// the directory holds more candidates than were read — a second matching
/// entry could be sitting unread past the cap — so a caller must treat the
/// result as incomplete rather than authoritative.
fn entries(root: &Path, now: DateTime<Utc>) -> (Vec<JoinEntry>, bool) {
    let Ok(dir) = std::fs::read_dir(root.join(JOIN_DIR)) else {
        return (Vec::new(), false);
    };
    let mut found = Vec::new();
    let mut seen = 0usize;
    let mut capped = false;
    for item in dir.flatten() {
        seen += 1;
        if seen > MAX_ENTRIES {
            capped = true;
            break;
        }
        let path = item.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Some(entry) = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<JoinEntry>(&bytes).ok())
        else {
            continue;
        };
        if entry.expired(now) {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        found.push(entry);
    }
    (found, capped)
}

/// The trace context a session's `session.summary` joins, or `None` (see the
/// module doc for the exactly-one rule).
#[must_use]
pub fn context_for_session(
    cwd: Option<&str>,
    issue: Option<u32>,
    started_at: Option<DateTime<Utc>>,
) -> Option<TraceContext> {
    context_for_session_at(cwd, issue, started_at, Utc::now())
}

/// [`context_for_session`] at an explicit `now` (tests).
#[must_use]
pub fn context_for_session_at(
    cwd: Option<&str>,
    issue: Option<u32>,
    started_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<TraceContext> {
    let (cwd, issue, started_at) = (cwd?, issue?, started_at?);
    let root = workspace_of(Path::new(cwd));
    let (found, capped) = entries(&root, now);
    if capped {
        // #9013 item 2: a second matching entry could be among whatever the
        // cap left unread. Guessing "no join" (unjoined) is safe; guessing a
        // single match here is not — report ambiguous instead.
        return None;
    }
    let mut matches = found
        .into_iter()
        .filter(|entry| entry.issue == issue && entry.covers(started_at));
    let only = matches.next()?;
    matches.next().is_none().then_some(only.context)
}
