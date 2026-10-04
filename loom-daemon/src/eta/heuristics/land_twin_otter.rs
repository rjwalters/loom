//! `land-2026-10-04-twin-otter` (#10243): the first **fitted** `land`
//! heuristic, registered as a shadow candidate.
//!
//! # What this module adds to the evaluation core
//!
//! [`crate::eta::twin_otter`] (#10222) is the pure evaluation: a per-stage
//! exit hazard walked stage by stage through a Monte Carlo, a pooled
//! log-normal direct model, and their blend. This module wires it in:
//!
//! - the **adapter** from an [`EstimateInput`] to the core's plain
//!   [`TwinOtterInput`] ([`adapt_input`]);
//! - the **seed**, one per stage visit ([`visit_seed`]), so a refresh with
//!   unchanged inputs replays the same uniforms instead of redrawing them;
//! - the **refusal map** below;
//! - the explanation: `combination`, the blended `result`, and a
//!   `twin_otter` record that [`recompute`] reproduces the answer from.
//!
//! # Pure: the coefficients arrive with the registry
//!
//! The heuristic holds the coefficient set it was built with
//! ([`crate::eta::Registry::with_fit`]) and never reads one. Loading happens
//! when the registry is built ([`crate::eta::Registry::load`]); the tracker
//! rebuilds the registry when a pass finds a fit with a new id. A set whose
//! cutoff is not strictly before `as_of` is never used, so a replay cannot
//! see a fit made after the instant it describes.
//!
//! # Refusals (a closed map)
//!
//! | condition | reason |
//! |---|---|
//! | the resolver refused (`Refused(r)`) | `r` |
//! | `ready_wait`, `sweep.curator`, `sweep.builder` (the model is PR-level; `land-2026-10-04-twin-otter-b`, #10244, composes these with `land-v2`) | `unknown_stage` |
//! | no set loaded, no direct model, cutoff ≥ `as_of`, malformed coefficients | `no_model` |
//! | the fit skipped the current stage | `insufficient_samples` |
//! | an out-of-domain input (tracker inputs cannot produce one) | `unknown_stage` |
//!
//! It never refuses `beyond_history`: the fitted age terms extrapolate, and
//! calibration on old items is measured (#10223), not refused.
//!
//! # Stage age and the seed
//!
//! Both are taken from the current stage **episode** ([`visit_entry`]). The
//! fit (#10245) trains on #10218's episodes, so serving measures age the same
//! way: after an operator hold is lifted the episode began at the release
//! (`episode_entered_at`), not at the approval the pooled `merge_wait` keeps.
//!
//! # `merge_hold`
//!
//! A held approved PR is estimated from the fit's own `merge_hold` stage,
//! unlike the path-engine heuristics, which refuse it as `blocked`. Its
//! `features` are those of that `blocked` refusal (#10218 keeps the shipped
//! explanations byte-identical), so its stage-dependent counts are `null`
//! and imputed; and the tracker emits a held PR's estimate at the hold's
//! entry without refreshing it.
//!
//! # Ships as shadow
//!
//! Registered last and never `current`: promotion is
//! [`crate::eta::shadow`]'s two-gate rule.

use super::{blank, refuse};
use crate::eta::explanation::{
    Combination, CurrentStageRecord, EstimateResult, TwinOtterModelRecord, TwinOtterRecord,
};
use crate::eta::fit::{CoefficientFile, FitStage};
use crate::eta::history::StageSamples;
use crate::eta::labels::{
    pr_flags, FLAG_BLOCKED, FLAG_CI_FAIL, FLAG_CONFLICT, FLAG_OP_HOLD, FLAG_SEQUENCED, FLAG_STARRED,
};
use crate::eta::simulate::parse_seed;
use crate::eta::twin_otter::{
    evaluate, seed_for_visit, EvalConfig, EvalError, Evaluation, TwinOtterInput, TwinOtterModel,
};
use crate::eta::{
    repo_key, CurrentStage, CurrentState, EstimateInput, Explanation, Heuristic, Kind,
    NoEstimateReason, Stage,
};
use chrono::{DateTime, Duration, Utc};
use std::sync::Arc;

/// The id. Immutable once shipped.
pub const LAND_TWIN_OTTER: &str = "land-2026-10-04-twin-otter";

/// `combination.method` of an answered estimate.
pub const METHOD: &str = "twin_otter_blend";

/// `combination.draw_order`: the evaluation core's per-path order, as built.
pub const DRAW_ORDER: &str = "per path: its own splitmix64 sub-stream, seeded by one draw of \
     the master stream (seeded with combination.seed); u1 places the exit from the current \
     stage: k = #{k in 1..=steps : S_k > u1}, cap_h when k = steps, else a first dwell of \
     (k + (S_k - u1) / (S_k - S_{k+1})) * step_h with S_0 = 1 (no separate u2); then, until \
     merged or cap_h, one uniform picks the next stage (next-stage probabilities in key order) \
     and one draws its Kaplan-Meier dwell (step inverse)";

/// `land-2026-10-04-twin-otter`, holding the coefficient set it was built
/// with (`None`: every estimate refuses `no_model`).
#[derive(Debug, Clone, Default)]
pub struct LandTwinOtter {
    fit: Option<Arc<CoefficientFile>>,
}

impl LandTwinOtter {
    /// The heuristic over `fit`.
    #[must_use]
    pub fn new(fit: Option<Arc<CoefficientFile>>) -> Self {
        LandTwinOtter { fit }
    }
}

impl Heuristic for LandTwinOtter {
    fn id(&self) -> &'static str {
        LAND_TWIN_OTTER
    }

    fn kind(&self) -> Kind {
        Kind::Land
    }

    fn estimate(&self, input: &EstimateInput, _history: &StageSamples) -> Explanation {
        let mut explanation = blank(LAND_TWIN_OTTER, Kind::Land, input);
        let current = match &input.current {
            CurrentState::Refused(reason) => return refuse(explanation, *reason),
            CurrentState::At(current) => current,
        };
        explanation.current_stage = Some(CurrentStageRecord {
            stage: current.stage,
            entered_at: current.entered_at,
            age_sec: current.age_sec.max(0),
            age_source: current.age_source,
            rework_rounds: at_least_one_in_doctor(current.stage, current.rework_rounds),
        });
        match self.answer(input, current) {
            Ok(answer) => answer.explain(explanation, input.as_of),
            Err(reason) => refuse(explanation, reason),
        }
    }
}

/// One evaluated estimate, before it is written into the explanation.
struct Answer<'a> {
    fit: &'a CoefficientFile,
    stage: FitStage,
    model: TwinOtterModel<'a>,
    input: TwinOtterInput,
    config: EvalConfig,
    evaluation: Evaluation,
}

impl LandTwinOtter {
    fn answer<'a>(
        &'a self,
        input: &EstimateInput,
        current: &CurrentStage,
    ) -> Result<Answer<'a>, NoEstimateReason> {
        let stage = FitStage::from_stage(current.stage).ok_or(NoEstimateReason::UnknownStage)?;
        let fit = self
            .fit
            .as_deref()
            .filter(|fit| fit.as_of < input.as_of)
            .ok_or(NoEstimateReason::NoModel)?;
        let model = TwinOtterModel::of(fit).ok_or(NoEstimateReason::NoModel)?;
        let adapted = adapt_input(input, current, stage);
        let config = EvalConfig {
            seed: visit_seed(input, current, stage),
            ..EvalConfig::default()
        };
        let evaluation = evaluate(&model, &adapted, &config).map_err(|error| match error {
            // A stage the fit skipped (`hazard_skipped`), or one missing from
            // the direct model or the path statistics.
            EvalError::UnknownStage(_) => NoEstimateReason::InsufficientSamples,
            EvalError::InvalidModel(_) => NoEstimateReason::NoModel,
            // The tracker's ages are clamped at 0 and its counts are
            // non-negative, so only a hand-built input lands here.
            EvalError::InvalidInput(_) => NoEstimateReason::UnknownStage,
        })?;
        Ok(Answer {
            fit,
            stage,
            model,
            input: adapted,
            config,
            evaluation,
        })
    }
}

impl Answer<'_> {
    /// Write the answer into `explanation`: the blend as `result`, the seed
    /// in `combination`, and everything [`recompute`] reads in `twin_otter`.
    fn explain(self, mut explanation: Explanation, as_of: DateTime<Utc>) -> Explanation {
        let [p25, p50, p75, p90] = self.evaluation.blend_q.map(to_sec);
        explanation.combination = Some(Combination {
            method: METHOD.to_string(),
            draws: self.config.paths,
            seed: format!("0x{:016x}", self.config.seed),
            rng: "splitmix64".to_string(),
            draw_order: DRAW_ORDER.to_string(),
            independence_assumed: false,
        });
        explanation.result = Some(EstimateResult {
            p25_sec: p25,
            p50_sec: p50,
            p75_sec: p75,
            p90_sec: Some(p90),
            eta_p50_at: as_of + Duration::seconds(p50),
            samples_min: self.model.hazard.get(&self.stage).map_or(0, |h| h.rows),
            stage_marks: Vec::new(),
        });
        explanation.twin_otter = Some(TwinOtterRecord {
            fit_id: self.fit.id.clone(),
            fit_as_of: self.fit.as_of,
            input: self.input,
            imputed: self
                .evaluation
                .imputed
                .iter()
                .map(|field| (*field).to_string())
                .collect(),
            step_h: self.config.step_h,
            steps: self.config.steps,
            cap_h: self.config.cap_h,
            paths: self.config.paths,
            age_clamp_h: self.config.age_clamp_h,
            age_clamp_applied: self.evaluation.age_clamp_applied,
            hazard_path_sec: self.evaluation.hazard_path_q.map(to_sec),
            aft_sec: self.evaluation.aft_q.map(to_sec),
            model: Some(TwinOtterModelRecord {
                features: self.model.features.to_vec(),
                hazard: self
                    .model
                    .hazard
                    .get_key_value(&self.stage)
                    .map(|(stage, fit)| (*stage, fit.clone()))
                    .into_iter()
                    .collect(),
                aft: self.model.aft.clone(),
                path_stats: self.model.path_stats.clone(),
            }),
        });
        explanation.enforce_cap();
        explanation
    }
}

/// Hours to whole seconds.
fn to_sec(hours: f64) -> i64 {
    (hours * 3600.0).round() as i64
}

/// An item in `doctor` has taken at least one rejection, whatever was
/// counted: `doctor` is entered only through one (as `estimate_path` does).
fn at_least_one_in_doctor(stage: Stage, rework: u32) -> u32 {
    if stage == Stage::Doctor {
        rework.max(1)
    } else {
        rework
    }
}

/// When the current stage episode began: `episode_entered_at` (the release
/// of an operator hold, #10218), else the stage's `entered_at`, else
/// `as_of − age_sec` when the resolver had no instant.
#[must_use]
pub fn visit_entry(current: &CurrentStage, as_of: DateTime<Utc>) -> DateTime<Utc> {
    current
        .episode_entered_at
        .or(current.entered_at)
        .unwrap_or_else(|| as_of - Duration::seconds(current.age_sec.max(0)))
}

/// The Monte Carlo seed for this stage visit:
/// `seed_for_visit("<repo_key>#<issue>", stage, visit_entry)`. Every refresh
/// of one visit derives the same seed, because `as_of` is not an input.
#[must_use]
pub fn visit_seed(input: &EstimateInput, current: &CurrentStage, stage: FitStage) -> u64 {
    let subject = format!("{}#{}", repo_key(&input.subject), input.subject.issue);
    seed_for_visit(&subject, stage.as_str(), visit_entry(current, input.as_of))
}

/// The evaluation core's input for `input`, in `stage`.
///
/// - `age_h`: whole seconds since [`visit_entry`], in hours.
/// - The counts are the same-named queue features (#10201), `None` kept
///   (imputed by the core); `since_merge_h` is `since_merge_sec / 3600`.
/// - `rework`: `doctor_cycles_so_far` (the Judge rejections so far), else
///   the resolver's count; at least 1 in `doctor`.
/// - The six flags: [`pr_flags`] of the labels, 0 when none were listed.
#[must_use]
pub fn adapt_input(
    input: &EstimateInput,
    current: &CurrentStage,
    stage: FitStage,
) -> TwinOtterInput {
    let features = &input.features;
    let age_sec = (input.as_of - visit_entry(current, input.as_of))
        .num_seconds()
        .max(0);
    let mask = features.labels.as_deref().map_or(0, pr_flags);
    let flag = |bit: u8| u8::from(mask & bit != 0);
    let rework = features
        .doctor_cycles_so_far
        .unwrap_or(current.rework_rounds);
    TwinOtterInput {
        stage: stage.as_str().to_string(),
        age_h: age_sec as f64 / 3600.0,
        as_of: input.as_of,
        ahead: features.ahead,
        n_stage_repo: features.n_stage_repo,
        exits_repo_6h: features.exits_repo_6h,
        exits_repo_24h: features.exits_repo_24h,
        exits_fleet_6h: features.exits_fleet_6h,
        merges_repo_24h: features.merges_repo_24h,
        merges_fleet_6h: features.merges_fleet_6h,
        since_merge_h: features.since_merge_sec.map(|s| s as f64 / 3600.0),
        n_stage_fleet: features.n_stage_fleet,
        rework: at_least_one_in_doctor(current.stage, rework),
        op_hold: flag(FLAG_OP_HOLD),
        sequenced: flag(FLAG_SEQUENCED),
        starred: flag(FLAG_STARRED),
        conflict: flag(FLAG_CONFLICT),
        ci_fail: flag(FLAG_CI_FAIL),
        blocked: flag(FLAG_BLOCKED),
    }
}

/// Recompute a twin-otter explanation's blended `(p25, p50, p75, p90)` from
/// its own fields: the record's input, config and model slice, and
/// `combination.seed`. `None` when the model slice was truncated away.
#[must_use]
pub(crate) fn recompute(
    explanation: &Explanation,
    record: &TwinOtterRecord,
) -> Option<(i64, i64, i64, i64)> {
    let slice = record.model.as_ref()?;
    let seed = parse_seed(&explanation.combination.as_ref()?.seed)?;
    let model = TwinOtterModel {
        features: &slice.features,
        hazard: &slice.hazard,
        aft: &slice.aft,
        path_stats: &slice.path_stats,
    };
    let config = EvalConfig {
        step_h: record.step_h,
        steps: record.steps,
        cap_h: record.cap_h,
        paths: record.paths,
        seed,
        age_clamp_h: record.age_clamp_h,
    };
    let [p25, p50, p75, p90] = evaluate(&model, &record.input, &config)
        .ok()?
        .blend_q
        .map(to_sec);
    Some((p25, p50, p75, p90))
}
