//! `land-2026-10-06-even-lark` (#10489): calm-plover's conformal calibration
//! on the seconds scale — the point-in-time leak test, censoring, per-quantile
//! hit rates, the additive rate limit, the explanation record (recomputable
//! from itself) and the shadow registration.

use super::conformal::log_calibrated;
use super::{as_of, history_a, input_at};
use crate::eta::conformal::{self, apply_seconds, Scale, Q4};
use crate::eta::heuristics::{LandEvenLark, LAND_EVEN_LARK, LAND_V2};
use crate::eta::recalibrate::{CalibrationObservation, OBSERVATION_SCHEMA};
use crate::eta::simulate::run_explanation;
use crate::eta::{Heuristic, Kind, Registry, Stage};
use chrono::{DateTime, Duration, Utc};

const DAY: i64 = 86_400;

fn at(secs: i64) -> DateTime<Utc> {
    as_of() + Duration::seconds(secs)
}

/// The base quantiles every logged observation carries.
const BASE: (i64, i64, i64, i64) = (1_800, 3_600, 7_200, 14_400);

fn obs(id: &str, stage: Stage, made: i64, remaining: Option<i64>) -> CalibrationObservation {
    CalibrationObservation {
        schema: OBSERVATION_SCHEMA.to_string(),
        estimate_id: id.to_string(),
        heuristic: LAND_V2.to_string(),
        repo: "rjwalters/loom".to_string(),
        issue: 1,
        stage,
        as_of: at(made),
        p50_sec: BASE.1,
        actual_at: remaining.map(|r| at(made + r)),
        resolved_at: remaining.map(|r| at(made + r)),
        age_sec: Some(0),
        p25_sec: Some(BASE.0),
        p75_sec: Some(BASE.2),
        p90_sec: Some(BASE.3),
    }
}

/// `n` landings whose remaining time is spread evenly over `[lo, hi]`
/// seconds, made over the past ten days and resolved before the fixture
/// instant.
fn landings(stage: Stage, n: usize, lo: i64, hi: i64, tag: &str) -> Vec<CalibrationObservation> {
    (0..n)
        .map(|i| {
            let remaining = lo + (hi - lo) * i as i64 / (n as i64 - 1);
            let made = -DAY * (1 + (i % 10) as i64) - 60 * i as i64 - 50_000;
            obs(&format!("{tag}-{i}"), stage, made, Some(remaining))
        })
        .collect()
}

fn estimate_with(observations: Vec<CalibrationObservation>) -> crate::eta::Explanation {
    let mut history = history_a();
    history.calibration = observations;
    LandEvenLark.estimate(&input_at(Stage::ReviewWait, 0, 0), &history)
}

fn bytes(e: &crate::eta::Explanation) -> String {
    serde_json::to_string(e).unwrap()
}

#[test]
fn even_lark_leak_perturbing_post_as_of_outcomes_is_bit_identical() {
    let mut base = landings(Stage::ReviewWait, 80, 600, 60_000, "a");
    // Still open at the fit instant.
    base.extend((0..10).map(|i| obs(&format!("o{i}"), Stage::ReviewWait, -7_200 - i, None)));
    let reference = estimate_with(base.clone());
    assert!(reference.calibration.is_some(), "calibrated");

    // Resolve every open estimate at or after `as_of`, wildly.
    let mut perturbed = base.clone();
    for o in perturbed.iter_mut().filter(|o| o.actual_at.is_none()) {
        o.actual_at = Some(at(1_000_000));
        o.resolved_at = Some(at(1_000_000));
    }
    assert_eq!(bytes(&estimate_with(perturbed)), bytes(&reference));

    // Landed before `as_of` but only *known* afterwards: a lower bound at
    // `as_of`, so its true landing time must not matter.
    let mut late_known = obs("late", Stage::ReviewWait, -3_600, Some(600));
    late_known.resolved_at = Some(at(500));
    let mut late_known_b = late_known.clone();
    late_known_b.actual_at = Some(at(-60));
    let (mut with_a, mut with_b) = (base.clone(), base.clone());
    with_a.push(late_known);
    with_b.push(late_known_b);
    assert_eq!(bytes(&estimate_with(with_a)), bytes(&estimate_with(with_b)));

    // Estimates made at or after `as_of` are never evidence either.
    let mut extra = base.clone();
    extra.extend((0..40).map(|i| {
        let mut o = obs(&format!("future{i}"), Stage::ReviewWait, 10 + i, Some(500_000));
        o.resolved_at = Some(at(600_000));
        o
    }));
    assert_eq!(bytes(&estimate_with(extra)), bytes(&reference));
}

#[test]
fn even_lark_right_censored_estimates_above_the_base_p90_raise_the_adjusted_quantiles() {
    let landed = landings(Stage::ReviewWait, 60, 600, 20_000, "l");
    let without = estimate_with(landed.clone());
    // Open estimates, long past the base p90 (elapsed ≫ 14 400 s).
    let mut with = landed;
    with.extend((0..30).map(|i| obs(&format!("o{i}"), Stage::ReviewWait, -200_000 + i, None)));
    let with = estimate_with(with);
    let (a, b) = (without.calibration.clone().unwrap(), with.calibration.clone().unwrap());
    assert_eq!((a.n_censored, b.n_censored), (0, 30));
    assert!(b.raw_shift.p90 > a.raw_shift.p90, "{b:?} vs {a:?}");
    assert!(b.raw_shift.p75 >= a.raw_shift.p75);
    let p90 = |e: crate::eta::Explanation| e.result.unwrap().p90_sec.unwrap();
    assert!(p90(with) > p90(without));
}

#[test]
fn even_lark_hits_each_rate_adds_its_shift_and_recomputes_from_itself() {
    // Base far too narrow: actual remaining spread over [600, 60 000] s.
    let set = landings(Stage::ReviewWait, 200, 600, 60_000, "h");
    let e = estimate_with(set.clone());
    let record = e.calibration.as_ref().expect("calibrated");
    assert_eq!(record.method, conformal::METHOD_SECONDS);
    assert_eq!(record.base, LAND_V2);
    assert_eq!(record.level, "stage_age");
    assert_eq!((record.n_events, record.n_censored), (200, 0));
    assert_eq!(record.max_daily_step, Some(conformal::MAX_DAILY_STEP_SEC));
    // The raw shift is in seconds and holds each rate on the calibration set.
    let hit = |q: i64, shift: f64| {
        let adjusted = (q as f64 + shift).round();
        set.iter()
            .filter(|o| ((o.actual_at.unwrap() - o.as_of).num_seconds() as f64) <= adjusted)
            .count() as f64
            / set.len() as f64
    };
    let raw = record.raw_shift.to_array();
    for (k, (q, tau)) in [(BASE.0, 0.25), (BASE.1, 0.5), (BASE.2, 0.75), (BASE.3, 0.9)]
        .into_iter()
        .enumerate()
    {
        let rate = hit(q, raw[k]);
        assert!((rate - tau).abs() <= 0.05, "tau {tau}: hit {rate}");
    }
    // The result is the base plus the applied shift, exactly.
    let base_q = record.base_quantiles_sec;
    let expected = apply_seconds((base_q.p25, base_q.p50, base_q.p75, base_q.p90), &record.shift);
    assert_eq!(e.quantiles_with_p90(), Some(expected));
    // And it recomputes from the explanation alone, also after a round trip.
    assert_eq!(run_explanation(&e), e.quantiles_with_p90());
    let parsed: crate::eta::Explanation = serde_json::from_str(&bytes(&e)).unwrap();
    assert_eq!(run_explanation(&parsed), e.quantiles_with_p90());
    let (p25, p50, p75, p90) = e.quantiles_with_p90().unwrap();
    assert!(p25 <= p50 && p50 <= p75 && p75 <= p90);
}

#[test]
fn even_lark_adds_time_where_the_log_scale_multiplies_it() {
    // Same evidence, same base path: the log scale moves each quantile by a
    // factor, the seconds scale by an amount. On a base whose quantiles are
    // proportional the two hit the same rates on the evidence, so the point
    // is the record: seconds for one, ln units for the other.
    let set = landings(Stage::ReviewWait, 120, 600, 60_000, "s");
    let mut history = history_a();
    history.calibration = set;
    let input = input_at(Stage::ReviewWait, 0, 0);
    let (log, seconds) =
        (log_calibrated(&input, &history), LandEvenLark.estimate(&input, &history));
    let (log, seconds) = (log.calibration.unwrap(), seconds.calibration.unwrap());
    assert_eq!(log.method, conformal::METHOD);
    assert_eq!(seconds.method, conformal::METHOD_SECONDS);
    // p90 needs ~54 000 s on a 14 400 s base: +~40 000 s, or ×~3.7 (ln ≈ 1.3).
    assert!(seconds.raw_shift.p90 > 30_000.0, "{seconds:?}");
    assert!(log.raw_shift.p90 < 2.0, "{log:?}");
}

/// The calibration of the same base estimate made `day` days after the
/// fixture instant, against the same evidence.
fn calibration_on(
    observations: &[CalibrationObservation],
    day: i64,
) -> Option<conformal::Calibration> {
    let mut bare = history_a();
    bare.calibration.clear();
    let mut base = LandEvenLark.estimate(&input_at(Stage::ReviewWait, 0, 0), &bare);
    assert!(base.calibration.is_none());
    base.as_of += Duration::days(day);
    conformal::calibrate_on(Scale::Seconds, base, observations, LAND_V2).calibration
}

#[test]
fn even_lark_shift_moves_at_most_two_hours_between_days_as_a_shock_enters_and_leaves() {
    // Six weeks of landings around the base, one every three hours, plus a
    // shock on day -12: 60 estimates that each ran ~70 000 s. The applied
    // shift must ramp up and back down by at most MAX_DAILY_STEP_SEC a day.
    let mut set: Vec<CalibrationObservation> = (0..336)
        .map(|i| {
            let remaining = 1_200 + 900 * (i % 21);
            obs(&format!("calm{i}"), Stage::ReviewWait, -42 * DAY + 10_800 * i, Some(remaining))
        })
        .collect();
    set.extend(
        (0..60)
            .map(|i| obs(&format!("bad{i}"), Stage::ReviewWait, -12 * DAY + 60 * i, Some(70_000))),
    );
    let records: Vec<(i64, conformal::Calibration)> = (-14..=6)
        .filter_map(|d| calibration_on(&set, d).map(|c| (d, c)))
        .collect();
    assert!(records.len() > 15, "calibrated on most days");
    for pair in records.windows(2) {
        let ((d0, a), (d1, b)) = (&pair[0], &pair[1]);
        if d1 - d0 != 1 {
            continue;
        }
        for (x, y) in a.shift.to_array().into_iter().zip(b.shift.to_array()) {
            assert!(
                (y - x).abs() <= conformal::MAX_DAILY_STEP_SEC + 1e-6,
                "day {d0} -> {d1}: applied shift {x} -> {y}"
            );
        }
    }
    assert!(
        records
            .iter()
            .any(|(_, c)| c.raw_shift.p90 - c.shift.p90 > conformal::MAX_DAILY_STEP_SEC),
        "the limit never bound"
    );
}

#[test]
fn too_little_evidence_leaves_even_lark_at_its_base() {
    let few = landings(Stage::ReviewWait, conformal::MIN_CELL_EVENTS - 1, 600, 60_000, "few");
    let e = estimate_with(few);
    assert!(e.calibration.is_none());
    let mut bare = history_a();
    bare.calibration.clear();
    let input = input_at(Stage::ReviewWait, 0, 0);
    assert_eq!(
        LandEvenLark.estimate(&input, &bare).quantiles_with_p90(),
        e.quantiles_with_p90()
    );
}

#[test]
fn apply_seconds_is_monotone_floored_and_even_lark_is_a_shadow() {
    let wild = Q4 {
        p25: 5_000.0,
        p50: -10_000.0,
        p75: 0.0,
        p90: -30_000.0,
    };
    let (a, b, c, d) = apply_seconds(BASE, &wild);
    assert!(0 <= a && a <= b && b <= c && c <= d, "{a} {b} {c} {d}");
    assert_eq!(apply_seconds(BASE, &Q4::from_array([0.0; 4])), BASE);

    let registry = Registry::builtin();
    assert!(registry
        .for_kind(Kind::Land)
        .any(|h| h.id() == LAND_EVEN_LARK));
    assert_eq!(registry.current(Kind::Land, None).id(), "land-v1");
    assert_eq!(Registry::default_current(Kind::Land), "land-v1");
}
