//! `land-2026-10-06-brisk-petrel` (#10528): the drift-gated regime
//! adjustment on the **served** path. The adaptation times are measured on
//! the quantiles [`regime::serve`] actually serves (not on the bare factor):
//! `t_p50` <= 6 h, `t_cov` <= 12 h and `t_alarm` <= 3 h on a shift fixture
//! where every review time doubles at `T` with no covariate change, and an
//! identity (byte-identical) serve on the no-shift fixture. Plus the leak
//! test, the recompute, and the registration.

use super::land_twin_otter::{fit_as_of, fixture_fit, review_input};
use super::{as_of, history_a, input_at};
use crate::eta::heuristics::{
    LandBriskPetrel, LandTwinOtterB, LandV2, LAND_BRISK_PETREL, LAND_TWIN_OTTER_B,
};
use crate::eta::recalibrate::{CalibrationObservation, OBSERVATION_SCHEMA};
use crate::eta::regime::{self, residuals_of};
use crate::eta::simulate::run_explanation;
use crate::eta::{estimate_id, Explanation, Heuristic, Kind, Registry, Stage, Tier};
use chrono::{DateTime, Duration, Utc};
use std::sync::Arc;

const HOUR: i64 = 3_600;
/// Estimates per hour in the fixture stream.
const PER_HOUR: i64 = 10;
/// Log-space spread of an outcome around the base p50.
const SIGMA: f64 = 0.3;
/// The base p50, seconds.
const P50: f64 = 3_600.0;
/// Hours of pre-shift stream.
const PRE_HOURS: i64 = 72;
/// Hours of stream after `T`.
const POST_HOURS: i64 = 24;
/// Standard-normal quantiles at 25 / 75 / 90%.
const Z75: f64 = 0.674_49;
const Z90: f64 = 1.281_55;

/// The base quantiles, exactly calibrated for the pre-shift regime.
fn base_q() -> (i64, i64, i64, i64) {
    let q = |z: f64| (P50 * (SIGMA * z).exp()).round() as i64;
    (q(-Z75), q(0.0), q(Z75), q(Z90))
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

/// One fixture estimate: when it was made and its true remaining seconds.
struct Made {
    at: DateTime<Utc>,
    remaining: i64,
}

/// A twin-otter-b `review_wait` estimate every 6 min from [`PRE_HOURS`]
/// before `t` to [`POST_HOURS`] after it, its base p50 [`P50`]; outcomes
/// log-normal around it, multiplied by `factor` for estimates made at or
/// after `t`. Returned as the calibration rows the server reads, and the
/// stream itself for scoring.
fn stream(t: DateTime<Utc>, factor: f64, seed: u64) -> (Vec<CalibrationObservation>, Vec<Made>) {
    let mut rng = seed;
    let (mut rows, mut made) = (Vec::new(), Vec::new());
    for i in 0..(PRE_HOURS + POST_HOURS) * PER_HOUR {
        let at = t + Duration::seconds((i - PRE_HOURS * PER_HOUR) * HOUR / PER_HOUR);
        let f = if at >= t { factor } else { 1.0 };
        let remaining = ((P50 * f * (SIGMA * normal(&mut rng)).exp()).round() as i64).max(1);
        rows.push(CalibrationObservation {
            schema: OBSERVATION_SCHEMA.to_string(),
            estimate_id: format!("e{i}"),
            heuristic: LAND_TWIN_OTTER_B.to_string(),
            repo: "rjwalters/loom".to_string(),
            issue: 1,
            stage: Stage::ReviewWait,
            as_of: at,
            p50_sec: base_q().1,
            actual_at: Some(at + Duration::seconds(remaining)),
            resolved_at: Some(at + Duration::seconds(remaining)),
            age_sec: Some(0),
            p25_sec: Some(base_q().0),
            p75_sec: Some(base_q().2),
            p90_sec: Some(base_q().3),
        });
        made.push(Made { at, remaining });
    }
    (rows, made)
}

/// A `review_wait` explanation with the calibrated [`base_q`] as its
/// result (any answered base will do: the server reads only the stage, the
/// instant and the quantiles).
fn base_at(at: DateTime<Utc>) -> Explanation {
    let mut e = LandV2.estimate(&input_at(Stage::ReviewWait, 0, 0), &history_a());
    let (p25, p50, p75, p90) = base_q();
    let r = e.result.as_mut().expect("the fixture base answers");
    (r.p25_sec, r.p50_sec, r.p75_sec, r.p90_sec) = (p25, p50, p75, Some(p90));
    r.eta_p50_at = at + Duration::seconds(p50);
    e.as_of = at;
    e
}

fn served_at(rows: &[CalibrationObservation], at: DateTime<Utc>) -> Explanation {
    regime::serve(base_at(at), rows, LAND_TWIN_OTTER_B)
}

#[derive(Debug, Default)]
struct Times {
    t_p50: Option<i64>,
    t_cov: Option<i64>,
    t_alarm: Option<i64>,
}

/// Replay hour by hour after `T`, prequentially, scoring what was served.
fn measure(t: DateTime<Utc>, rows: &[CalibrationObservation], made: &[Made]) -> Times {
    let mut out = Times::default();
    let residuals = residuals_of(rows, LAND_TWIN_OTTER_B);
    // Each estimate's served p25-p75, from what was known when it was made.
    let window_from = t - Duration::hours(6);
    let scored: Vec<(DateTime<Utc>, bool)> = made
        .iter()
        .filter(|m| m.at >= window_from)
        .map(|m| {
            let (p25, _, p75, _) = served_at(rows, m.at).quantiles_with_p90().unwrap();
            (m.at, (p25..=p75).contains(&m.remaining))
        })
        .collect();
    let mut in_band_since = None;
    for after in 1..=POST_HOURS {
        let now = t + Duration::hours(after);
        let served = served_at(rows, now);
        let p50 = served.quantiles_with_p90().unwrap().1 as f64;
        // Within 25% of the new truth (2x the old p50).
        if out.t_p50.is_none() && (p50 - 2.0 * P50).abs() <= 0.25 * 2.0 * P50 {
            out.t_p50 = Some(after);
        }
        if out.t_alarm.is_none() && regime::drift(&residuals, Stage::ReviewWait, now).drifted {
            out.t_alarm = Some(after);
        }
        // p25-p75 coverage of the estimates made in the trailing 6 h.
        let window: Vec<bool> = scored
            .iter()
            .filter(|(at, _)| *at >= now - Duration::hours(6) && *at < now)
            .map(|(_, hit)| *hit)
            .collect();
        let cov = window.iter().filter(|h| **h).count() as f64 / window.len() as f64;
        if (0.4..=0.6).contains(&cov) {
            in_band_since.get_or_insert(after);
        } else {
            in_band_since = None;
        }
    }
    out.t_cov = in_band_since;
    out
}

#[test]
fn served_shift_fixture_adapts_within_the_budgets() {
    for seed in [1_u64, 7, 42] {
        let (rows, made) = stream(as_of(), 2.0, seed);
        let t = measure(as_of(), &rows, &made);
        assert!(t.t_alarm.is_some_and(|h| h <= 3), "seed {seed}: {t:?}");
        assert!(t.t_p50.is_some_and(|h| h <= 6), "seed {seed}: {t:?}");
        assert!(t.t_cov.is_some_and(|h| h <= 12), "seed {seed}: {t:?}");
        // The adjustment is on the record once it serves.
        let late = served_at(&rows, as_of() + Duration::hours(12));
        let record = late.regime_adjustment.expect("adjusted after the shift");
        assert_eq!(record.stage, "review_wait");
        assert!(record.factor > 1.5 && record.factor <= regime::MAX_FACTOR, "{record:?}");
        assert_eq!(record.half_life, regime::ADJUST_HALF_LIFE_SEC);
    }
}

#[test]
fn served_no_shift_fixture_is_byte_identical_to_the_base_and_never_alarms() {
    for seed in [1_u64, 7, 42] {
        let (rows, _) = stream(as_of(), 1.0, seed);
        let residuals = residuals_of(&rows, LAND_TWIN_OTTER_B);
        for after in 1..=POST_HOURS {
            let now = as_of() + Duration::hours(after);
            assert!(!regime::drift(&residuals, Stage::ReviewWait, now).drifted);
            let served = served_at(&rows, now);
            assert!(served.regime_adjustment.is_none(), "seed {seed} +{after}h");
            assert_eq!(
                serde_json::to_string(&served).unwrap(),
                serde_json::to_string(&base_at(now)).unwrap(),
                "seed {seed} +{after}h"
            );
        }
    }
}

/// The gate: a significant residual that has not tripped the CUSUM (here,
/// a bias already present across the whole baseline) is not served.
#[test]
fn an_undrifted_bias_is_not_served() {
    // Only the shifted rows: every outcome on record is 2x slow.
    let (mut rows, _) = stream(as_of(), 2.0, 9);
    rows.retain(|o| o.as_of >= as_of());
    let now = as_of() + Duration::hours(POST_HOURS);
    let residuals = residuals_of(&rows, LAND_TWIN_OTTER_B);
    assert!(!regime::adjust(&residuals, Stage::ReviewWait, now).is_identity());
    assert!(!regime::drift(&residuals, Stage::ReviewWait, now).drifted);
    assert!(regime::gated(&residuals, Stage::ReviewWait, now).is_identity());
    assert!(served_at(&rows, now).regime_adjustment.is_none());
}

#[test]
fn other_heuristics_rows_are_not_this_track() {
    let (mut rows, _) = stream(as_of(), 2.0, 3);
    for r in &mut rows {
        r.heuristic = "land-v2".to_string();
    }
    let served = served_at(&rows, as_of() + Duration::hours(12));
    assert!(served.regime_adjustment.is_none());
}

#[test]
fn serving_ignores_everything_known_at_or_after_as_of() {
    let (rows, _) = stream(as_of(), 2.0, 11);
    let now = as_of() + Duration::hours(4);
    let reference = serde_json::to_string(&served_at(&rows, now)).unwrap();
    let mut perturbed = rows.clone();
    for o in &mut perturbed {
        if o.resolved_at.is_some_and(|k| k >= now) {
            o.actual_at = Some(now + Duration::days(30));
            o.resolved_at = Some(now + Duration::days(30));
        }
    }
    assert_eq!(serde_json::to_string(&served_at(&perturbed, now)).unwrap(), reference);
    // Deterministic: the same inputs serve the same bytes.
    assert_eq!(serde_json::to_string(&served_at(&rows, now)).unwrap(), reference);
}

#[test]
fn an_adjusted_explanation_recomputes_from_its_own_fields() {
    let (rows, _) = stream(as_of(), 2.0, 5);
    let mut base = LandV2.estimate(&input_at(Stage::ReviewWait, 0, 0), &history_a());
    base.as_of = as_of() + Duration::hours(8);
    let unadjusted = run_explanation(&base).expect("the base recomputes");
    let served = regime::serve(base, &rows, LAND_TWIN_OTTER_B);
    let record = served.regime_adjustment.clone().expect("adjusted");
    assert_eq!(run_explanation(&served), served.quantiles_with_p90());
    assert_eq!(served.quantiles_with_p90(), Some(regime::scale(unadjusted, record.factor)));
}

#[test]
fn brisk_petrel_is_twin_otter_b_until_drift_then_scaled() {
    let fit = Some(Arc::new(fixture_fit(fit_as_of())));
    let petrel = LandBriskPetrel::new(fit.clone());
    let base = LandTwinOtterB::new(fit);
    let input = review_input();
    let mut history = history_a();
    let b = base.estimate(&input, &history);
    assert!(b.result.is_some(), "the fixture fit answers");

    // No logged track: twin-otter-b's estimate, re-identified, byte for byte.
    let calm = petrel.estimate(&input, &history);
    let mut expected = b.clone();
    expected.heuristic = LAND_BRISK_PETREL.to_string();
    expected.estimate_id = estimate_id(&input.subject, Kind::Land, LAND_BRISK_PETREL, input.as_of);
    assert_eq!(serde_json::to_string(&calm).unwrap(), serde_json::to_string(&expected).unwrap());

    // Review times doubled from 8 h before the estimate: scaled and recorded.
    let (rows, _) = stream(input.as_of - Duration::hours(8), 2.0, 13);
    history.calibration = rows;
    let shifted = petrel.estimate(&input, &history);
    let record = shifted.regime_adjustment.clone().expect("adjusted");
    assert_eq!(
        shifted.quantiles_with_p90(),
        Some(regime::scale(b.quantiles_with_p90().unwrap(), record.factor))
    );
    assert_eq!(shifted.heuristic, LAND_BRISK_PETREL);
}

#[test]
fn brisk_petrel_is_a_registered_land_candidate_not_current() {
    let registry = Registry::builtin();
    assert!(registry.ids().contains(&LAND_BRISK_PETREL));
    assert_eq!(registry.tier_of(LAND_BRISK_PETREL), Some(Tier::Candidate));
    assert_ne!(registry.current(Kind::Land, None).id(), LAND_BRISK_PETREL);
    let h = registry.get(LAND_BRISK_PETREL).unwrap();
    assert_eq!(h.kind(), Kind::Land);
    assert!(h.models_hold(), "as its base, twin-otter-b");
}
