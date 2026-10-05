//! `land-2026-10-04-twin-otter`'s pure evaluation core (#10222, Slice A).
//!
//! Twin-otter blends two fitted models of the time a PR still needs to land,
//! both read from an `eta-fit/v1` coefficient set ([`crate::eta::fit`]):
//!
//! 1. **Stage by stage.** The current stage's exit hazard is evaluated at
//!    every future step (`step_h`, 30 minutes) with the age **and** the clock
//!    (hour of day, weekend) advancing and every other feature frozen at
//!    `as_of`. That gives the survival curve `S_1..S_steps`. A Monte Carlo
//!    over [`EvalConfig::paths`] paths draws the time to leave the current
//!    stage from that curve, then walks the later stages through the path
//!    statistics (next-stage probabilities and Kaplan–Meier dwell curves)
//!    until `merged`. Its p25/p50/p75/p90 are nearest-rank over the path
//!    totals.
//! 2. **Direct.** The pooled log-normal (AFT) model's quantiles,
//!    `exp(β·Z + σ_stage·Φ⁻¹(τ))`.
//! 3. **Blend.** The elementwise mean of the two parts, then a cumulative
//!    max so the quantiles never decrease.
//!
//! [`evaluate`] is the whole computation. This slice touches no registry,
//! explanation or tracker code: the `EstimateInput` adapter, loading the
//! coefficients, and shadow registration are #10243.
//!
//! # Purity
//!
//! [`evaluate`] is a function of its three arguments only. It reads no
//! clock, file, environment or global, and its only randomness is
//! [`SplitMix64`](crate::eta::simulate::SplitMix64) seeded with
//! [`EvalConfig::seed`]. This module's three
//! source files are in the purity scan (`eta/tests/fleet.rs`).
//!
//! # Semantics pinned by the #10223 parity fixture
//!
//! - Features are built by name, in the model's feature order, through the
//!   shared train/serve transform ([`crate::eta::fit::model_features`] and
//!   [`crate::eta::fit::clock`]). At step `j` (`t_j = as_of + j·step_h`) the
//!   age is `age_h + j·step_h` and the clock is read at `t_j`; the counts and
//!   `since_merge_h` stay as they were at `as_of`.
//! - `z = (x − mu) / sd` with the stored `sd`, and no epsilon.
//! - `h_j = sigmoid(intercept + coef·z_j)` and `S_k = Π_{j<k} (1 − h_j)`.
//! - Later stages draw their dwell with a **step-function** inverse of the
//!   Kaplan–Meier curve (the first `t_i` with `s_i ≤ u`), never an
//!   interpolation.
//! - Quantiles are nearest rank, `rank = ⌈τ·M⌉` clamped to `[1, M]`.
//!
//! # Decisions the fixture does not pin
//!
//! - **Cap.** Each part is capped at `cap_h` (336 h) before the blend: path
//!   totals are capped as they are drawn, and each AFT quantile is
//!   `min(q, cap_h)`. A path that survives every step of the horizon
//!   contributes exactly `cap_h`.
//! - **Age clamp** ([`EvalConfig::age_clamp_h`], off by default). When set,
//!   `min(age, clamp)` replaces the age at every step and in the AFT.
//! - **Missing counts.** A count or `since_merge_h` that is `None` is
//!   imputed at standardized 0 (the training mean) in each model and named
//!   in [`Evaluation::imputed`]. It is never a refusal.
//! - **A Kaplan–Meier curve that stops above `u`** (censoring kept it from
//!   reaching 0) gives its last stored time, the largest time the data has
//!   evidence about. This is the `grid::km_quantile` convention.
//! - **Hop guard.** A path that makes [`HOP_GUARD`] transitions without
//!   merging contributes `cap_h`, so a zero-dwell loop cannot spin.
//! - **`since_merge_h`** is capped at 168 h inside the shared transform, as
//!   in training. Every fixture row is far below the cap.
//!
//! # The seed, and why a refresh does not redraw (#10243)
//!
//! Each path draws from its **own** `SplitMix64` stream, seeded by one
//! draw of a master stream that is seeded with [`EvalConfig::seed`]. So a
//! path's draws depend on `(seed, path index)` alone, never on how many
//! draws the paths before it used. Within a path the order is fixed:
//!
//! 1. `u1` places the exit from the current stage. Let
//!    `k = #{k ∈ 1..=steps : S_k > u1}`. A path with `k = steps` survives the
//!    horizon and contributes `cap_h`, with no more draws. Otherwise the
//!    first dwell is `(k + f)·step_h`, where `f = (S_k − u1) / (S_k − S_{k+1})`
//!    and `S_0 = 1`.
//! 2. Then, while the total is below `cap_h`: one `u` picks the next stage,
//!    accumulating the next-stage probabilities in key order (the last entry
//!    absorbs rounding; `merged` ends the path), and one `u` draws that
//!    stage's Kaplan–Meier dwell.
//!
//! Given `k`, `f` is uniform on `(0, 1]`, exactly like the separate `u2` draw
//! the fixture describes, so the distribution, and therefore parity, is
//! unchanged. Reusing `u1` makes each path's total **continuous** in the
//! survival curve, though, and that is what keeps a refresh steady. If the
//! caller derives the seed from the stage visit ([`seed_for_visit`]) rather
//! than from `as_of`, a refresh five minutes later replays the same
//! uniforms. Every path then moves a little, and p50 moves by the model's
//! own drift plus a little Monte Carlo jitter instead of being redrawn.
//!
//! Measured on the five fixture rows at M = 256, over 40 seeds, a
//! five-minute refresh moved the blended p50 by at most 4.0% this way. With
//! one shared stream and a separate `u2` it moved by up to 10%, and with a
//! fresh seed per refresh by up to 39–56%.

mod eval;
mod path;

pub use eval::blend;
pub use path::{first_exit, km_inverse, nearest_rank};

use crate::eta::fit::{AftFit, CoefficientFile, FitStage, HazardFit, PathStats};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Hours per survival step.
pub const STEP_H: f64 = 0.5;

/// Survival steps: 672 × 30 min = 14 days.
pub const STEPS: usize = 672;

/// The cap on every part's quantiles, in hours (14 days).
pub const CAP_H: f64 = 336.0;

/// Monte Carlo paths per evaluation (the operator's specification). A
/// larger count is a new heuristic id, not an edit.
pub const PATHS: usize = 256;

/// The quantiles twin-otter reports.
pub const TAUS: [f64; 4] = [0.25, 0.5, 0.75, 0.9];

/// [`TAUS`] in whole percent, for integer nearest-rank arithmetic.
pub const TAU_PERCENT: [usize; 4] = [25, 50, 75, 90];

/// `Φ⁻¹(τ)` for each of [`TAUS`] (Python `statistics.NormalDist().inv_cdf`).
pub const PROBIT_TAUS: [f64; 4] = [
    -0.674_489_750_196_081_7,
    0.0,
    0.674_489_750_196_081_7,
    1.281_551_565_544_600_6,
];

/// Transitions after the current stage before a path counts as stuck and
/// contributes `cap_h`.
pub const HOP_GUARD: usize = 64;

/// The longest horizon (`steps · step_h`) a config may ask for, in hours,
/// so the clock arithmetic stays far inside chrono's range.
pub const MAX_HORIZON_H: f64 = 100_000.0;

/// One item to evaluate. Its JSON shape is the fixture's
/// `evaluation.rows[].input`, so a fixture row deserializes directly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TwinOtterInput {
    /// The coefficient set's stage name (`review_wait`, `doctor_wait`,
    /// `merge_wait`, `merge_hold`).
    pub stage: String,
    /// Hours spent in the current stage.
    pub age_h: f64,
    /// The instant of the estimate; the clock features advance from here.
    pub as_of: DateTime<Utc>,
    /// PRs in the same repo and stage that entered it earlier.
    #[serde(default)]
    pub ahead: Option<u32>,
    /// Other PRs in the same repo and stage.
    #[serde(default)]
    pub n_stage_repo: Option<u32>,
    /// Exits from this stage in this repo over the last 6 h.
    #[serde(default)]
    pub exits_repo_6h: Option<u32>,
    /// Exits from this stage in this repo over the last 24 h.
    #[serde(default)]
    pub exits_repo_24h: Option<u32>,
    /// Exits from this stage fleet-wide over the last 6 h.
    #[serde(default)]
    pub exits_fleet_6h: Option<u32>,
    /// Merges in the repo over the last 24 h.
    #[serde(default)]
    pub merges_repo_24h: Option<u32>,
    /// Merges fleet-wide over the last 6 h.
    #[serde(default)]
    pub merges_fleet_6h: Option<u32>,
    /// Hours since the repo's last merge. Frozen at `as_of`.
    #[serde(default)]
    pub since_merge_h: Option<f64>,
    /// Other PRs in the same stage, fleet-wide.
    #[serde(default)]
    pub n_stage_fleet: Option<u32>,
    /// Fix rounds so far.
    pub rework: u32,
    /// Carries an operator label (0/1; nonzero is set).
    pub op_hold: u8,
    /// Carries `loom:sequenced` (0/1).
    pub sequenced: u8,
    /// Carries `loom:operator-priority` (0/1).
    pub starred: u8,
    /// Carries `loom:merge-conflict` (0/1).
    pub conflict: u8,
    /// Carries `loom:ci-failure` (0/1).
    pub ci_fail: u8,
    /// Carries `loom:blocked` (0/1).
    pub blocked: u8,
}

/// The parts of a coefficient set twin-otter reads, by reference.
#[derive(Debug, Clone, Copy)]
pub struct TwinOtterModel<'a> {
    /// Feature names, in the order of every coefficient vector.
    pub features: &'a [String],
    /// Per-stage exit hazards (fitted stages only).
    pub hazard: &'a BTreeMap<FitStage, HazardFit>,
    /// The pooled direct model.
    pub aft: &'a AftFit,
    /// Kaplan–Meier dwell curves and next-stage probabilities.
    pub path_stats: &'a PathStats,
}

impl<'a> TwinOtterModel<'a> {
    /// The model in a coefficient file, or `None` when the file has no
    /// direct model (no stage passed its gate).
    #[must_use]
    pub fn of(file: &'a CoefficientFile) -> Option<Self> {
        Some(TwinOtterModel {
            features: &file.features,
            hazard: &file.hazard,
            aft: file.aft.as_ref()?,
            path_stats: &file.path_stats,
        })
    }
}

/// How one evaluation runs.
#[derive(Debug, Clone, PartialEq)]
pub struct EvalConfig {
    /// Hours per survival step.
    pub step_h: f64,
    /// Survival steps (the horizon is `steps · step_h`).
    pub steps: usize,
    /// The cap on every quantile, in hours.
    pub cap_h: f64,
    /// Monte Carlo paths.
    pub paths: usize,
    /// The master seed. Derive it from the stage visit
    /// ([`seed_for_visit`]), never from the clock.
    pub seed: u64,
    /// Optional cap on the age feature, in hours. `None`, the default,
    /// leaves the age unclamped.
    pub age_clamp_h: Option<f64>,
}

impl Default for EvalConfig {
    fn default() -> Self {
        EvalConfig {
            step_h: STEP_H,
            steps: STEPS,
            cap_h: CAP_H,
            paths: PATHS,
            seed: 0,
            age_clamp_h: None,
        }
    }
}

/// What [`evaluate`] returns. All quantiles are in hours, at [`TAUS`].
#[derive(Debug, Clone, PartialEq)]
pub struct Evaluation {
    /// `S_1..S_steps`: the probability of still being in the current stage
    /// after each step.
    pub survival: Vec<f64>,
    /// The stage-by-stage (path Monte Carlo) quantiles.
    pub hazard_path_q: [f64; 4],
    /// The direct (AFT) quantiles, each capped at `cap_h`.
    pub aft_q: [f64; 4],
    /// The blend: elementwise mean, then cumulative max.
    pub blend_q: [f64; 4],
    /// The age clamp was set and bound at one or more steps.
    pub age_clamp_applied: bool,
    /// Input fields that were `None` and were imputed at standardized 0.
    pub imputed: Vec<&'static str>,
}

/// Why [`evaluate`] gave no answer. It never panics instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalError {
    /// The stage is not a coefficient-set stage, or the hazard map, the
    /// direct model or the path statistics do not cover it. This is the only
    /// refusal an item can earn by itself.
    UnknownStage(String),
    /// The coefficients are malformed: a length mismatch, an unknown
    /// feature name, a non-positive or non-finite `sd`, a non-finite
    /// coefficient, a next-stage target with no dwell curve, and so on.
    InvalidModel(String),
    /// The input or the config is out of domain: a negative or non-finite
    /// age, zero paths, a non-positive step, and so on.
    InvalidInput(String),
}

impl std::fmt::Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EvalError::UnknownStage(stage) => write!(f, "unknown stage `{stage}`"),
            EvalError::InvalidModel(why) => write!(f, "invalid model: {why}"),
            EvalError::InvalidInput(why) => write!(f, "invalid input: {why}"),
        }
    }
}

impl std::error::Error for EvalError {}

/// The master seed for one **stage visit**: the subject, the stage, and the
/// instant the subject entered that stage. Every refresh of the same visit
/// derives the same seed, so it replays the same uniforms (see the module
/// docs). `as_of` is deliberately not an argument.
///
/// `subject` must stay the same for the item's whole life, for example
/// [`crate::eta::repo_key`] plus the PR number.
#[must_use]
pub fn seed_for_visit(subject: &str, stage: &str, entered_at: DateTime<Utc>) -> u64 {
    let at = crate::telemetry::trace::instant(entered_at);
    let hex = crate::telemetry::trace::derived_hex(
        &["loom.eta.twin-otter.seed", subject, stage, &at],
        16,
    );
    u64::from_str_radix(&hex, 16).unwrap_or(0)
}

/// Evaluate twin-otter for one item: the survival curve, both parts'
/// quantiles, and the blend.
pub fn evaluate(
    model: &TwinOtterModel<'_>,
    input: &TwinOtterInput,
    config: &EvalConfig,
) -> Result<Evaluation, EvalError> {
    check_config(config)?;
    let unknown = || EvalError::UnknownStage(input.stage.clone());
    let stage = FitStage::ALL
        .into_iter()
        .find(|s| s.as_str() == input.stage)
        .ok_or_else(unknown)?;
    let hazard = model.hazard.get(&stage).ok_or_else(unknown)?;
    let aft_stage = model
        .aft
        .stages
        .iter()
        .position(|s| *s == stage)
        .ok_or_else(unknown)?;
    if !model.path_stats.next.contains_key(&stage) {
        return Err(unknown());
    }
    path::check_paths(model.path_stats, stage)?;

    let features = eval::Features::new(model.features, input)?;
    eval::check_hazard(hazard, features.len())?;
    eval::check_aft(model.aft, features.len())?;

    let (survival, age_clamp_applied) = eval::survival_curve(hazard, &features, config);
    let aft_q = eval::aft_quantiles(model.aft, aft_stage, &features, config);
    let hazard_path_q = path::path_quantiles(&survival, stage, model.path_stats, config);
    let blend_q = blend(&hazard_path_q, &aft_q, config.cap_h);
    Ok(Evaluation {
        survival,
        hazard_path_q,
        aft_q,
        blend_q,
        age_clamp_applied,
        imputed: features.imputed().to_vec(),
    })
}

fn check_config(config: &EvalConfig) -> Result<(), EvalError> {
    let bad = |why: &str| Err(EvalError::InvalidInput(why.to_string()));
    if !(config.step_h.is_finite() && config.step_h > 0.0) {
        return bad("step_h must be positive and finite");
    }
    if config.steps == 0 {
        return bad("steps must be at least 1");
    }
    if config.steps as f64 * config.step_h > MAX_HORIZON_H {
        return bad("the horizon steps · step_h is too long");
    }
    if !(config.cap_h.is_finite() && config.cap_h > 0.0) {
        return bad("cap_h must be positive and finite");
    }
    if config.paths == 0 {
        return bad("paths must be at least 1");
    }
    if let Some(clamp) = config.age_clamp_h {
        if !(clamp.is_finite() && clamp >= 0.0) {
            return bad("age_clamp_h must be non-negative and finite");
        }
    }
    Ok(())
}
