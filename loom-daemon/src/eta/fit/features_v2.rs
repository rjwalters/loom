//! The `eta-fit/v2` feature set (#10508): the priority-aware successor of
//! [`super::FEATURES`], for a new datestamped heuristic after twin-otter-b.
//!
//! # A separate, versioned contract
//!
//! `eta-fit/v1` ([`super::SCHEMA`], [`super::FEATURES`],
//! [`super::model_features`]) is unchanged: twin-otter and twin-otter-b keep
//! reading it, and every v1 coefficient file keeps its positional meaning. A
//! v2 coefficient vector is positional against [`FEATURES_V2`] instead, and a
//! loader must dispatch on the file's `schema` tag, never reinterpret a v1
//! vector as v2 (or the reverse).
//!
//! [`FEATURES_V2`] is v1's list with position 16 (`starred`, the PR's own
//! star) replaced by `starred_any` (PR **or** linked issue, #10372), then six
//! appended priority columns. Each priority input that can be unknown
//! carries an indicator column, so "unknown" is never encoded as a known 0.
//!
//! # Unknown inputs
//!
//! [`PriorityInputs`] holds `None` for an input whose history does not cover
//! the instant (see [`crate::eta::priority_inputs`] for each rule). The
//! transform writes such an input as `0.0` with its `*_unknown` column at
//! `1.0`; a known input has its indicator at `0.0`. The one shared indicator
//! `star_unknown` covers both `starred_any` and `priority_level`, which are
//! unknown together.

use serde::{Deserialize, Serialize};

use super::features::{model_features, ModelInputs};
use super::N_FEATURES;

/// The schema tag of a v2 coefficient file.
pub const SCHEMA_V2: &str = "eta-fit/v2";

/// The position of `starred` in v1 and of `starred_any` in v2.
pub const STARRED_INDEX: usize = 16;

/// The 26 v2 model features, in coefficient order.
pub const FEATURES_V2: [&str; 26] = [
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
];

/// `FEATURES_V2.len()`.
pub const N_FEATURES_V2: usize = FEATURES_V2.len();

/// The priority inputs at one instant, as [`crate::eta::priority_inputs`]
/// builds them for training and serving alike. `None` = unknown.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct PriorityInputs {
    /// Starred through the PR's own labels or a linked issue (#10372).
    pub starred_any: Option<bool>,
    /// Effective operator priority level: 0 none, 1 star, 2 high (#10307).
    pub priority_level: Option<u8>,
    /// The repo's normalized dispatch rank in the historic fleet roster
    /// ([`crate::eta::repo_priority::repo_rank`]): 0 first, 1 last.
    pub repo_rank: Option<f64>,
    /// Other PRs in the same stage, fleet scope, that cross-repo dispatch
    /// order puts before this one (level, star, starred-at, repo priority,
    /// age, number).
    pub ahead_dispatch_fleet: Option<u32>,
}

/// Everything a v2 model reads at one instant: the v1 raw inputs (whose PR-only
/// `starred` v2 does not read) and the priority inputs.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct ModelInputsV2 {
    /// The v1 raw inputs.
    pub base: ModelInputs,
    /// The priority inputs.
    pub priority: PriorityInputs,
}

/// The v2 features, in [`FEATURES_V2`] order: v1's [`model_features`] with
/// `starred` swapped for `starred_any`, then the priority columns (counts
/// `ln_1p`, the level and rank raw, indicators 0.0/1.0).
#[must_use]
pub fn model_features_v2(m: &ModelInputsV2) -> [f64; N_FEATURES_V2] {
    let v1 = model_features(&m.base);
    let p = &m.priority;
    let unknown = |known: bool| if known { 0.0 } else { 1.0 };
    let mut out = [0.0; N_FEATURES_V2];
    out[..N_FEATURES].copy_from_slice(&v1);
    out[STARRED_INDEX] = f64::from(p.starred_any.unwrap_or(false));
    let star_known = p.starred_any.is_some() && p.priority_level.is_some();
    out[N_FEATURES] = unknown(star_known);
    out[N_FEATURES + 1] = f64::from(p.priority_level.unwrap_or(0));
    out[N_FEATURES + 2] = p.repo_rank.unwrap_or(0.0);
    out[N_FEATURES + 3] = unknown(p.repo_rank.is_some());
    out[N_FEATURES + 4] = f64::from(p.ahead_dispatch_fleet.unwrap_or(0)).ln_1p();
    out[N_FEATURES + 5] = unknown(p.ahead_dispatch_fleet.is_some());
    out
}

/// How many of a set of rows know each priority input: the coverage a fit
/// or a backtest reports next to its numbers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriorityCoverage {
    /// Rows counted.
    pub rows: usize,
    /// Rows whose star and level are known.
    pub star_known: usize,
    /// Rows whose repo rank is known.
    pub repo_rank_known: usize,
    /// Rows whose fleet-wide dispatch position is known.
    pub ahead_dispatch_fleet_known: usize,
}

impl PriorityCoverage {
    /// The coverage of `inputs`.
    #[must_use]
    pub fn of(inputs: &[PriorityInputs]) -> Self {
        let n = |f: fn(&PriorityInputs) -> bool| inputs.iter().filter(|p| f(p)).count();
        PriorityCoverage {
            rows: inputs.len(),
            star_known: n(|p| p.starred_any.is_some() && p.priority_level.is_some()),
            repo_rank_known: n(|p| p.repo_rank.is_some()),
            ahead_dispatch_fleet_known: n(|p| p.ahead_dispatch_fleet.is_some()),
        }
    }
}
