//! `land-2026-10-06-keen-wren` (#10508): the `eta-fit/v2` fit and its
//! schema-dispatched files, the v2 evaluation's train/serve transform
//! parity, the heuristic's composition and registration, and its
//! `ready_wait` following the real dispatch order.

use super::land_twin_otter::{fit_as_of, fixture_fit, review_input};
use super::ready::{history_ready, ready_input};
use super::{as_of, history_a};
use crate::eta::fit::features_v2::{
    model_features_v2, ModelInputsV2, PriorityInputs, FEATURES_V2, N_FEATURES_V2, SCHEMA_V2,
};
use crate::eta::fit::v2::{self, fit_dir_v2, fit_v2, load_latest_v2, read_v2};
use crate::eta::fit::{
    self, clock, CoefficientFile, FitMeta, FitStage, FitWindow, Fitter, MergeLabel, ModelInputs,
    TrainingRow, FEATURES, SCHEMA,
};
use crate::eta::heuristics::{
    LandKeenWren, LandTwinOtter, LandV2, KEEN_WREN_PRE_PR_METHOD, LAND_KEEN_WREN, LAND_TWIN_OTTER,
    LAND_TWIN_OTTER_B,
};
use crate::eta::simulate::run_explanation;
use crate::eta::twin_otter::{evaluate, EvalConfig, TwinOtterModel, PROBIT_TAUS};
use crate::eta::{
    CurrentState, DispatchInput, EstimateInput, Explanation, Heuristic, Kind, NoEstimateReason,
    Registry, Stage, StageSamples, Tier,
};
use crate::work_finder::ready_queue::candidate_keys;
use crate::work_finder::PriorityCandidate;
use crate::workspace_registry::DEFAULT_WORKSPACE_PRIORITY;
use chrono::{DateTime, Duration, TimeZone, Utc};
use std::sync::Arc;

/// `priority_level`'s position in [`FEATURES_V2`].
const LEVEL: usize = 21;

fn meta(as_of: DateTime<Utc>) -> FitMeta {
    FitMeta {
        as_of,
        window: FitWindow::standard(as_of),
        fitter: Fitter {
            version: "0.19.0".to_string(),
            revision: "0123456789abcdef0123456789abcdef01234567".to_string(),
        },
    }
}

/// `n` rows of `stage`, the first `exits` exiting, with priority inputs that
/// vary (some unknown), so every v2 column has variance.
fn rows(stage: FitStage, n: usize, exits: usize) -> (Vec<TrainingRow>, Vec<PriorityInputs>) {
    let rows = (0..n)
        .map(|i| TrainingRow {
            starred_any: None,
            star_source: None,
            stage,
            group: format!("g#{i}"),
            inputs: ModelInputs {
                age_h: (i % 37) as f64 * 0.7,
                ahead: (i % 5) as u32,
                hour_utc: (i % 24) as f64,
                op_hold: i % 3 == 0,
                ..ModelInputs::default()
            },
            exit: Some(i < exits),
            merge: MergeLabel {
                dur_h: 1.0 + (i % 11) as f64,
                merged: i % 4 != 0,
            },
        })
        .collect();
    let priority = (0..n)
        .map(|i| PriorityInputs {
            starred_any: (i % 7 != 0).then_some(i % 2 == 0),
            priority_level: (i % 7 != 0).then_some((i % 3) as u8),
            repo_rank: (i % 5 != 0).then_some((i % 4) as f64 / 3.0),
            ahead_dispatch_fleet: (i % 6 != 0).then_some((i % 9) as u32),
        })
        .collect();
    (rows, priority)
}

/// The #10223 fixture's v1 model, re-tagged `eta-fit/v2`: the 20 v1
/// coefficients at their positions (`starred` read as `starred_any`), the six
/// priority columns standardized as-is (mu 0, sd 1) with weight only on
/// `priority_level`: `hazard_level` on the exit hazard, `aft_level` on the
/// direct model. With both 0 it is twin-otter's model exactly.
pub(super) fn v2_fixture(
    as_of: DateTime<Utc>,
    hazard_level: f64,
    aft_level: f64,
) -> CoefficientFile {
    let mut file = fixture_fit(as_of);
    assert_eq!(file.features, FEATURES.map(String::from), "the fixture is v1-ordered");
    file.schema = SCHEMA_V2.to_string();
    file.features = FEATURES_V2.map(String::from).to_vec();
    let extra = |level: f64| {
        let mut v = vec![0.0; N_FEATURES_V2 - FEATURES.len()];
        v[LEVEL - FEATURES.len()] = level;
        v
    };
    for hazard in file.hazard.values_mut() {
        hazard.mu.extend(vec![0.0; 6]);
        hazard.sd.extend(vec![1.0; 6]);
        hazard.coef.extend(extra(hazard_level));
    }
    let aft = file.aft.as_mut().unwrap();
    aft.mu.extend(vec![0.0; 6]);
    aft.sd.extend(vec![1.0; 6]);
    aft.beta.extend(extra(aft_level));
    file.with_derived_id()
}

fn known(starred: bool, level: u8) -> PriorityInputs {
    PriorityInputs {
        starred_any: Some(starred),
        priority_level: Some(level),
        repo_rank: None,
        ahead_dispatch_fleet: None,
    }
}

fn at_stage(stage: Stage) -> EstimateInput {
    let mut input = review_input();
    if let CurrentState::At(current) = &mut input.current {
        current.stage = stage;
    }
    input.dispatch = Some(DispatchInput {
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

fn keen_wren(fit: Option<CoefficientFile>) -> LandKeenWren {
    LandKeenWren::new(fit.map(Arc::new))
}

fn reason(e: &Explanation) -> Option<NoEstimateReason> {
    e.no_estimate_reason
}

const PR_STAGES: [Stage; 4] = [
    Stage::ReviewWait,
    Stage::Doctor,
    Stage::MergeWait,
    Stage::MergeHold,
];

// ---- the v2 fit and its files ---------------------------------------------

#[test]
fn the_v2_fit_is_tagged_v2_and_positional_against_features_v2() {
    let (mut all, mut priority) = rows(FitStage::ReviewWait, 300, 60);
    let (merge, merge_p) = rows(FitStage::MergeWait, 300, 40);
    all.extend(merge);
    priority.extend(merge_p);
    let file = fit_v2(&meta(fit_as_of()), &all, &priority, &[]);
    assert_eq!(file.schema, SCHEMA_V2);
    assert_eq!(file.features, FEATURES_V2.map(String::from));
    assert_eq!(file.id, file.derive_id());
    for (stage, hazard) in &file.hazard {
        assert_eq!(hazard.coef.len(), N_FEATURES_V2, "{stage}");
        assert_eq!(hazard.mu.len(), N_FEATURES_V2, "{stage}");
        assert!(hazard.converged, "{stage}");
    }
    let aft = file.aft.as_ref().expect("the direct model fits");
    assert_eq!(aft.beta.len(), aft.stages.len() + N_FEATURES_V2);
    assert!(aft.converged);
    // The priority columns were actually fitted: not all-zero.
    let hazard = &file.hazard[&FitStage::ReviewWait];
    assert!(hazard.coef[20..].iter().any(|c| c.abs() > 1e-6), "{:?}", hazard.coef);

    // The v1 fit of the same rows is the v1 file it always was.
    let v1 = fit::fit(&meta(fit_as_of()), &all, &[]);
    assert_eq!(v1.schema, SCHEMA);
    assert_eq!(v1.features, FEATURES.map(String::from));
    assert_eq!(v1.hazard[&FitStage::ReviewWait].coef.len(), FEATURES.len());
    assert_eq!(v1.path_stats, file.path_stats, "the path statistics read no features");
    assert_eq!(v1.age_p95_sec, file.age_p95_sec);
}

#[test]
fn each_loader_reads_only_its_own_schema_and_directory() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let v1 = fixture_fit(fit_as_of());
    let v2 = v2_fixture(fit_as_of(), 0.0, 0.0);
    assert_ne!(v1.id, v2.id);
    let v1_path = fit::fit_dir(root).join(fit::path_for(v1.as_of));
    let v2_path = fit_dir_v2(root).join(fit::path_for(v2.as_of));
    assert!(fit_dir_v2(root).starts_with(fit::fit_dir(root)));
    fit::write(&v1_path, &v1).unwrap();
    fit::write(&v2_path, &v2).unwrap();
    let later = fit_as_of() + Duration::days(1);
    assert_eq!(fit::load_latest(root, later).map(|f| f.id), Some(v1.id.clone()));
    assert_eq!(load_latest_v2(root, later).map(|f| f.id), Some(v2.id.clone()));
    // Strictly before, for v2 as for v1.
    assert!(load_latest_v2(root, fit_as_of()).is_none());

    // A file of the other schema in either directory is refused, never
    // read against the wrong positions.
    assert!(fit::read(&v2_path).is_none());
    assert!(read_v2(&v1_path).is_none());
    let newer = fit_as_of() + Duration::hours(1);
    let mut stray_v2 = v2_fixture(newer, 0.0, 0.0);
    stray_v2.as_of = newer;
    fit::write(&fit::fit_dir(root).join(fit::path_for(newer)), &stray_v2).unwrap();
    let stray_v1 = fixture_fit(newer);
    fit::write(&fit_dir_v2(root).join(fit::path_for(newer)), &stray_v1).unwrap();
    assert_eq!(fit::load_latest(root, later).map(|f| f.id), Some(v1.id.clone()));
    assert_eq!(load_latest_v2(root, later).map(|f| f.id), Some(v2.id.clone()));

    // The registry gives each heuristic its own schema.
    let registry = Registry::load(root, later);
    assert_eq!(registry.fit_id(), Some(v1.id.as_str()));
    assert_eq!(registry.fit_v2_id(), Some(v2.id.as_str()));
}

// ---- train/serve parity of the v2 transform -------------------------------

/// The direct model's quantiles the evaluation core serves equal those
/// computed by hand from [`model_features_v2`] of the raw inputs the fit
/// would build: the same transform on both sides, with the priority inputs
/// (including unknowns) at their v2 positions.
#[test]
fn the_v2_evaluation_reads_the_fits_transform() {
    let file = v2_fixture(fit_as_of(), 0.4, -0.3);
    let model = TwinOtterModel::of(&file).unwrap();
    let review = review_input();
    let CurrentState::At(current) = &review.current else {
        unreachable!("the review fixture is at a stage")
    };
    let mut input = crate::eta::heuristics::adapt_input(&review, current, FitStage::ReviewWait);
    // Every count known, so no feature is imputed and each is the
    // transform's own value.
    input.ahead = Some(input.ahead.unwrap_or(2));
    input.n_stage_repo = Some(input.n_stage_repo.unwrap_or(3));
    input.exits_repo_6h = Some(input.exits_repo_6h.unwrap_or(1));
    input.exits_repo_24h = Some(input.exits_repo_24h.unwrap_or(4));
    input.exits_fleet_6h = Some(input.exits_fleet_6h.unwrap_or(5));
    input.merges_repo_24h = Some(input.merges_repo_24h.unwrap_or(2));
    input.merges_fleet_6h = Some(input.merges_fleet_6h.unwrap_or(6));
    input.since_merge_h = Some(input.since_merge_h.unwrap_or(1.5));
    input.n_stage_fleet = Some(input.n_stage_fleet.unwrap_or(7));
    for priority in [
        None,
        Some(known(false, 0)),
        Some(known(true, 1)),
        Some(known(true, 2)),
        Some(PriorityInputs {
            repo_rank: Some(0.5),
            ahead_dispatch_fleet: Some(3),
            ..known(true, 2)
        }),
    ] {
        input.priority = priority;
        let got = evaluate(&model, &input, &EvalConfig::default()).unwrap();
        let (hour_utc, weekend) = clock(input.as_of);
        let x = model_features_v2(&ModelInputsV2 {
            base: ModelInputs {
                age_h: input.age_h,
                ahead: input.ahead.unwrap_or(0),
                n_stage_repo: input.n_stage_repo.unwrap_or(0),
                exits_repo_6h: input.exits_repo_6h.unwrap_or(0),
                exits_repo_24h: input.exits_repo_24h.unwrap_or(0),
                exits_fleet_6h: input.exits_fleet_6h.unwrap_or(0),
                merges_repo_24h: input.merges_repo_24h.unwrap_or(0),
                merges_fleet_6h: input.merges_fleet_6h.unwrap_or(0),
                since_merge_h: input.since_merge_h.unwrap_or(0.0),
                n_stage_fleet: input.n_stage_fleet.unwrap_or(0),
                hour_utc,
                weekend,
                rework: input.rework,
                op_hold: input.op_hold != 0,
                sequenced: input.sequenced != 0,
                starred: input.starred != 0,
                conflict: input.conflict != 0,
                ci_fail: input.ci_fail != 0,
                blocked: input.blocked != 0,
            },
            priority: priority.unwrap_or_default(),
        });
        let aft = file.aft.as_ref().unwrap();
        let k = aft
            .stages
            .iter()
            .position(|s| *s == FitStage::ReviewWait)
            .unwrap();
        let linear = aft.beta[k]
            + (0..N_FEATURES_V2)
                .map(|j| aft.beta[aft.stages.len() + j] * (x[j] - aft.mu[j]) / aft.sd[j])
                .sum::<f64>();
        let sigma = aft.log_sigma[k].exp();
        let want = PROBIT_TAUS.map(|p| (linear + sigma * p).exp().min(EvalConfig::default().cap_h));
        assert!(got.imputed.is_empty(), "{:?}", got.imputed);
        for (g, w) in got.aft_q.iter().zip(want) {
            assert!((g - w).abs() < 1e-9 * w.max(1.0), "{priority:?}: {g} vs {w}");
        }
    }
}

#[test]
fn a_v2_model_with_no_priority_weight_reproduces_twin_otter() {
    let history = history_a();
    let original = LandTwinOtter::new(Some(Arc::new(fixture_fit(fit_as_of()))));
    let wren = keen_wren(Some(v2_fixture(fit_as_of(), 0.0, 0.0)));
    for stage in PR_STAGES {
        let mut input = at_stage(stage);
        // The fixture row's own star, as the builder would report it.
        let starred = input
            .features
            .labels
            .as_ref()
            .is_some_and(|l| l.iter().any(|x| x == "loom:operator-priority"));
        input.features.priority = Some(known(starred, u8::from(starred)));
        let a = original.estimate(&input, &history);
        let b = wren.estimate(&input, &history);
        assert_eq!(b.heuristic, LAND_KEEN_WREN);
        assert_eq!(b.no_estimate_reason, a.no_estimate_reason, "{stage}");
        assert_eq!(b.result.as_ref().map(|r| r.p50_sec), a.result.as_ref().map(|r| r.p50_sec));
        assert_eq!(b.result, a.result, "{stage}");
    }
}

// ---- the heuristic ---------------------------------------------------------

#[test]
fn keen_wren_is_a_land_candidate_registered_before_the_twin_otter_pair() {
    let fitted = Registry::with_fits(
        Some(Arc::new(fixture_fit(fit_as_of()))),
        Some(Arc::new(v2_fixture(fit_as_of(), 0.0, 0.0))),
    );
    for registry in [Registry::builtin(), fitted] {
        let land: Vec<&str> = registry.for_kind(Kind::Land).map(Heuristic::id).collect();
        assert_eq!(land[land.len() - 3..], [LAND_KEEN_WREN, LAND_TWIN_OTTER, LAND_TWIN_OTTER_B]);
        assert_eq!(registry.tier_of(LAND_KEEN_WREN), Some(Tier::Candidate));
        assert_eq!(registry.current(Kind::Land, None).id(), "land-v1", "shadow");
        assert!(registry.get(LAND_KEEN_WREN).unwrap().models_hold());
    }
}

#[test]
fn keen_wren_pr_stages_need_a_v2_fit_and_read_the_priority_inputs() {
    let history = history_a();
    let mut input = review_input();
    input.features.priority = Some(known(true, 2));
    // No file, or a v1 file handed in by mistake: `no_model`, never a v1
    // vector read against v2 positions.
    for fit in [None, Some(fixture_fit(fit_as_of()))] {
        let e = keen_wren(fit).estimate(&input, &history);
        assert_eq!(reason(&e), Some(NoEstimateReason::NoModel));
    }
    // A v2 file cut off at `as_of` is not yet usable.
    let at = keen_wren(Some(v2_fixture(input.as_of, 0.4, -0.3))).estimate(&input, &history);
    assert_eq!(reason(&at), Some(NoEstimateReason::NoModel));

    let file = v2_fixture(fit_as_of(), 0.4, -0.3);
    let wren = keen_wren(Some(file.clone()));
    let e = wren.estimate(&input, &history);
    assert_eq!(reason(&e), None);
    let record = e.twin_otter.as_ref().expect("a twin-otter record");
    assert_eq!(record.fit_id, file.id);
    assert_eq!(record.input.priority, Some(known(true, 2)), "the record carries what it read");
    // The answer recomputes from its own record (the v2 transform included),
    // after a JSON round trip.
    let back: Explanation = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
    let r = e.result.as_ref().unwrap();
    assert_eq!(
        run_explanation(&back),
        Some((r.p25_sec, r.p50_sec, r.p75_sec, r.p90_sec.unwrap()))
    );

    // The priority inputs move it: with this model's weights, the same
    // starred item at level 2 lands sooner than at level 1.
    let p50 = |priority: Option<PriorityInputs>| {
        let mut i = input.clone();
        i.features.priority = priority;
        wren.estimate(&i, &history).result.unwrap().p50_sec
    };
    assert!(p50(Some(known(true, 2))) < p50(Some(known(true, 1))));
    assert_eq!(p50(None), p50(Some(known(false, 0))), "unknown level reads as 0 here");

    // A resolver refusal is passed through.
    let mut refused = input.clone();
    refused.current = CurrentState::Refused(NoEstimateReason::Blocked);
    assert_eq!(reason(&wren.estimate(&refused, &history)), Some(NoEstimateReason::Blocked));
}

#[test]
fn keen_wren_pre_pr_answered_ness_equals_land_v2s() {
    let wren = keen_wren(Some(v2_fixture(fit_as_of(), 0.4, -0.3)));
    let nofit = keen_wren(None);
    let empty = StageSamples::default();
    let full = history_a();
    for (name, history) in [("empty", &empty), ("history_a", &full)] {
        for stage in [Stage::ReadyWait, Stage::SweepCurator, Stage::SweepBuilder] {
            let input = at_stage(stage);
            let v2 = LandV2.estimate(&input, history);
            for h in [&wren, &nofit] {
                let got = h.estimate(&input, history);
                assert_eq!(got.heuristic, LAND_KEEN_WREN);
                assert_eq!(got.result.is_some(), v2.result.is_some(), "{name} {stage}");
                assert_eq!(reason(&got), reason(&v2), "{name} {stage}");
                if let Some(c) = &got.combination {
                    assert_eq!(c.method, KEEN_WREN_PRE_PR_METHOD);
                }
            }
        }
    }
    let mut no_plan = at_stage(Stage::ReadyWait);
    no_plan.dispatch = None;
    assert_eq!(reason(&wren.estimate(&no_plan, &full)), Some(NoEstimateReason::NoDispatchPlan));
}

// ---- ready_wait follows the real dispatch order ---------------------------

fn at(hours: i64) -> String {
    (Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap() + Duration::hours(hours)).to_rfc3339()
}

/// Ready issues across three repos: `a` (fleet priority 10), `d` (no
/// `fleet_priority`: the comparator's default) and `b` (200), at levels
/// 0/1/2, with star times and an equal-age tie. Sorted by the work finder's
/// real comparator, the plan puts level before star time before repo
/// priority before age before number; keen-wren's start follows it.
#[test]
fn keen_wren_ready_wait_follows_the_real_dispatch_order() {
    let candidate =
        |number: u32, repo_priority: u32, level: u8, starred_at: Option<i64>, age: i64| {
            PriorityCandidate {
                workspace_priority: repo_priority,
                operator_level: level,
                operator_priority: level > 0,
                operator_priority_at: starred_at.map(at),
                created_at: Some(at(-age)),
                number,
                ..PriorityCandidate::default()
            }
        };
    let (a, d, b) = (10, DEFAULT_WORKSPACE_PRIORITY, 200);
    assert!(a < d && d < b);
    let mut plan = [
        candidate(5, b, 0, None, 90),    // oldest, but the lowest-priority repo
        candidate(7, a, 0, None, 50),    // ties 4 on age; higher number
        candidate(2, a, 1, Some(5), 10), // starred later than 3
        candidate(6, d, 0, None, 80),    // default repo priority: between a and b
        candidate(3, b, 1, Some(1), 10), // starred first: star time beats repo
        candidate(4, a, 0, None, 50),
        candidate(1, b, 2, Some(9), 1), // level 2 beats every star and repo
    ];
    plan.sort_by(|x, y| candidate_keys(x).cmp(&candidate_keys(y)));
    let order: Vec<u32> = plan.iter().map(|c| c.number).collect();
    assert_eq!(order, [1, 3, 2, 4, 7, 6, 5]);

    let wren = keen_wren(None);
    let history = history_ready();
    let p50s: Vec<i64> = (0..plan.len())
        .map(|i| {
            let ahead = u32::try_from(i).unwrap();
            let input = ready_input(Some(DispatchInput {
                position: ahead + 1,
                plan_state: "queued".to_string(),
                gate: Some("capacity".to_string()),
                ahead,
                free_slots: 0,
                max_admissions_per_tick: None,
                tick_interval_secs: 60,
                saturation_held: false,
                plan_at: as_of() - Duration::seconds(30),
            }));
            let e = wren.estimate(&input, &history);
            let path = e.path.as_ref().expect("a path");
            assert_eq!(path.dispatch.as_ref().unwrap().input.ahead, ahead);
            e.result.expect("answered").p50_sec
        })
        .collect();
    // Further back in the plan, more slot turnovers: the head of the plan
    // (the level-2 issue) starts, and lands, well before its tail.
    assert!(p50s[0] < p50s[plan.len() - 1], "{p50s:?}");
    assert!(p50s[0] < p50s[3] && p50s[3] < p50s[plan.len() - 1], "{p50s:?}");
}

// ---- the runner writes both files ------------------------------------------

#[test]
fn v2_features_are_one_vector_per_row() {
    let (rows, priority) = rows(FitStage::MergeWait, 10, 3);
    let xs = v2::features_v2(&rows, &priority);
    assert_eq!(xs.len(), rows.len());
    for ((row, p), x) in rows.iter().zip(&priority).zip(&xs) {
        let want = model_features_v2(&ModelInputsV2 {
            base: row.inputs,
            priority: *p,
        });
        assert_eq!(*x, want);
    }
}
