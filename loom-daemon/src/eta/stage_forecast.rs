//! Per-stage forecasts and their error attribution (#10929).
//!
//! An estimate says "lands at T". This module adds a forecast for each stage
//! still ahead: when the item enters it and how long it stays. When the
//! estimate resolves, the module splits its error across those stages.
//!
//! # Predictions ([`StagePrediction`])
//!
//! The path simulator ([`super::simulate::run`]) already draws every stage on
//! every path. The forecast is read off those same draws, so it uses no new
//! uniforms and leaves every quantile and stage mark byte-identical. All
//! values are whole seconds measured from the estimate's `as_of`.
//!
//! - `entry_p50` / `entry_p90`: the path's first entry into the stage,
//!   nearest-rank over the paths that visit it. Unlike a `stage_marks`
//!   terminal mark (the completion time), this is always the *entry*.
//! - `dwell_p50` / `dwell_p90`: the total time in the stage across all visits,
//!   over the same visiting paths. The current stage's dwell is what remains
//!   of it.
//! - `reach_pct`: the percentage of paths that visit the stage. Entry and
//!   dwell are conditional on reaching it.
//! - `alloc`: the stage's share of the p50 total, from the mean stage times of
//!   the paths ranked p40–p60 (the `contributions.p50_share` band). Every
//!   stage's `alloc` sums to the simulated p50 exactly, less an applied
//!   stall's share. This is the baseline [`attribute`] measures against.
//!
//! Only the path-engine heuristics simulate stages, so only they fill the
//! map. Every other heuristic leaves it empty, which means "not modelled".
//! It is never a guess.
//!
//! # Attribution ([`Attribution`])
//!
//! The error of a resolved estimate is `actual − p50`. Each stage contributes
//! `actual_dwell − alloc`, where `actual_dwell` is the time the item actually
//! spent in that stage after `as_of`, summed over visits
//! (`score.stages_actual`). Whatever the stages do not explain is
//! `unattributed_sec`:
//!
//! - time the tracker did not observe exactly (an inexact entry, a gap
//!   between a verdict and its label);
//! - an applied stall's term;
//! - a calibration or regime shift between the simulated and the served p50.
//!
//! So, by construction:
//!
//! ```text
//! Σ stages[s].contribution_sec + unattributed_sec == error_sec
//! ```
//!
//! On a fully observed, unshifted path, `unattributed_sec` is zero.

use super::score::{EstimateSummary, Score, StageActual};
use super::Stage;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One stage's forecast, in seconds from `as_of` (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagePrediction {
    /// First entry, median over the visiting paths.
    pub entry_p50: i64,
    /// First entry, 90th percentile over the visiting paths.
    pub entry_p90: i64,
    /// Total time in the stage, median over the visiting paths.
    pub dwell_p50: i64,
    /// Total time in the stage, 90th percentile over the visiting paths.
    pub dwell_p90: i64,
    /// The stage's share of the p50 total: the attribution baseline.
    pub alloc: i64,
    /// Percentage of paths that visit the stage (0–100).
    pub reach_pct: u8,
}

/// The per-stage forecast of one estimate, keyed by the fixed [`Stage`] enum.
pub type StagePredictions = BTreeMap<Stage, StagePrediction>;

/// Split `target` seconds across stages in proportion to `weights`, as
/// whole seconds summing to `target` exactly. Uses the largest-remainder
/// method, with ties broken by stage order. A zero or negative total
/// weight gives every stage zero.
#[must_use]
pub fn apportion(weights: &[(Stage, f64)], target: i64) -> BTreeMap<Stage, i64> {
    let total: f64 = weights.iter().map(|(_, w)| w.max(0.0)).sum();
    let mut out: BTreeMap<Stage, i64> = weights.iter().map(|(s, _)| (*s, 0)).collect();
    if total <= 0.0 || target <= 0 {
        return out;
    }
    let mut remainders = Vec::with_capacity(weights.len());
    let mut assigned = 0_i64;
    for (stage, weight) in weights {
        let raw = weight.max(0.0) / total * target as f64;
        let floor = raw.floor() as i64;
        out.insert(*stage, floor);
        assigned += floor;
        remainders.push((raw - raw.floor(), *stage));
    }
    remainders.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    let deficit = usize::try_from((target - assigned).max(0)).unwrap_or(0);
    for (_, stage) in remainders.into_iter().cycle().take(deficit) {
        *out.entry(stage).or_insert(0) += 1;
    }
    out
}

/// One stage's predicted vs actual time after `as_of`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageError {
    /// Predicted first entry (`entry_p50`), seconds from `as_of`. Absent
    /// when the estimate did not forecast the stage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predicted_entry_sec: Option<i64>,
    /// Predicted time in the stage (`alloc`); `0` when not forecast.
    pub predicted_dwell_sec: i64,
    /// Actual first entry after `as_of`, seconds from `as_of` (`0` for
    /// the stage the item was already in). Absent when no visit was observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_entry_sec: Option<i64>,
    /// Actual observed time in the stage after `as_of`, summed over visits.
    pub actual_dwell_sec: i64,
    /// `actual_dwell_sec − predicted_dwell_sec`.
    pub contribution_sec: i64,
}

/// A resolved estimate's error, split by stage (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attribution {
    /// Every stage forecast or observed after `as_of`.
    pub stages: BTreeMap<Stage, StageError>,
    /// `error_sec − Σ contribution_sec`.
    pub unattributed_sec: i64,
    /// The stage with the largest `|contribution_sec|` (ties: stage order).
    /// Absent when every contribution is zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dominant_stage: Option<Stage>,
}

/// Attribute `error_sec` of an estimate made at `as_of` across its
/// forecast stages, against the stages observed after it. `None` when the
/// estimate forecast no stage.
#[must_use]
pub fn attribute(
    predictions: &StagePredictions,
    as_of: DateTime<Utc>,
    error_sec: i64,
    observed: &[StageActual],
) -> Option<Attribution> {
    if predictions.is_empty() {
        return None;
    }
    let mut stages: BTreeMap<Stage, StageError> = predictions
        .iter()
        .map(|(stage, p)| {
            (
                *stage,
                StageError {
                    predicted_entry_sec: Some(p.entry_p50),
                    predicted_dwell_sec: p.alloc,
                    actual_entry_sec: None,
                    actual_dwell_sec: 0,
                    contribution_sec: 0,
                },
            )
        })
        .collect();
    for visit in observed {
        let start = visit.entered_at.max(as_of);
        let dwell = (visit.left_at - start).num_seconds().max(0);
        let entry = (start - as_of).num_seconds().max(0);
        let row = stages.entry(visit.stage).or_insert(StageError {
            predicted_entry_sec: None,
            predicted_dwell_sec: 0,
            actual_entry_sec: None,
            actual_dwell_sec: 0,
            contribution_sec: 0,
        });
        row.actual_dwell_sec += dwell;
        row.actual_entry_sec = Some(row.actual_entry_sec.map_or(entry, |e| e.min(entry)));
    }
    let mut explained = 0_i64;
    let mut dominant: Option<(i64, Stage)> = None;
    for (stage, row) in &mut stages {
        row.contribution_sec = row.actual_dwell_sec - row.predicted_dwell_sec;
        explained += row.contribution_sec;
        let size = row.contribution_sec.abs();
        if size > 0 && dominant.is_none_or(|(best, _)| size > best) {
            dominant = Some((size, *stage));
        }
    }
    Some(Attribution {
        stages,
        unattributed_sec: error_sec - explained,
        dominant_stage: dominant.map(|(_, stage)| stage),
    })
}

/// [`attribute`] for a scored outcome: `None` unless the estimate forecast
/// stages and the outcome has an `error_sec` (never for `abandoned`,
/// `censored` or a refusal).
#[must_use]
pub fn attribute_scored(estimate: &EstimateSummary, score: &Score) -> Option<Attribution> {
    attribute(
        &estimate.stage_predictions,
        estimate.as_of,
        score.error_sec?,
        &score.stages_actual,
    )
}
