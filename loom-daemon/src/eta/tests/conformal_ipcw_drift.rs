//! The drift-aware IPCW wrapper, `land-2026-10-06-swift-tern` (#10524 slice
//! 3, #10528): the drift-shortened half-life, the withheld inflation, the
//! leak test and the registration, on quick-tern's synthetic fixtures
//! (`tests::conformal_ipcw`).

use super::conformal_ipcw::{at, base_explanation, bytes, coverage, fixture, obs, BASE, HOUR};
use super::input_at;
use crate::eta::conformal::{apply, Calibration, Q4};
use crate::eta::conformal_ipcw::{self, HALF_LIFE_SEC, METHOD_DRIFT};
use crate::eta::heuristics::{LandSwiftTern, LAND_SWIFT_TERN, LAND_TWIN_OTTER_B};
use crate::eta::recalibrate::CalibrationObservation;
use crate::eta::recency::DRIFTED_DIVISOR;
use crate::eta::regime::MAX_INFLATION;
use crate::eta::simulate::run_explanation;
use crate::eta::{Explanation, Heuristic, Kind, Registry, Stage};
use chrono::Duration;

type Calibrator = fn(&Explanation, &[CalibrationObservation], i64) -> Option<Calibration>;

fn swift(base: &Explanation, rows: &[CalibrationObservation], secs: i64) -> Option<Calibration> {
    let mut e = base.clone();
    e.as_of = at(secs);
    conformal_ipcw::calibrate_drift_aware(e, rows, LAND_TWIN_OTTER_B).calibration
}

fn quick(base: &Explanation, rows: &[CalibrationObservation], secs: i64) -> Option<Calibration> {
    let mut e = base.clone();
    e.as_of = at(secs);
    conformal_ipcw::calibrate(e, rows, LAND_TWIN_OTTER_B).calibration
}

/// `(seconds after the shift, p25–p75 coverage, late surprise)` every half
/// hour over the first 24 h.
fn series(
    calibrator: Calibrator,
    rows: &[CalibrationObservation],
    factor: f64,
) -> Vec<(i64, f64, f64)> {
    let base = base_explanation();
    (0..=48)
        .map(|half_hour| {
            let record = calibrator(&base, rows, half_hour * 1_800).expect("calibrated");
            let (inside, late) = coverage(&record, factor);
            (half_hour * 1_800, inside, late)
        })
        .collect()
}

/// From when on p25–p75 coverage stays in [40%, 60%].
fn t_cov(series: &[(i64, f64, f64)]) -> Option<i64> {
    (0..series.len())
        .find(|&i| series[i..].iter().all(|s| (0.40..=0.60).contains(&s.1)))
        .map(|i| series[i].0)
}

/// When p25–p75 coverage first reaches 40%.
fn first_in(series: &[(i64, f64, f64)]) -> Option<i64> {
    series.iter().find(|s| s.1 >= 0.40).map(|s| s.0)
}

/// Mean `|coverage − 0.5|` over the first 12 h.
fn mean_error(series: &[(i64, f64, f64)]) -> f64 {
    let first: Vec<f64> = series
        .iter()
        .filter(|s| s.0 <= 12 * HOUR)
        .map(|s| (s.1 - 0.5).abs())
        .collect();
    first.iter().sum::<f64>() / first.len() as f64
}

/// `shift` widened about p50 by `factor` on the log scale for [`BASE`]: what
/// serving the withheld inflation would do.
fn inflated(shift: &Q4<f64>, factor: f64) -> Q4<f64> {
    let q = [BASE.0, BASE.1, BASE.2, BASE.3].map(|v| (v as f64).ln());
    let s = shift.to_array();
    let center = q[1] + s[1];
    let mut out = s;
    for k in [0, 2, 3] {
        out[k] = s[k] + (factor - 1.0) * (q[k] + s[k] - center);
    }
    Q4::from_array(out)
}

#[test]
fn swift_tern_on_the_no_shift_fixture_is_quick_tern_and_never_widens() {
    let base = base_explanation();
    let rows = fixture(1.0, 300);
    for half_hour in -24..=48 {
        let s = swift(&base, &rows, half_hour * 1_800).expect("calibrated");
        let q = quick(&base, &rows, half_hour * 1_800).expect("calibrated");
        assert_eq!(s.method, METHOD_DRIFT);
        assert_eq!(s.shift, q.shift, "{half_hour}: no drift, quick-tern's answer");
        let (p25, _, p75, p90) = apply(BASE, &s.shift);
        assert!(p75 - p25 <= BASE.2 - BASE.0, "{half_hour}: widened {s:?}");
        assert!(p90 <= BASE.3, "{half_hour}: p90 widened {s:?}");
        let ipcw = s.ipcw.as_ref().unwrap();
        assert_eq!(ipcw.half_life_sec, HALF_LIFE_SEC);
        let drift = ipcw
            .drift
            .as_ref()
            .expect("the check ran on dense evidence");
        assert!(!drift.drifted, "{half_hour}: false alarm {drift:?}");
        assert_eq!(drift.half_life_start_sec, HALF_LIFE_SEC);
        assert!((drift.withheld_inflation - 1.0).abs() < f64::EPSILON);
    }
}

#[test]
fn swift_tern_regime_shifts_recover_within_twelve_hours() {
    for factor in [3.0, 2.0, 0.5, 1.0 / 3.0, 0.1] {
        let rows = fixture(factor, 300);
        let s = series(swift, &rows, factor);
        let ts = t_cov(&s);
        assert!(ts.is_some_and(|t| t <= 12 * HOUR), "factor {factor}: t_cov {ts:?} {s:?}");
        for (t, _, late) in s.iter().filter(|x| x.0 >= 12 * HOUR) {
            assert!(*late <= 0.15, "factor {factor} at {t}: late {late}");
        }
    }
}

#[test]
fn a_strong_speed_up_trips_the_check_and_the_shorter_window_catches_up_sooner() {
    let base = base_explanation();
    let factor = 0.1;
    let rows = fixture(factor, 300);
    let mut tripped = 0;
    for half_hour in 0..=48 {
        let secs = half_hour * 1_800;
        let s = swift(&base, &rows, secs).expect("calibrated");
        let drift = s.ipcw.as_ref().unwrap().drift.clone().expect("checked");
        if drift.drifted {
            tripped += 1;
            assert_eq!(drift.half_life_start_sec, HALF_LIFE_SEC / DRIFTED_DIVISOR);
            assert!(s.ipcw.as_ref().unwrap().half_life_sec < HALF_LIFE_SEC, "{s:?}");
            assert!((1.0..=MAX_INFLATION).contains(&drift.withheld_inflation));
        } else {
            assert_eq!(drift.half_life_start_sec, HALF_LIFE_SEC);
            assert_eq!(s.shift, quick(&base, &rows, secs).unwrap().shift);
        }
    }
    assert!(tripped > 0, "a x0.1 speed-up trips the check");

    let (s, q) = (series(swift, &rows, factor), series(quick, &rows, factor));
    let (fs, fq) = (first_in(&s).unwrap(), first_in(&q).unwrap());
    assert!(fs <= HOUR && fq >= 4 * HOUR, "first in band: swift {fs}, quick {fq}");
    let (es, eq) = (mean_error(&s), mean_error(&q));
    // Measured 0.099 against 0.154; the first hour, before any check can
    // trip, is the same for both.
    assert!(es < eq * 0.75, "mean |coverage - 0.5| over 12 h: swift {es}, quick {eq}");
}

#[test]
fn applying_the_withheld_inflation_would_over_cover() {
    // Why the inflation is recorded and not served: right after the x0.1
    // shift the recent residuals mix both regimes, so the check stays up
    // after the shorter window has caught up, and the widening overshoots.
    let base = base_explanation();
    let factor = 0.1;
    let rows = fixture(factor, 300);
    let mut worst = (0.0_f64, 0.0_f64);
    for half_hour in 0..=24 {
        let s = swift(&base, &rows, half_hour * 1_800).unwrap();
        let drift = s.ipcw.as_ref().unwrap().drift.clone().unwrap();
        if drift.withheld_inflation > 1.0 {
            let widened = Calibration {
                shift: inflated(&s.shift, drift.withheld_inflation),
                ..s.clone()
            };
            let (served, _) = coverage(&s, factor);
            let (would, _) = coverage(&widened, factor);
            if would > worst.0 {
                worst = (would, served);
            }
        }
    }
    assert!(worst.0 > 0.8, "inflated coverage {worst:?}");
    assert!(worst.1 <= 0.65, "served coverage {worst:?}");
}

#[test]
fn swift_tern_leak_perturbing_post_as_of_outcomes_is_bit_identical() {
    let base = base_explanation();
    // A x0.1 speed-up two hours before `as_of`, so the drift path runs.
    let mut rows: Vec<CalibrationObservation> = fixture(0.1, 300)
        .into_iter()
        .map(|mut o| {
            o.as_of -= Duration::hours(2);
            o.actual_at = o.actual_at.map(|a| a - Duration::hours(2));
            o.resolved_at = o.actual_at;
            o
        })
        .collect();
    rows.retain(|o| o.as_of < at(0));
    let calibrate = |rows: &[CalibrationObservation]| {
        conformal_ipcw::calibrate_drift_aware(base.clone(), rows, LAND_TWIN_OTTER_B)
    };
    let reference = calibrate(&rows);
    let record = reference.calibration.as_ref().expect("calibrated");
    let drift = record
        .ipcw
        .as_ref()
        .unwrap()
        .drift
        .as_ref()
        .expect("checked");
    assert!(drift.drifted, "the fixture exercises the drift path: {drift:?}");

    // Every outcome known at or after `as_of`, moved wildly.
    let mut perturbed = rows.clone();
    for o in &mut perturbed {
        if o.resolved_at.is_some_and(|known| known >= at(0)) {
            o.actual_at = Some(at(1_000_000));
            o.resolved_at = Some(at(1_000_000));
        }
    }
    assert_eq!(bytes(&calibrate(&perturbed)), bytes(&reference));

    // A landing before `as_of` known only afterwards: its landing time is
    // invisible to the residuals too.
    let mut late = obs("late", Stage::ReviewWait, -HOUR, Some(600));
    late.resolved_at = Some(at(500));
    let mut late_b = late.clone();
    late_b.actual_at = Some(at(-60));
    let (mut with_a, mut with_b) = (rows.clone(), rows.clone());
    with_a.push(late);
    with_b.push(late_b);
    assert_eq!(bytes(&calibrate(&with_a)), bytes(&calibrate(&with_b)));

    // Estimates made at or after `as_of` are never evidence.
    let mut future = rows.clone();
    future.extend((0..60).map(|i| obs(&format!("f{i}"), Stage::ReviewWait, i * 60, Some(30))));
    assert_eq!(bytes(&calibrate(&future)), bytes(&reference));
}

#[test]
fn swift_tern_is_retired_but_still_recomputes() {
    // Retired from the registry (#10921); the module stays, so persisted
    // explanations and the offline `--wrap ipcw-drift` still work.
    let registry = Registry::builtin();
    assert!(!registry.ids().contains(&LAND_SWIFT_TERN));
    assert!(!registry
        .for_kind(Kind::Land)
        .any(|h| h.id() == LAND_SWIFT_TERN));
    assert_eq!(registry.current(Kind::Land, None).id(), "land-v1");
    let heuristic = LandSwiftTern::default();
    assert!(heuristic.models_hold(), "as twin-otter-b");

    let input = input_at(Stage::SweepBuilder, 0, 0);
    let mut history = super::ready::history_ready();
    history.calibration.clear();
    let bare = heuristic.estimate(&input, &history);
    assert_eq!(bare.heuristic, LAND_SWIFT_TERN);
    assert!(bare.quantiles_with_p90().is_some(), "twin-otter-b answers pre-PR");
    assert!(bare.calibration.is_none(), "no evidence, no record");

    // A x0.1 speed-up two hours ago, in the estimate's own stage.
    history.calibration = fixture(0.1, 300)
        .into_iter()
        .map(|mut o| {
            o.stage = Stage::SweepBuilder;
            o.as_of -= Duration::hours(2);
            o.actual_at = o.actual_at.map(|a| a - Duration::hours(2));
            o.resolved_at = o.actual_at;
            o
        })
        .filter(|o| o.as_of < at(0))
        .collect();
    let e = LandSwiftTern::default().estimate(&input, &history);
    let record = e.calibration.as_ref().expect("calibrated");
    assert_eq!(record.method, METHOD_DRIFT);
    assert_eq!(record.base, LAND_TWIN_OTTER_B);
    let drift = record
        .ipcw
        .as_ref()
        .unwrap()
        .drift
        .as_ref()
        .expect("checked");
    assert!(drift.drifted, "{drift:?}");
    let q = e.quantiles_with_p90().unwrap();
    assert!(q.0 <= q.1 && q.1 <= q.2 && q.2 <= q.3);
    // The explanation recomputes from itself, also after a JSON round trip.
    assert_eq!(run_explanation(&e), e.quantiles_with_p90());
    let parsed: Explanation = serde_json::from_str(&bytes(&e)).unwrap();
    assert_eq!(run_explanation(&parsed), e.quantiles_with_p90());
    assert_eq!(bytes(&parsed), bytes(&e));
    assert!(bytes(&e).contains("\"withheld_inflation\""));
}
