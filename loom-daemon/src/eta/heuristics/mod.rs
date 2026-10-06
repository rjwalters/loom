//! The shipped heuristics. `start-v1`, `finish-v1`, `land-v1`, `land-v2`
//! and `land-2026-10-04-fresh-tide` share one engine ([`estimate_path`]);
//! they differ in which history they read, where the path ends, and
//! (`land-2026-10-04-fresh-tide`) how samples are weighted by recency;
//! `land-v4` (#10210) is the `land-v3` grid step (`land-v3` itself is retired
//! from the registry, #10484; its module stays as `land-v4`'s step) plus whether a stall's term
//! is applied and an item beyond its history is answered rather than refused.
//! `land-2026-10-04-twin-otter` (#10243) reads no history: it evaluates a
//! fitted coefficient file handed to it when the registry was built.
//! `land-2026-10-06-calm-plover` (#10489) and `land-2026-10-06-quick-tern`
//! (#10524) are calibration wrappers over `land-v2` and
//! `land-2026-10-04-twin-otter-b` respectively.
//! Their ids are immutable: a behaviour change is a new id.

mod finish_v1;
mod land_calm_plover;
mod land_fresh_tide;
mod land_quick_tern;
mod land_twin_otter;
mod land_twin_otter_b;
mod land_v1;
mod land_v2;
mod land_v3;
mod land_v4;
mod little_v0;
mod start_v1;

pub use finish_v1::{FinishV1, FINISH_V1};
pub use land_calm_plover::{LandCalmPlover, LAND_CALM_PLOVER};
pub use land_fresh_tide::{LandFreshTide, LAND_FRESH_TIDE};
pub use land_quick_tern::{LandQuickTern, LAND_QUICK_TERN};
pub(crate) use land_twin_otter::recompute as recompute_twin_otter;
pub use land_twin_otter::{
    adapt_input, visit_entry, visit_seed, LandTwinOtter, DRAW_ORDER, LAND_TWIN_OTTER, METHOD,
};
pub use land_twin_otter_b::{LandTwinOtterB, LAND_TWIN_OTTER_B, PRE_PR_METHOD};
pub use land_v1::{LandV1, LAND_V1};
pub use land_v2::{LandV2, LAND_V2};
pub use land_v3::{
    complexity_scale, story_points, LandV3, COMPLEXITY_ELASTICITY, FRICTION_SEC_PER_RUNNING_SWEEP,
    INPUT_MISSING, LAND_V3, LOWER_STRETCH, REFERENCE_POINTS, REVIEW_FLOOR_SEC, UPPER_STRETCH,
};
pub use land_v4::{LandV4, LAND_V4};
pub use little_v0::{is_shadow_only, wait_sec, LittleV0, LITTLE_V0};
pub use start_v1::{StartV1, START_V1};

/// The heuristic whose track record the calibration log (#10207) records.
/// (The `land-2026-10-04-amber-heron` shadow that consumed it was retired
/// 2026-10-06, #10484; `land-2026-10-06-calm-plover` consumes it now, #10489.)
pub const CALIBRATION_BASE: &str = LAND_V2;

/// Every heuristic whose landed and still-open `land` estimates are kept as
/// calibration evidence ([`crate::eta::calibration_log`]): [`CALIBRATION_BASE`]
/// for `land-2026-10-06-calm-plover`, and [`LAND_TWIN_OTTER_B`] for
/// `land-2026-10-06-quick-tern` (#10524). Each calibrator filters the rows
/// on its own base, so they never mix.
pub const CALIBRATION_BASES: &[&str] = &[CALIBRATION_BASE, LAND_TWIN_OTTER_B];

use super::explanation::{
    Branches, ChangesRequested, Combination, Conditioning, CurrentStageRecord, DispatchRecord,
    Distribution, EstimateResult, Explanation, Filters, HistoryRecord, HistoryWindow, PathRecord,
    StageAdjustment, StageEntry,
};
use super::explanation::{CONDITIONING_RESIDUAL_LIFE, CONDITIONING_TRUNCATE};
use super::history::{window_from, Level, SampleSource, Selection, StageSamples};
use super::recency::WeightedSelection;
use super::simulate::{may_reject, reachable_path, run, spec_from_explanation};
use super::stall::{self, StallCause};
use super::{
    estimate_id, grid, round3, seed_for, CurrentStage, CurrentState, EstimateInput, Kind,
    NoEstimateReason, Stage, DRAWS, EXPLANATION_SCHEMA, MAX_REWORK_ROUNDS, MIN_COND, MIN_SAMPLES,
};
use chrono::Duration;
use std::collections::BTreeMap;

/// Fewest verdicts at an attempt for its own rejection rate to be used;
/// below it the previous attempt's rate is carried forward.
pub const MIN_VERDICTS: usize = 5;

/// How a heuristic's path is shaped.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PathRules {
    pub id: &'static str,
    pub kind: Kind,
    /// Which journals the stage distributions read.
    pub sources: &'static [SampleSource],
    /// `true`: an approved path always ends with `merge_wait`. `false`: the
    /// history's in-sweep merge share decides.
    pub always_merge: bool,
    /// `true`: build each stage grid with the Kaplan–Meier product-limit
    /// estimator over the observed durations **and** the stage's censored
    /// lower bounds (#9328). `false`: the plain nearest-rank grid over the
    /// observed durations alone — every v1 heuristic.
    pub censoring: bool,
    /// A calibrating heuristic's per-stage grid transform (#9970), applied
    /// to the raw grid before conditioning and simulation, so the explanation
    /// records — and recomputes from — the grid actually drawn from. `None`
    /// for every heuristic that draws from history unadjusted.
    pub adjust: Option<GridAdjust>,
    /// `true`: an item in `merge_hold` (#10218) is estimated, along
    /// `merge_hold → merge_wait` (the rest of the hold, conditioned on its
    /// age, then one merge wait). `false` — every path-engine heuristic —
    /// refuses it as `blocked` before any field is written, so the explanation is
    /// byte-identical to its refusal of a held PR before the stage existed.
    pub models_hold: bool,
    /// A recency-weighting heuristic's base half-life, seconds (#10209):
    /// each stage's samples are weighted `exp(−age / half_life)` with the
    /// effective-N fallback ([`crate::eta::recency`]) and summarised by the
    /// weighted grid. `None` for every heuristic that weighs the window flat.
    pub half_life_sec: Option<i64>,
    /// `true` (#10210): add the binding stall's term to the result
    /// (`explanation.stalled.applied`), and estimate an operator-held PR from
    /// the stage underneath its hold ([`EstimateInput::held`]) instead of
    /// refusing it as `blocked`. `false`: the stall is recorded, never
    /// applied — every heuristic before `land-v4`.
    pub stall_term: bool,
    /// `true` (#10210): an item older than its stage history is answered with
    /// the residual-life tail (flagged `result.tail_extrapolated`) instead of
    /// a `beyond_history` refusal. `false`: refuse, as every heuristic before
    /// `land-v4` does.
    pub residual_tail: bool,
}

/// `(stage, input, raw grid) → (grid, what was done)`. Must be pure.
pub(crate) type GridAdjust =
    fn(Stage, &EstimateInput, Vec<i64>) -> (Vec<i64>, Option<StageAdjustment>);

/// Round to six decimals — every float the simulation reads is stored
/// rounded, so the JSON value parses back to the exact `f64` used.
fn round6(x: f64) -> f64 {
    (x * 1_000_000.0).round() / 1_000_000.0
}

fn refuse(mut explanation: Explanation, reason: NoEstimateReason) -> Explanation {
    explanation.no_estimate_reason = Some(reason);
    // A refusal has no result for a stall term to be part of.
    if let Some(stalled) = &mut explanation.stalled {
        stalled.applied = false;
    }
    explanation.result = None;
    explanation.contributions = None;
    explanation.combination = None;
    explanation.twin_otter = None;
    explanation.enforce_cap();
    explanation
}

/// The stage `rules` estimate `input` from, or why there is none.
///
/// A stall-aware heuristic (#10210) reads an operator-held PR's underlying
/// stage ([`EstimateInput::held`]) through its `blocked` refusal, provided
/// the hold is in fact one of the item's stall signals; the hold's own
/// stall term then carries the wait. An approved held PR reaches it as
/// `At(MergeHold)` (#10218) rather than a `blocked` refusal; a stall-aware
/// heuristic that does not model the hold reads it the same way.
fn current_of(rules: PathRules, input: &EstimateInput) -> Result<&CurrentStage, NoEstimateReason> {
    let operator_held = rules.stall_term
        && !rules.models_hold
        && input
            .stalls
            .iter()
            .any(|s| s.cause == StallCause::OperatorHold);
    match (&input.current, &input.held) {
        (CurrentState::At(current), Some(held))
            if current.stage == Stage::MergeHold && operator_held =>
        {
            Ok(held)
        }
        (CurrentState::At(current), _) => Ok(current),
        (CurrentState::Refused(NoEstimateReason::Blocked), Some(held)) if operator_held => Ok(held),
        (CurrentState::Refused(reason), _) => Err(*reason),
    }
}

/// The explanation of `heuristic`'s estimate of `input` before anything is
/// estimated: identity, provenance, subject and the recorded features.
fn blank(heuristic: &'static str, kind: Kind, input: &EstimateInput) -> Explanation {
    let as_of = input.as_of;
    let features_omitted = input
        .features
        .complete_omissions(input.features_omitted.clone(), "not_collected");
    Explanation {
        schema: EXPLANATION_SCHEMA.to_string(),
        estimate_id: estimate_id(&input.subject, kind, heuristic, as_of),
        heuristic: heuristic.to_string(),
        kind,
        loom: input.provenance.clone(),
        as_of,
        subject: input.subject.clone(),
        current_stage: None,
        history_window: None,
        history: None,
        path: None,
        stages: Vec::new(),
        branches: None,
        combination: None,
        result: None,
        contributions: None,
        features: Some(input.features.clone()),
        features_omitted,
        no_estimate_reason: None,
        stalled: stall::binding(&input.stalls, as_of),
        truncated: Vec::new(),
        recalibration: None,
        calibration: None,
        twin_otter: None,
        queue: None,
    }
}

/// Estimate `input` along the path `rules` describe.
pub(crate) fn estimate_path(
    rules: PathRules,
    input: &EstimateInput,
    history: &StageSamples,
) -> Explanation {
    let as_of = input.as_of;
    let mut explanation = blank(rules.id, rules.kind, input);

    let current = match current_of(rules, input) {
        Ok(current) => current,
        Err(reason) => return refuse(explanation, reason),
    };
    // #10218: checked before every other refusal and before any field is
    // written, so a heuristic that does not model the hold returns exactly
    // the refusal a held PR got when the hold was a label-level `blocked`.
    if current.stage == Stage::MergeHold && !rules.models_hold {
        return refuse(explanation, NoEstimateReason::Blocked);
    }
    let start = current.stage;
    // Approved: no verdict ahead, and the path ends with `merge_wait`.
    let approved = matches!(start, Stage::MergeWait | Stage::MergeHold);
    // `start` exists only for a ready item, and a ready item has no running
    // sweep to `finish`. A ready item with no plan position has no estimate.
    let ready = start == Stage::ReadyWait;
    if (rules.kind == Kind::Start && !ready) || (rules.kind == Kind::Finish && ready) {
        return refuse(explanation, NoEstimateReason::UnknownStage);
    }
    let dispatch = match (&input.dispatch, ready) {
        (Some(d), true) => Some(DispatchRecord {
            input: d.clone(),
            turnovers: d.turnovers(),
            admission_delay_sec: d.admission_delay_sec(),
        }),
        (None, true) => return refuse(explanation, NoEstimateReason::NoDispatchPlan),
        (_, false) => None,
    };
    // An item in `doctor` has taken at least one rejection, whatever the
    // resolver counted: `doctor` is entered only through one.
    let rework_rounds = if start == Stage::Doctor {
        current.rework_rounds.max(1)
    } else {
        current.rework_rounds
    };
    let repo = input.subject.repo.as_str();
    explanation.current_stage = Some(CurrentStageRecord {
        stage: start,
        entered_at: current.entered_at,
        age_sec: current.age_sec.max(0),
        age_source: current.age_source,
        rework_rounds,
    });
    explanation.history = Some(HistoryRecord {
        scope: history.scope,
        sources: rules
            .sources
            .iter()
            .map(|s| s.journal().to_string())
            .collect(),
        samples_by_source: BTreeMap::new(),
        samples_by_host: BTreeMap::new(),
    });
    explanation.history_window = Some(HistoryWindow {
        from: window_from(as_of),
        to: as_of,
        sources: rules
            .sources
            .iter()
            .map(|s| s.journal().to_string())
            .collect(),
    });

    // `start` ends at dispatch: no merge share and no verdict to model.
    if rules.kind == Kind::Start {
        explanation.path = Some(PathRecord {
            start,
            include_merge: false,
            terminal: Stage::ReadyWait,
            merge_share: None,
            merge_share_n: None,
            dispatch,
        });
        return finish_estimate(explanation, rules, input, history, false, Vec::new());
    }

    // Where an approved path ends.
    let (include_merge, merge_share) = if rules.always_merge || approved {
        (true, None)
    } else {
        match history.merge_share(repo, as_of) {
            Some((share, n)) => (share >= 0.5, Some((round3(share), n))),
            None => return refuse(explanation, NoEstimateReason::InsufficientSamples),
        }
    };
    explanation.path = Some(PathRecord {
        start,
        include_merge,
        terminal: if include_merge {
            Stage::MergeWait
        } else {
            Stage::ReviewWait
        },
        merge_share: merge_share.map(|(s, _)| s),
        merge_share_n: merge_share.map(|(_, n)| n),
        dispatch,
    });

    // The Judge branch, when the path still reaches a verdict.
    let verdict_ahead = !approved && rework_rounds < MAX_REWORK_ROUNDS;
    let mut p_by_attempt = Vec::new();
    if !approved {
        let level = [Level::Repo, Level::Host].into_iter().find(|level| {
            history
                .verdict_counts(repo, *level, as_of, MAX_REWORK_ROUNDS)
                .first()
                .is_some_and(|(n, _)| *n >= MIN_SAMPLES)
        });
        let Some(level) = level else {
            if verdict_ahead {
                return refuse(explanation, NoEstimateReason::InsufficientSamples);
            }
            // At the cap no verdict is drawn; record the cap alone.
            explanation.branches = Some(Branches {
                changes_requested: ChangesRequested {
                    p_by_attempt: Vec::new(),
                    n_by_attempt: Vec::new(),
                    source_by_attempt: Vec::new(),
                    level: "none".to_string(),
                    cap: MAX_REWORK_ROUNDS,
                    after_cap: "approve".to_string(),
                    expected_rework_rounds: None,
                    source: "judge_verdicts".to_string(),
                },
            });
            return finish_estimate(explanation, rules, input, history, include_merge, Vec::new());
        };
        let counts = history.verdict_counts(repo, level, as_of, MAX_REWORK_ROUNDS);
        let mut sources = Vec::new();
        let mut previous = 0.0;
        for (n, rejected) in &counts {
            if *n >= MIN_VERDICTS {
                previous = round3(*rejected as f64 / *n as f64);
                sources.push("observed".to_string());
            } else {
                sources.push("carried_forward".to_string());
            }
            p_by_attempt.push(previous);
        }
        explanation.branches = Some(Branches {
            changes_requested: ChangesRequested {
                p_by_attempt: p_by_attempt.clone(),
                n_by_attempt: counts.iter().map(|(n, _)| *n).collect(),
                source_by_attempt: sources,
                level: level.as_str().to_string(),
                cap: MAX_REWORK_ROUNDS,
                after_cap: "approve".to_string(),
                expected_rework_rounds: None,
                source: "judge_verdicts".to_string(),
            },
        });
    }
    finish_estimate(explanation, rules, input, history, include_merge, p_by_attempt)
}

fn finish_estimate(
    mut explanation: Explanation,
    rules: PathRules,
    input: &EstimateInput,
    history: &StageSamples,
    include_merge: bool,
    p_by_attempt: Vec<f64>,
) -> Explanation {
    let Ok(current) = current_of(rules, input) else {
        unreachable!("refusals return before the path is built")
    };
    let start = current.stage;
    let age = current.age_sec.max(0);
    let repo = input.subject.repo.as_str();
    let as_of = input.as_of;
    // The rework count as recorded (normalised for `doctor`), so the
    // simulation and this stage list agree on the rejections still ahead.
    let rework_rounds = explanation
        .current_stage
        .as_ref()
        .map_or(current.rework_rounds, |c| c.rework_rounds);
    let rejectable = may_reject(&p_by_attempt, rework_rounds, MAX_REWORK_ROUNDS);
    let stop_at_dispatch = rules.kind == Kind::Start;
    let mut tail_extrapolated = false;
    // The stall term is applied before the explanation is simulated, so the
    // simulation (and anyone recomputing it) reads it from the record.
    if let Some(stalled) = &mut explanation.stalled {
        stalled.applied = rules.stall_term;
    }

    for stage in reachable_path(start, include_merge, rejectable, stop_at_dispatch) {
        let stage_samples = select_stage(history, rules, repo, stage, as_of);
        let Some((selection, weighted)) = stage_samples else {
            return refuse(explanation, NoEstimateReason::InsufficientSamples);
        };
        if let Some(record) = &mut explanation.history {
            for (source, n) in &selection.by_source {
                *record.samples_by_source.entry(source.clone()).or_insert(0) += n;
            }
            for (host, n) in &selection.by_host {
                *record.samples_by_host.entry(host.clone()).or_insert(0) += n;
            }
        }
        let sorted = &selection.sorted;
        // #9328: a censoring heuristic also reads the stage's right-censored
        // lower bounds, at the level the observed selection resolved to, and
        // summarises the pair by Kaplan–Meier. Every other heuristic passes an
        // empty slice, for which `km_grid_of` is exactly `grid_of`. #10209: a
        // recency-weighting heuristic summarises its weighted samples instead.
        let (censored, raw_grid, recency) = match weighted {
            Some(w) => {
                let censored: Vec<i64> = w.censored.iter().map(|c| c.0).collect();
                let raw = grid::weighted_grid_of(&w.observed, &w.censored);
                (censored, raw, Some((w.half_life_sec, w.effective_n)))
            }
            None => {
                let censored = if rules.censoring {
                    history.select_censored(repo, stage, as_of, rules.sources, selection.level)
                } else {
                    Vec::new()
                };
                let raw = grid::km_grid_of(sorted, &censored);
                (censored, raw, None)
            }
        };
        let (grid_sec, adjustment) = match rules.adjust {
            Some(adjust) => adjust(stage, input, raw_grid),
            None => (raw_grid, None),
        };
        // A queue wait is never age-conditioned (see `simulate`).
        let conditioning = if stage == start && age > 0 && stage != Stage::ReadyWait {
            // A censored sample longer than the age is evidence the stage can
            // outlive it just as an observed one is, so it counts here too.
            let n_above = sorted.iter().filter(|&&d| d > age).count()
                + censored.iter().filter(|&&d| d > age).count();
            let f_age = round6(grid::cdf(&grid_sec, age));
            let mut record = Conditioning {
                age_sec: age,
                f_age,
                n_above,
                method: CONDITIONING_TRUNCATE.to_string(),
            };
            let beyond = n_above < MIN_COND || f_age >= 1.0;
            if beyond && rules.residual_tail {
                // #10210: the item has outlived its history. Answer with the
                // residual-life tail of its own age, flagged, rather than
                // refusing exactly the slow tail accuracy most needs to see.
                record.method = CONDITIONING_RESIDUAL_LIFE.to_string();
                tail_extrapolated = true;
            } else if beyond {
                explanation.stages.push(stage_entry(
                    stage,
                    rules,
                    repo,
                    selection.level,
                    sorted,
                    &censored,
                    grid_sec,
                    adjustment,
                    recency,
                    Some(record),
                ));
                return refuse(explanation, NoEstimateReason::BeyondHistory);
            }
            Some(record)
        } else {
            None
        };
        explanation.stages.push(stage_entry(
            stage,
            rules,
            repo,
            selection.level,
            sorted,
            &censored,
            grid_sec,
            adjustment,
            recency,
            conditioning,
        ));
    }

    let seed = seed_for(&explanation.estimate_id);
    explanation.combination = Some(Combination {
        method: "monte_carlo_grid_resample".to_string(),
        draws: DRAWS,
        seed: format!("0x{seed:016x}"),
        rng: "splitmix64".to_string(),
        draw_order: if start == Stage::ReadyWait {
            "per path: one uniform per ready_wait turnover (path.dispatch.turnovers), plus path.dispatch.admission_delay_sec; then one uniform per stage visited; after each review_wait below the cap, one uniform for its verdict (u < p = rejected)"
        } else {
            "per path: one uniform per stage visited; after each review_wait below the cap, one uniform for its verdict (u < p = rejected)"
        }
        .to_string(),
        independence_assumed: true,
    });

    // Explanation first: the simulation reads only what the explanation holds.
    let Some(spec) = spec_from_explanation(&explanation) else {
        return refuse(explanation, NoEstimateReason::UnknownStage);
    };
    let Ok(simulation) = run(&spec) else {
        return refuse(explanation, NoEstimateReason::InsufficientSamples);
    };
    let (p25, p50, p75, p90) = simulation.quantiles;
    for entry in &mut explanation.stages {
        entry.reached_with_probability = Some(simulation.reached[entry.stage.index()]);
        entry.mean_visits = Some(simulation.mean_visits[entry.stage.index()]);
    }
    if let Some(branches) = &mut explanation.branches {
        branches.changes_requested.expected_rework_rounds = Some(simulation.expected_rework_rounds);
    }
    let samples_min = explanation
        .stages
        .iter()
        .map(|e| e.distribution.n)
        .min()
        .unwrap_or(0);
    explanation.result = Some(EstimateResult {
        p25_sec: p25,
        p50_sec: p50,
        p75_sec: p75,
        p90_sec: Some(p90),
        eta_p50_at: as_of + Duration::seconds(p50),
        samples_min,
        stage_marks: simulation.stage_marks(as_of),
        tail_extrapolated,
    });
    explanation.contributions = Some(simulation.contributions);
    explanation.enforce_cap();
    explanation
}

/// One stage's samples: the flat selection, plus — for a recency-weighting
/// heuristic (#10209) — the weighted samples over the same population.
fn select_stage(
    history: &StageSamples,
    rules: PathRules,
    repo: &str,
    stage: Stage,
    as_of: chrono::DateTime<chrono::Utc>,
) -> Option<(Selection, Option<WeightedSelection>)> {
    match rules.half_life_sec {
        None => history
            .select(repo, stage, as_of, rules.sources)
            .map(|s| (s, None)),
        Some(half_life) => history
            .select_weighted(repo, stage, as_of, rules.sources, half_life, rules.censoring)
            .map(|w| (w.selection.clone(), Some(w))),
    }
}

/// The grid index of percentile `pct` (`grid_pct()` is `0, 5, …, 100`).
fn at_pct(grid_sec: &[i64], pct: usize) -> i64 {
    grid_sec
        .get(pct / 5)
        .copied()
        .unwrap_or_else(|| grid_sec.last().copied().unwrap_or(0))
}

#[allow(clippy::too_many_arguments)]
fn stage_entry(
    stage: Stage,
    rules: PathRules,
    repo: &str,
    level: Level,
    sorted: &[i64],
    censored: &[i64],
    grid_sec: Vec<i64>,
    adjustment: Option<StageAdjustment>,
    recency: Option<(Option<i64>, f64)>,
    conditioning: Option<Conditioning>,
) -> StageEntry {
    // The quartiles are read off the same grid the simulation draws from, so
    // a censored distribution's summary and its draws cannot disagree. With
    // no censoring `grid_sec[pct / 5]` is `nearest_rank(sorted, pct)` by
    // construction, so every v1 value is unchanged.
    let (p25, p50, p75, p90) = (
        at_pct(&grid_sec, 25),
        at_pct(&grid_sec, 50),
        at_pct(&grid_sec, 75),
        at_pct(&grid_sec, 90),
    );
    StageEntry {
        stage,
        distribution: Distribution {
            n: sorted.len(),
            censored_n: rules.censoring.then_some(censored.len()),
            filters: Filters {
                repo: (level == Level::Repo).then(|| repo.to_string()),
                level: level.as_str().to_string(),
                size_bucket: None,
                sources: rules
                    .sources
                    .iter()
                    .map(|s| s.journal().to_string())
                    .collect(),
            },
            grid_pct: grid::grid_pct(),
            grid_sec,
            p25,
            p50,
            p75,
            p90,
            adjustment,
            half_life_sec: recency.and_then(|(h, _)| h),
            effective_n: recency.map(|(_, e)| e),
        },
        conditioning,
        reached_with_probability: None,
        mean_visits: None,
    }
}
