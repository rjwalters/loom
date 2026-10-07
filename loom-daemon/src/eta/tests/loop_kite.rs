//! `land-2026-10-06-loop-kite` (#10521): the `eta-fit/v3` fit and its
//! schema-dispatched files, the v3 evaluation's train/serve transform
//! parity (the friction block and the cumulative stage age included), the
//! heuristic's composition and registration, and the walk-forward replay
//! that pairs it with twin-otter-b.

use super::history_a;
use super::keen_wren::v2_fixture;
use super::land_twin_otter::{fit_as_of, fixture_fit, review_input};
use crate::eta::fit::features_v2::{
    ModelInputsV2, PriorityInputs, FEATURES_V2, N_FEATURES_V2, SCHEMA_V2,
};
use crate::eta::fit::features_v3::{
    model_features_v3, training_inputs_v3, ModelInputsV3, FEATURES_V3, N_FEATURES_V3, SCHEMA_V3,
};
use crate::eta::fit::rows::{Assembled, RowStats};
use crate::eta::fit::v2::{fit_dir_v2, load_latest_v2};
use crate::eta::fit::v3::{features_v3, fit_dir_v3, fit_v3, load_latest_v3, read_v3};
use crate::eta::fit::{
    self, clock, CoefficientFile, FitMeta, FitStage, FitWindow, Fitter, MergeLabel, ModelInputs,
    TrainingRow, FEATURES,
};
use crate::eta::heuristics::{
    LandKeenWren, LandLoopKite, LandV2, KEEN_WREN_PRE_PR_METHOD, LAND_BOLD_LARK, LAND_KEEN_WREN,
    LAND_LOOP_KITE, LAND_TANDEM_WREN, LAND_TWIN_OTTER_B,
};
use crate::eta::loop_features::{LoopCoverage, LoopFeatures, LOOP_FEATURES};
use crate::eta::simulate::run_explanation;
use crate::eta::twin_otter::{evaluate, EvalConfig, TwinOtterModel, PROBIT_TAUS};
use crate::eta::walk_forward::DatedFits;
use crate::eta::{
    CurrentState, DispatchInput, EstimateInput, Explanation, Heuristic, Kind, NoEstimateReason,
    Registry, Stage, StageSamples, Tier,
};
use chrono::{DateTime, Duration, Utc};
use std::sync::Arc;

/// `log_cum_stage`'s position in [`FEATURES_V3`].
const CUM: usize = N_FEATURES_V2;
/// `log_approvals_lost`'s position in [`FEATURES_V3`].
const LOST: usize = N_FEATURES_V2 + 2;

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

/// An assembled fit of `n` rows per stage, the first `exits` exiting, with
/// priority and friction inputs that vary (some unknown), so every v3 column
/// has variance.
fn assembled(n: usize, exits: usize) -> Assembled {
    let mut rows = Vec::new();
    for stage in [FitStage::ReviewWait, FitStage::MergeWait] {
        rows.extend((0..n).map(|i| TrainingRow {
            starred_any: None,
            star_source: None,
            stage,
            group: format!("{stage}#{i}"),
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
        }));
    }
    let priority_inputs = (0..rows.len())
        .map(|i| PriorityInputs {
            starred_any: (i % 7 != 0).then_some(i % 2 == 0),
            priority_level: (i % 7 != 0).then_some((i % 3) as u8),
            repo_rank: (i % 5 != 0).then_some((i % 4) as f64 / 3.0),
            ahead_dispatch_fleet: (i % 6 != 0).then_some((i % 9) as u32),
        })
        .collect();
    let loops = (0..rows.len()).map(friction).collect();
    Assembled {
        rows,
        row_keys: Vec::new(),
        dwells: Vec::new(),
        priority: Vec::new(),
        priority_inputs,
        loops,
        stats: RowStats::default(),
        data_through: fit_as_of(),
    }
}

/// A varied friction set; every unknowable input is unknown on some rows.
fn friction(i: usize) -> LoopFeatures {
    let every = |n: usize| i.is_multiple_of(n);
    LoopFeatures {
        cum_stage_h: (!every(9)).then_some((i % 41) as f64 * 1.3),
        stage_looped: every(4),
        review_requests: (i % 4) as u32,
        approvals_lost: (i % 3) as u32,
        judge_reject_rate_7d: (!every(5)).then_some((i % 10) as f64 / 10.0),
        overlap_prs: (!every(6)).then_some((i % 3) as u32),
        overlap_files: (!every(6)).then_some((i % 5) as u32),
        own_ci_failed: (!every(8)).then_some(every(2)),
    }
}

/// The #10223 fixture's v1 model, re-tagged `eta-fit/v3`: the v1
/// coefficients at their positions, every appended column standardized
/// as-is (mu 0, sd 1), with weight only on `log_cum_stage` (`cum`) and
/// `log_approvals_lost` (`lost`) in the exit hazard, and the opposite
/// weights in the direct model, so both parts agree on the direction (a
/// higher exit hazard, a shorter remaining time). With both 0 it is
/// keen-wren's zero-weight model exactly.
pub(super) fn v3_fixture(as_of: DateTime<Utc>, cum: f64, lost: f64) -> CoefficientFile {
    let mut file = fixture_fit(as_of);
    assert_eq!(file.features, FEATURES.map(String::from), "the fixture is v1-ordered");
    file.schema = SCHEMA_V3.to_string();
    file.features = FEATURES_V3.map(String::from).to_vec();
    let extra_n = N_FEATURES_V3 - FEATURES.len();
    let extra = |sign: f64| {
        let mut v = vec![0.0; extra_n];
        v[CUM - FEATURES.len()] = sign * cum;
        v[LOST - FEATURES.len()] = sign * lost;
        v
    };
    for hazard in file.hazard.values_mut() {
        hazard.mu.extend(vec![0.0; extra_n]);
        hazard.sd.extend(vec![1.0; extra_n]);
        hazard.coef.extend(extra(1.0));
    }
    let aft = file.aft.as_mut().unwrap();
    aft.mu.extend(vec![0.0; extra_n]);
    aft.sd.extend(vec![1.0; extra_n]);
    aft.beta.extend(extra(-1.0));
    file.with_derived_id()
}

fn loop_kite(fit: Option<CoefficientFile>) -> LandLoopKite {
    LandLoopKite::new(fit.map(Arc::new))
}

fn reason(e: &Explanation) -> Option<NoEstimateReason> {
    e.no_estimate_reason
}

fn looped(cum_h: f64, lost: u32) -> LoopFeatures {
    LoopFeatures {
        cum_stage_h: Some(cum_h),
        stage_looped: lost > 0,
        review_requests: lost + 1,
        approvals_lost: lost,
        ..LoopFeatures::default()
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

const PR_STAGES: [Stage; 4] = [
    Stage::ReviewWait,
    Stage::Doctor,
    Stage::MergeWait,
    Stage::MergeHold,
];

// ---- the v3 fit and its files ---------------------------------------------

#[test]
fn the_v3_fit_is_tagged_v3_and_positional_against_features_v3() {
    let a = assembled(300, 60);
    let file = fit_v3(&meta(fit_as_of()), &a);
    assert_eq!(file.schema, SCHEMA_V3);
    assert_eq!(file.features, FEATURES_V3.map(String::from));
    assert_eq!(file.id, file.derive_id());
    assert!(!file.hazard.is_empty());
    for (stage, hazard) in &file.hazard {
        assert_eq!(hazard.coef.len(), N_FEATURES_V3, "{stage}");
        assert_eq!(hazard.mu.len(), N_FEATURES_V3, "{stage}");
        assert!(hazard.converged, "{stage}");
    }
    let aft = file.aft.as_ref().expect("the direct model fits");
    assert_eq!(aft.beta.len(), aft.stages.len() + N_FEATURES_V3);
    // The friction columns were actually fitted: not all-zero.
    let hazard = &file.hazard[&FitStage::ReviewWait];
    assert!(hazard.coef[N_FEATURES_V2..].iter().any(|c| c.abs() > 1e-6), "{:?}", hazard.coef);

    // One vector per row, each the shared transform of the row's own inputs.
    let xs = features_v3(&a);
    assert_eq!(xs.len(), a.rows.len());
    for (i, x) in xs.iter().enumerate() {
        assert_eq!(*x, model_features_v3(&training_inputs_v3(&a, i).unwrap()), "row {i}");
    }

    // The v1 and v2 fits of the same rows are the files they always were.
    let v1 = fit::fit(&meta(fit_as_of()), &a.rows, &a.dwells);
    assert_eq!(v1.features, FEATURES.map(String::from));
    let v2 = fit::v2::fit_v2(&meta(fit_as_of()), &a.rows, &a.priority_inputs, &a.dwells);
    assert_eq!(v2.schema, SCHEMA_V2);
    assert_eq!(v2.features, FEATURES_V2.map(String::from));
    assert_eq!(v1.path_stats, file.path_stats, "the path statistics read no features");
    assert_eq!(v1.age_p95_sec, file.age_p95_sec);

    // The coverage the fit reports counts the known inputs.
    let c = LoopCoverage::of(&a.loops);
    assert_eq!(c.rows, a.rows.len());
    assert_eq!(c.cum_stage_known, a.loops.iter().filter(|l| l.cum_stage_h.is_some()).count());
    assert!(c.ci_known < c.rows && c.overlap_known < c.rows);
}

#[test]
fn each_loader_reads_only_its_own_schema_and_directory() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let v1 = fixture_fit(fit_as_of());
    let v2 = v2_fixture(fit_as_of(), 0.0, 0.0);
    let v3 = v3_fixture(fit_as_of(), 0.0, 0.0);
    assert_ne!(v2.id, v3.id);
    let v1_path = fit::fit_dir(root).join(fit::path_for(v1.as_of));
    let v2_path = fit_dir_v2(root).join(fit::path_for(v2.as_of));
    let v3_path = fit_dir_v3(root).join(fit::path_for(v3.as_of));
    assert!(fit_dir_v3(root).starts_with(fit::fit_dir(root)));
    assert_ne!(fit_dir_v3(root), fit_dir_v2(root));
    fit::write(&v1_path, &v1).unwrap();
    fit::write(&v2_path, &v2).unwrap();
    fit::write(&v3_path, &v3).unwrap();
    let later = fit_as_of() + Duration::days(1);
    assert_eq!(load_latest_v3(root, later).map(|f| f.id), Some(v3.id.clone()));
    assert!(load_latest_v3(root, fit_as_of()).is_none(), "strictly before");

    // A file of another schema is refused, never read against v3 positions.
    assert!(read_v3(&v1_path).is_none());
    assert!(read_v3(&v2_path).is_none());
    assert!(fit::read(&v3_path).is_none());
    assert!(fit::v2::read_v2(&v3_path).is_none());
    let newer = fit_as_of() + Duration::hours(1);
    let stray = v2_fixture(newer, 0.0, 0.0);
    fit::write(&fit_dir_v3(root).join(fit::path_for(newer)), &stray).unwrap();
    let stray_v3 = v3_fixture(newer, 0.0, 0.0);
    fit::write(&fit_dir_v2(root).join(fit::path_for(newer)), &stray_v3).unwrap();
    assert_eq!(load_latest_v3(root, later).map(|f| f.id), Some(v3.id.clone()));
    assert_eq!(load_latest_v2(root, later).map(|f| f.id), Some(v2.id.clone()));

    // The registry gives each heuristic its own schema.
    let registry = Registry::load(root, later);
    assert_eq!(registry.fit_id(), Some(v1.id.as_str()));
    assert_eq!(registry.fit_v2_id(), Some(v2.id.as_str()));
    assert_eq!(registry.fit_v3_id(), Some(v3.id.as_str()));
}

// ---- train/serve parity of the v3 transform -------------------------------

/// The direct model's quantiles the evaluation core serves equal those
/// computed by hand from [`model_features_v3`] of the inputs the fit would
/// build: the same transform on both sides, the friction block (including
/// unknowns) at its v3 positions.
#[test]
fn the_v3_evaluation_reads_the_fits_transform() {
    let file = v3_fixture(fit_as_of(), 0.3, 0.4);
    let model = TwinOtterModel::of(&file).unwrap();
    let review = review_input();
    let CurrentState::At(current) = &review.current else {
        unreachable!("the review fixture is at a stage")
    };
    let mut input = crate::eta::heuristics::adapt_input(&review, current, FitStage::ReviewWait);
    input.ahead = Some(input.ahead.unwrap_or(2));
    input.n_stage_repo = Some(input.n_stage_repo.unwrap_or(3));
    input.exits_repo_6h = Some(input.exits_repo_6h.unwrap_or(1));
    input.exits_repo_24h = Some(input.exits_repo_24h.unwrap_or(4));
    input.exits_fleet_6h = Some(input.exits_fleet_6h.unwrap_or(5));
    input.merges_repo_24h = Some(input.merges_repo_24h.unwrap_or(2));
    input.merges_fleet_6h = Some(input.merges_fleet_6h.unwrap_or(6));
    input.since_merge_h = Some(input.since_merge_h.unwrap_or(1.5));
    input.n_stage_fleet = Some(input.n_stage_fleet.unwrap_or(7));
    input.priority = Some(PriorityInputs {
        starred_any: Some(true),
        priority_level: Some(1),
        repo_rank: None,
        ahead_dispatch_fleet: Some(2),
    });
    for loops in [
        None,
        Some(LoopFeatures::default()),
        Some(looped(30.0, 3)),
        Some(friction(7)),
    ] {
        input.loops = loops.clone();
        let got = evaluate(&model, &input, &EvalConfig::default()).unwrap();
        let (hour_utc, weekend) = clock(input.as_of);
        let x = model_features_v3(&ModelInputsV3 {
            v2: ModelInputsV2 {
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
                priority: input.priority.unwrap_or_default(),
            },
            loops: loops.clone().unwrap_or_default(),
        });
        let aft = file.aft.as_ref().unwrap();
        let k = aft
            .stages
            .iter()
            .position(|s| *s == FitStage::ReviewWait)
            .unwrap();
        let linear = aft.beta[k]
            + (0..N_FEATURES_V3)
                .map(|j| aft.beta[aft.stages.len() + j] * (x[j] - aft.mu[j]) / aft.sd[j])
                .sum::<f64>();
        let sigma = aft.log_sigma[k].exp();
        let want = PROBIT_TAUS.map(|p| (linear + sigma * p).exp().min(EvalConfig::default().cap_h));
        assert!(got.imputed.is_empty(), "{:?}", got.imputed);
        for (g, w) in got.aft_q.iter().zip(want) {
            assert!((g - w).abs() < 1e-9 * w.max(1.0), "{loops:?}: {g} vs {w}");
        }
    }
}

/// The hazard reads the cumulative stage age, and it advances with the
/// clock: a PR whose stage has already accumulated many hours across loops
/// is not read as a fresh visit, whatever its `age_h`.
#[test]
fn the_v3_hazard_reads_the_cumulative_stage_age() {
    let review = review_input();
    let CurrentState::At(current) = &review.current else {
        unreachable!("the review fixture is at a stage")
    };
    let mut input = crate::eta::heuristics::adapt_input(&review, current, FitStage::ReviewWait);
    let path_q = |file: &CoefficientFile, loops: Option<LoopFeatures>| {
        let mut i = input.clone();
        i.loops = loops;
        evaluate(&TwinOtterModel::of(file).unwrap(), &i, &EvalConfig::default())
            .unwrap()
            .hazard_path_q
    };
    // No weight: the loop block is inert.
    let flat = v3_fixture(fit_as_of(), 0.0, 0.0);
    assert_eq!(path_q(&flat, None), path_q(&flat, Some(looped(200.0, 3))));
    // A positive exit weight on `log_cum_stage`: more accumulated time in the
    // stage, an earlier exit.
    let weighted = v3_fixture(fit_as_of(), 0.8, 0.0);
    let fresh = path_q(&weighted, Some(looped(0.0, 0)));
    let old = path_q(&weighted, Some(looped(200.0, 3)));
    assert!(old[1] < fresh[1], "{old:?} vs {fresh:?}");
    // An invalid cumulative age is an input error, not a silent read.
    input.loops = Some(looped(f64::NAN, 0));
    assert!(
        evaluate(&TwinOtterModel::of(&weighted).unwrap(), &input, &EvalConfig::default()).is_err()
    );
}

#[test]
fn a_v3_model_with_no_friction_weight_reproduces_keen_wren() {
    let history = history_a();
    let wren = LandKeenWren::new(Some(Arc::new(v2_fixture(fit_as_of(), 0.0, 0.0))));
    let kite = loop_kite(Some(v3_fixture(fit_as_of(), 0.0, 0.0)));
    for stage in PR_STAGES {
        let mut input = at_stage(stage);
        input.features.loops = Some(looped(12.0, 2));
        let a = wren.estimate(&input, &history);
        let b = kite.estimate(&input, &history);
        assert_eq!(b.heuristic, LAND_LOOP_KITE);
        assert_eq!(b.no_estimate_reason, a.no_estimate_reason, "{stage}");
        assert_eq!(b.result, a.result, "{stage}");
    }
}

// ---- the heuristic ---------------------------------------------------------

#[test]
fn loop_kite_is_a_land_candidate_registered_after_keen_wren_and_bold_lark() {
    let fitted = Registry::with_all_fits(
        Some(Arc::new(fixture_fit(fit_as_of()))),
        Some(Arc::new(v2_fixture(fit_as_of(), 0.0, 0.0))),
        Some(Arc::new(v3_fixture(fit_as_of(), 0.0, 0.0))),
    );
    for registry in [Registry::builtin(), fitted] {
        let land: Vec<&str> = registry.for_kind(Kind::Land).map(Heuristic::id).collect();
        // After keen-wren and its conformal wrapper bold-lark (#10524);
        // `land-2026-10-06-tandem-wren` (#10510) stays last.
        assert_eq!(
            land[land.len() - 5..],
            [
                LAND_KEEN_WREN,
                LAND_BOLD_LARK,
                LAND_LOOP_KITE,
                LAND_TWIN_OTTER_B,
                LAND_TANDEM_WREN
            ]
        );
        assert_eq!(registry.tier_of(LAND_LOOP_KITE), Some(Tier::Candidate));
        assert_eq!(registry.current(Kind::Land, None).id(), "land-v1", "shadow");
        assert!(registry.get(LAND_LOOP_KITE).unwrap().models_hold());
        assert!(registry
            .check_budget(crate::eta::shadow_fleet::DEFAULT_MAX_ACTIVE)
            .is_ok());
    }
}

#[test]
fn loop_kite_pr_stages_need_a_v3_fit_and_read_the_friction_inputs() {
    let history = history_a();
    let mut input = review_input();
    input.features.loops = Some(looped(20.0, 1));
    // No file, or a v1 / v2 file handed in by mistake: `no_model`, never a
    // narrower vector read against v3 positions.
    for fit in [
        None,
        Some(fixture_fit(fit_as_of())),
        Some(v2_fixture(fit_as_of(), 0.0, 0.0)),
    ] {
        let e = loop_kite(fit).estimate(&input, &history);
        assert_eq!(reason(&e), Some(NoEstimateReason::NoModel));
    }
    // A v3 file cut off at `as_of` is not yet usable.
    let at = loop_kite(Some(v3_fixture(input.as_of, 0.3, 0.4))).estimate(&input, &history);
    assert_eq!(reason(&at), Some(NoEstimateReason::NoModel));

    let file = v3_fixture(fit_as_of(), 0.3, -0.4);
    let kite = loop_kite(Some(file.clone()));
    let e = kite.estimate(&input, &history);
    assert_eq!(reason(&e), None);
    let record = e.twin_otter.as_ref().expect("a twin-otter record");
    assert_eq!(record.fit_id, file.id);
    assert_eq!(record.input.loops, Some(looped(20.0, 1)), "the record carries what it read");
    // The answer recomputes from its own record (the v3 transform included),
    // after a JSON round trip.
    let back: Explanation = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
    let r = e.result.as_ref().unwrap();
    assert_eq!(
        run_explanation(&back),
        Some((r.p25_sec, r.p50_sec, r.p75_sec, r.p90_sec.unwrap()))
    );

    // The friction inputs move it: with this model's negative exit weight on
    // approvals lost, a PR that lost approvals lands later.
    let p50 = |loops: Option<LoopFeatures>| {
        let mut i = input.clone();
        i.features.loops = loops;
        kite.estimate(&i, &history).result.unwrap().p50_sec
    };
    assert!(p50(Some(looped(20.0, 3))) > p50(Some(looped(20.0, 0))));
    assert_eq!(p50(None), p50(Some(LoopFeatures::default())), "unknown reads as the default");

    // A resolver refusal is passed through.
    let mut refused = input.clone();
    refused.current = CurrentState::Refused(NoEstimateReason::Blocked);
    assert_eq!(reason(&kite.estimate(&refused, &history)), Some(NoEstimateReason::Blocked));
}

#[test]
fn loop_kite_pre_pr_answered_ness_equals_land_v2s() {
    let kite = loop_kite(Some(v3_fixture(fit_as_of(), 0.3, 0.4)));
    let nofit = loop_kite(None);
    let empty = StageSamples::default();
    let full = history_a();
    for (name, history) in [("empty", &empty), ("history_a", &full)] {
        for stage in [Stage::ReadyWait, Stage::SweepCurator, Stage::SweepBuilder] {
            let input = at_stage(stage);
            let v2 = LandV2.estimate(&input, history);
            for h in [&kite, &nofit] {
                let got = h.estimate(&input, history);
                assert_eq!(got.heuristic, LAND_LOOP_KITE);
                assert_eq!(got.result.is_some(), v2.result.is_some(), "{name} {stage}");
                assert_eq!(reason(&got), reason(&v2), "{name} {stage}");
                if let Some(c) = &got.combination {
                    assert_eq!(c.method, KEEN_WREN_PRE_PR_METHOD);
                }
            }
        }
    }
}

// ---- the walk-forward replay (paired backtest against twin-otter-b) -------

#[test]
fn walk_forward_dates_each_schema_on_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("fits");
    let first = fit_as_of() - Duration::days(1);
    let second = fit_as_of();
    let v1a = fixture_fit(first);
    let v1b = fixture_fit(second);
    // The v3 file starts a day later than v1, as on the day v3 shipped.
    let v3 = v3_fixture(second, 0.0, 0.0);
    fit::write(&root.join(fit::path_for(first)), &v1a).unwrap();
    fit::write(&root.join(fit::path_for(second)), &v1b).unwrap();
    fit::write(&root.join("v3").join(fit::path_for(second)), &v3).unwrap();
    // A v3 file in the v1 directory is not a v1 fit.
    fit::write(&root.join("fit-stray.json"), &v3_fixture(second + Duration::hours(1), 0.0, 0.0))
        .unwrap();

    let fits = DatedFits::load_dir(&root).unwrap();
    assert_eq!(fits.cutoffs(), vec![first, second]);
    let early = fits.at(first + Duration::hours(1));
    assert_eq!(early.fit_id(), Some(v1a.id.as_str()));
    assert_eq!(early.fit_v3_id(), None, "no v3 file yet");
    let late = fits.at(second + Duration::hours(1));
    assert_eq!(late.fit_id(), Some(v1b.id.as_str()));
    assert_eq!(late.fit_v3_id(), Some(v3.id.as_str()));
    assert_eq!(late.fit_v2_id(), None);

    // Both sides of the pair replay: twin-otter-b and loop-kite answer the
    // same PR-stage case from that day's files.
    let mut input = review_input();
    input.as_of = second + Duration::hours(1);
    input.features.loops = Some(looped(5.0, 1));
    let history = history_a();
    for id in [LAND_TWIN_OTTER_B, LAND_LOOP_KITE] {
        let e = fits.heuristic(id).unwrap().estimate(&input, &history);
        assert_eq!(reason(&e), None, "{id}");
    }
    input.as_of = first + Duration::hours(1);
    let e = fits
        .heuristic(LAND_LOOP_KITE)
        .unwrap()
        .estimate(&input, &history);
    assert_eq!(reason(&e), Some(NoEstimateReason::NoModel), "before the first v3 file");

    // Only v1 files: the registries are the v1-only ones they always were.
    let only_v1 = DatedFits::new(vec![v1a.clone()]);
    assert_eq!(only_v1.at(first + Duration::hours(1)).fit_v3_id(), None);
    assert_eq!(LOOP_FEATURES.len(), N_FEATURES_V3 - N_FEATURES_V2);
}
