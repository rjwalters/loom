//! `land-2026-10-04-twin-otter`'s evaluation core (#10222, Slice A) against
//! the #10223 parity fixture, plus the unit properties the fixture cannot
//! show: the step inverse, the cap, the blend's monotonicity, the age clamp,
//! imputation, refusals and determinism.

use super::TWIN_OTTER_PARITY;
use crate::eta::fit::{
    AftFit, CoefficientFile, FitMeta, FitStage, FitWindow, Fitter, HazardFit, KmCurve, PathStats,
};
use crate::eta::twin_otter::{
    blend, evaluate, first_exit, km_inverse, nearest_rank, seed_for_visit, EvalConfig, EvalError,
    Evaluation, TwinOtterInput, TwinOtterModel, CAP_H, PATHS, PROBIT_TAUS, STEPS, STEP_H, TAUS,
};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use std::collections::BTreeMap;

/// The Monte Carlo size the fixture's 3% tolerance is stated at.
const PARITY_PATHS: usize = 200_000;

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

#[derive(Deserialize, Clone)]
struct Fitted {
    hazard: BTreeMap<FitStage, HazardFit>,
    aft: AftFit,
}

#[derive(Deserialize)]
struct FixtureEvaluation {
    step_h: f64,
    steps: usize,
    cap_h: f64,
    taus: Vec<f64>,
    path_stats: PathStats,
    rows: Vec<Row>,
}

#[derive(Deserialize)]
struct Row {
    input: TwinOtterInput,
    survival_k1_48: Vec<f64>,
    hazard_path_q: [f64; 4],
    aft_q: [f64; 4],
    blend_q: [f64; 4],
}

fn fixture() -> Fixture {
    serde_json::from_str(TWIN_OTTER_PARITY).expect("the parity fixture parses")
}

impl Fixture {
    fn model(&self) -> TwinOtterModel<'_> {
        TwinOtterModel {
            features: &self.generator.features,
            hazard: &self.fitted.hazard,
            aft: &self.fitted.aft,
            path_stats: &self.evaluation.path_stats,
        }
    }

    fn config(&self, paths: usize, seed: u64) -> EvalConfig {
        EvalConfig {
            step_h: self.evaluation.step_h,
            steps: self.evaluation.steps,
            cap_h: self.evaluation.cap_h,
            paths,
            seed,
            age_clamp_h: None,
        }
    }
}

/// The seed a tracker would derive for `row`'s stage visit.
fn visit_seed(input: &TwinOtterInput) -> u64 {
    let entered = input.as_of - Duration::seconds((input.age_h * 3600.0).round() as i64);
    seed_for_visit("github:1073994527#10222", &input.stage, entered)
}

fn relative(actual: f64, expected: f64) -> f64 {
    (actual - expected).abs() / expected.abs()
}

// ------------------------------------------------------------------ parity

#[test]
fn the_fixture_matches_the_constants() {
    let fx = fixture();
    assert_eq!(fx.evaluation.step_h, STEP_H);
    assert_eq!(fx.evaluation.steps, STEPS);
    assert_eq!(fx.evaluation.cap_h, CAP_H);
    assert_eq!(fx.evaluation.taus, TAUS.to_vec());
    assert_eq!(fx.evaluation.rows.len(), 5);
}

/// The hard-coded `Φ⁻¹(τ)` are the fit's own probit at the four taus.
#[test]
fn the_hard_coded_probits_are_the_fit_probit() {
    for (tau, probit) in TAUS.iter().zip(PROBIT_TAUS) {
        let fitted = crate::eta::fit::math::probit(*tau);
        assert!((fitted - probit).abs() <= 1e-15, "Φ⁻¹({tau}): {fitted} vs {probit}");
    }
}

#[test]
fn parity_survival_and_aft_quantiles_within_1e_6() {
    let fx = fixture();
    let model = fx.model();
    for (n, row) in fx.evaluation.rows.iter().enumerate() {
        let got = evaluate(&model, &row.input, &fx.config(PATHS, 1)).expect("row evaluates");
        assert_eq!(got.survival.len(), fx.evaluation.steps);
        for (k, (s, want)) in got.survival.iter().zip(&row.survival_k1_48).enumerate() {
            assert!((s - want).abs() <= 1e-6, "row {n} S_{}: {s} vs fixture {want}", k + 1);
        }
        for (t, (q, want)) in got.aft_q.iter().zip(&row.aft_q).enumerate() {
            assert!((q - want).abs() <= 1e-6, "row {n} aft_q[{t}]: {q} vs fixture {want}");
        }
        assert!(got.imputed.is_empty());
        assert!(!got.age_clamp_applied);
    }
}

#[test]
fn parity_path_and_blend_quantiles_within_3_percent_at_200k_paths() {
    let fx = fixture();
    let model = fx.model();
    for (n, row) in fx.evaluation.rows.iter().enumerate() {
        let got = evaluate(&model, &row.input, &fx.config(PARITY_PATHS, visit_seed(&row.input)))
            .expect("row evaluates");
        for t in 0..4 {
            let path = relative(got.hazard_path_q[t], row.hazard_path_q[t]);
            let blended = relative(got.blend_q[t], row.blend_q[t]);
            assert!(
                path <= 0.03,
                "row {n} hazard_path_q[{t}]: {} vs fixture {} ({:.2}%)",
                got.hazard_path_q[t],
                row.hazard_path_q[t],
                path * 100.0
            );
            assert!(
                blended <= 0.03,
                "row {n} blend_q[{t}]: {} vs fixture {} ({:.2}%)",
                got.blend_q[t],
                row.blend_q[t],
                blended * 100.0
            );
        }
    }
}

#[test]
fn every_part_is_non_decreasing_and_capped() {
    let fx = fixture();
    let model = fx.model();
    for row in &fx.evaluation.rows {
        let got = evaluate(&model, &row.input, &fx.config(PATHS, visit_seed(&row.input))).unwrap();
        for q in [got.hazard_path_q, got.aft_q, got.blend_q] {
            assert!(q.windows(2).all(|w| w[0] <= w[1]), "{q:?} decreases");
            assert!(q.iter().all(|v| *v <= CAP_H && *v > 0.0), "{q:?} escapes (0, cap]");
        }
    }
}

// ----------------------------------------------------- the seed and refresh

fn bits(e: &Evaluation) -> Vec<u64> {
    e.survival
        .iter()
        .chain(&e.hazard_path_q)
        .chain(&e.aft_q)
        .chain(&e.blend_q)
        .map(|v| v.to_bits())
        .collect()
}

#[test]
fn the_same_inputs_and_seed_give_a_bitwise_identical_evaluation() {
    let fx = fixture();
    let model = fx.model();
    for row in &fx.evaluation.rows {
        let config = fx.config(PATHS, visit_seed(&row.input));
        let first = evaluate(&model, &row.input, &config).unwrap();
        let second = evaluate(&model, &row.input, &config).unwrap();
        assert_eq!(bits(&first), bits(&second));
        assert_eq!(first, second);
    }
}

#[test]
fn the_seed_is_a_function_of_the_stage_visit_alone() {
    let entered: DateTime<Utc> = "2026-10-04T08:00:00Z".parse().unwrap();
    let seed = seed_for_visit("github:1#7", "review_wait", entered);
    assert_eq!(seed, seed_for_visit("github:1#7", "review_wait", entered));
    assert_ne!(seed, seed_for_visit("github:1#8", "review_wait", entered));
    assert_ne!(seed, seed_for_visit("github:1#7", "merge_wait", entered));
    assert_ne!(
        seed,
        seed_for_visit("github:1#7", "review_wait", entered + Duration::seconds(1))
    );
}

/// A refresh five minutes later (the tracker's default cadence) of an
/// unchanged item — same visit, so the same seed — replays the same
/// uniforms: p50 moves by the model's own drift and a little jitter, not by
/// a redraw (#10243 asks for under 5%).
#[test]
fn an_unchanged_refresh_moves_the_blended_p50_by_under_5_percent() {
    let fx = fixture();
    let model = fx.model();
    for (n, row) in fx.evaluation.rows.iter().enumerate() {
        let seed = visit_seed(&row.input);
        let mut later = row.input.clone();
        later.as_of += Duration::minutes(5);
        later.age_h += 5.0 / 60.0;
        assert_eq!(visit_seed(&later), seed, "the refresh keeps its seed");
        let before = evaluate(&model, &row.input, &fx.config(PATHS, seed)).unwrap();
        let after = evaluate(&model, &later, &fx.config(PATHS, seed)).unwrap();
        let moved = relative(after.blend_q[1], before.blend_q[1]);
        assert!(
            moved < 0.05,
            "row {n}: blended p50 {} -> {} ({:.2}%)",
            before.blend_q[1],
            after.blend_q[1],
            moved * 100.0
        );
    }
}

// ---------------------------------------------------------- the primitives

fn curve(t: &[f64], s: &[f64]) -> KmCurve {
    serde_json::from_value(serde_json::json!({"t": t, "s": s})).unwrap()
}

#[test]
fn the_km_inverse_is_a_step_function() {
    let c = curve(&[0.0, 1.0, 2.0, 5.0], &[1.0, 0.6, 0.3, 0.0]);
    // u exactly on a stored survival takes that point, not the next one.
    assert_eq!(km_inverse(&c, 0.6), 1.0);
    assert_eq!(km_inverse(&c, 0.3), 2.0);
    // Between two points: the step, never an interpolation.
    assert_eq!(km_inverse(&c, 0.45), 2.0);
    // u → 1 leaves at the first event; u → 0 at the last.
    assert_eq!(km_inverse(&c, 1.0 - 1e-12), 1.0);
    assert_eq!(km_inverse(&c, 1e-12), 5.0);
    assert_eq!(km_inverse(&c, 0.0), 5.0);
    // A curve censoring kept above u returns its last time.
    let censored = curve(&[0.0, 1.0, 2.0], &[1.0, 0.6, 0.4]);
    assert_eq!(km_inverse(&censored, 0.1), 2.0);
}

#[test]
fn the_first_exit_inverts_the_survival_curve_within_its_step() {
    let survival = [0.8, 0.5, 0.2];
    // u1 = 0.9: still above S_1, so step 0, 1/2 of the way through.
    assert!((first_exit(&survival, 0.9, 0.5).unwrap() - 0.25).abs() < 1e-12);
    // u1 = 0.5 = S_2 exactly: S_1 > u1 ≥ S_2, the end of step 1.
    assert!((first_exit(&survival, 0.5, 0.5).unwrap() - 1.0).abs() < 1e-12);
    // Surviving every step is `None`.
    assert_eq!(first_exit(&survival, 0.1, 0.5), None);
    assert_eq!(first_exit(&survival, 0.2 - 1e-12, 0.5), None);
}

#[test]
fn nearest_rank_matches_the_simulate_rule() {
    let sorted: Vec<f64> = (1..=256).map(f64::from).collect();
    assert_eq!(nearest_rank(&sorted, 25), 64.0);
    assert_eq!(nearest_rank(&sorted, 50), 128.0);
    assert_eq!(nearest_rank(&sorted, 90), 231.0);
    assert_eq!(nearest_rank(&[7.0], 90), 7.0);
}

#[test]
fn the_blend_is_a_cumulative_max_of_the_capped_means() {
    // The means cross: 5.5, 3.5, 4.5, 5.5.
    assert_eq!(blend(&[1.0, 2.0, 3.0, 4.0], &[10.0, 5.0, 6.0, 7.0], CAP_H), [5.5; 4]);
    // Each part is capped before the mean.
    assert_eq!(blend(&[1.0, 2.0, 3.0, 4.0], &[1.0, 2.0, 3.0, 1e9], 10.0), [1.0, 2.0, 3.0, 7.0]);
}

#[test]
fn a_path_that_survives_the_horizon_contributes_exactly_the_cap() {
    let mut fx = fixture();
    let stage = FitStage::MergeWait;
    let hazard = fx.fitted.hazard.get_mut(&stage).unwrap();
    hazard.intercept = -60.0; // h ≈ 1e-26: nobody leaves
    hazard.coef.iter_mut().for_each(|c| *c = 0.0);
    let row = fx
        .evaluation
        .rows
        .iter()
        .find(|r| r.input.stage == "merge_wait")
        .unwrap();
    let got = evaluate(&fx.model(), &row.input, &fx.config(PATHS, 3)).unwrap();
    assert_eq!(got.hazard_path_q, [CAP_H; 4]);
    assert!(got.blend_q.iter().all(|q| *q <= CAP_H));
}

// --------------------------------------------------------------- age clamp

/// A model whose hazard reads the age alone, so the clamp is visible as a
/// constant hazard.
fn age_only(fx: &mut Fixture, stage: FitStage) -> (f64, f64, f64) {
    let age = fx
        .generator
        .features
        .iter()
        .position(|f| f == "log_age")
        .unwrap();
    let hazard = fx.fitted.hazard.get_mut(&stage).unwrap();
    for (i, c) in hazard.coef.iter_mut().enumerate() {
        if i != age {
            *c = 0.0;
        }
    }
    (hazard.coef[age], hazard.mu[age], hazard.sd[age])
}

#[test]
fn the_age_clamp_is_off_by_default() {
    assert_eq!(EvalConfig::default().age_clamp_h, None);
}

#[test]
fn a_clamp_below_the_age_holds_every_step_at_the_clamp() {
    let mut fx = fixture();
    let (coef, mu, sd) = age_only(&mut fx, FitStage::MergeHold);
    let intercept = fx.fitted.hazard[&FitStage::MergeHold].intercept;
    let row = &fx.evaluation.rows[1]; // merge_hold, age 10 h
    let mut config = fx.config(PATHS, 5);
    config.age_clamp_h = Some(4.0);
    let got = evaluate(&fx.model(), &row.input, &config).unwrap();
    assert!(got.age_clamp_applied);
    let h = 1.0 / (1.0 + (-(intercept + coef * ((4.0_f64).ln_1p() - mu) / sd)).exp());
    for (k, s) in got.survival.iter().enumerate().take(48) {
        let want = (1.0 - h).powi(k as i32 + 1);
        assert!((s - want).abs() < 1e-12, "S_{}: {s} vs {want}", k + 1);
    }
}

#[test]
fn a_clamp_beyond_the_horizon_changes_nothing() {
    let fx = fixture();
    for row in &fx.evaluation.rows {
        let off = evaluate(&fx.model(), &row.input, &fx.config(PATHS, 9)).unwrap();
        let mut config = fx.config(PATHS, 9);
        config.age_clamp_h = Some(row.input.age_h + STEPS as f64 * STEP_H + 1.0);
        let on = evaluate(&fx.model(), &row.input, &config).unwrap();
        assert!(!on.age_clamp_applied);
        assert_eq!(bits(&on), bits(&off));
    }
}

// --------------------------------------------------------------- imputation

#[test]
fn a_missing_count_is_imputed_at_the_training_mean_and_named() {
    let fx = fixture();
    let row = &fx.evaluation.rows[3]; // review_wait
    let mut missing = row.input.clone();
    missing.ahead = None;
    missing.since_merge_h = None;
    let got = evaluate(&fx.model(), &missing, &fx.config(PATHS, 11)).unwrap();
    assert_eq!(got.imputed, vec!["ahead", "since_merge_h"]);

    // Standardized 0 contributes nothing, exactly like a zero coefficient.
    let mut zeroed = fixture();
    let k = zeroed.fitted.aft.stages.len();
    for name in ["log_ahead", "log_since_merge"] {
        let i = zeroed
            .generator
            .features
            .iter()
            .position(|f| f == name)
            .unwrap();
        for hazard in zeroed.fitted.hazard.values_mut() {
            hazard.coef[i] = 0.0;
        }
        zeroed.fitted.aft.beta[k + i] = 0.0;
    }
    let want = evaluate(&zeroed.model(), &row.input, &zeroed.config(PATHS, 11)).unwrap();
    assert_eq!(got.survival, want.survival);
    assert_eq!(got.aft_q, want.aft_q);
}

// ----------------------------------------------------------------- refusals

#[test]
fn an_unknown_stage_is_refused() {
    let fx = fixture();
    let mut input = fx.evaluation.rows[0].input.clone();
    for stage in ["sweep.builder", "ready_wait", "doctor", ""] {
        input.stage = stage.to_string();
        assert_eq!(
            evaluate(&fx.model(), &input, &fx.config(PATHS, 1)),
            Err(EvalError::UnknownStage(stage.to_string()))
        );
    }
}

#[test]
fn a_coefficient_file_is_a_model_when_it_has_a_direct_model() {
    let fx = fixture();
    let as_of: DateTime<Utc> = "2026-10-04T00:00:00Z".parse().unwrap();
    let meta = FitMeta {
        as_of,
        window: FitWindow::standard(as_of),
        fitter: Fitter {
            version: "0.0.0".to_string(),
            revision: "0".repeat(40),
        },
    };
    let mut file = CoefficientFile::empty(&meta);
    file.features.clone_from(&fx.generator.features);
    file.hazard.clone_from(&fx.fitted.hazard);
    file.aft = Some(fx.fitted.aft.clone());
    file.path_stats = fx.evaluation.path_stats.clone();
    let model = TwinOtterModel::of(&file).expect("a file with a direct model");
    let row = &fx.evaluation.rows[2];
    assert_eq!(
        evaluate(&model, &row.input, &fx.config(PATHS, 4)),
        evaluate(&fx.model(), &row.input, &fx.config(PATHS, 4))
    );
    file.aft = None;
    assert!(TwinOtterModel::of(&file).is_none());
}

#[test]
fn a_stage_the_model_does_not_cover_is_unknown() {
    let row_input = fixture().evaluation.rows[0].input.clone(); // merge_hold
    let mut no_hazard = fixture();
    no_hazard.fitted.hazard.remove(&FitStage::MergeHold);
    assert!(matches!(
        evaluate(&no_hazard.model(), &row_input, &no_hazard.config(PATHS, 1)),
        Err(EvalError::UnknownStage(_))
    ));
    let mut no_aft = fixture();
    no_aft
        .fitted
        .aft
        .stages
        .retain(|s| *s != FitStage::MergeHold);
    assert!(matches!(
        evaluate(&no_aft.model(), &row_input, &no_aft.config(PATHS, 1)),
        Err(EvalError::UnknownStage(_))
    ));
    let mut no_paths = fixture();
    no_paths
        .evaluation
        .path_stats
        .next
        .remove(&FitStage::MergeHold);
    assert!(matches!(
        evaluate(&no_paths.model(), &row_input, &no_paths.config(PATHS, 1)),
        Err(EvalError::UnknownStage(_))
    ));
}

fn invalid(fx: &Fixture, row: usize) -> bool {
    matches!(
        evaluate(&fx.model(), &fx.evaluation.rows[row].input, &fx.config(PATHS, 1)),
        Err(EvalError::InvalidModel(_))
    )
}

#[test]
fn a_malformed_model_is_invalid_and_never_panics() {
    let mut fx = fixture();
    fx.fitted.hazard.get_mut(&FitStage::MergeHold).unwrap().sd[3] = 0.0;
    assert!(invalid(&fx, 0), "a zero sd");

    let mut fx = fixture();
    fx.fitted
        .hazard
        .get_mut(&FitStage::MergeHold)
        .unwrap()
        .coef
        .pop();
    assert!(invalid(&fx, 0), "a short coefficient vector");

    let mut fx = fixture();
    fx.fitted.aft.log_sigma.pop();
    assert!(invalid(&fx, 0), "a short log_sigma");

    let mut fx = fixture();
    fx.generator.features[4] = "log_mystery".to_string();
    assert!(invalid(&fx, 0), "an unknown feature name");

    let mut fx = fixture();
    fx.evaluation.path_stats.km.remove(&FitStage::DoctorWait);
    assert!(invalid(&fx, 0), "a next target with no dwell curve");

    let mut fx = fixture();
    fx.evaluation
        .path_stats
        .km
        .get_mut(&FitStage::MergeWait)
        .unwrap()
        .s
        .pop();
    assert!(invalid(&fx, 0), "a dwell curve with mismatched lengths");
}

#[test]
fn an_out_of_domain_input_or_config_is_refused() {
    let fx = fixture();
    let mut input = fx.evaluation.rows[0].input.clone();
    input.age_h = -1.0;
    assert!(matches!(
        evaluate(&fx.model(), &input, &fx.config(PATHS, 1)),
        Err(EvalError::InvalidInput(_))
    ));
    let input = &fx.evaluation.rows[0].input;
    assert!(matches!(
        evaluate(&fx.model(), input, &fx.config(0, 1)),
        Err(EvalError::InvalidInput(_))
    ));
}
