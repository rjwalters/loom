//! Parity with the #10223 twin-otter fixture (#10221): regenerate its 6,000
//! training rows from `generator`, fit, and match `fitted`.
//!
//! The draw order is the fixture comment's, exactly: one `SplitMix64`
//! stream seeded `generator.seed`, stages in `generator.stages` order, 1,500
//! rows each; per row 19 feature draws, then the exit-label uniform, the AFT
//! normal uniform (clipped to `[1e-12, 1 − 1e-12]`, `z = Φ⁻¹(u)`) and the
//! censoring uniform (`c = 200u` hours) — 22 draws.

use super::TWIN_OTTER_PARITY;
use crate::eta::fit::math::{probit, sigmoid};
use crate::eta::fit::{
    self, model_features, AftFit, CoefficientFile, FitMeta, FitStage, FitWindow, Fitter, HazardFit,
    MergeLabel, ModelInputs, TrainingRow, FEATURES,
};
use crate::eta::simulate::SplitMix64;
use chrono::{TimeZone, Utc};
use serde_json::Value;

fn fixture() -> Value {
    serde_json::from_str(TWIN_OTTER_PARITY).unwrap()
}

/// A generator coefficient map, in [`FEATURES`] order.
fn by_feature(map: &Value) -> Vec<f64> {
    FEATURES.iter().map(|f| map[*f].as_f64().unwrap()).collect()
}

/// The fixture's 6,000 training rows, each in its own group.
fn generate() -> Vec<TrainingRow> {
    let fx = fixture();
    let g = &fx["generator"];
    let per_stage = g["rows_per_stage"].as_u64().unwrap() as usize;
    let stages: Vec<FitStage> = serde_json::from_value(g["stages"].clone()).unwrap();
    let exit_coef = by_feature(&g["true_exit_coef"]);
    let aft_coef = by_feature(&g["true_aft_coef"]);
    let mut rng = SplitMix64::new(g["seed"].as_u64().unwrap());
    let mut rows = Vec::with_capacity(stages.len() * per_stage);
    for stage in stages {
        let name = stage.as_str();
        let exit_b = g["true_exit_intercept"][name].as_f64().unwrap();
        let aft_mu = g["true_aft_mu"][name].as_f64().unwrap();
        let aft_sigma = g["true_aft_sigma"][name].as_f64().unwrap();
        for _ in 0..per_stage {
            // `from_fn` fills in ascending index order: the draw order.
            let d: [f64; 19] = std::array::from_fn(|_| rng.next_f64());
            let count = |u: f64, k: f64| (k * u).floor() as u32;
            let inputs = ModelInputs {
                age_h: 48.0 * (d[0] * d[0]),
                ahead: count(d[1], 10.0),
                n_stage_repo: count(d[2], 15.0),
                exits_repo_6h: count(d[3], 20.0),
                exits_repo_24h: count(d[4], 60.0),
                exits_fleet_6h: count(d[5], 150.0),
                merges_repo_24h: count(d[6], 50.0),
                merges_fleet_6h: count(d[7], 120.0),
                since_merge_h: 72.0 * d[8],
                n_stage_fleet: count(d[9], 60.0),
                hour_utc: 24.0 * d[10],
                weekend: d[11] < 2.0 / 7.0,
                rework: count(d[12], 3.0),
                op_hold: d[13] < 0.4,
                sequenced: d[14] < 0.05,
                starred: d[15] < 0.03,
                conflict: d[16] < 0.09,
                ci_fail: d[17] < 0.05,
                blocked: d[18] < 0.08,
            };
            let x = model_features(&inputs);
            let dot = |c: &[f64]| c.iter().zip(&x).map(|(a, b)| a * b).sum::<f64>();
            let exit = rng.next_f64() < sigmoid(exit_b + dot(&exit_coef));
            let z = probit(rng.next_f64().clamp(1e-12, 1.0 - 1e-12));
            let t = (aft_mu + dot(&aft_coef) + aft_sigma * z).exp();
            let c = 200.0 * rng.next_f64();
            rows.push(TrainingRow {
                stage,
                group: format!("fixture#{}", rows.len()),
                inputs,
                exit: Some(exit),
                merge: MergeLabel {
                    dur_h: t.min(c),
                    merged: t <= c,
                },
            });
        }
    }
    rows
}

fn meta() -> FitMeta {
    let as_of = Utc.with_ymd_and_hms(2026, 10, 4, 0, 0, 0).unwrap();
    FitMeta {
        as_of,
        window: FitWindow::standard(as_of),
        fitter: Fitter {
            version: "0.19.676".to_string(),
            revision: "0123456789abcdef0123456789abcdef01234567".to_string(),
        },
    }
}

fn max_abs_diff(a: &[f64], b: &[f64]) -> f64 {
    assert_eq!(a.len(), b.len());
    a.iter().zip(b).fold(0.0, |m, (x, y)| m.max((x - y).abs()))
}

#[test]
fn generator_regenerates_the_fixture_rows() {
    let rows = generate();
    assert_eq!(rows.len(), 6000);
    let exits: Vec<usize> = FitStage::ALL
        .iter()
        .map(|s| {
            rows.iter()
                .filter(|r| r.stage == *s && r.exit == Some(true))
                .count()
        })
        .collect();
    assert_eq!(exits, [89, 60, 161, 72]);
    assert_eq!(rows.iter().filter(|r| r.merge.merged).count(), 5798);
    let floored = rows
        .iter()
        .filter(|r| r.merge.dur_h < fit::MIN_DUR_H)
        .count();
    assert_eq!(floored, 3);
}

#[test]
fn age_p95_matches_the_verified_values() {
    let p95 = fit::age_p95_sec(&generate());
    let got: Vec<(FitStage, i64)> = p95.into_iter().collect();
    assert_eq!(
        got,
        [
            (FitStage::ReviewWait, 154_081),
            (FitStage::DoctorWait, 152_228),
            (FitStage::MergeWait, 154_064),
            (FitStage::MergeHold, 155_732),
        ]
    );
}

#[test]
fn fit_matches_the_fixture_within_tolerance() {
    let fx = fixture();
    let file = fit::fit(&meta(), &generate(), &[]);
    assert!(file.hazard_skipped.is_empty(), "{:?}", file.hazard_skipped);

    for stage in FitStage::ALL {
        let want: HazardFit =
            serde_json::from_value(fx["fitted"]["hazard"][stage.as_str()].clone()).unwrap();
        let got = &file.hazard[&stage];
        assert!(max_abs_diff(&got.mu, &want.mu) < 1e-9, "{stage} mu");
        assert!(max_abs_diff(&got.sd, &want.sd) < 1e-9, "{stage} sd");
        let coef = max_abs_diff(&got.coef, &want.coef);
        let intercept = (got.intercept - want.intercept).abs();
        // The issue's AC is 1e-3; the prototype reached 2.2e-7, so anything
        // above 1e-5 is a bug rather than solver noise.
        assert!(
            coef < 1e-5 && intercept < 1e-5,
            "{stage}: coef {coef:e}, intercept {intercept:e}"
        );
        assert!(got.converged, "{stage} converged after {}", got.iterations);
        assert_eq!(got.rows, 1500);
    }

    let want: AftFit = serde_json::from_value(fx["fitted"]["aft"].clone()).unwrap();
    let got = file.aft.as_ref().unwrap();
    assert_eq!(got.stages, want.stages);
    assert!(max_abs_diff(&got.mu, &want.mu) < 1e-9, "aft mu");
    assert!(max_abs_diff(&got.sd, &want.sd) < 1e-9, "aft sd");
    let beta = max_abs_diff(&got.beta, &want.beta);
    let log_sigma = max_abs_diff(&got.log_sigma, &want.log_sigma);
    assert!(beta < 1e-5 && log_sigma < 1e-5, "aft: beta {beta:e}, log_sigma {log_sigma:e}");
    assert!(
        (got.objective - 1.453_215_125_245_752_4).abs() < 1e-6,
        "aft objective {}",
        got.objective
    );
    assert!(got.converged, "aft converged after {}", got.iterations);
    assert_eq!((got.rows, got.events, got.groups), (6000, 5798, 6000));
}

#[test]
fn fitting_twice_is_byte_identical_and_the_id_tracks_content() {
    let rows = generate();
    let a = fit::fit(&meta(), &rows, &[]);
    let b = fit::fit(&meta(), &rows, &[]);
    assert_eq!(fit::to_json(&a), fit::to_json(&b));
    assert_eq!(a.id, b.id);
    assert_eq!(a.id, a.derive_id());

    let mut changed: CoefficientFile = a.clone();
    changed.hazard.get_mut(&FitStage::ReviewWait).unwrap().coef[0] += 1e-9;
    assert_ne!(changed.derive_id(), a.id);
}

#[test]
fn group_weighting_counts_each_pr_once() {
    let rows = generate();
    let single = fit::aft::fit(&rows).unwrap();
    // Every row three times over, in one group per original row: the weights
    // fall to 1/3 and the fit must not move.
    let tripled: Vec<TrainingRow> = rows
        .iter()
        .flat_map(|r| std::iter::repeat_n(r.clone(), 3))
        .collect();
    let weighted = fit::aft::fit(&tripled).unwrap();
    assert_eq!((weighted.rows, weighted.groups), (18_000, 6000));
    assert!(max_abs_diff(&weighted.beta, &single.beta) < 1e-9);
    assert!(max_abs_diff(&weighted.log_sigma, &single.log_sigma) < 1e-9);
    assert!((weighted.objective - single.objective).abs() < 1e-9);

    // And the weights are live: lumping half the rows into one PR moves it.
    let mut uneven = rows.clone();
    for r in uneven.iter_mut().take(3000) {
        r.group = "one-long-lived-pr".to_string();
    }
    let lumped = fit::aft::fit(&uneven).unwrap();
    assert_eq!(lumped.groups, 3001);
    assert!(max_abs_diff(&lumped.beta, &single.beta) > 1e-3);
}
