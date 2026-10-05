//! `land-2026-10-04-twin-otter` wired in (#10243): registration, the
//! refusal map, the `EstimateInput` adapter, the explanation and its
//! recomputation, the size cap, the tracker pass, and refresh stability.
//!
//! The model is the #10223 parity fixture's, packed into a coefficient file
//! the way #10222's tests do. Hot reload is tested next to the swap helper
//! in `observability/eta_tests.rs`.

use super::{as_of, history_a, provenance, subject, EXPLANATION_GOLDEN, TWIN_OTTER_PARITY};
use crate::eta::explanation::{
    Explanation, Features, MAX_BYTES, TRUNCATED_FEATURES, TRUNCATED_TWIN_OTTER_MODEL,
};
use crate::eta::fit::{
    AftFit, CoefficientFile, FitMeta, FitStage, FitWindow, Fitter, HazardFit, HazardSkip,
    PathStats, SkipReason,
};
use crate::eta::heuristics::{
    adapt_input, visit_entry, visit_seed, LandTwinOtter, LandTwinOtterB, LandV2, DRAW_ORDER,
    LAND_TWIN_OTTER, LAND_TWIN_OTTER_B, METHOD, PRE_PR_METHOD,
};
use crate::eta::labels::{
    pr_flags, FLAG_BLOCKED, FLAG_CI_FAIL, FLAG_CONFLICT, FLAG_OP_HOLD, FLAG_SEQUENCED, FLAG_STARRED,
};
use crate::eta::simulate::{run_explanation, run_marks};
use crate::eta::tracker::{EstimateContext, PrView, Tracker};
use crate::eta::twin_otter::{
    evaluate, seed_for_visit, EvalConfig, TwinOtterInput, TwinOtterModel,
};
use crate::eta::{
    AgeSource, CurrentStage, CurrentState, EstimateInput, Heuristic, Kind, NoEstimateReason,
    Registry, Stage, StageSamples, EXPLANATION_SCHEMA,
};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::Arc;

/// The tracker's default refresh cadence, in seconds.
const REFRESH_SEC: i64 = 300;

/// A cutoff before every fixture row (the earliest is 2026-09-27T03:00Z).
pub(crate) fn fit_as_of() -> DateTime<Utc> {
    "2026-09-26T00:00:00Z".parse().unwrap()
}

#[derive(Deserialize)]
struct Fixture {
    generator: Generator,
    fitted: Fitted,
    evaluation: FixtureEvaluation,
}

#[derive(Deserialize)]
struct Generator {
    features: Vec<String>,
}

#[derive(Deserialize)]
struct Fitted {
    hazard: BTreeMap<FitStage, HazardFit>,
    aft: AftFit,
}

#[derive(Deserialize)]
struct FixtureEvaluation {
    path_stats: PathStats,
    rows: Vec<Row>,
}

#[derive(Deserialize)]
struct Row {
    input: TwinOtterInput,
}

fn fixture() -> Fixture {
    serde_json::from_str(TWIN_OTTER_PARITY).expect("the parity fixture parses")
}

/// A coefficient file carrying the parity fixture's models, cut off at
/// `as_of`, with its content-derived id.
pub(crate) fn fixture_fit(as_of: DateTime<Utc>) -> CoefficientFile {
    let fx = fixture();
    let meta = FitMeta {
        as_of,
        window: FitWindow::standard(as_of),
        fitter: Fitter {
            version: "0.0.0".to_string(),
            revision: "0".repeat(40),
        },
    };
    let mut file = CoefficientFile::empty(&meta);
    file.features = fx.generator.features;
    file.hazard = fx.fitted.hazard;
    file.aft = Some(fx.fitted.aft);
    file.path_stats = fx.evaluation.path_stats;
    file.with_derived_id()
}

/// The daemon stage of a fixture stage.
fn daemon_stage(stage: &str) -> Option<Stage> {
    match stage {
        "review_wait" => Some(Stage::ReviewWait),
        "doctor_wait" => Some(Stage::Doctor),
        "merge_wait" => Some(Stage::MergeWait),
        "merge_hold" => Some(Stage::MergeHold),
        _ => None,
    }
}

/// The fixture rows whose stage the daemon has, by index.
fn mapped_rows() -> Vec<(usize, TwinOtterInput)> {
    fixture()
        .evaluation
        .rows
        .into_iter()
        .map(|r| r.input)
        .enumerate()
        .filter(|(_, row)| daemon_stage(&row.stage).is_some())
        .collect()
}

/// Labels whose [`pr_flags`] are `row`'s six flags, plus its stage label.
fn labels_for(row: &TwinOtterInput) -> Vec<String> {
    let stage_label = match row.stage.as_str() {
        "review_wait" => "loom:review-requested",
        "doctor_wait" => "loom:changes-requested",
        _ => "loom:pr",
    };
    let mut labels = vec![stage_label.to_string()];
    for (flag, label) in [
        (row.op_hold, "loom:operator"),
        (row.sequenced, "loom:sequenced"),
        (row.starred, "loom:operator-priority"),
        (row.conflict, "loom:merge-conflict"),
        (row.ci_fail, "loom:ci-failure"),
        (row.blocked, "loom:blocked"),
    ] {
        if flag != 0 {
            labels.push(label.to_string());
        }
    }
    labels
}

/// When `row`'s stage visit began.
fn entered(row: &TwinOtterInput) -> DateTime<Utc> {
    row.as_of - Duration::seconds((row.age_h * 3600.0).round() as i64)
}

/// The tracker's input for `row`'s visit, observed at `at`: the same
/// features, labels and stage entry whatever `at` is.
pub(crate) fn input_for(row: &TwinOtterInput, at: DateTime<Utc>) -> EstimateInput {
    let stage = daemon_stage(&row.stage).expect("a daemon stage");
    let entered_at = entered(row);
    EstimateInput {
        subject: subject(),
        as_of: at,
        current: CurrentState::At(CurrentStage {
            stage,
            entered_at: Some(entered_at),
            age_sec: (at - entered_at).num_seconds(),
            age_source: AgeSource::LabelEvent,
            rework_rounds: row.rework,
            episode_entered_at: None,
        }),
        features: Features {
            labels: Some(labels_for(row)),
            doctor_cycles_so_far: Some(row.rework),
            ahead: row.ahead,
            n_stage_repo: row.n_stage_repo,
            n_stage_fleet: row.n_stage_fleet,
            exits_repo_6h: row.exits_repo_6h,
            exits_repo_24h: row.exits_repo_24h,
            exits_fleet_6h: row.exits_fleet_6h,
            merges_repo_24h: row.merges_repo_24h,
            merges_fleet_6h: row.merges_fleet_6h,
            since_merge_sec: row.since_merge_h.map(|h| (h * 3600.0).round() as i64),
            ..Features::default()
        },
        features_omitted: Vec::new(),
        provenance: provenance(),
        dispatch: None,
        stalls: Vec::new(),
        held: None,
    }
}

/// The `review_wait` fixture row (row 3) at its own `as_of`.
pub(crate) fn review_input() -> EstimateInput {
    let row = fixture().evaluation.rows.swap_remove(3).input;
    assert_eq!(row.stage, "review_wait");
    input_for(&row, row.as_of)
}

fn twin_otter(fit: Option<CoefficientFile>) -> LandTwinOtter {
    LandTwinOtter::new(fit.map(Arc::new))
}

fn reason(explanation: &Explanation) -> Option<NoEstimateReason> {
    explanation.no_estimate_reason
}

fn the_current(input: &EstimateInput) -> &CurrentStage {
    match &input.current {
        CurrentState::At(current) => current,
        CurrentState::Refused(_) => panic!("an input at a stage"),
    }
}

fn hours_to_sec(q: [f64; 4]) -> (i64, i64, i64, i64) {
    let [a, b, c, d] = q.map(|h| (h * 3600.0).round() as i64);
    (a, b, c, d)
}

// ---------------------------------------------------------- registration

#[test]
fn twin_otter_is_registered_last_as_a_land_shadow_with_or_without_a_fit() {
    let fitted = Registry::with_fit(Some(Arc::new(fixture_fit(fit_as_of()))));
    for registry in [Registry::builtin(), fitted] {
        let ids = registry.ids();
        assert_eq!(ids[ids.len() - 2], LAND_TWIN_OTTER, "-b (#10244) follows it");
        assert_eq!(registry.get(LAND_TWIN_OTTER).map(Heuristic::kind), Some(Kind::Land));
        let land: Vec<&str> = registry.for_kind(Kind::Land).map(Heuristic::id).collect();
        assert_eq!(land[land.len() - 2], LAND_TWIN_OTTER);
        // Shadow: never the default `current`.
        assert_eq!(registry.current(Kind::Land, None).id(), "land-v1");
        assert_eq!(Registry::default_current(Kind::Land), "land-v1");
    }
    assert_eq!(Registry::builtin().fit_id(), None, "builtin() reads no file");
    let fit = fixture_fit(fit_as_of());
    let id = fit.id.clone();
    assert_eq!(Registry::with_fit(Some(Arc::new(fit))).fit_id(), Some(id.as_str()));
}

// ---------------------------------------------------------- refusals

#[test]
fn without_a_usable_fit_every_estimate_refuses_no_model() {
    let input = review_input();
    let history = StageSamples::default();
    // No file at all: `builtin()`.
    let builtin = Registry::builtin();
    let none = builtin
        .get(LAND_TWIN_OTTER)
        .unwrap()
        .estimate(&input, &history);
    // A file with no direct model.
    let mut no_aft = fixture_fit(fit_as_of());
    no_aft.aft = None;
    // A file cut off at, or after, the estimate's own instant: the leak guard.
    let at_as_of = fixture_fit(input.as_of);
    let after = fixture_fit(input.as_of + Duration::days(1));
    // Malformed coefficients.
    let mut malformed = fixture_fit(fit_as_of());
    malformed.hazard.get_mut(&FitStage::ReviewWait).unwrap().sd[0] = 0.0;
    for (what, explanation) in [
        ("no file", none),
        ("no direct model", twin_otter(Some(no_aft)).estimate(&input, &history)),
        ("cutoff == as_of", twin_otter(Some(at_as_of)).estimate(&input, &history)),
        ("cutoff > as_of", twin_otter(Some(after)).estimate(&input, &history)),
        ("malformed", twin_otter(Some(malformed)).estimate(&input, &history)),
    ] {
        assert_eq!(reason(&explanation), Some(NoEstimateReason::NoModel), "{what}");
        assert!(explanation.result.is_none(), "{what}");
        assert!(explanation.combination.is_none(), "{what}");
        assert!(explanation.twin_otter.is_none(), "{what}");
        assert_eq!(explanation.heuristic, LAND_TWIN_OTTER);
        assert_eq!(run_explanation(&explanation), None, "{what}");
    }
    assert_eq!(NoEstimateReason::NoModel.as_str(), "no_model");
    assert_eq!(serde_json::to_value(NoEstimateReason::NoModel).unwrap(), "no_model");
}

#[test]
fn the_refusal_map_is_closed() {
    let history = StageSamples::default();
    let fitted = twin_otter(Some(fixture_fit(fit_as_of())));
    let base = review_input();
    // A pre-PR stage: the model is PR-level.
    for stage in [Stage::ReadyWait, Stage::SweepCurator, Stage::SweepBuilder] {
        let mut input = base.clone();
        if let CurrentState::At(current) = &mut input.current {
            current.stage = stage;
        }
        let e = fitted.estimate(&input, &history);
        assert_eq!(reason(&e), Some(NoEstimateReason::UnknownStage), "{stage}");
    }
    // A resolver refusal passes through untouched.
    let mut blocked = base.clone();
    blocked.current = CurrentState::Refused(NoEstimateReason::Blocked);
    let e = fitted.estimate(&blocked, &history);
    assert_eq!(reason(&e), Some(NoEstimateReason::Blocked));
    assert!(e.current_stage.is_none());
    // A stage the fit skipped is a sample shortfall, not a missing model.
    let mut skipped = fixture_fit(fit_as_of());
    skipped.hazard.remove(&FitStage::ReviewWait);
    skipped.hazard_skipped.insert(
        FitStage::ReviewWait,
        HazardSkip {
            reason: SkipReason::BelowMinExits,
            rows: 300,
            exits: 4,
        },
    );
    let e = twin_otter(Some(skipped)).estimate(&base, &history);
    assert_eq!(reason(&e), Some(NoEstimateReason::InsufficientSamples));
    // A hand-built out-of-domain input (a negative `since_merge`) is refused,
    // never guessed.
    let mut odd = base.clone();
    odd.features.since_merge_sec = Some(-60);
    let e = fitted.estimate(&odd, &history);
    assert_eq!(reason(&e), Some(NoEstimateReason::UnknownStage));
    // Nothing above is `beyond_history`, and a very old item still answers.
    let mut old = base;
    if let CurrentState::At(current) = &mut old.current {
        current.entered_at = Some(old.as_of - Duration::days(30));
        current.age_sec = 30 * 86_400;
    }
    assert!(fitted.estimate(&old, &history).result.is_some());
}

// ---------------------------------------------------------- the adapter

#[test]
fn pr_flags_is_one_bit_per_flag() {
    let mask = |labels: &[&str]| {
        pr_flags(
            &labels
                .iter()
                .map(|l| (*l).to_string())
                .collect::<Vec<String>>(),
        )
    };
    assert_eq!(
        [
            FLAG_OP_HOLD,
            FLAG_SEQUENCED,
            FLAG_STARRED,
            FLAG_CONFLICT,
            FLAG_CI_FAIL,
            FLAG_BLOCKED
        ],
        [1, 2, 4, 8, 16, 32]
    );
    assert_eq!(mask(&[]), 0);
    assert_eq!(mask(&["loom:pr", "loom:review-requested", "loom:building"]), 0);
    // `loom:operator-mechanical` only ever accompanies a hold label.
    assert_eq!(mask(&["loom:operator-mechanical"]), 0);
    for (label, bit) in [
        ("loom:operator", FLAG_OP_HOLD),
        ("loom:operator-only", FLAG_OP_HOLD),
        ("loom:operator-decision", FLAG_OP_HOLD),
        ("loom:sequenced", FLAG_SEQUENCED),
        ("loom:operator-priority", FLAG_STARRED),
        ("loom:merge-conflict", FLAG_CONFLICT),
        ("loom:ci-failure", FLAG_CI_FAIL),
        ("loom:blocked", FLAG_BLOCKED),
    ] {
        assert_eq!(mask(&["loom:pr", label]), bit, "{label}");
    }
    assert_eq!(
        mask(&[
            "loom:operator-only",
            "loom:operator",
            "loom:sequenced",
            "loom:operator-priority",
            "loom:merge-conflict",
            "loom:ci-failure",
            "loom:blocked",
        ]),
        0b11_1111
    );
}

#[test]
fn each_fixture_row_adapts_to_its_own_input() {
    let rows = mapped_rows();
    assert_eq!(
        rows.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 4],
        "merge_hold ×2 (#10218), merge_wait, review_wait, doctor_wait"
    );
    for (n, row) in rows {
        let input = input_for(&row, row.as_of);
        let current = the_current(&input);
        let stage = FitStage::from_stage(current.stage).unwrap();
        assert_eq!(adapt_input(&input, current, stage), row, "row {n}");
    }
}

#[test]
fn the_adapter_reads_the_episode_age_and_falls_back_cleanly() {
    let mut input = review_input();
    // No entry instant: the age is the resolver's own `age_sec`.
    if let CurrentState::At(current) = &mut input.current {
        current.entered_at = None;
        current.age_sec = 5400;
    }
    let current = the_current(&input).clone();
    assert_eq!(visit_entry(&current, input.as_of), input.as_of - Duration::seconds(5400));
    let adapted = adapt_input(&input, &current, FitStage::ReviewWait);
    assert_eq!(adapted.age_h, 1.5);
    // No labels, no counts and no rework feature: flags 0, counts imputed,
    // rework from the resolver (at least 1 in `doctor`).
    let bare = EstimateInput {
        features: Features::default(),
        ..input.clone()
    };
    let mut doctor = current;
    doctor.stage = Stage::Doctor;
    doctor.rework_rounds = 0;
    let adapted = adapt_input(&bare, &doctor, FitStage::DoctorWait);
    assert_eq!(adapted.stage, "doctor_wait");
    assert_eq!(adapted.rework, 1);
    assert_eq!(adapted.ahead, None);
    assert_eq!(adapted.since_merge_h, None);
    let flags = [
        adapted.op_hold,
        adapted.sequenced,
        adapted.starred,
        adapted.conflict,
        adapted.ci_fail,
        adapted.blocked,
    ];
    assert_eq!(flags, [0; 6]);
    let e = twin_otter(Some(fixture_fit(fit_as_of()))).estimate(
        &EstimateInput {
            current: CurrentState::At(doctor),
            ..bare
        },
        &StageSamples::default(),
    );
    let record = e.twin_otter.expect("an imputed input still answers");
    assert_eq!(record.imputed.len(), 9, "{:?}", record.imputed);
}

// ---------------------------------------------------------- the explanation

#[test]
fn an_answer_explains_itself_and_recomputes_after_a_json_round_trip() {
    let fit = fixture_fit(fit_as_of());
    let heuristic = twin_otter(Some(fit.clone()));
    for (n, row) in mapped_rows() {
        let input = input_for(&row, row.as_of);
        let e = heuristic.estimate(&input, &StageSamples::default());
        assert_eq!(e.schema, EXPLANATION_SCHEMA, "the schema stays v1");
        assert_eq!(e.heuristic, LAND_TWIN_OTTER);
        assert_eq!(e.kind, Kind::Land);
        assert_eq!(reason(&e), None, "row {n}");
        assert!(e.truncated.is_empty() && e.size_bytes() <= MAX_BYTES, "row {n}");
        assert!(e.path.is_none() && e.branches.is_none() && e.contributions.is_none());
        assert!(e.history.is_none() && e.history_window.is_none() && e.stages.is_empty());
        assert!(e.features.is_some() && e.current_stage.is_some());

        let combination = e.combination.as_ref().unwrap();
        assert_eq!(combination.method, METHOD);
        assert_eq!(combination.draws, 256);
        assert_eq!(combination.rng, "splitmix64");
        assert_eq!(combination.draw_order, DRAW_ORDER);
        assert!(!combination.independence_assumed);

        // `result` is the blend of Slice A's own evaluation.
        let current = the_current(&input);
        let stage = FitStage::from_stage(current.stage).unwrap();
        let seed = visit_seed(&input, current, stage);
        assert_eq!(combination.seed, format!("0x{seed:016x}"));
        let model = TwinOtterModel::of(&fit).unwrap();
        let config = EvalConfig {
            seed,
            ..EvalConfig::default()
        };
        let evaluation = evaluate(&model, &row, &config).unwrap();
        let (p25, p50, p75, p90) = hours_to_sec(evaluation.blend_q);
        assert_eq!(e.quantiles_with_p90(), Some((p25, p50, p75, p90)), "row {n}");
        let result = e.result.as_ref().unwrap();
        assert_eq!(result.eta_p50_at, input.as_of + Duration::seconds(p50));
        assert!(result.stage_marks.is_empty());
        assert_eq!(result.samples_min, fit.hazard[&stage].rows);

        let record = e.twin_otter.as_ref().unwrap();
        assert_eq!(record.fit_id, fit.id);
        assert_eq!(record.fit_as_of, fit.as_of);
        assert_eq!(record.input, row);
        assert!(record.imputed.is_empty());
        let config = EvalConfig::default();
        assert_eq!(
            (record.step_h, record.steps, record.cap_h, record.paths, record.age_clamp_h),
            (config.step_h, config.steps, config.cap_h, config.paths, None)
        );
        let (a, b, c, d) = hours_to_sec(evaluation.hazard_path_q);
        assert_eq!(record.hazard_path_sec, [a, b, c, d]);
        let (a, b, c, d) = hours_to_sec(evaluation.aft_q);
        assert_eq!(record.aft_sec, [a, b, c, d]);
        let slice = record.model.as_ref().unwrap();
        assert_eq!(slice.hazard.keys().copied().collect::<Vec<_>>(), vec![stage]);
        assert_eq!(slice.path_stats, fit.path_stats);

        // Recomputed from the JSON alone, through the public entry point.
        let json = serde_json::to_string(&e).unwrap();
        let parsed: Explanation = serde_json::from_str(&json).unwrap();
        assert_eq!(run_explanation(&parsed), e.quantiles_with_p90(), "row {n}");
        assert_eq!(run_marks(&parsed), None, "twin-otter has no stage marks");
    }
}

#[test]
fn an_explanation_recorded_before_twin_otter_still_parses_and_keeps_its_shape() {
    let golden: serde_json::Value = serde_json::from_str(EXPLANATION_GOLDEN).unwrap();
    assert!(golden.get("twin_otter").is_none());
    let parsed: Explanation = serde_json::from_value(golden.clone()).unwrap();
    assert!(parsed.twin_otter.is_none());
    assert_eq!(serde_json::to_value(&parsed).unwrap(), golden);
    // Every other heuristic's output carries no `twin_otter` key at all.
    let e = crate::eta::heuristics::LandV1.estimate(&review_input(), &history_a());
    assert!(serde_json::to_value(&e)
        .unwrap()
        .get("twin_otter")
        .is_none());
}

#[test]
fn the_cap_drops_the_model_slice_after_features_and_only_when_present() {
    let mut e = twin_otter(Some(fixture_fit(fit_as_of())))
        .estimate(&review_input(), &StageSamples::default());
    let answer = e.quantiles_with_p90();
    // Inflate the model slice past the cap: four 64-point curves are 13 KB,
    // so this is far beyond anything a real fit writes.
    let curve = e
        .twin_otter
        .as_mut()
        .unwrap()
        .model
        .as_mut()
        .unwrap()
        .path_stats
        .km
        .get_mut(&FitStage::ReviewWait)
        .unwrap();
    curve.t.extend((0..3000).map(f64::from));
    curve.s.resize(curve.s.len() + 3000, 0.0);
    assert!(e.size_bytes() > MAX_BYTES);
    e.enforce_cap();
    assert!(e.size_bytes() <= MAX_BYTES, "{} bytes", e.size_bytes());
    assert_eq!(
        e.truncated,
        vec![
            TRUNCATED_FEATURES.to_string(),
            TRUNCATED_TWIN_OTTER_MODEL.to_string()
        ]
    );
    let record = e.twin_otter.as_ref().unwrap();
    assert!(record.model.is_none());
    assert_eq!(record.fit_id.len(), 16, "the identity survives");
    assert_eq!(e.quantiles_with_p90(), answer, "the numbers survive");
    assert_eq!(run_explanation(&e), None, "without the slice nothing recomputes");

    // An explanation with no slice never names one.
    let mut v1 = crate::eta::heuristics::LandV1.estimate(&review_input(), &history_a());
    v1.features.as_mut().unwrap().labels =
        Some((0..2000).map(|i| format!("label-{i:>20}")).collect());
    v1.enforce_cap();
    assert!(!v1
        .truncated
        .contains(&TRUNCATED_TWIN_OTTER_MODEL.to_string()));
}

// ---------------------------------------------------------- the seed

/// The Judge's pin on #10260: `seed_for_visit` runs through
/// `telemetry::trace::derived_hex` (SHA-256) and `trace::instant` (RFC 3339,
/// nanoseconds). A change to either would silently re-seed every twin-otter
/// estimate under the same immutable id and break recomputation of older
/// explanations, so the value for one fixed visit is pinned here, computed
/// independently as `sha256("loom.eta.twin-otter.seed\0github:1073994527#9289\0
/// review_wait\02026-10-01T08:30:00.000000000Z\0")[..16]`.
#[test]
fn the_visit_seed_is_pinned() {
    const GOLDEN: u64 = 0x3d2a_6c20_818e_4fd1;
    let entered: DateTime<Utc> = "2026-10-01T08:30:00Z".parse().unwrap();
    assert_eq!(seed_for_visit("github:1073994527#9289", "review_wait", entered), GOLDEN);
    let input = review_input();
    assert_eq!(visit_seed(&input, the_current(&input), FitStage::ReviewWait), GOLDEN);
    let e = twin_otter(Some(fixture_fit(fit_as_of()))).estimate(&input, &StageSamples::default());
    assert_eq!(e.combination.unwrap().seed, "0x3d2a6c20818e4fd1");
}

/// The orchestrator's stability requirement on #10243: a refresh with
/// unchanged inputs (only `as_of` and the age advance, by the tracker's
/// 300 s cadence) keeps the seed and moves p50 by under 5%. #10260 measured
/// 0.24–2.30% on these rows, so a failure here is broken wiring, not noise.
#[test]
fn an_unchanged_input_refresh_moves_the_twin_otter_p50_by_under_5_percent() {
    let fit = fixture_fit(fit_as_of());
    let heuristic = twin_otter(Some(fit.clone()));
    let model = TwinOtterModel::of(&fit).unwrap();
    let history = StageSamples::default();
    for (n, row) in mapped_rows() {
        let first = heuristic.estimate(&input_for(&row, row.as_of), &history);
        let refresh = row.as_of + Duration::seconds(REFRESH_SEC);
        let second = heuristic.estimate(&input_for(&row, refresh), &history);
        let seed = |e: &Explanation| e.combination.as_ref().unwrap().seed.clone();
        assert_eq!(seed(&first), seed(&second), "row {n}: one visit, one seed");
        assert_eq!(
            second.twin_otter.as_ref().unwrap().input.age_h,
            row.age_h + REFRESH_SEC as f64 / 3600.0,
            "row {n}: the age advanced with the clock"
        );
        let p50 = |e: &Explanation| e.result.as_ref().unwrap().p50_sec as f64;
        let moved = (p50(&second) - p50(&first)).abs() / p50(&first);
        assert!(moved < 0.05, "row {n}: p50 moved {:.2}% on a refresh", moved * 100.0);
        // The heuristic is Slice A's `evaluate` under the visit seed.
        let visit = seed_for_visit("github:1073994527#9289", &row.stage, entered(&row));
        let config = EvalConfig {
            seed: visit,
            ..EvalConfig::default()
        };
        let want = evaluate(&model, &row, &config).unwrap().blend_q[1];
        assert_eq!(p50(&first), (want * 3600.0).round(), "row {n}");
        // A new visit (a different entry) is a different seed.
        let mut revisit = input_for(&row, row.as_of);
        if let CurrentState::At(current) = &mut revisit.current {
            current.entered_at = Some(entered(&row) + Duration::seconds(1));
        }
        assert_ne!(seed(&heuristic.estimate(&revisit, &history)), seed(&first), "row {n}");
    }
}

// ---------------------------------------------------------- merge_hold

/// The #10243 / #10246 contract: `merge_hold` (#10218) is the fit's own
/// stage in the no-wildcard `FitStage::from_stage`, so twin-otter estimates a
/// held PR where every path-engine heuristic refuses it `blocked`. Without a
/// usable model it refuses as for any stage, never `blocked`.
#[test]
fn a_held_pr_is_estimated_from_the_fits_merge_hold_stage() {
    assert_eq!(FitStage::from_stage(Stage::MergeHold), Some(FitStage::MergeHold));
    let history = StageSamples::default();
    let fit = fixture_fit(fit_as_of());
    let row = fixture().evaluation.rows.swap_remove(0).input;
    assert_eq!(row.stage, "merge_hold");
    let held = input_for(&row, row.as_of);
    let e = twin_otter(Some(fit.clone())).estimate(&held, &history);
    assert_eq!(reason(&e), None);
    assert!(e.result.is_some());
    assert_eq!(e.current_stage.as_ref().map(|c| c.stage), Some(Stage::MergeHold));
    let record = e.twin_otter.as_ref().unwrap();
    assert_eq!(record.input, row);
    assert_eq!(record.input.op_hold, 1);
    assert_eq!(run_explanation(&e), e.quantiles_with_p90(), "recomputes from merge_hold");

    let none = twin_otter(None).estimate(&held, &history);
    assert_eq!(reason(&none), Some(NoEstimateReason::NoModel));
    let mut skipped = fit;
    skipped.hazard.remove(&FitStage::MergeHold);
    skipped.hazard_skipped.insert(
        FitStage::MergeHold,
        HazardSkip {
            reason: SkipReason::BelowMinExits,
            rows: 300,
            exits: 4,
        },
    );
    let e = twin_otter(Some(skipped)).estimate(&held, &history);
    assert_eq!(reason(&e), Some(NoEstimateReason::InsufficientSamples));
}

/// After a hold is lifted the PR is back in the pooled `merge_wait` (entered
/// at the approval), but twin-otter's age and seed run from the release
/// (`episode_entered_at`): the episode #10245 trains on.
#[test]
fn after_a_hold_the_age_and_seed_run_from_the_release() {
    let row = fixture().evaluation.rows.swap_remove(2).input;
    assert_eq!(row.stage, "merge_wait");
    let release = entered(&row);
    let approval = release - Duration::hours(5);
    let mut input = input_for(&row, row.as_of);
    if let CurrentState::At(current) = &mut input.current {
        current.entered_at = Some(approval);
        current.age_sec = (row.as_of - approval).num_seconds();
        current.episode_entered_at = Some(release);
    }
    let current = the_current(&input).clone();
    assert_eq!(visit_entry(&current, input.as_of), release);
    assert_eq!(adapt_input(&input, &current, FitStage::MergeWait), row);
    assert_eq!(
        visit_seed(&input, &current, FitStage::MergeWait),
        seed_for_visit("github:1073994527#9289", "merge_wait", release)
    );
}

// ---------------------------------------------------------- the tracker

/// One tracker pass at `as_of() + 60 s` over PR #501 carrying `labels`.
fn tracker_pass(registry: &Registry, labels: &[&str]) -> Vec<crate::eta::tracker::Emission> {
    const REPO: &str = "rjwalters/loom";
    let history = history_a();
    let repo_ids: BTreeMap<String, u64> = [(REPO.to_string(), 1_073_994_527_u64)]
        .into_iter()
        .collect();
    let ctx = EstimateContext {
        registry,
        current_start: None,
        current_finish: None,
        current_land: None,
        history: &history,
        refresh_secs: 300,
        host_id: Some("host-test"),
        repo_ids: &repo_ids,
        stalls: &super::NO_STALLS,
    };
    let mut tracker = Tracker::new(provenance());
    let pr = PrView {
        number: 501,
        issue: 50,
        labels: labels.iter().map(|l| (*l).to_string()).collect(),
        created_at: Some(as_of() - Duration::hours(2)),
        updated_at: Some(as_of() - Duration::hours(1)),
    };
    tracker.on_listing(REPO, &[pr], as_of(), 300);
    tracker.estimate(None, &ctx, as_of() + Duration::seconds(60))
}

/// `(primary, explanation)` of the `land` emission by `id`.
fn land_emission(emissions: &[crate::eta::tracker::Emission], id: &str) -> (bool, Explanation) {
    emissions
        .iter()
        .find(|e| e.explanation.kind == Kind::Land && e.explanation.heuristic == id)
        .map(|e| (e.primary, e.explanation.clone()))
        .unwrap_or_else(|| panic!("{id} emitted"))
}

/// End to end: a held approved PR (`loom:pr` + `loom:operator`) gets a
/// twin-otter shadow answer from `merge_hold`, while the primary `land-v1`
/// still refuses it `blocked`.
#[test]
fn a_tracker_pass_estimates_a_held_pr_with_twin_otter_only() {
    let fitted = Registry::with_fit(Some(Arc::new(fixture_fit(as_of() - Duration::days(1)))));
    let emissions = tracker_pass(&fitted, &["loom:pr", "loom:operator"]);
    let (primary, twin) = land_emission(&emissions, LAND_TWIN_OTTER);
    assert!(!primary);
    assert!(twin.result.is_some(), "{:?}", twin.no_estimate_reason);
    assert_eq!(twin.current_stage.as_ref().map(|c| c.stage), Some(Stage::MergeHold));
    let input = &twin.twin_otter.as_ref().unwrap().input;
    assert_eq!((input.stage.as_str(), input.op_hold), ("merge_hold", 1));
    let (primary, v1) = land_emission(&emissions, "land-v1");
    assert!(primary);
    assert_eq!(v1.no_estimate_reason, Some(NoEstimateReason::Blocked));
}

#[test]
fn a_tracker_pass_with_a_fit_adds_a_twin_otter_shadow_and_leaves_the_primary_alone() {
    let labels = ["loom:review-requested", "loom:sequenced"];
    let fitted = Registry::with_fit(Some(Arc::new(fixture_fit(as_of() - Duration::days(1)))));
    let with_fit = tracker_pass(&fitted, &labels);
    let without = tracker_pass(&Registry::builtin(), &labels);
    let land = land_emission;

    let (primary, twin) = land(&with_fit, LAND_TWIN_OTTER);
    assert!(!primary, "a shadow, never the primary");
    assert!(twin.result.is_some(), "{:?}", twin.no_estimate_reason);
    assert_eq!(twin.twin_otter.as_ref().unwrap().input.sequenced, 1);
    let (_, refused) = land(&without, LAND_TWIN_OTTER);
    assert_eq!(refused.no_estimate_reason, Some(NoEstimateReason::NoModel));

    // The primary `land-v1` explanation is byte-identical either way.
    let (is_primary, v1_with) = land(&with_fit, "land-v1");
    let (_, v1_without) = land(&without, "land-v1");
    assert!(is_primary);
    assert_eq!(
        serde_json::to_string(&v1_with).unwrap(),
        serde_json::to_string(&v1_without).unwrap()
    );
}

// ---------------------------------------------------------- twin-otter-b (#10244)

const PRE_PR: [Stage; 3] = [Stage::ReadyWait, Stage::SweepCurator, Stage::SweepBuilder];

fn at_stage(stage: Stage) -> EstimateInput {
    let mut input = review_input();
    if let CurrentState::At(current) = &mut input.current {
        current.stage = stage;
    }
    // `ready_wait` needs a dispatch plan, for land-v2 and -b alike.
    input.dispatch = Some(crate::eta::DispatchInput {
        position: 1,
        plan_state: "next".to_string(),
        gate: None,
        ahead: 0,
        free_slots: 1,
        max_admissions_per_tick: None,
        tick_interval_secs: 60,
        saturation_held: false,
        plan_at: input.as_of,
    });
    input
}

#[test]
fn twin_otter_b_is_registered_after_twin_otter_as_a_land_shadow() {
    let fitted = Registry::with_fit(Some(Arc::new(fixture_fit(fit_as_of()))));
    for registry in [Registry::builtin(), fitted] {
        assert_eq!(registry.ids().last(), Some(&LAND_TWIN_OTTER_B));
        let land: Vec<&str> = registry.for_kind(Kind::Land).map(Heuristic::id).collect();
        assert_eq!(land[land.len() - 2..], [LAND_TWIN_OTTER, LAND_TWIN_OTTER_B]);
        assert_eq!(registry.current(Kind::Land, None).id(), "land-v1");
    }
}

#[test]
fn twin_otter_b_answers_pre_pr_stages_with_a_fit_loaded() {
    let history = super::ready::history_ready();
    let b = LandTwinOtterB::new(Some(Arc::new(fixture_fit(fit_as_of()))));
    let mut ready = super::ready::ready_input(Some(super::ready::dispatch()));
    ready.features = review_input().features;
    for (stage, input) in [
        (Stage::ReadyWait, ready),
        (Stage::SweepCurator, super::input_at(Stage::SweepCurator, 0, 0)),
        (Stage::SweepBuilder, super::input_at(Stage::SweepBuilder, 0, 0)),
    ] {
        let e = b.estimate(&input, &history);
        assert_eq!(e.heuristic, LAND_TWIN_OTTER_B);
        assert_eq!(reason(&e), None, "{stage}: {:?}", e.no_estimate_reason);
        assert!(e.result.is_some(), "{stage}");
        // The explanation names land-v2's path as the source, and recomputes.
        assert_eq!(e.combination.as_ref().unwrap().method, PRE_PR_METHOD);
        let r = e.result.as_ref().unwrap();
        let (p25, p50, p75, _) = run_explanation(&e).expect("recomputes");
        assert_eq!((p25, p50, p75), (r.p25_sec, r.p50_sec, r.p75_sec), "{stage}");
    }
}

#[test]
fn twin_otter_b_answered_ness_on_pre_pr_items_equals_land_v2s() {
    let fit = Some(Arc::new(fixture_fit(fit_as_of())));
    let b = LandTwinOtterB::new(fit.clone());
    let empty = StageSamples::default();
    let full = history_a();
    for (name, history) in [("empty", &empty), ("history_a", &full)] {
        for stage in PRE_PR {
            let input = at_stage(stage);
            let v2 = LandV2.estimate(&input, history);
            let got = b.estimate(&input, history);
            assert_eq!(got.result.is_some(), v2.result.is_some(), "{name} {stage}");
            assert_eq!(reason(&got), reason(&v2), "{name} {stage}");
        }
    }
    // A ready_wait item with no dispatch plan is refused identically.
    let mut no_plan = at_stage(Stage::ReadyWait);
    no_plan.dispatch = None;
    assert_eq!(reason(&b.estimate(&no_plan, &full)), reason(&LandV2.estimate(&no_plan, &full)));
    assert_eq!(reason(&b.estimate(&no_plan, &full)), Some(NoEstimateReason::NoDispatchPlan));
    // And with no fit at all: pre-PR still answers like land-v2.
    let nofit = LandTwinOtterB::new(None);
    for stage in PRE_PR {
        let input = at_stage(stage);
        assert_eq!(
            nofit.estimate(&input, &full).result.is_some(),
            LandV2.estimate(&input, &full).result.is_some()
        );
    }
}

#[test]
fn twin_otter_b_leaves_pr_level_stages_and_refusals_to_twin_otter() {
    let history = history_a();
    let fit = fixture_fit(fit_as_of());
    let original = twin_otter(Some(fit.clone()));
    let b = LandTwinOtterB::new(Some(Arc::new(fit)));
    for stage in [
        Stage::ReviewWait,
        Stage::Doctor,
        Stage::MergeWait,
        Stage::MergeHold,
    ] {
        let input = at_stage(stage);
        let a = original.estimate(&input, &history);
        let c = b.estimate(&input, &history);
        assert_eq!(c.heuristic, LAND_TWIN_OTTER_B);
        assert_eq!(c.result, a.result, "{stage}");
        assert_eq!(c.twin_otter, a.twin_otter, "{stage}");
        assert_eq!(c.no_estimate_reason, a.no_estimate_reason, "{stage}");
    }
    let mut refused = review_input();
    refused.current = CurrentState::Refused(NoEstimateReason::Blocked);
    assert_eq!(reason(&b.estimate(&refused, &history)), Some(NoEstimateReason::Blocked));
}
