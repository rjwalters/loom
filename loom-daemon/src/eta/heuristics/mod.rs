//! The shipped heuristics. `finish-v1` and `land-v1` share one engine
//! ([`estimate_path`]); they differ in which history they read and where the
//! path ends. Their ids are immutable: a behaviour change is a new id.

mod finish_v1;
mod land_v1;

pub use finish_v1::{FinishV1, FINISH_V1};
pub use land_v1::{LandV1, LAND_V1};

use super::explanation::{
    Branches, ChangesRequested, Combination, Conditioning, CurrentStageRecord, Distribution,
    EstimateResult, Explanation, Filters, HistoryRecord, HistoryWindow, PathRecord, StageEntry,
};
use super::history::{window_from, Level, SampleSource, StageSamples};
use super::simulate::{may_reject, reachable, run, spec_from_explanation};
use super::{
    estimate_id, grid, round3, seed_for, CurrentState, EstimateInput, Kind, NoEstimateReason,
    Stage, DRAWS, EXPLANATION_SCHEMA, MAX_REWORK_ROUNDS, MIN_COND, MIN_SAMPLES,
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
}

/// Round to six decimals — every float the simulation reads is stored
/// rounded, so the JSON value parses back to the exact `f64` used.
fn round6(x: f64) -> f64 {
    (x * 1_000_000.0).round() / 1_000_000.0
}

fn refuse(mut explanation: Explanation, reason: NoEstimateReason) -> Explanation {
    explanation.no_estimate_reason = Some(reason);
    explanation.result = None;
    explanation.contributions = None;
    explanation.combination = None;
    explanation.enforce_cap();
    explanation
}

/// Estimate `input` along the path `rules` describe.
pub(crate) fn estimate_path(
    rules: PathRules,
    input: &EstimateInput,
    history: &StageSamples,
) -> Explanation {
    let as_of = input.as_of;
    let id = estimate_id(&input.subject, rules.kind, rules.id, as_of);
    let features_omitted = input
        .features
        .complete_omissions(input.features_omitted.clone(), "not_collected");
    let mut explanation = Explanation {
        schema: EXPLANATION_SCHEMA.to_string(),
        estimate_id: id,
        heuristic: rules.id.to_string(),
        kind: rules.kind,
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
        truncated: Vec::new(),
    };

    let current = match &input.current {
        CurrentState::Refused(reason) => return refuse(explanation, *reason),
        CurrentState::At(current) => current,
    };
    let start = current.stage;
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

    // Where an approved path ends.
    let (include_merge, merge_share) = if rules.always_merge || start == Stage::MergeWait {
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
    });

    // The Judge branch, when the path still reaches a verdict.
    let verdict_ahead = start != Stage::MergeWait && rework_rounds < MAX_REWORK_ROUNDS;
    let mut p_by_attempt = Vec::new();
    if start != Stage::MergeWait {
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
    let CurrentState::At(current) = &input.current else {
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

    for stage in reachable(start, include_merge, rejectable) {
        let Some(selection) = history.select(repo, stage, as_of, rules.sources) else {
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
        let grid_sec = grid::grid_of(sorted);
        let conditioning = if stage == start && age > 0 {
            let n_above = sorted.iter().filter(|&&d| d > age).count();
            let f_age = round6(grid::cdf(&grid_sec, age));
            let record = Conditioning {
                age_sec: age,
                f_age,
                n_above,
                method: "truncate_inverse_cdf".to_string(),
            };
            if n_above < MIN_COND || f_age >= 1.0 {
                explanation.stages.push(stage_entry(
                    stage,
                    rules,
                    repo,
                    selection.level,
                    sorted,
                    grid_sec,
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
            grid_sec,
            conditioning,
        ));
    }

    let seed = seed_for(&explanation.estimate_id);
    explanation.combination = Some(Combination {
        method: "monte_carlo_grid_resample".to_string(),
        draws: DRAWS,
        seed: format!("0x{seed:016x}"),
        rng: "splitmix64".to_string(),
        draw_order: "per path: one uniform per stage visited; after each review_wait below the cap, one uniform for its verdict (u < p = rejected)".to_string(),
        independence_assumed: true,
    });

    // Explanation first: the simulation reads only what the explanation holds.
    let Some(spec) = spec_from_explanation(&explanation) else {
        return refuse(explanation, NoEstimateReason::UnknownStage);
    };
    let Ok(simulation) = run(&spec) else {
        return refuse(explanation, NoEstimateReason::InsufficientSamples);
    };
    let (p25, p50, p75) = simulation.quantiles;
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
        eta_p50_at: as_of + Duration::seconds(p50),
        samples_min,
    });
    explanation.contributions = Some(simulation.contributions);
    explanation.enforce_cap();
    explanation
}

fn stage_entry(
    stage: Stage,
    rules: PathRules,
    repo: &str,
    level: Level,
    sorted: &[i64],
    grid_sec: Vec<i64>,
    conditioning: Option<Conditioning>,
) -> StageEntry {
    StageEntry {
        stage,
        distribution: Distribution {
            n: sorted.len(),
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
            p25: grid::quantile(sorted, 25),
            p50: grid::quantile(sorted, 50),
            p75: grid::quantile(sorted, 75),
            p90: grid::quantile(sorted, 90),
        },
        conditioning,
        reached_with_probability: None,
        mean_visits: None,
    }
}
