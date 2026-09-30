//! The stage-sample journal, `.loom/logs/eta-stage-samples.jsonl`: every
//! stage boundary the ETA tracker observes, appended the moment it is seen,
//! with its raw fields.
//!
//! Eager and generous by design (operator decision on #9289): a transition is
//! written when it is observed, never batched to a later pass, and an event
//! that may or may not be a stage boundary is written anyway with `stage`
//! absent. Backtests read this file; estimators read only the rows that are
//! complete stage durations ([`JournalEntry::history_sample`]).
//!
//! Rows the sweep-outcome journal already carries (in-sweep phases) are
//! marked `in_sweep` and are not read back as history, so no duration is
//! counted twice. Rotation matches the outcome journal: 5 MiB or 30 days,
//! one `.1` generation.

use super::history::{SampleSource, StageSample, StageSamples, VerdictSample};
use super::{Provenance, Stage};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

/// Schema tag of one journal row.
pub const JOURNAL_SCHEMA: &str = "eta-stage-sample/v1";

/// File name under `<workspace>/.loom/logs/`.
pub const JOURNAL_FILENAME: &str = "eta-stage-samples.jsonl";

/// Test seam: overrides the journal path.
pub const JOURNAL_PATH_ENV: &str = "LOOM_ETA_STAGE_JOURNAL_PATH";

/// One observed boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalEntry {
    /// Always [`JOURNAL_SCHEMA`].
    pub schema: String,
    /// When the tracker observed it.
    pub observed_at: DateTime<Utc>,
    /// What was observed: `sweep.dispatch`, `sweep.phase`, `sweep.terminal`,
    /// `label.transition`, `label.first_seen`, `pr.resolved`, `verdict`.
    pub event: String,
    /// `owner/repo`.
    pub repo: String,
    /// The issue, when known.
    pub issue: Option<u32>,
    /// The PR, when known.
    pub pr_number: Option<u32>,
    /// The sweep, when one was involved.
    pub sweep_id: Option<String>,
    /// The stage this row completes, when it completes one.
    pub stage: Option<Stage>,
    /// When that stage was entered, when observed.
    pub entered_at: Option<DateTime<Utc>>,
    /// When it was left.
    pub left_at: Option<DateTime<Utc>>,
    /// `left_at − entered_at`, when both were observed (never a lower bound).
    pub duration_sec: Option<i64>,
    /// The stage entered next, when known.
    pub next_stage: Option<Stage>,
    /// Judge verdict this row records (`pass`/`fail`), and its 1-based attempt.
    pub verdict: Option<String>,
    /// Attempt the verdict settled.
    pub attempt: Option<u32>,
    /// The sweep-outcome journal also records this duration.
    pub in_sweep: bool,
    /// How late the observation can be, in seconds (a pass interval for a
    /// listing diff, 0 for a bus event).
    pub resolution_sec: Option<i64>,
    /// Raw fields as observed (phase marker, labels, exit code, …).
    pub raw: serde_json::Value,
    /// The observing build.
    pub loom: Provenance,
}

impl JournalEntry {
    /// A row for `event` in `repo`, observed at `observed_at`, all else empty.
    #[must_use]
    pub fn new(event: &str, repo: &str, observed_at: DateTime<Utc>, loom: &Provenance) -> Self {
        JournalEntry {
            schema: JOURNAL_SCHEMA.to_string(),
            observed_at,
            event: event.to_string(),
            repo: repo.to_string(),
            issue: None,
            pr_number: None,
            sweep_id: None,
            stage: None,
            entered_at: None,
            left_at: None,
            duration_sec: None,
            next_stage: None,
            verdict: None,
            attempt: None,
            in_sweep: false,
            resolution_sec: None,
            raw: serde_json::Value::Object(serde_json::Map::new()),
            loom: loom.clone(),
        }
    }

    /// The stage sample this row contributes to history: a complete,
    /// non-negative duration that the sweep-outcome journal does not already
    /// carry. `host` is the host whose journal this is.
    #[must_use]
    pub fn history_sample(&self, host: &str) -> Option<StageSample> {
        if self.in_sweep {
            return None;
        }
        let (stage, duration_sec) = (self.stage?, self.duration_sec?);
        (duration_sec >= 0).then(|| StageSample {
            repo: self.repo.clone(),
            stage,
            duration_sec,
            observed_at: self.observed_at,
            source: SampleSource::StageJournal,
            host: host.to_string(),
        })
    }

    /// The Judge verdict this row contributes to history (external Judge
    /// only; in-sweep verdicts come from `sweep.outcome`).
    #[must_use]
    pub fn history_verdict(&self) -> Option<VerdictSample> {
        if self.in_sweep {
            return None;
        }
        let rejected = match self.verdict.as_deref()? {
            "fail" => true,
            "pass" => false,
            _ => return None,
        };
        Some(VerdictSample {
            repo: self.repo.clone(),
            attempt: self.attempt?,
            rejected,
            observed_at: self.observed_at,
        })
    }
}

/// The journal path for `workspace_root` (env override first).
#[must_use]
pub fn journal_path(workspace_root: &Path) -> PathBuf {
    match std::env::var(JOURNAL_PATH_ENV) {
        Ok(path) if !path.is_empty() => PathBuf::from(path),
        _ => workspace_root
            .join(".loom")
            .join("logs")
            .join(JOURNAL_FILENAME),
    }
}

fn rotated(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(JOURNAL_FILENAME);
    path.with_file_name(format!("{name}.1"))
}

/// Rotate `path` to `.1` when it is over the size cap or its first row is
/// older than the age cap (the sweep-outcome journal's thresholds).
fn rotate_if_needed(path: &Path) -> std::io::Result<()> {
    let Ok(meta) = std::fs::metadata(path) else {
        return Ok(());
    };
    let oversized = meta.len() >= crate::sweep_outcomes::MAX_JOURNAL_BYTES;
    let stale = !oversized
        && first_row(path).is_some_and(|row| {
            Utc::now() - row.observed_at
                > chrono::Duration::days(crate::sweep_outcomes::MAX_JOURNAL_AGE_DAYS)
        });
    if oversized || stale {
        std::fs::rename(path, rotated(path))?;
    }
    Ok(())
}

/// The journal's first (oldest) row, reading one line only.
fn first_row(path: &Path) -> Option<JournalEntry> {
    let file = std::fs::File::open(path).ok()?;
    let mut line = String::new();
    std::io::BufReader::new(file).read_line(&mut line).ok()?;
    serde_json::from_str(line.trim_end()).ok()
}

/// Append `entries` to the journal at `path`, one line each, creating the
/// directory and rotating first when needed.
pub fn append(path: &Path, entries: &[JournalEntry]) -> std::io::Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    rotate_if_needed(path)?;
    let mut text = String::new();
    for entry in entries {
        if let Ok(line) = serde_json::to_string(entry) {
            text.push_str(&line);
            text.push('\n');
        }
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(text.as_bytes())
}

/// Every parseable row of the journal at `path` and its `.1` rotation, oldest
/// generation first. Malformed lines are skipped.
#[must_use]
pub fn read(path: &Path) -> Vec<JournalEntry> {
    let mut rows = Vec::new();
    for file in [rotated(path), path.to_path_buf()] {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        rows.extend(
            text.lines()
                .filter_map(|line| serde_json::from_str::<JournalEntry>(line).ok()),
        );
    }
    rows
}

impl StageSamples {
    /// Add every history sample and verdict `entries` carry; `host` is the
    /// host whose journal they come from.
    pub fn push_journal(&mut self, entries: &[JournalEntry], host: &str) {
        for entry in entries {
            if let Some(sample) = entry.history_sample(host) {
                self.stages.push(sample);
            }
            if let Some(verdict) = entry.history_verdict() {
                self.verdicts.push(verdict);
            }
        }
    }
}
