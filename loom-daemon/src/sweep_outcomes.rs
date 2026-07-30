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

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
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
}
