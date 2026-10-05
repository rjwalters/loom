//! The `eta-fit/v1` coefficient file: the owned types, its content-derived
//! id, and the only I/O in [`super`] (`write`, `read`, `load_latest`).
//!
//! Each part has the shape of the #10223 fixture object it names, so the
//! fixture deserializes straight into it; file-only fields are
//! `#[serde(default)]`. Stage-keyed maps are `BTreeMap`s keyed by
//! [`FitStage`] or [`NextStep`], so key order is canonical, and struct fields
//! serialize in declaration order.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::{
    aft, logistic, paths, DwellRow, FitStage, TrainingRow, AFT_L2, EXIT_HORIZON_SEC, FEATURES,
    HAZARD_C, KM_MAX_POINTS, KNOWABLE_LAG_SEC, MIN_DUR_H, MIN_STAGE_EXITS, MIN_STAGE_ROWS,
    ROW_STEP_SEC, SCHEMA, STD_EPS, WINDOW_DAYS,
};
use crate::pr_latency::stats::nearest_rank;

/// Test seam: overrides the directory coefficient files live in (mirrors
/// `LOOM_ETA_FLEET_SNAPSHOT_DIR`).
pub const FIT_DIR_ENV: &str = "LOOM_ETA_FIT_DIR";

/// One stage's fitted exit hazard (`fitted.hazard.<stage>` in the fixture):
/// `P(exit within 30 min) = sigmoid(coef · z + intercept)`, where
/// `z = (x − mu) / sd` over [`super::FEATURES`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HazardFit {
    /// Per-feature mean over this stage's labelled rows.
    pub mu: Vec<f64>,
    /// Per-feature population standard deviation, plus [`super::STD_EPS`].
    pub sd: Vec<f64>,
    /// Coefficients on the standardized features.
    pub coef: Vec<f64>,
    /// The (unpenalized) intercept.
    pub intercept: f64,
    /// Labelled rows fitted.
    #[serde(default)]
    pub rows: usize,
    /// Exits among them.
    #[serde(default)]
    pub exits: usize,
    /// The objective at the optimum (a sum, not a mean).
    #[serde(default)]
    pub objective: f64,
    /// Newton iterations taken.
    #[serde(default)]
    pub iterations: u32,
    /// Whether Newton met its stopping rule.
    #[serde(default)]
    pub converged: bool,
}

/// Why a stage has no hazard fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// Fewer than [`super::MIN_STAGE_ROWS`] labelled rows.
    BelowMinRows,
    /// Enough rows, but fewer than [`super::MIN_STAGE_EXITS`] exits.
    BelowMinExits,
}

/// A stage left out of `hazard`, and why (file only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HazardSkip {
    /// The gate it failed.
    pub reason: SkipReason,
    /// Labelled rows it had.
    pub rows: usize,
    /// Exits among them.
    pub exits: usize,
}

/// The pooled direct model (`fitted.aft` in the fixture): log-normal AFT,
/// `ln T = Z·beta + σ_stage·ε`, with `Z = [stage one-hot | standardized
/// features]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AftFit {
    /// The stages in the model, in [`FitStage`] order; `beta`'s first
    /// `stages.len()` entries and `log_sigma` are positional against it.
    pub stages: Vec<FitStage>,
    /// Per-feature mean, pooled over the included rows (unweighted).
    pub mu: Vec<f64>,
    /// Per-feature population standard deviation, plus [`super::STD_EPS`].
    pub sd: Vec<f64>,
    /// `K` stage intercepts, then 20 feature coefficients.
    pub beta: Vec<f64>,
    /// `ln σ` per stage, `K` entries.
    pub log_sigma: Vec<f64>,
    /// The (weighted mean) objective at the optimum.
    pub objective: f64,
    /// Whether Newton met its stopping rule.
    pub converged: bool,
    /// Rows fitted.
    #[serde(default)]
    pub rows: usize,
    /// Merge events among them (the rest are censored).
    #[serde(default)]
    pub events: usize,
    /// Distinct groups (PRs) among them.
    #[serde(default)]
    pub groups: usize,
    /// Newton iterations taken.
    #[serde(default)]
    pub iterations: u32,
}

/// One stage's dwell-time Kaplan–Meier curve (`evaluation.path_stats.km.<stage>`
/// in the fixture): a right-continuous step function, `S(t) = s[i]` for
/// `t[i] ≤ t < t[i+1]`, at most [`super::KM_MAX_POINTS`] points.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KmCurve {
    /// Hours, ascending, starting at 0.
    pub t: Vec<f64>,
    /// Survival after each `t`, non-increasing, starting at 1.
    pub s: Vec<f64>,
    /// Episodes in the full data (not the stored points).
    #[serde(default)]
    pub episodes: usize,
    /// Events (`Next` or `Merged` ends) in the full data.
    #[serde(default)]
    pub events: usize,
}

/// Where a stage episode goes next: the keys of
/// `evaluation.path_stats.next.<stage>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NextStep {
    /// To `review_wait`.
    ReviewWait,
    /// To `doctor_wait`.
    DoctorWait,
    /// To `merge_wait`.
    MergeWait,
    /// To `merge_hold`.
    MergeHold,
    /// The PR merged.
    Merged,
}

impl From<FitStage> for NextStep {
    fn from(stage: FitStage) -> Self {
        match stage {
            FitStage::ReviewWait => NextStep::ReviewWait,
            FitStage::DoctorWait => NextStep::DoctorWait,
            FitStage::MergeWait => NextStep::MergeWait,
            FitStage::MergeHold => NextStep::MergeHold,
        }
    }
}

/// The path statistics (`evaluation.path_stats` in the fixture).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PathStats {
    /// Dwell-time curve per stage.
    pub km: BTreeMap<FitStage, KmCurve>,
    /// Next-step probabilities per stage, over `Next` and `Merged` ends only
    /// (closes excluded); each table sums to 1.
    pub next: BTreeMap<FitStage, BTreeMap<NextStep, f64>>,
}

/// The training window the rows were built over.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FitWindow {
    /// `as_of − days`.
    pub start: DateTime<Utc>,
    /// Window length in days.
    pub days: i64,
    /// Spacing of row instants, in seconds.
    pub row_step_sec: i64,
    /// The exit label's horizon, in seconds.
    pub exit_horizon_sec: i64,
    /// Knowability lag, in seconds.
    pub knowable_lag_sec: i64,
    /// The data horizon `H` (#10245): every label and censoring instant of a
    /// fleet fit is at most this, `min(as_of − knowable_lag, oldest snapshot
    /// as_of)`. `None` for rows not built from fleet snapshots, and then not
    /// written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_through: Option<DateTime<Utc>>,
}

impl FitWindow {
    /// The standard window ending at `as_of`, from the module's constants.
    #[must_use]
    pub fn standard(as_of: DateTime<Utc>) -> Self {
        FitWindow {
            start: as_of - Duration::days(WINDOW_DAYS),
            days: WINDOW_DAYS,
            row_step_sec: ROW_STEP_SEC,
            exit_horizon_sec: EXIT_HORIZON_SEC,
            knowable_lag_sec: KNOWABLE_LAG_SEC,
            data_through: None,
        }
    }
}

/// The build that produced the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fitter {
    /// Loom version.
    pub version: String,
    /// Full git revision.
    pub revision: String,
}

/// The fit's settings, recorded so a reader never has to assume them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FitSettings {
    /// [`super::HAZARD_C`].
    pub hazard_c: f64,
    /// [`super::AFT_L2`].
    pub aft_l2: f64,
    /// [`super::STD_EPS`].
    pub std_eps: f64,
    /// [`super::MIN_DUR_H`].
    pub min_dur_h: f64,
    /// [`super::MIN_STAGE_ROWS`].
    pub min_stage_rows: usize,
    /// [`super::MIN_STAGE_EXITS`].
    pub min_stage_exits: usize,
    /// [`super::KM_MAX_POINTS`].
    pub km_max_points: usize,
}

impl FitSettings {
    /// The settings this build fits with.
    #[must_use]
    pub fn current() -> Self {
        FitSettings {
            hazard_c: HAZARD_C,
            aft_l2: AFT_L2,
            std_eps: STD_EPS,
            min_dur_h: MIN_DUR_H,
            min_stage_rows: MIN_STAGE_ROWS,
            min_stage_exits: MIN_STAGE_EXITS,
            km_max_points: KM_MAX_POINTS,
        }
    }
}

/// What a fit is told besides its rows. No wall-clock time, host id or input
/// snapshot id: a snapshot id also digests facts after the cutoff, so
/// recording one would break the leak test (#10245).
#[derive(Debug, Clone, PartialEq)]
pub struct FitMeta {
    /// The cutoff `T`.
    pub as_of: DateTime<Utc>,
    /// The window the rows were built over.
    pub window: FitWindow,
    /// The build fitting.
    pub fitter: Fitter,
}

/// One fit (`eta-fit/v1`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoefficientFile {
    /// [`super::SCHEMA`].
    pub schema: String,
    /// 16 hex digits derived from the rest of the file
    /// ([`CoefficientFile::derive_id`]).
    pub id: String,
    /// The cutoff `T`.
    pub as_of: DateTime<Utc>,
    /// The training window.
    pub window: FitWindow,
    /// The build that fitted.
    pub fitter: Fitter,
    /// The settings fitted with.
    pub settings: FitSettings,
    /// [`super::FEATURES`], recorded.
    pub features: Vec<String>,
    /// Fitted stages only; the shape of the fixture's `fitted.hazard`.
    pub hazard: BTreeMap<FitStage, HazardFit>,
    /// Stages with rows but no hazard fit, and why.
    pub hazard_skipped: BTreeMap<FitStage, HazardSkip>,
    /// The direct model; `None` when no stage passes its gate.
    pub aft: Option<AftFit>,
    /// Dwell curves and next-step probabilities.
    pub path_stats: PathStats,
    /// Nearest-rank p95 of training-row age per stage, in whole seconds;
    /// stages with no rows are absent.
    pub age_p95_sec: BTreeMap<FitStage, i64>,
}

impl CoefficientFile {
    /// An empty file for `meta`, with the current settings and no id yet.
    #[must_use]
    pub fn empty(meta: &FitMeta) -> Self {
        CoefficientFile {
            schema: SCHEMA.to_string(),
            id: String::new(),
            as_of: meta.as_of,
            window: meta.window.clone(),
            fitter: meta.fitter.clone(),
            settings: FitSettings::current(),
            features: FEATURES.iter().map(|f| (*f).to_string()).collect(),
            hazard: BTreeMap::new(),
            hazard_skipped: BTreeMap::new(),
            aft: None,
            path_stats: PathStats::default(),
            age_p95_sec: BTreeMap::new(),
        }
    }

    /// The content-derived id:
    /// `derived_hex(["loom.eta.fit", <compact JSON with "id": "">], 16)`.
    #[must_use]
    pub fn derive_id(&self) -> String {
        let mut blank = self.clone();
        blank.id = String::new();
        let text = serde_json::to_string(&blank).expect("an eta-fit/v1 file always serializes");
        crate::telemetry::trace::derived_hex(&["loom.eta.fit", &text], 16)
    }

    /// `self` with its id set from its content.
    #[must_use]
    pub fn with_derived_id(mut self) -> Self {
        self.id = self.derive_id();
        self
    }
}

/// Fit everything from `rows` and `dwells`, processed in the order given
/// (canonical ordering is the caller's job), into a file with its derived id.
///
/// Every [`FitStage`] lands in exactly one of `hazard` (fitted) and
/// `hazard_skipped` (with the gate it failed, even at zero rows).
#[must_use]
pub fn fit(meta: &FitMeta, rows: &[TrainingRow], dwells: &[DwellRow]) -> CoefficientFile {
    let mut file = CoefficientFile::empty(meta);
    for stage in FitStage::ALL {
        let of_stage: Vec<&TrainingRow> = rows.iter().filter(|r| r.stage == stage).collect();
        match logistic::fit_stage(&of_stage) {
            Ok(hazard) => {
                file.hazard.insert(stage, hazard);
            }
            Err(skip) => {
                file.hazard_skipped.insert(stage, skip);
            }
        }
    }
    file.aft = aft::fit(rows);
    file.path_stats = paths::path_stats(dwells);
    file.age_p95_sec = age_p95_sec(rows);
    file.with_derived_id()
}

/// The training p95 age per stage, in whole seconds: the nearest-rank p95
/// (rank `⌈95·n/100⌉`) of `round(age_h·3600)` over **all** of the stage's
/// rows, labelled or not, unweighted. Stages with no rows are absent.
#[must_use]
pub fn age_p95_sec(rows: &[TrainingRow]) -> BTreeMap<FitStage, i64> {
    let mut by_stage: BTreeMap<FitStage, Vec<i64>> = BTreeMap::new();
    for r in rows {
        by_stage
            .entry(r.stage)
            .or_default()
            .push((r.inputs.age_h * 3600.0).round() as i64);
    }
    by_stage
        .into_iter()
        .map(|(stage, mut ages)| {
            ages.sort_unstable();
            (stage, nearest_rank(&ages, 95))
        })
        .collect()
}

/// The directory coefficient files live in: `<root>/.loom/state/eta/fit`, a
/// sibling of the fleet snapshots (whose directory `fleet::load_all` parses
/// wholesale), or [`FIT_DIR_ENV`].
#[must_use]
pub fn fit_dir(workspace_root: &Path) -> PathBuf {
    match std::env::var(FIT_DIR_ENV) {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => workspace_root
            .join(".loom")
            .join("state")
            .join("eta")
            .join("fit"),
    }
}

/// The file name for a fit at `as_of`: `fit-<YYYYMMDDTHHMMSSZ>.json`.
#[must_use]
pub fn path_for(as_of: DateTime<Utc>) -> String {
    format!("fit-{}.json", as_of.format("%Y%m%dT%H%M%SZ"))
}

/// The file's bytes: pretty JSON plus a trailing newline.
#[must_use]
pub fn to_json(file: &CoefficientFile) -> String {
    let text = serde_json::to_string_pretty(file).expect("an eta-fit/v1 file always serializes");
    format!("{text}\n")
}

/// Write `file` to `path` through a temp file and a rename, so a crash
/// mid-write cannot leave a half-written file behind.
///
/// # Errors
///
/// The parent could not be created, or the write/rename failed.
pub fn write(path: &Path, file: &CoefficientFile) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, to_json(file))?;
    std::fs::rename(&tmp, path)
}

/// Read the file at `path`. `None` when it is absent, unreadable, or not
/// [`super::SCHEMA`] — an unknown schema is a refusal, never a partial parse.
///
/// The id is **not** re-derived: without serde_json's `float_roundtrip`
/// feature a parsed `f64` can be one ulp off, so a re-derived id could differ
/// from the stored one even for an untouched file.
#[must_use]
pub fn read(path: &Path) -> Option<CoefficientFile> {
    let text = std::fs::read_to_string(path).ok()?;
    let file: CoefficientFile = serde_json::from_str(&text).ok()?;
    (file.schema == SCHEMA).then_some(file)
}

/// The newest readable fit under `workspace_root` whose `as_of` is strictly
/// before `before` (the point-in-time rule), or `None`. Ties on `as_of` go to
/// the later file name.
#[must_use]
pub fn load_latest(workspace_root: &Path, before: DateTime<Utc>) -> Option<CoefficientFile> {
    let entries = std::fs::read_dir(fit_dir(workspace_root)).ok()?;
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    paths
        .iter()
        .filter_map(|p| read(p))
        .filter(|f| f.as_of < before)
        .max_by(|a, b| a.as_of.cmp(&b.as_of))
}
