//! The `session.summary` ↔ execution trace join (Issue #8908).
//!
//! The transcript-ingest pass reads Claude Code transcripts on its own
//! schedule and knows nothing about which daemon execution a session belongs
//! to; a transcript records no environment, so the `LOOM_TRACEPARENT` its
//! process inherited is gone. This module leaves the pass one small,
//! local-only index to join on.
//!
//! **Write side.** When a traced sweep is dispatched, [`open`] writes
//! `.loom/logs/trace-joins/<trace-id>.json` = `{issue, context, started_at}`
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
/// Most entries read per lookup.
const MAX_ENTRIES: usize = 1024;

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

fn entry_path(root: &Path, context: &TraceContext) -> PathBuf {
    root.join(JOIN_DIR)
        .join(format!("{}.json", context.trace_id.as_str()))
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
    let Some(mut entry) = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<JoinEntry>(&bytes).ok())
    else {
        return;
    };
    entry.ended_at = Some(ended_at.max(entry.started_at));
    if let Err(error) = write(root, &entry) {
        log::warn!("observability: session trace join not closed: {error}");
    }
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

/// Every entry under `root`, pruning expired ones as a side effect.
fn entries(root: &Path, now: DateTime<Utc>) -> Vec<JoinEntry> {
    let Ok(dir) = std::fs::read_dir(root.join(JOIN_DIR)) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for item in dir.flatten().take(MAX_ENTRIES) {
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
    found
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
    let mut matches = entries(&root, now)
        .into_iter()
        .filter(|entry| entry.issue == issue && entry.covers(started_at));
    let only = matches.next()?;
    matches.next().is_none().then_some(only.context)
}
