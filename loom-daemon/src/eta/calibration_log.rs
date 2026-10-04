//! The calibration outcome log, `.loom/state/eta/calibration.jsonl` (#10207):
//! every **landed** `land-v2` estimate the tracker scored, kept so the
//! recalibrating heuristic can refit its interval from `land-v2`'s own track
//! record. Scored outcomes are otherwise OTLP-only, so without this log a
//! daemon restart would forget every landing.
//!
//! This module is the I/O half; [`super::recalibrate`] is the pure half and
//! never sees a path. The still-open half of the evidence is not logged here
//! at all: it is the tracker's pending store, converted on read.

use super::heuristics::CALIBRATION_BASE;
use super::recalibrate::CalibrationObservation;
use super::score::EstimateSummary;
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

/// File name under `<workspace>/.loom/state/eta/`.
pub const FILENAME: &str = "calibration.jsonl";

/// Rows kept after a compaction (the newest). Compaction runs when the file
/// holds twice this many, so a row costs one append, not one rewrite.
pub const MAX_ROWS: usize = 20_000;

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
/// skipped — a lost row costs a wider fallback, never a wrong estimate.
#[must_use]
pub fn read(path: &Path) -> Vec<CalibrationObservation> {
    std::fs::read_to_string(path)
        .map(|text| {
            text.lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Append `rows`, compacting to the newest [`MAX_ROWS`] (by `as_of`) once the
/// file holds twice that many.
///
/// # Errors
///
/// The directory could not be created, or the append/compaction failed.
pub fn append(path: &Path, rows: &[CalibrationObservation]) -> std::io::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = String::new();
    for row in rows {
        text.push_str(&serde_json::to_string(row).map_err(std::io::Error::other)?);
        text.push('\n');
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(text.as_bytes())?;
    let lines = std::fs::read(path)?.iter().filter(|&&b| b == b'\n').count();
    if lines < 2 * MAX_ROWS {
        return Ok(());
    }
    let mut all = read(path);
    all.sort_by_key(|r| r.as_of);
    let keep = &all[all.len() - MAX_ROWS..];
    let mut text = String::new();
    for row in keep {
        text.push_str(&serde_json::to_string(row).map_err(std::io::Error::other)?);
        text.push('\n');
    }
    let tmp = path.with_extension("jsonl.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// The landed estimates in `scored` plus every still-open
/// [`CALIBRATION_BASE`] estimate in `pending`, deduplicated by estimate id
/// (a scored row wins: it carries the landing).
#[must_use]
pub fn combine(
    scored: Vec<CalibrationObservation>,
    pending: &[EstimateSummary],
) -> Vec<CalibrationObservation> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::with_capacity(scored.len());
    for row in scored {
        if seen.insert(row.estimate_id.clone()) {
            out.push(row);
        }
    }
    for summary in pending {
        if summary.heuristic != CALIBRATION_BASE || seen.contains(&summary.estimate_id) {
            continue;
        }
        if let Some(row) = CalibrationObservation::from_pending(summary) {
            seen.insert(row.estimate_id.clone());
            out.push(row);
        }
    }
    out
}

/// What `workspace_root` holds: the log plus the persisted pending store —
/// for a reader with no live tracker (`eta view`/`eta list`).
#[must_use]
pub fn load(workspace_root: &Path) -> Vec<CalibrationObservation> {
    let pending: Vec<EstimateSummary> =
        std::fs::read_to_string(crate::observability::eta::pending_path(workspace_root))
            .map(|text| {
                text.lines()
                    .filter_map(|line| serde_json::from_str(line).ok())
                    .collect()
            })
            .unwrap_or_default();
    combine(read(&path(workspace_root)), &pending)
}
