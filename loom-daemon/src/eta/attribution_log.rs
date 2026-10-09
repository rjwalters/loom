//! The stage-attribution log, `.loom/state/eta/attribution.jsonl` (#10957):
//! one row per scored `land` outcome that carries a per-stage
//! [`Attribution`] (#10929), kept so the nightly fold can roll the error up
//! by heuristic and stage with no forge call.
//!
//! The attribution otherwise rides only on the OTLP `eta.outcome` record, and
//! the nightly fold's replay re-predicts from `ReplayCase`s that carry no
//! per-stage visits, so it cannot recompute it. There is no backfill: rows
//! exist only from the day this log shipped, so the first 7-day window is
//! partial.
//!
//! Same shape and compaction as [`super::calibration_log`]. This module is
//! the I/O half; [`super::stage_attribution_fold`] is the pure half.

use super::stage_forecast::{attribute_scored, Attribution};
use super::tracker::Resolved;
use super::Kind;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// File name under `<workspace>/.loom/state/eta/`.
pub const FILENAME: &str = "attribution.jsonl";

/// Rows kept after a compaction (the newest). Compaction runs when the file
/// holds twice this many.
pub const MAX_ROWS: usize = 20_000;

/// One scored outcome's stage attribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttributionRow {
    /// The estimate scored.
    pub estimate_id: String,
    /// The heuristic that made it.
    pub heuristic: String,
    /// `land` today.
    pub kind: Kind,
    /// `owner/repo`.
    pub repo: String,
    /// The issue.
    pub issue: u32,
    /// The PR, when one existed: with `issue` and `actual_at` it names the
    /// resolved case. Absent in a row written before it was logged.
    #[serde(default)]
    pub pr_number: Option<u32>,
    /// When the estimate was made.
    pub as_of: DateTime<Utc>,
    /// When the outcome happened.
    pub actual_at: DateTime<Utc>,
    /// When this daemon scored it: the knowable-at instant.
    pub observed_at: DateTime<Utc>,
    /// `actual − p50`, seconds.
    pub error_sec: i64,
    /// The error split by stage.
    pub attribution: Attribution,
}

impl AttributionRow {
    /// The row for a resolved outcome: `land` only, with an `error_sec` and
    /// an attribution (so never abandoned, censored or refused).
    #[must_use]
    pub fn from_resolved(resolved: &Resolved, observed_at: DateTime<Utc>) -> Option<Self> {
        if resolved.estimate.kind != Kind::Land {
            return None;
        }
        let attribution = attribute_scored(&resolved.estimate, &resolved.score)?;
        Some(AttributionRow {
            estimate_id: resolved.estimate.estimate_id.clone(),
            heuristic: resolved.estimate.heuristic.clone(),
            kind: resolved.estimate.kind,
            repo: resolved.estimate.repo.clone(),
            issue: resolved.estimate.issue,
            pr_number: resolved.estimate.pr_number,
            as_of: resolved.estimate.as_of,
            actual_at: resolved.score.actual_at,
            observed_at,
            error_sec: resolved.score.error_sec?,
            attribution,
        })
    }
}

/// The log for `workspace_root`.
#[must_use]
pub fn path(workspace_root: &Path) -> PathBuf {
    workspace_root
        .join(".loom")
        .join("state")
        .join("eta")
        .join(FILENAME)
}

/// Every parseable row; an absent file is no rows, a malformed line is
/// skipped.
#[must_use]
pub fn read(path: &Path) -> Vec<AttributionRow> {
    std::fs::read_to_string(path)
        .map(|text| {
            text.lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect()
        })
        .unwrap_or_default()
}

fn render(rows: &[AttributionRow]) -> std::io::Result<String> {
    let mut text = String::new();
    for row in rows {
        text.push_str(&serde_json::to_string(row).map_err(std::io::Error::other)?);
        text.push('\n');
    }
    Ok(text)
}

/// Append `rows`, compacting to the newest [`MAX_ROWS`] (by `observed_at`)
/// once the file holds twice that many.
///
/// # Errors
///
/// The directory could not be created, or the append/compaction failed.
pub fn append(path: &Path, rows: &[AttributionRow]) -> std::io::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(render(rows)?.as_bytes())?;
    let lines = std::fs::read(path)?.iter().filter(|&&b| b == b'\n').count();
    if lines < 2 * MAX_ROWS {
        return Ok(());
    }
    let mut all = read(path);
    all.sort_by_key(|r| r.observed_at);
    let text = render(&all[all.len() - MAX_ROWS..])?;
    let tmp = path.with_extension("jsonl.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}
