//! The raw inputs at one instant, and the one transform train (#10245) and
//! serve (#10222) both call to turn them into the 20 model features.
//!
//! Field names match the keys of the #10223 fixture's
//! `evaluation.rows[].input` (which carries the flags as 0/1 integers).

use chrono::{DateTime, Datelike, Timelike, Utc, Weekday};
use serde::{Deserialize, Serialize};
use std::f64::consts::TAU;

use super::N_FEATURES;

/// The cap applied to [`ModelInputs::since_merge_h`] before its log, in hours
/// (one week).
pub const SINCE_MERGE_CAP_H: f64 = 168.0;

/// Everything the models read about one PR at one instant, before any
/// transform.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct ModelInputs {
    /// Hours in the current stage (`age_sec / 3600`); not clamped.
    pub age_h: f64,
    /// PRs in the same repo and stage that entered it earlier.
    pub ahead: u32,
    /// Other PRs in the same repo and stage.
    pub n_stage_repo: u32,
    /// Exits from this stage in this repo over the last 6 h.
    pub exits_repo_6h: u32,
    /// Exits from this stage in this repo over the last 24 h.
    pub exits_repo_24h: u32,
    /// Exits from this stage fleet-wide over the last 6 h.
    pub exits_fleet_6h: u32,
    /// Merges in the repo over the last 24 h.
    pub merges_repo_24h: u32,
    /// Merges fleet-wide over the last 6 h.
    pub merges_fleet_6h: u32,
    /// Hours since the repo's last merge; capped at [`SINCE_MERGE_CAP_H`]
    /// inside [`model_features`].
    pub since_merge_h: f64,
    /// Other PRs in the same stage, fleet-wide.
    pub n_stage_fleet: u32,
    /// Fractional UTC hour of day, in `[0, 24)` (see [`clock`]).
    pub hour_utc: f64,
    /// Saturday or Sunday, UTC (see [`clock`]).
    pub weekend: bool,
    /// Fix rounds so far.
    pub rework: u32,
    /// Carries an operator label.
    pub op_hold: bool,
    /// Carries `loom:sequenced`.
    pub sequenced: bool,
    /// Carries `loom:operator-priority`.
    pub starred: bool,
    /// Carries `loom:merge-conflict`.
    pub conflict: bool,
    /// Carries `loom:ci-failure`.
    pub ci_fail: bool,
    /// Carries `loom:blocked`.
    pub blocked: bool,
}

/// The 20 model features, in [`super::FEATURES`] order:
///
/// - `ln_1p` of `age_h`, of every count, and of `min(since_merge_h, 168)`;
/// - `sin` and `cos` of `2π·hour_utc/24`;
/// - `weekend` and the six label flags as 0.0 or 1.0;
/// - `rework` **raw**, not logged (the generator's `rework = ⌊3u⌋`).
#[must_use]
pub fn model_features(m: &ModelInputs) -> [f64; N_FEATURES] {
    let count = |n: u32| f64::from(n).ln_1p();
    let angle = TAU * m.hour_utc / 24.0;
    [
        m.age_h.ln_1p(),
        count(m.ahead),
        count(m.n_stage_repo),
        count(m.exits_repo_6h),
        count(m.exits_repo_24h),
        count(m.exits_fleet_6h),
        count(m.merges_repo_24h),
        count(m.merges_fleet_6h),
        m.since_merge_h.min(SINCE_MERGE_CAP_H).ln_1p(),
        count(m.n_stage_fleet),
        angle.sin(),
        angle.cos(),
        f64::from(m.weekend),
        f64::from(m.rework),
        f64::from(m.op_hold),
        f64::from(m.sequenced),
        f64::from(m.starred),
        f64::from(m.conflict),
        f64::from(m.ci_fail),
        f64::from(m.blocked),
    ]
}

/// The clock features at `at`: (whole seconds since midnight UTC / 3600, is
/// Saturday or Sunday UTC).
#[must_use]
pub fn clock(at: DateTime<Utc>) -> (f64, bool) {
    let hour = f64::from(at.num_seconds_from_midnight()) / 3600.0;
    let weekend = matches!(at.weekday(), Weekday::Sat | Weekday::Sun);
    (hour, weekend)
}
