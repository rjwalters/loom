//! `eta backtest --adaptation` (#10528): the regime layer's adaptation times
//! on a heuristic's replayed residuals, under an injected x2 shift.

use super::{history_a, history_a_envelopes, provenance};
use crate::eta::backtest::adaptation::{
    measure, measure_at, ScoredRow, HORIZON_H, TARGET_T_ALARM_H, TARGET_T_COV_H, TARGET_T_P50_H,
};
use crate::eta::backtest::{self, Filter};
use crate::eta::heuristics::LandV1;
use crate::eta::{Stage, MIN_SAMPLES};
use chrono::{DateTime, Duration, TimeZone, Utc};

const PER_HOUR: i64 = 10;
const SIGMA: f64 = 0.3;
const STAGE: Stage = Stage::ReviewWait;

fn epoch() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap()
}

/// Deterministic standard-normal-ish draw (sum of 12 uniforms, minus 6).
fn normal(state: &mut u64) -> f64 {
    let mut s = 0.0;
    for _ in 0..12 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        s += (*state >> 11) as f64 / (1u64 << 53) as f64;
    }
    s - 6.0
}

/// A calm, calibrated residual stream: `hours` of `PER_HOUR` outcomes with
/// log-normal noise about `bias`, each carrying its p25-p75 band.
fn calm(hours: i64, seed: u64, bias: f64) -> Vec<ScoredRow> {
    let mut rng = seed;
    let half = 0.6745 * SIGMA;
    let mut rows = Vec::new();
    for h in 0..hours {
        for k in 0..PER_HOUR {
            rows.push(ScoredRow {
                stage: STAGE,
                known_at: epoch() + Duration::minutes(h * 60 + k * (60 / PER_HOUR) + 1),
                log_ratio: bias + SIGMA * normal(&mut rng),
                band: Some((-half, half)),
            });
        }
    }
    rows
}

/// The shipped regime layer on a calm, calibrated stream: it alarms within
/// the hour and serves the new p50 within 6 h. Coverage of the post-shift
/// outcomes alone is back in band after 10-13 h, around the 12 h target
/// (`eta/tests/regime.rs` reports less because its trailing window still
/// holds in-band pre-shift outcomes); the bound below pins that, so a
/// regression in `eta::regime` shows here.
#[test]
fn a_calm_calibrated_stream_adapts_and_never_false_alarms() {
    for seed in [1_u64, 5, 7, 42] {
        let rows = calm(96, seed, 0.0);
        let a = measure(&rows).expect("measured");
        assert_eq!(a.stage, STAGE);
        // The shift goes in at the median outcome.
        assert_eq!(a.shift_at, rows[rows.len() / 2].known_at, "seed {seed}");
        assert_eq!((a.n_before, a.n_after, a.horizon_h), (480, 240, HORIZON_H));
        assert!(a.t_p50_h.is_some_and(|h| h <= TARGET_T_P50_H), "seed {seed}: {a:?}");
        assert!(a.t_alarm_h.is_some_and(|h| h <= TARGET_T_ALARM_H), "seed {seed}: {a:?}");
        assert!(a.t_cov_h.is_some_and(|h| h <= TARGET_T_COV_H + 1), "seed {seed}: {a:?}");
        assert_eq!(a.false_alarm_h, None, "seed {seed}: {a:?}");
        assert_eq!(a.meets_targets(), a.t_cov_h <= Some(TARGET_T_COV_H), "seed {seed}");
        let pre = a.pre_coverage.unwrap();
        assert!((0.4..=0.6).contains(&pre), "seed {seed}: {pre}");
    }
}

#[test]
fn a_biased_heuristic_is_measured_against_its_own_old_truth() {
    // Every outcome 1.6x the p50 before the shift: the new truth is 3.2x.
    let rows = calm(96, 11, 0.5);
    let a = measure(&rows).expect("measured");
    assert!(a.t_p50_h.is_some_and(|h| h <= TARGET_T_P50_H), "{a:?}");
    assert_eq!(a.false_alarm_h, None, "{a:?}");
    // Its own band misses the bias, and the report says so.
    assert!(a.pre_coverage.unwrap() < 0.4, "{a:?}");
}

#[test]
fn too_few_outcomes_on_either_side_measure_nothing() {
    assert!(measure(&[]).is_none());
    let rows = calm(96, 3, 0.0);
    let t = rows[MIN_SAMPLES - 1].known_at;
    assert!(measure_at(&rows, STAGE, t).is_none(), "fewer than the floor before T");
    let last = rows.last().unwrap().known_at;
    assert!(measure_at(&rows, STAGE, last).is_none(), "fewer than the floor after T");
    assert!(measure_at(&rows, Stage::Doctor, rows[400].known_at).is_none(), "no such track");
}

#[test]
fn deterministic_and_blind_to_everything_past_the_horizon() {
    let rows = calm(120, 5, 0.0);
    let t = rows[480].known_at;
    let a = measure_at(&rows, STAGE, t).unwrap();
    assert_eq!(Some(a.clone()), measure_at(&rows, STAGE, t));
    let end = t + Duration::hours(HORIZON_H);
    let mut perturbed = rows.clone();
    for r in perturbed.iter_mut().filter(|r| r.known_at >= end) {
        r.log_ratio = 9.0;
    }
    assert_eq!(measure_at(&perturbed, STAGE, t), Some(a));
}

#[test]
fn the_busiest_stage_is_the_one_measured() {
    let mut rows = calm(96, 9, 0.0);
    // A thinner second track.
    rows.extend(calm(96, 10, 0.0).into_iter().step_by(3).map(|r| ScoredRow {
        stage: Stage::Doctor,
        ..r
    }));
    rows.sort_by_key(|r| r.known_at);
    assert_eq!(measure(&rows).unwrap().stage, STAGE);
}

#[test]
fn run_with_adaptation_is_run_plus_the_measurement() {
    let envelopes = history_a_envelopes();
    let history = history_a();
    let cases = backtest::cases_from_envelopes(&envelopes);
    let loom = provenance();
    let plain = backtest::run(&LandV1, &history, &cases, Filter::default(), &loom);
    assert_eq!(plain.regime_adaptation, None);
    let with = backtest::run_with_adaptation(&LandV1, &history, &cases, Filter::default(), &loom);
    assert_eq!(
        backtest::BacktestReport {
            regime_adaptation: None,
            ..with.clone()
        },
        plain
    );
    // `None` is never serialised, so a plain report's JSON is unchanged.
    let json = serde_json::to_value(&plain).unwrap();
    assert!(json.get("regime_adaptation").is_none());
    if let Some(a) = &with.regime_adaptation {
        assert!(a.n_before >= MIN_SAMPLES && a.n_after >= MIN_SAMPLES, "{a:?}");
        let json = serde_json::to_value(&with).unwrap();
        for key in [
            "t_p50_h",
            "t_cov_h",
            "t_alarm_h",
            "false_alarm_h",
            "shift_at",
        ] {
            assert!(json["regime_adaptation"].get(key).is_some(), "{key}");
        }
    }
}
