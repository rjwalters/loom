//! Durable, append-only journal of terminal sweep outcomes (Issue #4644).
//!
//! ## Problem
//!
//! The event bus ([`crate::event_bus`]) is an in-memory-only `tokio::sync::
//! broadcast` channel: an `Event::SweepExited` / `Event::SweepCrashed`
//! published with no attached subscriber is simply gone. The sweep registry's
//! in-memory entries additionally GC ~1h after a terminal transition
//! (`TERMINAL_RETENTION_SECS` in [`crate::sweep_registry`]). So the ONLY trace
//! of a terminal sweep outcome that survives longer than an hour is prose
//! inside the per-issue log file (`.loom/logs/sweep-issue-N.log`) — invisible
//! to any tool and expensive for an operator to grep across many issues after
//! the fact (the incident this issue documents: half of an 8-issue overnight
//! batch died at token selection, in under a second each, with zero durable
//! trace anywhere other than log prose).
//!
//! ## Fix
//!
//! [`SweepRegistry`](crate::sweep_registry::SweepRegistry) appends one JSON
//! line ([`OutcomeRecord`]) to `<workspace>/.loom/logs/sweep-outcomes.jsonl`
//! (override via [`OUTCOMES_JOURNAL_PATH_ENV`], mirroring
//! [`crate::sweep_journal::JOURNAL_PATH_ENV`]) at every site that already
//! emits a terminal `SweepExited`/`SweepCrashed` bus event for a single-issue
//! sweep — the reaper's dead-child handling in `reap_once`, and the
//! operator/watchdog-initiated `finish_cancel`. This is purely additive: the
//! bus events are unchanged, and the write is best-effort (a failure is logged
//! and swallowed, never allowed to block reaping — see the module doc on
//! [`crate::sweep_journal`] for the same philosophy applied to the sibling
//! liveness journal).
//!
//! Each line carries everything a post-hoc reader needs without touching log
//! prose: `issue`, `sweep_id`, `outcome` (`"exited"`/`"crashed"`),
//! `exit_code`, `death_class` (carries e.g. `preflight-token-selection-failed`
//! for the token-selection-death shape this issue targets — see
//! `sweep_registry::preflight_death_signatures`), `token_name`, and
//! `duration_sec`.
//!
//! Unlike [`crate::sweep_journal`] (a machine-level, upsert-keyed *liveness*
//! snapshot — one row per currently-tracked sweep, pruned as sweeps end), this
//! is a per-workspace, **append-only history** — every terminal outcome gets
//! its own line, forever (bounded by rotation, see below). The two journals
//! answer different questions ("is this sweep still alive?" vs. "what
//! happened to every sweep that has ever ended?") and are intentionally
//! separate files.
//!
//! ## Rotation
//!
//! A daemon runs for weeks, so the journal is bounded: [`append_outcome`]
//! rotates the current file to a single `.1` sibling (overwriting any
//! previous backup) once it exceeds [`MAX_JOURNAL_BYTES`] OR its oldest
//! (first) line is older than [`MAX_JOURNAL_AGE_DAYS`]. Rotation keeps at
//! most one full generation of history beyond the live file — adequate for
//! the "query recent terminal outcomes" use case this journal exists for,
//! without unbounded growth.
//!
//! ## `sweep.outcome` telemetry journal (Issue #4704, absorbs #4137)
//!
//! The [`OutcomeRecord`] above is deliberately narrow — it exists to answer
//! "how did this sweep die" for the reaper's own death-classification (#4644).
//! It carries no model, no config, no PR/result classification, so it cannot
//! answer #4137's question ("do sweeps dispatched at `sonnet` reach a merged
//! PR more often than at `opus`?"). [`append_outcome_telemetry`] is a second,
//! independent append-only journal populated with
//! [`crate::telemetry::SweepOutcomeRecord`] — the versioned schema type
//! [`crate::telemetry`] (#4703) defines for exactly this purpose — wrapped in
//! a [`crate::telemetry::TelemetryEnvelope`]. It is written at the same three
//! terminal-transition call sites as [`append_outcome`] (see
//! `SweepRegistry::append_outcome_journal` in `sweep_registry.rs`), as an
//! independent best-effort side effect: a failure here never blocks reaping,
//! and is logged rather than propagated, matching this module's existing
//! philosophy.
//!
//! Kept as a **separate file** from [`OUTCOMES_JOURNAL_FILENAME`] rather than
//! merged into `OutcomeRecord` — the two answer different questions ("why did
//! this specific death happen" vs. "how do sweeps perform in aggregate by
//! model/config") and a reader of one should not need to skip fields it does
//! not understand from the other. Same rotation policy
//! ([`MAX_JOURNAL_BYTES`] / [`MAX_JOURNAL_AGE_DAYS`]), same best-effort
//! read-skips-malformed-lines contract.
//!
//! ### Read surface
//!
//! [`read_all_sweep_outcomes`] plus [`summarize_by_model`] back the
//! `loom-daemon sweep-outcomes` CLI subcommand (`main.rs`) — a local
//! inspection path that answers "success rate and median duration by model"
//! without any exporter or UI (#4704 AC3, satisfying #4137 AC4). See
//! `.loom/docs/telemetry-schema.md` for the wire format and the CLI's
//! documented usage.
//!
//! ### `phase_durations`: sampled, not read back
//!
//! The per-phase breakdown comes from transition observations the registry
//! samples while the sweep is alive (`SweepRegistry::sample_phase_transition`),
//! not from any on-disk history — because none exists. `sweep-checkpoint.sh`
//! *overwrites* `.loom/sweep-checkpoint/issue-<N>.json` at every phase
//! boundary, and the sweep skill **deletes** it on success, so the file is a
//! point-in-time value that is gone precisely when a successful sweep's record
//! is written. Sampling it once per reaper tick (≤30s, finer in practice — the
//! read-path reaps sample too) and keeping the observations in memory is what
//! lets a completed sweep's record carry a real curator/builder/judge/doctor/
//! merge breakdown.
//!
//! Three honest consequences of a polled source, all visible in the data:
//!
//! - Each phase's duration is attributed from the previous observation to its
//!   own, so it is accurate to within one sampling interval.
//! - The trailing in-flight segment (last observed completion → terminal
//!   transition) is **not** attributed to any phase, since the daemon does not
//!   know which phase was running. Entries therefore sum to at most
//!   `total_duration_sec`, never more.
//! - A daemon restart mid-sweep loses the observations taken before it. Such a
//!   record falls back to a single best-effort entry (the last known phase,
//!   attributed the whole duration) or, with no phase known at all, an empty
//!   list — never a fabricated phase name.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use crate::telemetry;

/// Environment override for the outcomes journal path (test seam), mirrors
/// [`crate::sweep_journal::JOURNAL_PATH_ENV`].
pub const OUTCOMES_JOURNAL_PATH_ENV: &str = "LOOM_SWEEP_OUTCOMES_JOURNAL_PATH";

/// Default filename under `<workspace_root>/.loom/logs/`.
pub const OUTCOMES_JOURNAL_FILENAME: &str = "sweep-outcomes.jsonl";

/// Rotate the journal once it grows past this size — keeps the file bounded
/// across a daemon that runs for weeks (#4644).
pub const MAX_JOURNAL_BYTES: u64 = 5 * 1024 * 1024; // 5 MiB

/// Rotate the journal once its oldest (first) recorded line is older than
/// this many days, even if it never reached [`MAX_JOURNAL_BYTES`] (a
/// low-traffic workspace should not carry multi-month-old lines forever).
pub const MAX_JOURNAL_AGE_DAYS: i64 = 30;

/// One append-only journal line: a single terminal sweep outcome, carrying
/// everything a post-hoc reader needs to diagnose it without reading log
/// prose (#4644 AC1/AC2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomeRecord {
    /// When this outcome was recorded (reap/cancel time, not the original
    /// spawn time — `duration_sec` carries the elapsed lifetime).
    pub timestamp: DateTime<Utc>,
    /// The owning workspace root, formatted identically to
    /// [`crate::types::SweepInfo::repo`].
    pub repo: String,
    /// The GitHub/Gitea issue number this sweep was working.
    pub issue: u32,
    /// Stable opaque ID assigned at dispatch time.
    pub sweep_id: String,
    /// `"exited"` (clean/dirty process exit observed by the reaper, or an
    /// operator/watchdog-initiated cancel) or `"crashed"` (the reaper found a
    /// checkpoint, i.e. this run made some prior lifecycle progress before
    /// dying).
    pub outcome: String,
    /// Process exit code, when known. `None` for a cancel (no code to report)
    /// or an unrecoverable signal death.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Machine-readable death classification, when the reaper's pre-flight
    /// classifier matched — see `sweep_registry::classify_preflight_outcome`.
    /// Carries `preflight-token-selection-failed` for the token-selection-death
    /// shape this issue targets, distinguishable here without reading log
    /// prose (#4644 AC2). `None` for a death that reached `# CLAUDE_CLI_START`,
    /// an unreadable log, or a manual cancel.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub death_class: Option<String>,
    /// Token account name selected by `spawn-claude.sh` for this run, or
    /// `"unknown"` when never surfaced (mirrors `SweepInfo::token_name`).
    pub token_name: String,
    /// Elapsed wall-clock seconds from dispatch to this terminal outcome.
    pub duration_sec: i64,
}

/// Resolve the default outcomes journal path: [`OUTCOMES_JOURNAL_PATH_ENV`]
/// override (non-empty), else `<workspace_root>/.loom/logs/sweep-outcomes.jsonl`.
#[must_use]
pub fn default_outcomes_path(workspace_root: &Path) -> PathBuf {
    if let Ok(path) = std::env::var(OUTCOMES_JOURNAL_PATH_ENV) {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    workspace_root
        .join(".loom")
        .join("logs")
        .join(OUTCOMES_JOURNAL_FILENAME)
}

/// Append `record` as one JSON line to `path`, creating parent directories as
/// needed and rotating the file first if it has grown oversized/stale (see
/// [`rotate_if_needed`]).
///
/// Best-effort by contract: callers (`SweepRegistry::reap_once` /
/// `finish_cancel`) log a warning on `Err` but never let a journal-write
/// failure block reaping (#4644 — mirrors [`crate::sweep_journal`]'s
/// philosophy). Deliberately NOT coupled to event-bus publish success: the two
/// are independent best-effort side effects of the same terminal transition.
pub fn append_outcome(path: &Path, record: &OutcomeRecord) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating outcomes journal dir {}", parent.display()))?;
    }
    rotate_if_needed(path)?;
    let line = serde_json::to_string(record).context("serializing sweep outcome record")?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening outcomes journal {}", path.display()))?;
    writeln!(file, "{line}")
        .with_context(|| format!("appending to outcomes journal {}", path.display()))?;
    Ok(())
}

/// Rotate `path` to a single `.1` sibling (overwriting any previous backup)
/// when it has grown past [`MAX_JOURNAL_BYTES`] OR its oldest recorded line is
/// older than [`MAX_JOURNAL_AGE_DAYS`]. A missing file is neither oversized
/// nor stale (no-op) — the common "first write ever" case.
fn rotate_if_needed(path: &Path) -> Result<()> {
    let Ok(meta) = std::fs::metadata(path) else {
        return Ok(());
    };
    let oversized = meta.len() >= MAX_JOURNAL_BYTES;
    let stale = !oversized && is_stale(path);
    if !oversized && !stale {
        return Ok(());
    }
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(OUTCOMES_JOURNAL_FILENAME);
    let backup = path.with_file_name(format!("{file_name}.1"));
    std::fs::rename(path, &backup)
        .with_context(|| format!("rotating {} -> {}", path.display(), backup.display()))?;
    Ok(())
}

/// Whether `path`'s oldest (first) line is older than [`MAX_JOURNAL_AGE_DAYS`].
/// Any read/parse failure is treated as "not stale" — rotation is a bounded-
/// growth nicety, not something a corrupt first line should trigger
/// spuriously.
fn is_stale(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut reader = std::io::BufReader::new(file);
    let mut first_line = String::new();
    if reader.read_line(&mut first_line).unwrap_or(0) == 0 {
        return false;
    }
    let Ok(record) = serde_json::from_str::<OutcomeRecord>(first_line.trim()) else {
        return false;
    };
    Utc::now() - record.timestamp > chrono::Duration::days(MAX_JOURNAL_AGE_DAYS)
}

/// Read every parseable [`OutcomeRecord`] line from `path`, in file order.
/// Missing file yields an empty vec; a malformed line is skipped (best-effort
/// reader, matching the writer's best-effort contract) rather than aborting
/// the whole read.
#[must_use]
pub fn read_all(path: &Path) -> Vec<OutcomeRecord> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    contents
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

// ============================================================================
// `sweep.outcome` telemetry journal (Issue #4704, absorbs #4137)
// ============================================================================

/// Environment override for the `sweep.outcome` telemetry journal path (test
/// seam), mirrors [`OUTCOMES_JOURNAL_PATH_ENV`].
pub const OUTCOME_TELEMETRY_JOURNAL_PATH_ENV: &str = "LOOM_SWEEP_OUTCOME_TELEMETRY_JOURNAL_PATH";

/// Default filename under `<workspace_root>/.loom/logs/`.
pub const OUTCOME_TELEMETRY_JOURNAL_FILENAME: &str = "sweep-outcome-telemetry.jsonl";

/// Resolve the default `sweep.outcome` telemetry journal path:
/// [`OUTCOME_TELEMETRY_JOURNAL_PATH_ENV`] override (non-empty), else
/// `<workspace_root>/.loom/logs/sweep-outcome-telemetry.jsonl`. Mirrors
/// [`default_outcomes_path`].
#[must_use]
pub fn default_outcome_telemetry_path(workspace_root: &Path) -> PathBuf {
    if let Ok(path) = std::env::var(OUTCOME_TELEMETRY_JOURNAL_PATH_ENV) {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    workspace_root
        .join(".loom")
        .join("logs")
        .join(OUTCOME_TELEMETRY_JOURNAL_FILENAME)
}

/// Append `envelope` as one JSON line to `path`, creating parent directories
/// as needed and rotating the file first if it has grown oversized/stale.
/// Mirrors [`append_outcome`]'s contract exactly (best-effort by convention —
/// callers log a warning on `Err` but never let a write failure block
/// reaping) using the same [`MAX_JOURNAL_BYTES`] / [`MAX_JOURNAL_AGE_DAYS`]
/// thresholds, keyed on [`crate::telemetry::TelemetryEnvelope::emitted_at`]
/// instead of [`OutcomeRecord::timestamp`].
pub fn append_outcome_telemetry(
    path: &Path,
    envelope: &telemetry::TelemetryEnvelope,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("creating sweep.outcome telemetry journal dir {}", parent.display())
        })?;
    }
    rotate_telemetry_if_needed(path)?;
    let line = serde_json::to_string(envelope).context("serializing telemetry envelope")?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening sweep.outcome telemetry journal {}", path.display()))?;
    writeln!(file, "{line}").with_context(|| {
        format!("appending to sweep.outcome telemetry journal {}", path.display())
    })?;
    Ok(())
}

/// Rotate `path` to a single `.1` sibling (overwriting any previous backup)
/// once it exceeds [`MAX_JOURNAL_BYTES`] OR its oldest line's `emitted_at` is
/// older than [`MAX_JOURNAL_AGE_DAYS`]. Mirrors [`rotate_if_needed`].
fn rotate_telemetry_if_needed(path: &Path) -> Result<()> {
    let Ok(meta) = std::fs::metadata(path) else {
        return Ok(());
    };
    let oversized = meta.len() >= MAX_JOURNAL_BYTES;
    let stale = !oversized && is_telemetry_stale(path);
    if !oversized && !stale {
        return Ok(());
    }
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(OUTCOME_TELEMETRY_JOURNAL_FILENAME);
    let backup = path.with_file_name(format!("{file_name}.1"));
    std::fs::rename(path, &backup)
        .with_context(|| format!("rotating {} -> {}", path.display(), backup.display()))?;
    Ok(())
}

/// Whether `path`'s oldest (first) line's `emitted_at` is older than
/// [`MAX_JOURNAL_AGE_DAYS`]. Mirrors [`is_stale`] — any read/parse failure is
/// treated as "not stale".
fn is_telemetry_stale(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut reader = std::io::BufReader::new(file);
    let mut first_line = String::new();
    if reader.read_line(&mut first_line).unwrap_or(0) == 0 {
        return false;
    }
    let Ok(envelope) = serde_json::from_str::<telemetry::TelemetryEnvelope>(first_line.trim())
    else {
        return false;
    };
    Utc::now() - envelope.emitted_at > chrono::Duration::days(MAX_JOURNAL_AGE_DAYS)
}

/// Read every parseable [`crate::telemetry::TelemetryEnvelope`] line from
/// `path`, in file order. Mirrors [`read_all`] — a missing file yields an
/// empty vec; a malformed line is skipped rather than aborting the read.
#[must_use]
pub fn read_all_outcome_telemetry(path: &Path) -> Vec<telemetry::TelemetryEnvelope> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    contents
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Read every [`crate::telemetry::SweepOutcomeRecord`] from `path` (unwrapping
/// each envelope and discarding any non-`sweep.outcome` record kind — this
/// journal only ever carries that one kind today, but the filter keeps the
/// reader forward-compatible with a future kind sharing the file). The local
/// inspection path (#4704 AC3): `loom-daemon sweep-outcomes` and
/// [`summarize_by_model`] both build on this.
#[must_use]
pub fn read_all_sweep_outcomes(path: &Path) -> Vec<telemetry::SweepOutcomeRecord> {
    read_all_outcome_telemetry(path)
        .into_iter()
        .filter_map(|envelope| match envelope.record {
            telemetry::TelemetryRecord::SweepOutcome(record) => Some(record),
            _ => None,
        })
        .collect()
}

/// One model's aggregate slice of a [`summarize_by_model`] report — the
/// "success rate and median duration grouped by model" #4137 AC4 asked for.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SweepOutcomeModelSummary {
    /// The dispatched model, or `"default"` for records with no explicit
    /// model (mirrors [`crate::telemetry::SweepOutcomeRecord::model`]'s
    /// empty-means-unset contract).
    pub model: String,
    /// Total records attributed to this model.
    pub total: usize,
    /// Records whose `result` is [`crate::telemetry::SweepResult::Success`].
    pub success: usize,
    /// `success / total`; `0.0` for an empty group (never `NaN`).
    pub success_rate: f64,
    /// Median `total_duration_sec` across the group (integer median: the
    /// average of the two central values for an even-sized group).
    pub median_duration_sec: i64,
}

/// Group `records` by model and compute a [`SweepOutcomeModelSummary`] per
/// group, sorted by model name (via the `BTreeMap` grouping key) for
/// deterministic output. This is the query #4137 asked for — "success rate
/// and median duration grouped by model" — computed locally over the durable
/// journal, no exporter or database required.
#[must_use]
pub fn summarize_by_model(
    records: &[telemetry::SweepOutcomeRecord],
) -> Vec<SweepOutcomeModelSummary> {
    let mut groups: std::collections::BTreeMap<String, Vec<&telemetry::SweepOutcomeRecord>> =
        std::collections::BTreeMap::new();
    for record in records {
        let key = record
            .model
            .clone()
            .unwrap_or_else(|| "default".to_string());
        groups.entry(key).or_default().push(record);
    }
    groups
        .into_iter()
        .map(|(model, recs)| {
            let total = recs.len();
            let success = recs
                .iter()
                .filter(|r| r.result == telemetry::SweepResult::Success)
                .count();
            #[allow(clippy::cast_precision_loss)]
            let success_rate = if total == 0 {
                0.0
            } else {
                success as f64 / total as f64
            };
            let mut durations: Vec<i64> = recs.iter().map(|r| r.total_duration_sec).collect();
            durations.sort_unstable();
            SweepOutcomeModelSummary {
                model,
                total,
                success,
                success_rate,
                median_duration_sec: median_i64(&durations),
            }
        })
        .collect()
}

/// Integer median of an already-sorted slice: the middle value for an odd
/// length, the average of the two central values for an even length. `0` for
/// an empty slice.
#[must_use]
fn median_i64(sorted: &[i64]) -> i64 {
    let len = sorted.len();
    if len == 0 {
        return 0;
    }
    let mid = len / 2;
    if len.is_multiple_of(2) {
        (sorted[mid - 1] + sorted[mid]) / 2
    } else {
        sorted[mid]
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::tempdir;

    fn record(issue: u32, outcome: &str) -> OutcomeRecord {
        OutcomeRecord {
            timestamp: Utc::now(),
            repo: "/repo/a".to_string(),
            issue,
            sweep_id: format!("sweep-issue-{issue}-0"),
            outcome: outcome.to_string(),
            exit_code: Some(78),
            death_class: Some("preflight-token-selection-failed".to_string()),
            token_name: "agent-1".to_string(),
            duration_sec: 1,
        }
    }

    #[test]
    fn append_then_read_all_round_trips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sweep-outcomes.jsonl");

        append_outcome(&path, &record(1, "crashed")).unwrap();
        append_outcome(&path, &record(2, "exited")).unwrap();

        let records = read_all(&path);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].issue, 1);
        assert_eq!(records[0].outcome, "crashed");
        assert_eq!(records[0].death_class.as_deref(), Some("preflight-token-selection-failed"));
        assert_eq!(records[1].issue, 2);
        assert_eq!(records[1].outcome, "exited");
    }

    #[test]
    fn append_creates_parent_dirs() {
        let dir = tempdir().unwrap();
        let path = dir
            .path()
            .join("nested")
            .join("logs")
            .join("sweep-outcomes.jsonl");
        append_outcome(&path, &record(1, "crashed")).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn read_all_missing_file_is_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sweep-outcomes.jsonl");
        assert!(read_all(&path).is_empty());
    }

    #[test]
    fn read_all_skips_malformed_lines() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sweep-outcomes.jsonl");
        std::fs::write(&path, "{ not json }\n").unwrap();
        append_outcome(&path, &record(1, "crashed")).unwrap();
        let records = read_all(&path);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].issue, 1);
    }

    #[test]
    fn rotate_on_oversized_journal_preserves_one_backup_generation() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sweep-outcomes.jsonl");

        // Seed a file already past the size cap so the NEXT append rotates it
        // before writing the new line.
        let padding = "x".repeat(MAX_JOURNAL_BYTES as usize + 1);
        std::fs::write(&path, format!("{padding}\n")).unwrap();

        append_outcome(&path, &record(99, "crashed")).unwrap();

        let backup = dir.path().join("sweep-outcomes.jsonl.1");
        assert!(backup.exists(), "oversized journal should be rotated to a .1 backup");
        assert!(std::fs::read_to_string(&backup).unwrap().contains(&padding));

        // The live file now contains ONLY the fresh line, not the old padding.
        let records = read_all(&path);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].issue, 99);
    }

    #[test]
    fn rotate_on_stale_first_line_even_when_small() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sweep-outcomes.jsonl");

        let mut stale = record(1, "crashed");
        stale.timestamp = Utc::now() - chrono::Duration::days(MAX_JOURNAL_AGE_DAYS + 1);
        std::fs::write(&path, format!("{}\n", serde_json::to_string(&stale).unwrap())).unwrap();

        append_outcome(&path, &record(2, "exited")).unwrap();

        let backup = dir.path().join("sweep-outcomes.jsonl.1");
        assert!(backup.exists(), "a journal whose oldest line is stale should rotate");
        let records = read_all(&path);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].issue, 2, "live file should only carry the fresh line post-rotation");
    }

    #[test]
    fn default_outcomes_path_is_under_loom_logs() {
        let workspace = Path::new("/workspace/repo");
        let path = default_outcomes_path(workspace);
        assert_eq!(
            path,
            workspace
                .join(".loom")
                .join("logs")
                .join("sweep-outcomes.jsonl")
        );
    }

    // ------------------------------------------------------------------
    // `sweep.outcome` telemetry journal (Issue #4704).
    // ------------------------------------------------------------------

    fn outcome_envelope(
        issue: u32,
        model: Option<&str>,
        result: telemetry::SweepResult,
        duration_sec: i64,
    ) -> telemetry::TelemetryEnvelope {
        telemetry::TelemetryEnvelope::new(
            "host-test",
            telemetry::TelemetryRecord::SweepOutcome(telemetry::SweepOutcomeRecord {
                repo: "rjwalters/loom".to_string(),
                visibility: telemetry::RepoVisibility::Public,
                issue,
                sweep_id: format!("sweep-issue-{issue}-0"),
                model: model.map(str::to_string),
                effort: None,
                config: std::collections::BTreeMap::new(),
                phase_durations: Vec::new(),
                total_duration_sec: duration_sec,
                result,
                pr_number: None,
            }),
        )
    }

    #[test]
    fn append_telemetry_then_read_all_round_trips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sweep-outcome-telemetry.jsonl");

        append_outcome_telemetry(
            &path,
            &outcome_envelope(1, Some("opus"), telemetry::SweepResult::Success, 100),
        )
        .unwrap();
        append_outcome_telemetry(
            &path,
            &outcome_envelope(2, Some("sonnet"), telemetry::SweepResult::Failure, 50),
        )
        .unwrap();

        let envelopes = read_all_outcome_telemetry(&path);
        assert_eq!(envelopes.len(), 2);

        let records = read_all_sweep_outcomes(&path);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].issue, 1);
        assert_eq!(records[0].model.as_deref(), Some("opus"));
        assert_eq!(records[1].issue, 2);
        assert_eq!(records[1].result, telemetry::SweepResult::Failure);
    }

    #[test]
    fn telemetry_journal_survives_missing_file_and_skips_malformed_lines() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sweep-outcome-telemetry.jsonl");
        assert!(read_all_outcome_telemetry(&path).is_empty());
        assert!(read_all_sweep_outcomes(&path).is_empty());

        std::fs::write(&path, "{ not json }\n").unwrap();
        append_outcome_telemetry(
            &path,
            &outcome_envelope(3, None, telemetry::SweepResult::Cancelled, 10),
        )
        .unwrap();
        let records = read_all_sweep_outcomes(&path);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].issue, 3);
        assert_eq!(records[0].model, None);
    }

    #[test]
    fn telemetry_journal_rotates_on_oversized_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sweep-outcome-telemetry.jsonl");

        let padding = "x".repeat(MAX_JOURNAL_BYTES as usize + 1);
        std::fs::write(&path, format!("{padding}\n")).unwrap();

        append_outcome_telemetry(
            &path,
            &outcome_envelope(99, Some("opus"), telemetry::SweepResult::Success, 1),
        )
        .unwrap();

        let backup = dir.path().join("sweep-outcome-telemetry.jsonl.1");
        assert!(backup.exists(), "oversized telemetry journal should rotate to a .1 backup");
        let records = read_all_sweep_outcomes(&path);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].issue, 99);
    }

    #[test]
    fn telemetry_journal_rotates_on_stale_first_line() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sweep-outcome-telemetry.jsonl");

        let mut stale = outcome_envelope(1, None, telemetry::SweepResult::Failure, 1);
        stale.emitted_at = Utc::now() - chrono::Duration::days(MAX_JOURNAL_AGE_DAYS + 1);
        std::fs::write(&path, format!("{}\n", serde_json::to_string(&stale).unwrap())).unwrap();

        append_outcome_telemetry(
            &path,
            &outcome_envelope(2, None, telemetry::SweepResult::Success, 1),
        )
        .unwrap();

        let backup = dir.path().join("sweep-outcome-telemetry.jsonl.1");
        assert!(backup.exists(), "a telemetry journal whose oldest line is stale should rotate");
        let records = read_all_sweep_outcomes(&path);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].issue, 2);
    }

    // Serialized against `observability::backfill`'s tests: they mutate the
    // process-wide `OUTCOME_TELEMETRY_JOURNAL_PATH_ENV` var (leaked for the
    // duration of their body) via this same constant, and `#[serial]` only
    // serializes against other `#[serial]` tests — a non-serial reader can
    // otherwise observe that leaked override and flake nondeterministically
    // (#5133, same class as `observability::backfill`'s
    // `default_backfill_state_path_is_under_loom_logs`).
    #[test]
    #[serial]
    fn default_outcome_telemetry_path_is_under_loom_logs() {
        let workspace = Path::new("/workspace/repo");
        let path = default_outcome_telemetry_path(workspace);
        assert_eq!(
            path,
            workspace
                .join(".loom")
                .join("logs")
                .join("sweep-outcome-telemetry.jsonl")
        );
    }

    // ------------------------------------------------------------------
    // summarize_by_model — success rate and median duration by model
    // (#4137 AC4, computed locally over the durable journal).
    // ------------------------------------------------------------------

    #[test]
    fn summarize_by_model_computes_success_rate_and_median_duration() {
        let records = vec![
            outcome_envelope(1, Some("opus"), telemetry::SweepResult::Success, 100),
            outcome_envelope(2, Some("opus"), telemetry::SweepResult::Success, 200),
            outcome_envelope(3, Some("opus"), telemetry::SweepResult::Failure, 300),
            outcome_envelope(4, Some("sonnet"), telemetry::SweepResult::Success, 10),
            outcome_envelope(5, None, telemetry::SweepResult::Cancelled, 5),
        ]
        .into_iter()
        .map(|envelope| match envelope.record {
            telemetry::TelemetryRecord::SweepOutcome(r) => r,
            _ => unreachable!(),
        })
        .collect::<Vec<_>>();

        let summary = summarize_by_model(&records);
        // Sorted by model key: "default" < "opus" < "sonnet".
        assert_eq!(summary.len(), 3);

        let default_group = &summary[0];
        assert_eq!(default_group.model, "default");
        assert_eq!(default_group.total, 1);
        assert_eq!(default_group.success, 0);
        assert_eq!(default_group.success_rate, 0.0);
        assert_eq!(default_group.median_duration_sec, 5);

        let opus = &summary[1];
        assert_eq!(opus.model, "opus");
        assert_eq!(opus.total, 3);
        assert_eq!(opus.success, 2);
        assert!((opus.success_rate - (2.0 / 3.0)).abs() < 1e-9);
        assert_eq!(opus.median_duration_sec, 200, "median of [100, 200, 300]");

        let sonnet = &summary[2];
        assert_eq!(sonnet.model, "sonnet");
        assert_eq!(sonnet.total, 1);
        assert_eq!(sonnet.success, 1);
        assert_eq!(sonnet.success_rate, 1.0);
        assert_eq!(sonnet.median_duration_sec, 10);
    }

    #[test]
    fn summarize_by_model_empty_input_is_empty_output() {
        assert!(summarize_by_model(&[]).is_empty());
    }

    #[test]
    fn median_i64_even_length_averages_center_pair() {
        assert_eq!(median_i64(&[10, 20]), 15);
        assert_eq!(median_i64(&[10, 20, 30, 40]), 25);
    }

    #[test]
    fn median_i64_odd_length_is_middle_value() {
        assert_eq!(median_i64(&[7]), 7);
        assert_eq!(median_i64(&[1, 2, 3]), 2);
    }

    #[test]
    fn median_i64_empty_is_zero() {
        assert_eq!(median_i64(&[]), 0);
    }
}
