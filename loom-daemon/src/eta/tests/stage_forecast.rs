//! Per-stage forecasts and error attribution (#10929): the simulator's
//! stage predictions, their point-in-time replay, and the attribution
//! identity `Σ contribution + unattributed = error`.

use super::{as_of, history_a, input_at, provenance};
use crate::eta::heuristics::{FinishV1, LandV1};
use crate::eta::history::{SampleSource, StageSample};
use crate::eta::score::{score, EstimateSummary, OutcomeKind, StageActual};
use crate::eta::simulate::{run_explanation, run_predictions};
use crate::eta::stage_forecast::{
    apportion, attribute, attribute_scored, StagePrediction, StagePredictions,
};
use crate::eta::tracker::{EstimateContext, Tracker};
use crate::eta::{Explanation, Heuristic, Kind, Registry, Stage, StageSamples};
use chrono::{DateTime, Duration, Utc};
use std::collections::BTreeMap;

const REPO: &str = "rjwalters/loom";

fn t(secs: i64) -> DateTime<Utc> {
    as_of() + Duration::seconds(secs)
}

fn cases() -> Vec<(Stage, i64, u32)> {
    vec![
        (Stage::SweepCurator, 0, 0),
        (Stage::SweepBuilder, 1800, 0),
        (Stage::ReviewWait, 0, 0),
        (Stage::ReviewWait, 900, 0),
        (Stage::Doctor, 300, 1),
        (Stage::MergeWait, 60, 0),
    ]
}

fn sum_alloc(predictions: &StagePredictions) -> i64 {
    predictions.values().map(|p| p.alloc).sum()
}

#[test]
fn the_path_engine_forecasts_every_stage_ahead_and_alloc_sums_to_the_p50() {
    for (stage, age, rework) in cases() {
        let e = LandV1.estimate(&input_at(stage, age, rework), &history_a());
        let result = e.result.as_ref().expect("fixture estimates");
        let p = &e.stage_predictions;
        assert!(!p.is_empty(), "a simulated estimate forecasts its stages ({stage})");
        // The current stage is entered now and reached on every path.
        let current = p.get(&stage).expect("the current stage is forecast");
        assert_eq!((current.entry_p50, current.entry_p90, current.reach_pct), (0, 0, 100));
        assert_eq!(sum_alloc(p), result.p50_sec, "alloc is a split of the p50 ({stage})");
        for (s, f) in p {
            assert!(f.entry_p50 <= f.entry_p90, "{s}: entry p50 <= p90");
            assert!(f.dwell_p50 <= f.dwell_p90, "{s}: dwell p50 <= p90");
            assert!(f.entry_p50 >= 0 && f.dwell_p50 >= 0 && f.alloc >= 0);
            assert!(f.reach_pct <= 100);
        }
        // `merge_wait` is the last stage: no path is still inside it at p90.
        if let Some(merge) = p.get(&Stage::MergeWait) {
            assert!(merge.entry_p90 + merge.dwell_p90 >= merge.entry_p50);
        }
    }
}

#[test]
fn forecasting_moves_no_quantile_and_no_mark() {
    // The forecast reads draws already made: the explanation still
    // recomputes its own four quantiles exactly.
    for (stage, age, rework) in cases() {
        let e = LandV1.estimate(&input_at(stage, age, rework), &history_a());
        assert_eq!(run_explanation(&e), e.quantiles_with_p90(), "{stage}");
    }
}

#[test]
fn a_refusal_and_a_finish_estimate_carry_what_they_model() {
    let finish = FinishV1.estimate(&input_at(Stage::SweepBuilder, 0, 0), &history_a());
    assert!(finish.stage_predictions.contains_key(&Stage::SweepBuilder));
    // Only stages the path can visit, never a stage behind the item.
    let on_path: Vec<Stage> = finish.stages.iter().map(|e| e.stage).collect();
    for stage in finish.stage_predictions.keys() {
        assert!(on_path.contains(stage), "{stage} is not on finish's path");
    }
    assert!(!finish.stage_predictions.contains_key(&Stage::SweepCurator));
    let refused = LandV1.estimate(&input_at(Stage::ReviewWait, 0, 0), &StageSamples::default());
    assert!(refused.result.is_none());
    assert!(refused.stage_predictions.is_empty(), "a refusal forecasts nothing");
    let wire = serde_json::to_value(&refused).unwrap();
    assert!(wire.get("stage_predictions").is_none(), "absent, never empty");
}

/// Point-in-time replay: a stage forecast is a function of what was known at
/// `as_of` alone. It recomputes from the serialized explanation's own
/// fields, and history first known after `as_of` cannot move it. The
/// positive control proves the perturbation would bite if it leaked.
#[test]
fn stage_predictions_are_point_in_time_and_replay_from_the_explanation() {
    let future = |known_at: DateTime<Utc>| {
        let mut h = history_a();
        for stage in Stage::ALL {
            for i in 0..40 {
                h.stages.push(StageSample {
                    repo: REPO.to_string(),
                    stage,
                    duration_sec: 400_000 + i * 1_000,
                    observed_at: known_at,
                    source: SampleSource::StageJournal,
                    host: "host-future".to_string(),
                    worked: None,
                });
            }
        }
        h
    };
    for (stage, age, rework) in cases() {
        let input = input_at(stage, age, rework);
        let base = LandV1.estimate(&input, &history_a());

        let json = serde_json::to_string(&base).unwrap();
        let parsed: Explanation = serde_json::from_str(&json).unwrap();
        assert_eq!(
            run_predictions(&parsed).as_ref(),
            Some(&base.stage_predictions),
            "recomputes from the explanation alone ({stage})"
        );

        let after = LandV1.estimate(&input, &future(as_of() + Duration::seconds(1)));
        assert_eq!(
            after.stage_predictions, base.stage_predictions,
            "history known after as_of leaked into the forecast ({stage})"
        );
        let before = LandV1.estimate(&input, &future(as_of() - Duration::hours(1)));
        assert_ne!(
            before.stage_predictions, base.stage_predictions,
            "positive control: the same history known before as_of moves it ({stage})"
        );
    }
}

#[test]
fn apportion_is_exact_and_proportional() {
    let weights = [
        (Stage::ReviewWait, 1.0),
        (Stage::Doctor, 1.0),
        (Stage::MergeWait, 1.0),
    ];
    for target in [0, 1, 2, 3, 10, 100, 12_345] {
        let split = apportion(&weights, target);
        assert_eq!(split.values().sum::<i64>(), target, "target {target}");
        let (lo, hi) = (split.values().min().unwrap(), split.values().max().unwrap());
        assert!(hi - lo <= 1, "equal weights split within a second: {split:?}");
    }
    let skewed = apportion(&[(Stage::ReviewWait, 3.0), (Stage::MergeWait, 1.0)], 1000);
    assert_eq!(skewed[&Stage::ReviewWait], 750);
    assert_eq!(skewed[&Stage::MergeWait], 250);
    assert!(apportion(&[(Stage::Doctor, 0.0)], 50)
        .values()
        .all(|v| *v == 0));
}

fn prediction(entry: i64, alloc: i64) -> StagePrediction {
    StagePrediction {
        entry_p50: entry,
        entry_p90: entry,
        dwell_p50: alloc,
        dwell_p90: alloc,
        alloc,
        reach_pct: 100,
    }
}

fn visit(stage: Stage, from: i64, to: i64) -> StageActual {
    StageActual {
        stage,
        entered_at: t(from),
        left_at: t(to),
        duration_sec: to - from,
        predicted_p25: None,
        predicted_p50: None,
        predicted_p75: None,
        error_sec: None,
        source: "bus".to_string(),
    }
}

/// The acceptance identity: per-stage contributions plus the unattributed
/// rest equal the total error, whatever was or was not observed.
#[test]
fn per_stage_contributions_sum_to_the_total_error() {
    // Forecast: review 1000 s, doctor 300 s, merge 200 s (p50 = 1500).
    let predictions: StagePredictions = BTreeMap::from([
        (Stage::ReviewWait, prediction(0, 1000)),
        (Stage::Doctor, prediction(1000, 300)),
        (Stage::MergeWait, prediction(1300, 200)),
    ]);
    let p50 = sum_alloc(&predictions);

    // Fully observed: in review since before as_of (counted from as_of),
    // two review visits around a rework, then merge.
    let full = [
        visit(Stage::ReviewWait, -500, 1800),
        visit(Stage::Doctor, 1800, 2400),
        visit(Stage::ReviewWait, 2400, 3000),
        visit(Stage::MergeWait, 3000, 3600),
    ];
    let error = 3600 - p50;
    let a = attribute(&predictions, as_of(), error, &full).unwrap();
    let total: i64 = a.stages.values().map(|s| s.contribution_sec).sum();
    assert_eq!(total + a.unattributed_sec, error);
    assert_eq!(a.unattributed_sec, 0, "a fully observed path leaves nothing unexplained");
    let review = &a.stages[&Stage::ReviewWait];
    assert_eq!(review.actual_dwell_sec, 1800 + 600, "both visits, counted from as_of");
    assert_eq!(review.actual_entry_sec, Some(0));
    assert_eq!(review.contribution_sec, 2400 - 1000);
    assert_eq!(a.stages[&Stage::Doctor].actual_entry_sec, Some(1800));
    assert_eq!(a.dominant_stage, Some(Stage::ReviewWait));

    // Partly observed, an unforecast stage, and a stall-like offset: the
    // identity still holds, with the rest unattributed.
    let partial = [
        visit(Stage::ReviewWait, 200, 900),
        visit(Stage::MergeHold, 900, 5000),
    ];
    for error in [-1500, 0, 777, 90_000] {
        let a = attribute(&predictions, as_of(), error, &partial).unwrap();
        let total: i64 = a.stages.values().map(|s| s.contribution_sec).sum();
        assert_eq!(total + a.unattributed_sec, error, "error {error}");
        assert_eq!(a.stages[&Stage::MergeHold].predicted_entry_sec, None);
        assert_eq!(a.stages[&Stage::MergeHold].contribution_sec, 4100);
    }

    // Nothing forecast: no attribution, never a fabricated one.
    assert!(attribute(&StagePredictions::new(), as_of(), 10, &full).is_none());
}

fn context<'a>(
    registry: &'a Registry,
    history: &'a StageSamples,
    repo_ids: &'a BTreeMap<String, u64>,
    stalls: &'a crate::eta::stall::StallSnapshot,
) -> EstimateContext<'a> {
    EstimateContext {
        registry,
        current_start: None,
        current_finish: None,
        current_land: None,
        history,
        refresh_secs: 300,
        host_id: Some("host-test"),
        repo_ids,
        stalls,
    }
}

/// End to end through the tracker: a fully bus-observed sweep journals one
/// row per stage it left, and the primary `land` outcome's attribution sums
/// to its error with nothing unattributed.
#[test]
fn a_tracked_sweep_journals_its_stages_and_attributes_its_landing() {
    let registry = Registry::builtin();
    let history = history_a();
    let repo_ids = BTreeMap::from([(REPO.to_string(), 1_073_994_527_u64)]);
    let stalls = crate::eta::stall::StallSnapshot::default();
    let ctx = context(&registry, &history, &repo_ids, &stalls);
    let mut tracker = Tracker::new(provenance());

    let mut rows = Vec::new();
    let mut outcomes = Vec::new();
    let mut step = |effects: crate::eta::tracker::Effects| {
        rows.extend(effects.journal);
        outcomes.extend(effects.outcomes);
    };
    step(tracker.on_dispatch(REPO, 42, "sweep-issue-42-1", t(0)));
    let first = tracker.estimate(None, &ctx, t(1));
    // `land-v1`: the plain path engine, so its served p50 is the simulated one.
    let land_v1 = first
        .iter()
        .find(|e| e.explanation.kind == Kind::Land && e.explanation.heuristic == "land-v1")
        .expect("a land-v1 estimate")
        .explanation
        .clone();
    assert!(!land_v1.stage_predictions.is_empty());
    step(tracker.on_phase(REPO, 42, "curator", None, t(600)));
    tracker.estimate(None, &ctx, t(601));
    step(tracker.on_phase(REPO, 42, "builder", Some(4242), t(3000)));
    step(tracker.on_phase(REPO, 42, "judge", Some(4242), t(3900)));
    step(tracker.on_phase(REPO, 42, "doctor", Some(4242), t(5100)));
    step(tracker.on_phase(REPO, 42, "judge", Some(4242), t(6000)));
    let pending_before_merge: Vec<EstimateSummary> = tracker.pending().to_vec();
    let merge = tracker.on_phase(REPO, 42, "merge", Some(4242), t(6600));
    assert!(merge
        .journal
        .iter()
        .any(|r| r.stage == Some(Stage::MergeWait)));
    step(merge);

    // Every stage the sweep left is journaled, in order.
    let left: Vec<Stage> = rows
        .iter()
        .filter(|r| r.issue.is_some() && r.left_at.is_some())
        .filter_map(|r| r.stage)
        .collect();
    assert_eq!(
        left,
        vec![
            Stage::SweepCurator,
            Stage::SweepBuilder,
            Stage::ReviewWait,
            Stage::Doctor,
            Stage::ReviewWait,
            Stage::MergeWait,
        ]
    );
    assert!(!pending_before_merge.is_empty());

    // The land-v1 estimate's attribution: nothing unattributed on a
    // fully observed, unshifted path, and the identity holds exactly.
    let resolved = outcomes
        .iter()
        .find(|r| r.estimate.estimate_id == land_v1.estimate_id)
        .expect("the landing scored the first estimate");
    let error = resolved.score.error_sec.expect("landed, scored");
    let a = attribute_scored(&resolved.estimate, &resolved.score).expect("forecast stages");
    let total: i64 = a.stages.values().map(|s| s.contribution_sec).sum();
    assert_eq!(total + a.unattributed_sec, error);
    assert_eq!(a.unattributed_sec, 0, "{a:?}");
    assert_eq!(a.stages[&Stage::SweepCurator].actual_dwell_sec, 599, "from as_of = t(1)");
    assert_eq!(a.stages[&Stage::ReviewWait].actual_dwell_sec, 900 + 900);

    // A refusal and an abandoned outcome are never attributed.
    let abandoned = score(&resolved.estimate, OutcomeKind::Abandoned, t(6600), &[]);
    assert!(attribute_scored(&resolved.estimate, &abandoned).is_none());
}
