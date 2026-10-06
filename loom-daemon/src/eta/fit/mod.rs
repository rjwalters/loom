//! Fitted ETA coefficients: the pure core of the daily point-in-time fit
//! (`eta-fit/v1`, #10221).
//!
//! `land-2026-10-04-twin-otter` (#10222, epic #10223) blends two **fitted**
//! models. An estimator must stay pure (it reads nothing but its two
//! arguments), so fitting is a separate step whose output file the registry
//! loads. This module is that step's pure core:
//!
//! - a per-stage **exit hazard**: L2 logistic regression of "left the stage
//!   within the next 30 minutes" on standardized features ([`logistic`]);
//! - the **direct model**: a pooled, censoring-aware log-normal AFT
//!   regression of time-to-merge ([`aft`]);
//! - **path statistics**: delayed-entry Kaplan–Meier dwell curves and
//!   next-stage probabilities ([`paths`]);
//! - the versioned coefficient file that carries all three ([`coeffs`]),
//!   built by [`fit()`].
//!
//! [`math`] holds the hand-ported numerics (erfc, the normal tail and
//! quantile functions, Cholesky) — no new crate, since `Cargo.toml` is a
//! Champion auto-merge veto pattern.
//!
//! Two modules around that core are #10245's:
//!
//! - [`rows`] builds the training rows and dwells from fleet snapshots at a
//!   cutoff `T` (pure; the point-in-time rules are in its docs);
//! - [`run`] is the I/O around it: read the snapshots, fit, write and prune
//!   the coefficient files, and decide when the daily refit is due. The
//!   `eta fit` CLI and the daemon's daily task both call it.
//!
//! # Purity and determinism
//!
//! No clock, no globals beyond the [`coeffs::FIT_DIR_ENV`] test seam, no
//! randomness, and no I/O outside [`coeffs::write`], [`coeffs::read`],
//! [`coeffs::load_latest`] and [`run`]. Rows are processed in the order given — canonical
//! ordering is the caller's job — so the same input gives a byte-identical
//! file. Byte identity is per build and platform: `ln`, `exp` and `sin` come
//! from the platform libm. The file's `id` is derived from its content, never
//! random (trace-identity policy).
//!
//! # Pinned contracts
//!
//! Train (#10245) and serve (#10222) share every type below; #10222 imports
//! them from `crate::eta::fit` and never redefines them. Every coefficient
//! vector is positional against the #10223 parity fixture, so none of these
//! may be reordered or renamed:
//!
//! - [`FitStage`]: `review_wait`, `doctor_wait`, `merge_wait`, `merge_hold`,
//!   in that order (`Ord` follows it). Deliberately not [`super::Stage`]:
//!   it has only the PR stages, and the daemon's `doctor` is `doctor_wait`
//!   here ([`FitStage::from_stage`] maps one onto the other).
//! - [`FEATURES`]: the 20 model features, in the fixture's
//!   `generator.features` order (not the order of #10221's feature table).
//! - [`ModelInputs`], [`model_features`] and [`clock`]: the raw per-instant
//!   inputs, and the one transform train and serve both call to turn them
//!   into [`FEATURES`] order.
//! - [`TrainingRow`] and [`MergeLabel`]: one row per open PR per 30 minutes.
//! - [`DwellRow`] and [`DwellEnd`]: one stage episode, the path-statistics
//!   input.
//! - [`CoefficientFile`] (`eta-fit/v1`) and its parts — [`HazardFit`],
//!   [`HazardSkip`], [`AftFit`], [`PathStats`], [`KmCurve`], [`NextStep`] —
//!   owned by [`coeffs`]. Each part has the shape of the fixture object it
//!   names, so the fixture deserializes straight into it; file-only fields are
//!   `#[serde(default)]`.

pub mod aft;
pub mod coeffs;
pub mod features;
pub mod features_v2;
pub mod features_v3;
pub mod logistic;
pub mod math;
pub mod paths;
pub mod publish;
pub mod rows;
pub mod run;
pub mod v2;
pub mod v3;

use serde::{Deserialize, Serialize};
use std::fmt;

pub use coeffs::{
    age_p95_sec, fit, fit_dir, load_latest, path_for, read, to_json, write, AftFit,
    CoefficientFile, FitMeta, FitSettings, FitWindow, Fitter, HazardFit, HazardSkip, KmCurve,
    NextStep, PathStats, SkipReason,
};
pub use features::{clock, model_features, ModelInputs};
pub use paths::downsample_km;

/// The schema tag of a coefficient file.
pub const SCHEMA: &str = "eta-fit/v1";

/// The 20 model features, in the order every coefficient vector uses: the
/// #10223 fixture's `generator.features`.
pub const FEATURES: [&str; 20] = [
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
    "starred",
    "conflict",
    "ci_fail",
    "blocked",
];

/// `FEATURES.len()`.
pub const N_FEATURES: usize = FEATURES.len();

/// The hazard model's inverse L2 strength, as scikit-learn's `C`: the
/// objective is `Σ softplus + ‖w‖² / (2C)`.
pub const HAZARD_C: f64 = 0.5;

/// The direct model's L2 weight on its (standardized) feature coefficients.
pub const AFT_L2: f64 = 1e-3;

/// Added to every population standard deviation, so a constant feature
/// standardizes to 0 instead of dividing by zero.
pub const STD_EPS: f64 = 1e-9;

/// The floor on a merge duration before its log is taken, in hours (1 min).
pub const MIN_DUR_H: f64 = 1.0 / 60.0;

/// Fewest rows a stage is fitted from (the hazard's labelled rows; the
/// direct model's rows).
pub const MIN_STAGE_ROWS: usize = 200;

/// Fewest exits a stage's hazard is fitted from, and fewest merge events a
/// stage needs to enter the direct model.
pub const MIN_STAGE_EXITS: usize = 20;

/// Most points a stored Kaplan–Meier curve keeps, so the explanation that
/// carries it stays under its size cap.
pub const KM_MAX_POINTS: usize = 64;

/// Training window before the cutoff, in days.
pub const WINDOW_DAYS: i64 = 14;

/// Spacing of training-row instants, in seconds.
pub const ROW_STEP_SEC: i64 = 1800;

/// The exit label's horizon, in seconds.
pub const EXIT_HORIZON_SEC: i64 = 1800;

/// How long after its own instant an event becomes knowable, in seconds.
pub const KNOWABLE_LAG_SEC: i64 = 120;

/// Most Newton iterations either fit takes.
pub const MAX_NEWTON_ITERATIONS: u32 = 100;

/// A stage the fitted models know, in the fixture's order. `Ord` follows the
/// declaration order, so every stage-keyed map in the file is canonical.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FitStage {
    /// Waiting for a Judge verdict.
    ReviewWait,
    /// Waiting on a Doctor fix round (the daemon's `doctor` stage).
    DoctorWait,
    /// Approved and waiting to merge.
    MergeWait,
    /// Approved but held from merging (#10218).
    MergeHold,
}

impl FitStage {
    /// Every stage, in `Ord` order.
    pub const ALL: [FitStage; 4] = [
        FitStage::ReviewWait,
        FitStage::DoctorWait,
        FitStage::MergeWait,
        FitStage::MergeHold,
    ];

    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            FitStage::ReviewWait => "review_wait",
            FitStage::DoctorWait => "doctor_wait",
            FitStage::MergeWait => "merge_wait",
            FitStage::MergeHold => "merge_hold",
        }
    }

    /// The position in [`FitStage::ALL`].
    #[must_use]
    pub fn index(self) -> usize {
        self as usize
    }

    /// The coefficient-set stage of a daemon stage: the PR stages map, and
    /// every pre-PR stage is `None` (the models are PR-level). The one
    /// definition train (#10245) and serve (#10243) share. The match has no
    /// wildcard, so a new [`super::Stage`] does not compile until it is
    /// mapped here; `merge_hold` (#10218) is its own fit stage.
    #[must_use]
    pub fn from_stage(stage: super::Stage) -> Option<FitStage> {
        use super::Stage;
        match stage {
            Stage::ReviewWait => Some(FitStage::ReviewWait),
            Stage::Doctor => Some(FitStage::DoctorWait),
            Stage::MergeWait => Some(FitStage::MergeWait),
            Stage::MergeHold => Some(FitStage::MergeHold),
            Stage::ReadyWait | Stage::SweepCurator | Stage::SweepBuilder => None,
        }
    }
}

impl fmt::Display for FitStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One training row: an open PR at one row instant `t`, with its features and
/// both labels. #10245 builds these from fleet history; the parity test
/// generates them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrainingRow {
    /// The PR's stage at `t`.
    pub stage: FitStage,
    /// The PR, e.g. `"rjwalters/loom#10221"`. The direct model weights each
    /// row `1 / (rows in its group)`.
    pub group: String,
    /// The raw inputs at `t`.
    pub inputs: ModelInputs,
    /// The PR is starred through its own labels **or** a linked issue
    /// (#10372). Recorded, never a model input: `inputs.starred` stays the
    /// PR's own flag, so coefficient files are unchanged. `None` = unknown
    /// (no star inputs, or the raw event cache does not cover the cutoff).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starred_any: Option<bool>,
    /// Where the star comes from; see [`crate::eta::star::star_state_at`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub star_source: Option<crate::eta::star::StarSource>,
    /// Left the stage within the next 30 min. `None` = not fully observable
    /// before the cutoff; the hazard fit skips the row.
    pub exit: Option<bool>,
    /// Time from `t` to merge, possibly censored.
    pub merge: MergeLabel,
}

/// Hours to merge; `merged == false` means censored at `dur_h`.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct MergeLabel {
    /// Hours from the row instant to the merge, or to the censoring instant.
    pub dur_h: f64,
    /// Whether the merge was observed (an event) rather than censored.
    pub merged: bool,
}

/// One stage episode, the input to the path statistics. #10245 maps #10218's
/// `StageEpisode` into it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DwellRow {
    /// The stage.
    pub stage: FitStage,
    /// Delayed entry: hours the episode had already run when the window
    /// opened (0 if it entered inside the window).
    pub entry_h: f64,
    /// Stage entry to leaving it, or to the censoring instant.
    pub dwell_h: f64,
    /// How the episode ended.
    pub end: DwellEnd,
}

/// How a stage episode ended. `Next` and `Merged` are events in the
/// Kaplan–Meier curve; `Closed` and `Censored` are censored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DwellEnd {
    /// Moved on to another fit stage.
    Next(FitStage),
    /// The PR merged.
    Merged,
    /// The PR closed without merging.
    Closed,
    /// Still running (or left for a non-fit stage) at the censoring instant.
    Censored,
}
