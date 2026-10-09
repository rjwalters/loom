//! `eta explain` (#10930): registry-wide replay parity, the truncation
//! guard, marginal contributions and the input diff.

use std::collections::BTreeSet;
use std::sync::Arc;

use super::held_heron::{held, hold_history};
use super::keen_wren::v2_fixture;
use super::land_twin_otter::{fit_as_of, fixture_fit, review_input};
use super::ready::{dispatch, history_ready, ready_input};
use super::{history_a, input_at};
use crate::eta::explain::{self, context, diff, inputs, marginal, with_input, Parity};
use crate::eta::explanation::{
    Explanation, Features, ReplayEngine, MAX_BYTES, TRUNCATED_CONTEXT, TRUNCATED_FEATURES,
    TRUNCATED_GRIDS, TRUNCATED_STAGE_MARKS,
};
use crate::eta::heuristics::LittleV0;
use crate::eta::history::StageSamples;
use crate::eta::simulate::run_explanation;
use crate::eta::stage_queue::{QueueScope, StageQueue, HALF_LIFE_SEC, WINDOW_SEC};
use crate::eta::{EstimateInput, Heuristic, Kind, Registry, Stage};

/// Heuristics the fixtures below cannot make answer, with where their replay
/// parity is tested instead. Adding a heuristic means either making it
/// answer here or naming it here — never neither. Empty today: every
/// registered heuristic answers at least one case.
const NOT_ANSWERED_HERE: &[(&str, &str)] = &[];

/// Replay engines the fixtures below cannot reach, with where they are
/// replayed instead.
const ENGINES_NOT_REACHED_HERE: &[(ReplayEngine, &str)] = &[(
    ReplayEngine::Dependency,
    "needs a dependency graph with answered parents; tests/dependency.rs replays it",
)];

fn queued(stage: Stage, items_ahead: u32) -> EstimateInput {
    let mut input = input_at(stage, 0, 0);
    input.queue = vec![StageQueue {
        stage,
        scope: QueueScope::of(stage),
        items_ahead,
        drain_rate_per_hr: 2.0,
        half_life_sec: HALF_LIFE_SEC,
        window_sec: WINDOW_SEC,
        exits: 30,
    }];
    input
}

/// Every fixture input, with the history it is estimated over.
fn cases() -> Vec<(&'static str, EstimateInput, StageSamples)> {
    let mut cases = Vec::new();
    for (stage, age, rework) in [
        (Stage::SweepCurator, 0, 0),
        (Stage::SweepBuilder, 1800, 0),
        (Stage::ReviewWait, 0, 0),
        (Stage::ReviewWait, 900, 0),
        (Stage::Doctor, 300, 1),
        (Stage::MergeWait, 60, 0),
    ] {
        cases.push(("input_at", input_at(stage, age, rework), history_a()));
    }
    cases.push(("ready", ready_input(Some(dispatch())), history_ready()));
    cases.push(("twin_otter_row", review_input(), history_a()));
    cases.push(("held", held(), hold_history("rjwalters/loom")));
    cases.push(("queued", queued(Stage::ReviewWait, 6), history_a()));
    cases
}

fn registry() -> Registry {
    Registry::with_fits(
        Some(Arc::new(fixture_fit(fit_as_of()))),
        Some(Arc::new(v2_fixture(fit_as_of(), 0.0, 0.0))),
    )
}

#[test]
fn every_registered_heuristic_replays_its_logged_explanation_exactly() {
    let registry = registry();
    let mut answered = BTreeSet::new();
    let mut engines = Vec::new();
    for kind in [Kind::Start, Kind::Finish, Kind::Land] {
        for heuristic in registry.for_kind(kind) {
            for (what, input, history) in cases() {
                let made = heuristic.estimate(&input, &history);
                let id = heuristic.id();
                // Only the logged JSON crosses this line.
                let json = serde_json::to_string(&made).unwrap();
                let logged: Explanation = serde_json::from_str(&json).unwrap();
                let Some(recorded) = logged.quantiles_with_p90() else {
                    assert!(logged.result.is_none(), "{id} on {what}: an answer without p90");
                    continue;
                };
                assert_eq!(
                    run_explanation(&logged),
                    Some(recorded),
                    "{id} ({kind:?}) on {what}: the replay reproduces p25/p50/p75/p90 \
                     (engine {})",
                    logged.replay_engine().as_str()
                );
                answered.insert(id);
                if !engines.contains(&logged.replay_engine()) {
                    engines.push(logged.replay_engine());
                }
            }
        }
    }
    for id in registry.ids() {
        let excused = NOT_ANSWERED_HERE.iter().any(|(x, _)| *x == id);
        assert!(
            answered.contains(id) != excused,
            "{id}: answered here = {}, listed in NOT_ANSWERED_HERE = {excused}; \
             a heuristic must be one or the other",
            answered.contains(id)
        );
    }
    for engine in [
        ReplayEngine::Dependency,
        ReplayEngine::HeldHeron,
        ReplayEngine::Queue,
        ReplayEngine::TwinOtter,
        ReplayEngine::Path,
    ] {
        let excused = ENGINES_NOT_REACHED_HERE.iter().any(|(x, _)| *x == engine);
        assert!(
            engines.contains(&engine) != excused,
            "engine {}: reached here = {}, excused = {excused}",
            engine.as_str(),
            engines.contains(&engine)
        );
    }
}

#[test]
fn little_v0_replays_from_its_queue_record() {
    let e = LittleV0.estimate(&queued(Stage::ReviewWait, 6), &history_a());
    assert_eq!(e.replay_engine(), ReplayEngine::Queue);
    assert_eq!(run_explanation(&e), e.quantiles_with_p90());
    let zero = LittleV0.estimate(&queued(Stage::ReviewWait, 0), &history_a());
    assert_eq!(run_explanation(&zero), zero.quantiles_with_p90());
}

// ---- the truncation guard -------------------------------------------------

/// `e` padded through a field no tier drops (the sweep id), so that it is
/// `over` bytes past the cap (under it, when negative) once `strip` has run
/// on a copy.
fn padded(mut e: Explanation, strip: impl Fn(&mut Explanation), over: isize) -> Explanation {
    let mut probe = e.clone();
    strip(&mut probe);
    let room = MAX_BYTES as isize - probe.size_bytes() as isize;
    let id = probe.subject.sweep_id.as_deref().unwrap_or_default().len() as isize;
    let len = usize::try_from(id + room + over).expect("room for the pad");
    e.subject.sweep_id = Some("x".repeat(len));
    e
}

fn strip_to_context(e: &mut Explanation) {
    e.features = e.features.as_ref().map(Features::input_vector);
    e.features_omitted.clear();
    if let Some(r) = &mut e.result {
        r.stage_marks.clear();
    }
    e.stage_predictions.clear();
    e.contributions = None;
    if let Some(w) = &mut e.history_window {
        w.sources.clear();
    }
    if let Some(h) = &mut e.history {
        h.sources.clear();
        h.samples_by_source.clear();
        h.samples_by_host.clear();
    }
    for s in &mut e.stages {
        s.distribution.grid_pct.clear();
    }
}

#[test]
fn the_cap_drops_every_non_replay_field_before_a_replay_input() {
    let base =
        crate::eta::heuristics::LandV1.estimate(&input_at(Stage::ReviewWait, 900, 0), &history_a());
    let recorded = base.quantiles_with_p90();
    assert!(recorded.is_some());

    // Just over the cap after the context tier would still be over: the
    // grids go, and the record says the replay is lost.
    let mut e = padded(base.clone(), strip_to_context, 64);
    e.enforce_cap();
    assert!(e.size_bytes() <= MAX_BYTES, "{} bytes", e.size_bytes());
    assert_eq!(
        e.truncated,
        [
            TRUNCATED_FEATURES,
            TRUNCATED_STAGE_MARKS,
            TRUNCATED_CONTEXT,
            TRUNCATED_GRIDS
        ]
    );
    assert_eq!(e.replayable, Some(false));
    assert_eq!(e.replayable_reason.as_deref(), Some("truncated:stages.distribution.grid"));
    assert_eq!(run_explanation(&e), None, "never a silent mismatch");
    assert_eq!(e.quantiles_with_p90(), recorded, "the numbers survive");
    let r = explain::report(&e);
    assert!(matches!(r.parity, Parity::NotReplayable { .. }), "{:?}", r.parity);

    // Just under it: every tier up to the context one ran, and it still
    // replays exactly, with no replayable flag written.
    let mut e = padded(base.clone(), strip_to_context, -64);
    e.enforce_cap();
    assert!(e.size_bytes() <= MAX_BYTES);
    assert_eq!(e.truncated, [TRUNCATED_FEATURES, TRUNCATED_STAGE_MARKS, TRUNCATED_CONTEXT]);
    assert_eq!(e.replayable, None);
    assert_eq!(run_explanation(&e), recorded, "a capped record still replays exactly");
    let json = serde_json::to_value(&e).unwrap();
    assert!(json.get("replayable").is_none(), "absent unless lost");
}

#[test]
fn the_cap_keeps_the_input_vector() {
    let mut input = input_at(Stage::ReviewWait, 0, 0);
    input.features.labels = Some((0..2000).map(|i| format!("label-{i:>20}")).collect());
    input.features.queue_rank = Some(3);
    input.features.max_concurrent = Some(6);
    input.features.queue_running = Some(4);
    input.features.operator_hold = Some(false);
    let e = crate::eta::heuristics::LandV1.estimate(&input, &history_a());
    assert_eq!(e.truncated, [TRUNCATED_FEATURES]);
    let f = e.features.as_ref().expect("the input vector survives");
    assert_eq!(f.labels, None, "the bulk is cut");
    assert_eq!(f.complexity_marker, None);
    assert_eq!(
        (f.queue_rank, f.max_concurrent, f.queue_running, f.operator_hold),
        (Some(3), Some(6), Some(4), Some(false))
    );
    assert_eq!(e.replayable, None);
    assert_eq!(run_explanation(&e), e.quantiles_with_p90());
    let names: Vec<String> = context(&e).into_iter().map(|(n, _)| n).collect();
    assert_eq!(
        names,
        [
            "queue_rank",
            "queue_running",
            "max_concurrent",
            "operator_hold"
        ]
    );
}

#[test]
fn every_input_vector_name_is_a_feature() {
    for name in Features::INPUT_VECTOR {
        assert!(
            Features::NAMES.contains(&name)
                || ["starred_any", "star_source", "priority"].contains(&name),
            "{name} is not a feature"
        );
    }
}

// ---- marginal ---------------------------------------------------------------

#[test]
fn marginal_is_deterministic_and_a_no_op_moves_nothing() {
    let e =
        crate::eta::heuristics::LandV1.estimate(&input_at(Stage::ReviewWait, 900, 0), &history_a());
    let (_, p50, _, p90) = e.quantiles_with_p90().unwrap();
    for input in inputs(&e) {
        let same = with_input(&e, &input.name, input.value).expect("a number in the record");
        assert_eq!(
            run_explanation(&same).map(|q| (q.1, q.3)),
            Some((p50, p90)),
            "setting {} to its own value is a no-op",
            input.name
        );
    }
    assert_eq!(marginal(&e), marginal(&e), "deterministic");
    let names: Vec<_> = inputs(&e).into_iter().map(|i| i.name).collect();
    assert!(names.contains(&"current_stage.age_sec".to_string()), "{names:?}");
    assert!(names.contains(&"current_stage.rework_rounds".to_string()), "{names:?}");
}

#[test]
fn marginal_ranks_the_only_input_that_matters_first() {
    // A merge_wait path has one stage and no review loop: only the age in
    // it moves the answer.
    let e =
        crate::eta::heuristics::LandV1.estimate(&input_at(Stage::MergeWait, 60, 0), &history_a());
    let m = marginal(&e);
    assert_eq!(m[0].input.name, "current_stage.age_sec", "{m:?}");
    assert_ne!(m[0].dp50_sec, Some(0));
    let rework = m
        .iter()
        .find(|x| x.input.name == "current_stage.rework_rounds")
        .expect("listed");
    assert_eq!(rework.dp50_sec, Some(0), "no review stage ahead: rework moves nothing");
}

#[test]
fn queue_inputs_are_perturbable() {
    let e = LittleV0.estimate(&queued(Stage::ReviewWait, 6), &history_a());
    let m = marginal(&e);
    let ahead = m
        .iter()
        .find(|x| x.input.name == "queue.items_ahead")
        .expect("listed");
    // One more item ahead at 2 per hour is 30 more minutes.
    assert_eq!(ahead.dp50_sec, Some(1800));
}

// ---- diff -------------------------------------------------------------------

#[test]
fn diff_names_the_one_changed_input_with_no_residual() {
    let a = LittleV0.estimate(&queued(Stage::ReviewWait, 6), &history_a());
    let b = LittleV0.estimate(&queued(Stage::ReviewWait, 12), &history_a());
    let d = diff(&a, &b).expect("both replay");
    assert!(d.same_engine);
    assert_eq!(d.swaps[0].name, "queue.items_ahead");
    assert_eq!((d.swaps[0].from, d.swaps[0].to), (6.0, 12.0));
    assert_eq!(d.swaps.len(), 1, "{:?}", d.swaps);
    assert_eq!(d.swaps[0].dp50_sec, Some(d.b_p50_sec - d.a_p50_sec));
    assert_eq!((d.residual_p50_sec, d.residual_p90_sec), (0, 0));
}

#[test]
fn diff_reports_the_residual_and_the_changed_context() {
    let mut ia = input_at(Stage::ReviewWait, 900, 0);
    ia.features.queue_rank = Some(3);
    let mut ib = input_at(Stage::ReviewWait, 1800, 1);
    ib.features.queue_rank = Some(11);
    ib.as_of = ia.as_of + chrono::Duration::minutes(5);
    let a = crate::eta::heuristics::LandV1.estimate(&ia, &history_a());
    let b = crate::eta::heuristics::LandV1.estimate(&ib, &history_a());
    let d = diff(&a, &b).expect("both replay");
    let names: Vec<&str> = d.swaps.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"current_stage.age_sec"), "{names:?}");
    assert!(names.contains(&"current_stage.rework_rounds"), "{names:?}");
    let explained: i64 = d.swaps.iter().filter_map(|s| s.dp50_sec).sum();
    assert_eq!(
        explained + d.residual_p50_sec,
        d.b_p50_sec - d.a_p50_sec,
        "the residual closes the sum"
    );
    assert!(d
        .context_changed
        .iter()
        .any(|c| c.name == "queue_rank" && c.from == 3 && c.to == 11));
}
