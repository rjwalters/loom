//! Live `session.output` emission (Issue #9764) — a separate, fast-tick
//! thread that tails active Claude Code transcripts and forwards each
//! session's NEW output text as bounded `session.output` chunks, while the
//! agent is still running.
//!
//! # Relationship to the 900 s ingest pass
//!
//! This is deliberately **not** a change to
//! [`super::transcript_ingest`]'s cadence: that pass (and the
//! `session.summary` / `session.analysis` records it emits) keeps its own
//! schedule untouched. This thread only reads — nothing it does affects the
//! ledger, `activity.db`, or any other consumer of the transcript files.
//!
//! # How a tick works
//!
//! Discovery reuses [`super::transcript_ingest::collect_transcripts`]. The
//! append-only read is [`TailSet`](crate::observability::ops::quota::tail::TailSet)'s:
//! per-file byte cursors, truncation/replacement reset, the 16 MiB line
//! guard, and idle-file eviction (a transcript not written for ~two tick
//! intervals is dropped from the set and no longer polled). Emission is
//! chunked exactly like `ci_telemetry::logs` chunks a job log —
//! [`CHUNK_BYTES`] pieces on line boundaries (a single line longer than a
//! chunk is the documented char-boundary exception) — and bounded by the
//! per-session [`LiveOutputSettings::max_bytes_per_session`] cap, with
//! `truncated` marking every chunk from the cap-hit onward and
//! `output_bytes_total` admitting the dropped remainder.
//!
//! **No duplication across cursor resets.** `TailSet` forgets a file it
//! dropped as idle and re-reads it from byte 0 when it is written again; for
//! quota burns the caller filters by record id, but a raw transcript re-read
//! would re-emit text. Each session therefore counts the complete lines it
//! has already absorbed and skips re-delivered lines by position — exact for
//! an append-only file, which is `TailSet`'s own contract. A file that
//! *shrank* (a genuine replacement) is re-absorbed from the start instead,
//! because its earlier lines no longer exist.
//!
//! # Attribution
//!
//! Join keys (`repo`, `issue`, `role`, `session_kind`) come from
//! [`SessionContext::resolve`] — the #9445 identity resolution the summary
//! pass uses — resolved once per transcript path (memoised; the first
//! resolve includes one bounded parse of the file). An unresolved `issue`
//! re-resolves on later ticks up to a small attempt cap, covering a
//! transcript first seen before its first user message named the work; it is
//! never guessed from output text. The same memoised parse yields the #8908
//! trace join, so an emitted chunk lands inside its sweep's trace exactly
//! like a `session.summary` does.
//!
//! # Runtime coverage (explicit, not silent)
//!
//! Claude Code transcripts only — the same coverage the ingest pass has.
//! Codex / Pi / OpenCode sessions are **not** reported as live by this
//! producer; their file-log receivers keep their own (body-stripping)
//! pipeline. Widening coverage is a later slice, not a silent gap.
//!
//! # Flags
//!
//! Default **off** (`autonomous.transcriptIngest.liveOutput.enabled`, env
//! `LOOM_TRANSCRIPT_OUTPUT`) — see
//! [`super::transcript_ingest::resolve_live_output_enabled`]. When
//! observability has no exporters configured there is no sink and the tick
//! is a no-op, so enabling the emitter without an exporter costs one
//! discovery walk per interval and emits nothing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::session_context::SessionContext;
use super::transcript_ingest::{
    collect_transcripts, resolve_live_output_settings, LiveOutputSettings,
};
use super::transcript_parse::parse_transcript;
use crate::ci_telemetry::logs::CHUNK_BYTES;
use crate::observability::ops::quota::tail::TailSet;
use crate::observability::runtime_usage::join;
use crate::observability::session_output::{global_session_output_sink, SessionOutputSink};
use crate::telemetry::kinds::session_output::SessionOutputRecord;
use crate::telemetry::trace::TraceContext;

/// Runtime label this emitter stamps — it tails Claude Code transcripts only
/// (the same coverage `activity::session_summary` has). Unsupported runtimes
/// are documented as uncovered, never reported as live.
const RUNTIME: &str = "claude";

/// How many ticks an unresolved-`issue` attribution may be re-resolved for,
/// so a transcript first polled before its first user message landed still
/// ends up attributed. Bounded because a genuinely interactive session never
/// resolves: without the cap it would be re-parsed every tick forever.
const MAX_ATTRIBUTION_ATTEMPTS: u8 = 3;

/// Extract the transcript line's own `timestamp`, without parsing the whole
/// line as JSON — a targeted scan for the `"timestamp":"…"` field, which
/// every Claude Code record carries. `None` when absent or unparsable (the
/// caller then falls back to the poll instant).
fn line_timestamp(line: &str) -> Option<DateTime<Utc>> {
    const KEY: &str = "\"timestamp\":\"";
    let start = line.find(KEY)? + KEY.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    DateTime::parse_from_rfc3339(&rest[..end])
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Length of the longest prefix of `text` that is whole lines and at most
/// `max_bytes` long — byte-for-byte the semantics of
/// `ci_telemetry::logs::whole_line_prefix` (kept local rather than exported:
/// it is eight lines, and exporting it would couple the CI family to this
/// one). Truncation happens on a line boundary; a first line already longer
/// than the cap yields `0` and the caller force-splits, exactly like `chunk`
/// does.
fn whole_line_prefix(text: &str, max_bytes: usize) -> usize {
    if text.len() <= max_bytes {
        return text.len();
    }
    text.as_bytes()[..max_bytes]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1)
}

/// The identity one transcript is emitted under, mirroring
/// `activity::session_summary`'s parent/subagent split.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionIdentity {
    session_id: String,
    parent_session_id: Option<String>,
}

impl SessionIdentity {
    /// The path-derived fallback: the file stem, composed onto the enclosing
    /// session's directory name for a `subagents/` transcript.
    fn from_path(path: &Path) -> Self {
        SessionIdentity {
            session_id: path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            parent_session_id: parent_uuid(path),
        }
    }

    /// The parse-derived identity — the same rules
    /// `build_session_summary` applies: a subagent's records may restate the
    /// parent's `sessionId`; only a distinct id is the subagent's own.
    fn from_parse(path: &Path, session_id: Option<&str>, parent_dir: Option<String>) -> Self {
        let fallback = Self::from_path(path);
        let Some(session_id) = session_id.filter(|s| !s.is_empty()) else {
            return fallback;
        };
        if is_subagent_path(path) {
            let parent = parent_dir.unwrap_or_default();
            let own = if session_id != parent {
                session_id.to_string()
            } else {
                fallback.session_id
            };
            SessionIdentity {
                session_id: own,
                parent_session_id: Some(parent),
            }
        } else {
            SessionIdentity {
                session_id: session_id.to_string(),
                parent_session_id: None,
            }
        }
    }
}

/// Whether `path` is a `subagents/` transcript.
fn is_subagent_path(path: &Path) -> bool {
    path.parent()
        .and_then(Path::file_name)
        .is_some_and(|n| n == "subagents")
}

/// The enclosing session's uuid for a `subagents/` transcript — the
/// directory above `subagents/`.
fn parent_uuid(path: &Path) -> Option<String> {
    is_subagent_path(path).then(|| {
        path.parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    })
}

/// The memoised attribution for one transcript: the #9445 join keys, the
/// attributed role, and the #8908 trace join derived from the same parse.
#[derive(Debug, Clone)]
struct Attribution {
    context: SessionContext,
    role: Option<String>,
    trace: Option<TraceContext>,
}

/// Per-transcript emitter state: identity, attribution, and the stream's
/// progress (line count, observed bytes, emitted chunks).
#[derive(Debug)]
struct SessionTail {
    identity: SessionIdentity,
    attribution: Option<Attribution>,
    resolve_attempts: u8,
    /// Complete lines absorbed so far — the duplication guard against a
    /// `TailSet` cursor reset re-delivering the file from byte 0.
    lines_absorbed: u64,
    /// Text bytes observed, complete-line lengths as read (best-effort; the
    /// cap's `output_bytes_total` contract only needs a consistent measure).
    observed_bytes: u64,
    /// Text bytes forwarded as records so far — capped at
    /// `max_bytes_per_session`.
    emitted_bytes: usize,
    /// The session's chunk sequence — the record's `chunk_index` and the
    /// stable event order consumers sort by.
    sequence: u32,
    /// Sticky from the first cap-hit; every chunk emitted after it carries
    /// `truncated: true`.
    truncated: bool,
    /// Complete lines read but not yet chunked out.
    pending: String,
    /// Newest source timestamp seen in `pending` — the chunk's
    /// `recorded_at`, so a quiet run is distinguishable from a stalled
    /// export. Falls back to the poll instant when the tail carried no
    /// parseable source timestamp.
    last_source_ts: Option<DateTime<Utc>>,
}

impl SessionTail {
    fn new(path: &Path) -> Self {
        SessionTail {
            identity: SessionIdentity::from_path(path),
            attribution: None,
            resolve_attempts: 0,
            lines_absorbed: 0,
            observed_bytes: 0,
            emitted_bytes: 0,
            sequence: 0,
            truncated: false,
            pending: String::new(),
            last_source_ts: None,
        }
    }

    /// Absorb one delivered complete line, skipping re-deliveries from a
    /// `TailSet` cursor reset (`seen` counts the line's position in this
    /// file's read; absorbed lines are exactly the first `lines_absorbed` of
    /// them).
    fn absorb(&mut self, seen: u64, line: &str) {
        if seen <= self.lines_absorbed {
            return;
        }
        self.lines_absorbed = seen;
        self.observed_bytes += line.len() as u64 + 1;
        self.pending.push_str(line);
        self.pending.push('\n');
        if let Some(ts) = line_timestamp(line) {
            self.last_source_ts = Some(ts);
        }
    }
}

/// The live-output emitter: one `TailSet` plus per-transcript state, driven
/// one [`LiveOutputEmitter::tick`] per interval.
pub struct LiveOutputEmitter {
    projects_dir: PathBuf,
    interval_secs: u64,
    max_bytes_per_session: usize,
    tail: TailSet<u64>,
    sessions: HashMap<PathBuf, SessionTail>,
}

impl LiveOutputEmitter {
    /// An emitter over `projects_dir`, ticking every `interval_secs` with the
    /// given per-session text cap.
    #[must_use]
    pub fn new(projects_dir: PathBuf, interval_secs: u64, max_bytes_per_session: usize) -> Self {
        LiveOutputEmitter {
            projects_dir,
            interval_secs,
            max_bytes_per_session,
            tail: TailSet::default(),
            sessions: HashMap::new(),
        }
    }

    /// Poll every recently-written transcript and emit the newly appended
    /// output as `session.output` chunks. A no-op when no sink is registered
    /// (observability off, or no exporter configured).
    pub fn tick(&mut self) {
        let Some(sink) = global_session_output_sink() else {
            return;
        };
        let now = Utc::now();
        // ~2× the tick interval: a file not written for that long is idle
        // and dropped from the set (bounded memory); written again, it comes
        // back and `absorb`'s line-position guard keeps the re-read from
        // duplicating anything.
        let active_since = now - chrono::Duration::seconds((self.interval_secs * 2) as i64);
        for path in collect_transcripts(&self.projects_dir, None) {
            let key = path.canonicalize().unwrap_or_else(|_| path.clone());
            // A transcript that shrank was replaced, not appended to: its
            // earlier lines no longer exist, so the session re-absorbs from
            // the start instead of skipping them (never observed for Claude
            // transcripts — append-only — but the safe reading).
            if let Some(state) = self.sessions.get_mut(&key) {
                if std::fs::metadata(&path).is_ok_and(|m| m.len() < state.observed_bytes) {
                    state.lines_absorbed = 0;
                    state.observed_bytes = 0;
                }
            }
            {
                let sessions = &mut self.sessions;
                let mut tail = std::mem::take(&mut self.tail);
                // Resolve (or refresh) attribution before the read so the
                // first flush already carries the join keys.
                let state = sessions
                    .entry(key.clone())
                    .or_insert_with(|| SessionTail::new(&path));
                refresh_attribution(state, &path);
                tail.poll([key.clone()], active_since, |seen: &mut u64, line: &str| {
                    *seen += 1;
                    if let Some(state) = sessions.get_mut(&key) {
                        state.absorb(*seen, line);
                    }
                });
                self.tail = tail;
            }
            if let Some(state) = self.sessions.get_mut(&key) {
                flush(state, now, self.max_bytes_per_session, sink);
            }
        }
        // Drop sessions whose files went idle (TailSet no longer tracks
        // them) so the map cannot grow without bound across a long uptime.
        let tracked: std::collections::HashSet<PathBuf> = self.tail.tracked().cloned().collect();
        self.sessions.retain(|key, _| tracked.contains(key));
    }
}

/// Resolve (or re-resolve, while the issue is still unknown and attempts
/// remain) the memoised attribution for one transcript.
fn refresh_attribution(state: &mut SessionTail, path: &Path) {
    let unresolved = state
        .attribution
        .as_ref()
        .is_none_or(|a| a.context.issue.is_none());
    if !unresolved || state.resolve_attempts >= MAX_ATTRIBUTION_ATTEMPTS {
        return;
    }
    state.resolve_attempts += 1;
    // The same per-file ceiling the ingest pass applies before parsing.
    if std::fs::metadata(path)
        .is_ok_and(|m| m.len() > crate::activity::transcript_ingest::MAX_TRANSCRIPT_BYTES)
    {
        return;
    }
    let modified = std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .map(DateTime::<Utc>::from)
        .unwrap_or_else(Utc::now);
    let parsed = parse_transcript(path, modified);
    // Refine the identity once a parse names the session (the path-stem
    // fallback stands in until then); it is frozen from the first parse that
    // answers, so every chunk of a session shares one id.
    if state.attribution.is_none() {
        state.identity =
            SessionIdentity::from_parse(path, parsed.session_id.as_deref(), parent_uuid(path));
    }
    let context = SessionContext::resolve(&parsed);
    let trace = join::context_for_session(
        parsed.cwd.as_deref(),
        context.issue,
        parsed.slash_role.as_deref(),
        parsed.first_timestamp,
    );
    state.attribution = Some(Attribution {
        context,
        role: parsed.role.clone(),
        trace,
    });
}

/// Drain `state.pending` into `session.output` records: whole-line chunks of
/// at most [`CHUNK_BYTES`], bounded by `max_bytes` per session, each carrying
/// the chunk protocol and the source timestamp.
fn flush(state: &mut SessionTail, now: DateTime<Utc>, max_bytes: usize, sink: &SessionOutputSink) {
    let attribution = state.attribution.clone();
    let trace = attribution.as_ref().and_then(|a| a.trace.clone());
    while !state.pending.is_empty() {
        if state.emitted_bytes >= max_bytes {
            // Over the cap: nothing more is forwarded. `truncated` already
            // rides on the last emitted chunk (set below, before it went
            // out); the gap stays explicit via `output_bytes_total`.
            state.truncated = true;
            state.pending.clear();
            break;
        }
        let budget = (max_bytes - state.emitted_bytes).min(CHUNK_BYTES);
        let take = whole_line_prefix(&state.pending, budget);
        let text = if take > 0 {
            state.pending.drain(..take).collect::<String>()
        } else {
            // A single line longer than the budget — the documented
            // exception to "never mid-line" (`ci_telemetry::logs`): split it
            // at char boundaries.
            split_oversized_first(&mut state.pending, budget)
        };
        state.emitted_bytes += text.len();
        if state.emitted_bytes >= max_bytes && !state.pending.is_empty() {
            state.truncated = true;
        }
        let record = SessionOutputRecord {
            repo: attribution.as_ref().and_then(|a| a.context.repo.clone()),
            visibility: attribution
                .as_ref()
                .map_or(crate::telemetry::RepoVisibility::Private, |a| a.context.visibility),
            session_id: state.identity.session_id.clone(),
            parent_session_id: state.identity.parent_session_id.clone(),
            runtime: RUNTIME.to_string(),
            role: attribution.as_ref().and_then(|a| a.role.clone()),
            issue: attribution.as_ref().and_then(|a| a.context.issue),
            session_kind: attribution.as_ref().map(|a| a.context.kind),
            chunk_index: state.sequence,
            chunk_count: state.sequence.saturating_add(1),
            truncated: state.truncated,
            output_bytes_total: state.observed_bytes,
            recorded_at: state.last_source_ts.unwrap_or(now),
            text,
        };
        state.sequence = state.sequence.saturating_add(1);
        sink.push_traced(record, trace.clone());
    }
}

/// Split at most `limit` bytes off the front of `pending`'s first line, at a
/// char boundary — `ci_telemetry::logs::split_oversized`'s per-piece rule,
/// applied to the head of a stream.
fn split_oversized_first(pending: &mut String, limit: usize) -> String {
    let mut cut = limit.min(pending.len());
    while cut > 0 && !pending.is_char_boundary(cut) {
        cut -= 1;
    }
    pending.drain(..cut).collect()
}

/// Start the periodic live-output thread when this host opted in
/// (`autonomous.transcriptIngest.liveOutput.enabled` / env
/// `LOOM_TRANSCRIPT_OUTPUT`; **default off** — the FLAGS-OFF polarity,
/// #9764). Mirrors
/// [`super::transcript_ingest::try_init_transcript_ingest`]: returns the
/// `JoinHandle` (the thread keeps running when the handle is dropped) or
/// `None` when the feature is off. Settings are resolved once here and
/// frozen for the life of the process — restart required after a config
/// edit.
pub fn try_init_transcript_output(repo_root: &Path) -> Option<std::thread::JoinHandle<()>> {
    let config = super::transcript_ingest::read_transcript_ingest_config(repo_root);
    let LiveOutputSettings {
        interval_secs,
        max_bytes_per_session,
    } = resolve_live_output_settings(&config)?;
    let projects_dir = crate::transcript_tokens::claude_projects_dir()
        .unwrap_or_else(|| PathBuf::from("projects"));
    log::info!(
        "🛰️ Transcript live output enabled (session.output every {interval_secs}s, \
         {max_bytes_per_session}-byte per-session cap)"
    );
    Some(std::thread::spawn(move || {
        let mut emitter =
            LiveOutputEmitter::new(projects_dir, interval_secs, max_bytes_per_session);
        loop {
            std::thread::sleep(Duration::from_secs(interval_secs));
            emitter.tick();
        }
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
#[path = "transcript_output_tests.rs"]
mod tests;
