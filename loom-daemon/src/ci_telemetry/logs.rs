//! Completed-job log capture (Issue #8825, phase 2 of the build/CI
//! observability work under epic #8522).
//!
//! # What GitHub actually serves
//!
//! `GET /repos/{o}/{r}/actions/jobs/{job_id}/logs` answers `302` to a signed
//! blob URL whose body is **plain `text/plain`** — one UTF-8 stream, BOM
//! first, one `<RFC3339 timestamp> <text>` line per line, `##[group]` /
//! `##[endgroup]` markers and ANSI escapes inline. Verified live against
//! `rjwalters/loom` job `103352058104` on 2026-09-25 (`Content-Type:
//! text/plain`, `Content-Length: 29637`, served by `Windows-Azure-Blob/1.0`).
//!
//! Two consequences the issue text did not anticipate:
//!
//! 1. **It is not a zip, and there are no `Step:` headers.** The per-job
//!    endpoint returns one undelimited stream; only the *run*-level archive
//!    (`…/runs/{id}/logs`) is a zip with a file per step. There is therefore
//!    nothing to parse a `step` attribute out of, and deriving one from the
//!    runner's own `##[group]Run …` markers was rejected deliberately: the
//!    marker text is log content, and an *attribute* rides straight past the
//!    gateway's body scrub stage. See `defaults/docs/ci-observability.md`
//!    §"Why there is no `step` attribute".
//! 2. **`gh api` refuses to emit a body containing terminal escape
//!    sequences** unless `--allow-escape-sequences` is passed — without it
//!    the command exits 1 with an empty body, which would have looked
//!    exactly like an empty log. [`super::api::GhCliApi::get_document`]
//!    passes the flag (and falls back for a `gh` too old to know it).
//!
//! # What this module does
//!
//! Nothing but chunking and capping: the operator decision for #8825 is that
//! the **gateway** is the redaction boundary, so the daemon forwards what
//! GitHub sent. [`chunk`] splits the text into ≤ [`CHUNK_BYTES`] records on
//! line boundaries and stops at the per-job cap, appending one marker record
//! that names the cap. A truncated log always reads as truncated.

use crate::telemetry::ci::CiJobLogRecord;
use crate::telemetry::{RepoVisibility, TelemetryEnvelope, TelemetryRecord};

/// Maximum log text carried by one `ci.job.log` record. Small enough that a
/// chunk plus its attributes rides the existing 50-record OTLP batches under
/// the gateway's 1 MiB request cap with room to spare.
pub const CHUNK_BYTES: usize = 8 * 1024;

/// Default `autonomous.ciTelemetry.logCaptureMaxBytes` — per-job cap on
/// captured log **text** (the marker record is the documented one-record
/// overshoot).
pub const DEFAULT_MAX_BYTES: usize = 5 * 1024 * 1024;

/// How many job logs one cycle downloads before deferring the rest to the
/// next one. Bounds a cycle's wall time and request budget after a backlog
/// (first enable, or a long outage); the ledger keeps the remainder pending.
pub const MAX_DOWNLOADS_PER_CYCLE: usize = 50;

/// How many times one job's log download is retried before the poller gives
/// up on it and `status` reports it as failed. Without a cap, a job whose
/// logs GitHub has expired would be retried on every cycle forever.
pub const MAX_ATTEMPTS: u32 = 3;

/// `GET` path for one completed job's log.
#[must_use]
pub fn logs_path(repo: &str, job_id: u64) -> String {
    format!("repos/{repo}/actions/jobs/{job_id}/logs")
}

/// One job whose log is wanted but not yet captured — everything needed to
/// build its records without re-listing the run (the run is already "seen",
/// so a re-poll never lists its jobs again).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LogTarget {
    pub repo: String,
    #[serde(default)]
    pub visibility: RepoVisibility,
    pub run_id: u64,
    pub job_id: u64,
    /// The job's run attempt (the trace/span identity derives from it).
    pub attempt: u32,
    pub workflow: String,
    pub job: String,
    pub completed_at: chrono::DateTime<chrono::Utc>,
}

/// The result of chunking one job's log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunked {
    /// Chunk bodies in order; the last is the truncation marker when
    /// [`truncated`](Self::truncated) is set.
    pub chunks: Vec<String>,
    /// Bytes GitHub returned for this job, before the cap.
    pub total_bytes: usize,
    /// Whether the cap dropped any of it.
    pub truncated: bool,
    /// The marker chunk's note, when truncated.
    pub note: Option<String>,
}

/// Strip a UTF-8 BOM, which GitHub prefixes to every job log.
#[must_use]
pub fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

/// Length of the longest prefix of `text` that is whole lines and at most
/// `max_bytes` long. Truncation happens on a line boundary so a capped log
/// never ends mid-line; a first line already longer than the cap yields `0`
/// (nothing is emitted but the marker — half a line is not evidence).
///
/// The search is over **bytes**, deliberately: `&text[..max_bytes]` would
/// panic whenever the cap lands inside a multi-byte character, and a cap is an
/// arbitrary number against arbitrary UTF-8 build output. `\n` is ASCII and
/// UTF-8 is self-synchronizing, so a `0x0A` byte is always a real newline and
/// the returned index is always a char boundary.
fn whole_line_prefix(text: &str, max_bytes: usize) -> usize {
    if text.len() <= max_bytes {
        return text.len();
    }
    text.as_bytes()[..max_bytes]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1)
}

/// Split `text` at char boundaries into pieces of at most `limit` bytes.
/// Only reached by a single line longer than one chunk: the ≤ `limit`
/// per-record invariant is absolute, so such a line is the one documented
/// exception to "never mid-line".
fn split_oversized(text: &str, limit: usize) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut rest = text;
    while rest.len() > limit {
        let mut cut = limit;
        while cut > 0 && !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        if cut == 0 {
            // A single char wider than `limit` cannot happen for limit >= 4,
            // but never loop forever if a caller passes a tiny limit.
            break;
        }
        pieces.push(rest[..cut].to_string());
        rest = &rest[cut..];
    }
    if !rest.is_empty() {
        pieces.push(rest.to_string());
    }
    pieces
}

/// Chunk one job's log text: whole lines, ≤ `chunk_bytes` per chunk, at most
/// `max_bytes` of text in total plus one marker chunk naming the cap.
#[must_use]
pub fn chunk(text: &str, max_bytes: usize, chunk_bytes: usize) -> Chunked {
    let text = strip_bom(text);
    let total_bytes = text.len();
    let kept = &text[..whole_line_prefix(text, max_bytes)];
    let truncated = kept.len() < total_bytes;

    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in kept.split_inclusive('\n') {
        if line.len() > chunk_bytes {
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
            }
            chunks.extend(split_oversized(line, chunk_bytes));
            continue;
        }
        if current.len() + line.len() > chunk_bytes {
            chunks.push(std::mem::take(&mut current));
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        chunks.push(current);
    }

    let note = truncated.then(|| {
        format!(
            "[loom ci-telemetry] log truncated: this job's log is {total_bytes} byte(s); \
capture stopped at the {max_bytes}-byte per-job cap \
(autonomous.ciTelemetry.logCaptureMaxBytes). The chunks above are the first \
{} byte(s) only — read the rest in GitHub Actions.",
            kept.len()
        )
    });
    if let Some(note) = &note {
        chunks.push(note.clone());
    }
    Chunked {
        chunks,
        total_bytes,
        truncated,
        note,
    }
}

/// The `ci.job.log` envelopes for one captured job log. Empty when the job
/// produced no log text at all (nothing is fabricated; the ledger still
/// records the capture as done so it is never re-downloaded).
#[must_use]
pub fn log_envelopes(
    target: &LogTarget,
    chunked: &Chunked,
    host_id: &str,
) -> Vec<TelemetryEnvelope> {
    let count = u32::try_from(chunked.chunks.len()).unwrap_or(u32::MAX);
    let ctx =
        super::records::job_context(&target.repo, target.run_id, target.attempt, target.job_id);
    chunked
        .chunks
        .iter()
        .enumerate()
        .map(|(index, text)| {
            let index = u32::try_from(index).unwrap_or(u32::MAX);
            let is_marker = chunked.truncated && index + 1 == count;
            let record = CiJobLogRecord {
                repo: target.repo.clone(),
                visibility: target.visibility,
                run_id: target.run_id,
                job_id: target.job_id,
                workflow: target.workflow.clone(),
                job: target.job.clone(),
                chunk_index: index,
                chunk_count: count,
                log_bytes_total: chunked.total_bytes as u64,
                // Every chunk of a capped log reads as truncated, not just
                // the marker: one record read in isolation must never look
                // like a complete log (ci-principles rule 6).
                truncated: chunked.truncated,
                truncation_note: is_marker.then(|| chunked.note.clone()).flatten(),
                completed_at: target.completed_at,
                text: text.clone(),
            };
            let mut envelope = TelemetryEnvelope::new(host_id, TelemetryRecord::CiJobLog(record));
            envelope.trace_context = Some(ctx.clone());
            envelope
        })
        .collect()
}
