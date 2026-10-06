//! The `eta-fit/v3` feature set (#10521): v2 plus the friction predictors of
//! [`crate::eta::loop_features`], for a new datestamped heuristic registered
//! after the latest shadow id.
//!
//! # A separate, versioned contract
//!
//! `eta-fit/v1` and `eta-fit/v2` are unchanged: their ids, coefficient files
//! and fixtures keep their positional meaning, and a loader dispatches on the
//! file's `schema` tag, never reinterpreting one version's vector as
//! another's. A v3 vector is positional against [`FEATURES_V3`]: the 26 v2
//! columns, then the 11 [`LOOP_FEATURES`] columns in their declared order.
//! `age_h` keeps its meaning (time since the last stage entry) in column 0;
//! the stage-age fix is the appended `log_cum_stage` (time in the stage
//! summed across every loop, `ln(1 + h)`).
//!
//! # One builder
//!
//! [`model_features_v3`] is the only transform. Training reaches it through
//! [`training_inputs_v3`] (the row's v1 inputs, its priority inputs and its
//! [`LoopFeatures`], all recorded at `t - lag` by [`super::rows::build`]);
//! serving builds a [`ModelInputsV3`] from the tracker's reads of the same
//! builders. Neither side owns a copy of the arithmetic.
//!
//! # Unknown inputs
//!
//! Every loop feature that can be unknown (Judge rate, file overlap, own CI)
//! has its `*_known` column, so a source that is not logged yet (file lists,
//! CI today) is encoded as 0 with its indicator at 0, never as "no overlap".

use serde::{Deserialize, Serialize};

use super::features_v2::{model_features_v2, ModelInputsV2, N_FEATURES_V2};
use crate::eta::loop_features::{loop_vector, LoopFeatures, N_LOOP_FEATURES};

/// The schema tag of a v3 coefficient file.
pub const SCHEMA_V3: &str = "eta-fit/v3";

/// The 37 v3 model features, in coefficient order: [`super::features_v2::FEATURES_V2`]
/// then [`crate::eta::loop_features::LOOP_FEATURES`] (pinned by a test).
pub const FEATURES_V3: [&str; 37] = [
    "log_age",
    "log_ahead",
    "log_n_stage_repo",
    "log_exits_repo_6h",
    "log_exits_repo_24h",
    "log_exits_fleet_6h",
    "log_merges_repo_24h",
    "log_merges_fleet_6h",
    "log_since_merge",
    "log_n_stage_fleet",
    "hour_sin",
    "hour_cos",
    "weekend",
    "rework",
    "op_hold",
    "sequenced",
    "starred_any",
    "conflict",
    "ci_fail",
    "blocked",
    "star_unknown",
    "priority_level",
    "repo_rank",
    "repo_rank_unknown",
    "log_ahead_dispatch_fleet",
    "ahead_dispatch_fleet_unknown",
    "log_cum_stage",
    "log_review_requests",
    "log_approvals_lost",
    "judge_reject_rate_7d",
    "judge_rate_known",
    "log_overlap_prs",
    "log_overlap_files",
    "overlap_known",
    "own_ci_failed",
    "ci_known",
    "stage_looped",
];

/// `FEATURES_V3.len()`.
pub const N_FEATURES_V3: usize = FEATURES_V3.len();

/// Everything a v3 model reads at one instant.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ModelInputsV3 {
    /// The v2 inputs (v1 raw inputs and priority inputs).
    pub v2: ModelInputsV2,
    /// The friction predictors.
    pub loops: LoopFeatures,
}

/// The v3 features, in [`FEATURES_V3`] order.
#[must_use]
pub fn model_features_v3(m: &ModelInputsV3) -> [f64; N_FEATURES_V3] {
    let mut out = [0.0; N_FEATURES_V3];
    out[..N_FEATURES_V2].copy_from_slice(&model_features_v2(&m.v2));
    out[N_FEATURES_V2..].copy_from_slice(&loop_vector(&m.loops));
    debug_assert_eq!(N_FEATURES_V2 + N_LOOP_FEATURES, N_FEATURES_V3);
    out
}

/// The v3 inputs of training row `i` of an assembled fit: the row's own
/// recorded inputs, never recomputed. `None` if `i` is out of range.
#[must_use]
pub fn training_inputs_v3(a: &super::rows::Assembled, i: usize) -> Option<ModelInputsV3> {
    Some(ModelInputsV3 {
        v2: ModelInputsV2 {
            base: a.rows.get(i)?.inputs,
            priority: *a.priority_inputs.get(i)?,
        },
        loops: a.loops.get(i)?.clone(),
    })
}
