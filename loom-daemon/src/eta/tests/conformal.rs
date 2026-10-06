//! Conformal calibration wrapper (#10489): the point-in-time leak test,
//! censoring, per-quantile hit rates, the rate limit, the explanation
//! record, and the shadow registration.

use super::{as_of, history_a, input_at};
use crate::eta::conformal::{self, apply, Q4};
use crate::eta::heuristics::{LandCalmPlover, LAND_CALM_PLOVER, LAND_V2};
use crate::eta::recalibrate::{CalibrationObservation, OBSERVATION_SCHEMA};
use crate::eta::simulate::run_explanation;
use crate::eta::{Heuristic, Kind, Registry, Stage};
use chrono::{DateTime, Duration, Utc};

fn at(secs: i64) -> DateTime<Utc> {
    as_of() + Duration::seconds(secs)
}

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

/// `n` landings whose `ln(actual / p50)` is spread evenly over `[lo, hi]`,
/// made over the past ten days and resolved before the fixture instant.
fn landings(stage: Stage, n: usize, lo: f64, hi: f64, tag: &str) -> Vec<CalibrationObservation> {
    (0..n)
        .map(|i| {
            let z = lo + (hi - lo) * i as f64 / (n - 1) as f64;
            let remaining = (BASE.1 as f64 * z.exp()).round() as i64;
            let made = -86_400 * (1 + (i % 10) as i64) - 60 * i as i64 - 50_000;
            obs(&format!("{tag}-{i}"), stage, made, Some(remaining))
        })
        .collect()
}

fn estimate_with(observations: Vec<CalibrationObservation>) -> crate::eta::Explanation {
    let mut history = history_a();
    history.calibration = observations;
    LandCalmPlover.estimate(&input_at(Stage::ReviewWait, 0, 0), &history)
}

fn bytes(e: &crate::eta::Explanation) -> String {
    serde_json::to_string(e).unwrap()
}

#[test]
fn calm_plover_leak_perturbing_post_as_of_outcomes_is_bit_identical() {
    let mut base = landings(Stage::ReviewWait, 80, -0.5, 2.5, "a");
    // Still open at the fit instant.
    base.extend((0..10).map(|i| obs(&format!("o{i}"), Stage::ReviewWait, -7_200 - i, None)));
    let reference = estimate_with(base.clone());
    assert!(reference.calibration.is_some(), "calibrated");

    let mut perturbed = base.clone();
    for o in &mut perturbed {
        // Resolve (or move a resolution) at or after `as_of`, wildly.
        if o.actual_at.is_none() {
            o.actual_at = Some(at(1_000_000));
            o.resolved_at = Some(at(1_000_000));
        }
    }
    // An outcome landed before `as_of` but only *known* afterwards is just a
    // lower bound at `as_of`: its true landing time must not matter.
    let mut late_known = obs("late", Stage::ReviewWait, -3_600, Some(600));
    late_known.resolved_at = Some(at(500));
    let mut late_known_b = late_known.clone();
    late_known_b.actual_at = Some(at(-60));
    let mut with_a = base.clone();
    with_a.push(late_known);
    let mut with_b = base.clone();
    with_b.push(late_known_b);
    assert_eq!(bytes(&estimate_with(with_a)), bytes(&estimate_with(with_b)));
    assert_eq!(bytes(&estimate_with(perturbed)), bytes(&reference));

    // Estimates made at or after `as_of` are never evidence either.
    let mut extra = base.clone();
    extra.extend(
        landings(Stage::ReviewWait, 40, 3.0, 5.0, "future")
            .into_iter()
            .map(|mut o| {
                o.as_of = at(10 + o.as_of.timestamp() % 100);
                o.actual_at = Some(at(100_000));
                o.resolved_at = Some(at(100_000));
                o
            }),
    );
    assert_eq!(bytes(&estimate_with(extra)), bytes(&reference));
}

#[test]
fn right_censored_estimates_above_the_base_p90_raise_the_adjusted_quantiles() {
    let landed = landings(Stage::ReviewWait, 60, -0.5, 1.5, "l");
    let without = estimate_with(landed.clone());
    // Open estimates, long past the base p90 (elapsed ≫ 14 400 s).
    let mut with = landed;
    with.extend((0..30).map(|i| obs(&format!("o{i}"), Stage::ReviewWait, -200_000 + i, None)));
    let with = estimate_with(with);
    let (a, b) = (without.calibration.unwrap(), with.calibration.clone().unwrap());
    assert_eq!(b.n_censored, 30);
    assert_eq!(a.n_censored, 0);
    assert!(b.raw_shift.p90 > a.raw_shift.p90, "{b:?} vs {a:?}");
    assert!(b.raw_shift.p75 >= a.raw_shift.p75);
    let (p90_without, p90_with) =
        (without.result.unwrap().p90_sec.unwrap(), with.result.unwrap().p90_sec.unwrap());
    assert!(p90_with > p90_without);
}

#[test]
fn an_unresolved_censored_tail_reads_its_bounds_not_its_largest_outlier() {
    // 60 landings, all at or below the base p90 · e^0.11, and 30 estimates
    // still open past every landing: 29 open 20 000 s (score
    // ln(20 000 / 14 400) ≈ 0.33) and one open 1 000 000 s (≈ 4.24). The
    // events alone leave a third of the mass unresolved, so the p90 is read
    // off the bounds: the 29 common ones, not the single outlier (#10489).
    let mut set = landings(Stage::ReviewWait, 60, -0.5, 1.5, "l");
    set.extend((0..29).map(|i| obs(&format!("o{i}"), Stage::ReviewWait, -20_000 - i, None)));
    set.push(obs("outlier", Stage::ReviewWait, -1_000_000, None));
    let record = estimate_with(set).calibration.expect("calibrated");
    assert_eq!(record.n_censored, 30);
    let common = (20_000.0_f64 / BASE.3 as f64).ln();
    let outlier = (1_000_000.0_f64 / BASE.3 as f64).ln();
    assert!(record.raw_shift.p90 >= common - 0.01, "{record:?}");
    assert!(record.raw_shift.p90 < outlier - 1.0, "{record:?}");
}

#[test]
fn each_quantile_hits_its_own_rate_on_the_calibration_set() {
    // Base far too narrow: actual is 3600·exp(z), z in [-0.5, 2.5].
    let set = landings(Stage::ReviewWait, 200, -0.5, 2.5, "h");
    let e = estimate_with(set.clone());
    let record = e.calibration.as_ref().expect("calibrated");
    assert_eq!(record.level, "stage_age");
    assert_eq!(record.age_bucket.as_deref(), Some("lt_1h"));
    assert_eq!(record.base, LAND_V2);
    assert_eq!((record.n_events, record.n_censored), (200, 0));
    assert_eq!(record.window.days, conformal::WINDOW_DAYS);
    // Use the raw (un-rate-limited) shift: the hit rate is a property of it.
    let hit = |q: i64, shift: f64| {
        let adjusted = (q as f64 * shift.exp()).round();
        set.iter()
            .filter(|o| ((o.actual_at.unwrap() - o.as_of).num_seconds() as f64) <= adjusted)
            .count() as f64
            / set.len() as f64
    };
    for (q, shift, tau) in [
        (BASE.0, record.raw_shift.p25, 0.25),
        (BASE.1, record.raw_shift.p50, 0.50),
        (BASE.2, record.raw_shift.p75, 0.75),
        (BASE.3, record.raw_shift.p90, 0.90),
    ] {
        let rate = hit(q, shift);
        assert!((rate - tau).abs() <= 0.05, "tau {tau}: hit {rate}");
    }
    // The explanation recomputes from itself.
    assert_eq!(run_explanation(&e), e.quantiles_with_p90());
    let parsed: crate::eta::Explanation = serde_json::from_str(&bytes(&e)).unwrap();
    assert_eq!(run_explanation(&parsed), e.quantiles_with_p90());
    let (p25, p50, p75, p90) = e.quantiles_with_p90().unwrap();
    assert!(p25 <= p50 && p50 <= p75 && p75 <= p90);
}

const DAY: i64 = 86_400;

/// The calibration record of the same base estimate made `day` days after
/// the fixture instant, against the same `observations` — only `as_of`
/// moves, so any change is the evidence that became visible in that day.
fn calibration_on(
    observations: &[CalibrationObservation],
    day: i64,
) -> Option<conformal::Calibration> {
    let mut bare = history_a();
    bare.calibration.clear();
    let mut base = LandCalmPlover.estimate(&input_at(Stage::ReviewWait, 0, 0), &bare);
    assert!(base.calibration.is_none());
    base.as_of += Duration::days(day);
    conformal::calibrate(base, observations, LAND_V2).calibration
}

/// Every pair of evaluations one day apart in `days` (same evidence) moves
/// each applied shift by at most [`conformal::MAX_DAILY_STEP`]; returns the
/// records for further assertions.
fn assert_daily_bound(
    observations: &[CalibrationObservation],
    days: std::ops::RangeInclusive<i64>,
) -> Vec<(i64, conformal::Calibration)> {
    let records: Vec<(i64, conformal::Calibration)> = days
        .filter_map(|d| calibration_on(observations, d).map(|c| (d, c)))
        .collect();
    for pair in records.windows(2) {
        let ((d0, a), (d1, b)) = (&pair[0], &pair[1]);
        if d1 - d0 != 1 {
            continue;
        }
        for (x, y) in a.shift.to_array().into_iter().zip(b.shift.to_array()) {
            assert!(
                (y - x).abs() <= conformal::MAX_DAILY_STEP + 1e-6,
                "day {d0} -> {d1}: applied shift {x} -> {y} (raw {:?} -> {:?})",
                a.raw_shift,
                b.raw_shift
            );
        }
    }
    records
}

#[test]
fn the_applied_shift_moves_at_most_one_step_between_days_when_a_cohort_leaves_the_window() {
    // The Judge's reproduction on PR #10497: a large calm cohort at day
    // -20.5 and a small 20x-late cohort at day -9. The calm cohort leaves the
    // 14-day window between days -7 and -6, so the raw p50 shift jumps from
    // ~0 to ~3 and stays there; a replay anchored a fixed number of days
    // back re-seeded from that jumped raw fit the next day.
    let late = (3_600.0 * 3.0_f64.exp()).round() as i64;
    let mut set: Vec<CalibrationObservation> = (0..1_000)
        .map(|i| obs(&format!("calm{i}"), Stage::ReviewWait, -20 * DAY - DAY / 2, Some(3_600)))
        .collect();
    set.extend((0..100).map(|i| obs(&format!("late{i}"), Stage::ReviewWait, -9 * DAY, Some(late))));
    let records = assert_daily_bound(&set, -12..=4);
    let today = &records
        .iter()
        .find(|(d, _)| *d == 0)
        .expect("calibrated today")
        .1;
    assert!(today.raw_shift.p50 > 2.9, "{today:?}");
    assert!(today.shift.p50 < today.raw_shift.p50, "limited: {today:?}");
    // It does converge, one step a day.
    let last = &records.last().unwrap().1;
    assert!(last.shift.p50 > today.shift.p50);
}

#[test]
fn the_applied_shift_moves_at_most_one_step_between_days_as_a_shock_enters_and_leaves() {
    // Six weeks of well-calibrated landings, one every three hours, plus a
    // shock: 60 estimates on day -12 that all ran ~20x late. The shock
    // enters the window, then leaves it (and leaves any fixed seven-day
    // replay horizon) while the calm evidence stays: the applied shift must
    // ramp up and back down one step a day, not jump when an anchor moves.
    let mut set: Vec<CalibrationObservation> = (0..336)
        .map(|i| {
            let z = -1.0 + 2.0 * (i % 21) as f64 / 20.0;
            let remaining = (BASE.1 as f64 * z.exp()).round() as i64;
            obs(&format!("calm{i}"), Stage::ReviewWait, -42 * DAY + 10_800 * i, Some(remaining))
        })
        .collect();
    set.extend(
        (0..60)
            .map(|i| obs(&format!("bad{i}"), Stage::ReviewWait, -12 * DAY + 60 * i, Some(70_000))),
    );
    let records = assert_daily_bound(&set, -14..=6);
    // The shock was visible and limited...
    assert!(
        records
            .iter()
            .any(|(_, c)| c.raw_shift.p90 - c.shift.p90 > conformal::MAX_DAILY_STEP),
        "the limit never bound"
    );
    // ...and after it left the window the applied shift is still walking
    // back down rather than snapping to the raw fit.
    let (_, after) = records.iter().find(|(d, _)| *d == 3).expect("calibrated");
    assert!(after.shift.p90 > after.raw_shift.p90 + 1e-6, "{after:?}");
}

#[test]
fn the_applied_shift_is_a_function_of_the_evidence_not_of_the_replay_length() {
    // Two evaluations one day apart agree on everything up to the earlier
    // one: the later is exactly one clamped step from it.
    let mut set = landings(Stage::ReviewWait, 120, -1.0, 1.0, "calm");
    set.extend(
        (0..60).map(|i| obs(&format!("bad{i}"), Stage::ReviewWait, -80_000 - 10 * i, Some(70_000))),
    );
    let (today, tomorrow) = (calibration_on(&set, 0).unwrap(), calibration_on(&set, 1).unwrap());
    for ((prev, raw), next) in today
        .shift
        .to_array()
        .into_iter()
        .zip(tomorrow.raw_shift.to_array())
        .zip(tomorrow.shift.to_array())
    {
        let expected =
            raw.clamp(prev - conformal::MAX_DAILY_STEP, prev + conformal::MAX_DAILY_STEP);
        assert!((next - expected).abs() <= 1e-6, "{prev} {raw} {next}");
    }
}

#[test]
fn too_little_evidence_leaves_the_base_estimate_unchanged() {
    let few = landings(Stage::ReviewWait, conformal::MIN_CELL_EVENTS - 1, -0.5, 2.5, "few");
    let e = estimate_with(few);
    assert!(e.calibration.is_none());
    let mut bare = history_a();
    bare.calibration.clear();
    let input = input_at(Stage::ReviewWait, 0, 0);
    assert_eq!(
        LandCalmPlover.estimate(&input, &bare).quantiles_with_p90(),
        e.quantiles_with_p90()
    );
    // Rows persisted before the base quantiles were logged are no evidence.
    let mut old = landings(Stage::ReviewWait, 80, -0.5, 2.5, "old");
    for o in &mut old {
        o.p25_sec = None;
        o.p75_sec = None;
        o.p90_sec = None;
        o.age_sec = None;
    }
    assert!(estimate_with(old).calibration.is_none());
}

#[test]
fn a_thin_age_bucket_falls_back_to_the_stage_then_the_pool() {
    let mut set = landings(Stage::ReviewWait, 60, -0.5, 2.5, "s");
    for o in &mut set {
        o.age_sec = Some(5 * 3_600); // 4h-24h bucket
    }
    let e = estimate_with(set.clone());
    assert_eq!(e.calibration.as_ref().unwrap().level, "stage");
    // Another stage's landings only: the pooled level.
    let pooled = estimate_with(landings(Stage::Doctor, 60, -0.5, 2.5, "d"));
    assert_eq!(pooled.calibration.unwrap().level, "pooled");
}

#[test]
fn apply_is_monotone_and_the_wrapper_is_shadow_registered() {
    let wild = Q4 {
        p25: 2.0,
        p50: -1.0,
        p75: 0.0,
        p90: -3.0,
    };
    let (a, b, c, d) = apply(BASE, &wild);
    assert!(a <= b && b <= c && c <= d, "{a} {b} {c} {d}");

    let registry = Registry::builtin();
    assert!(registry
        .for_kind(Kind::Land)
        .any(|h| h.id() == LAND_CALM_PLOVER));
    assert_eq!(Registry::default_current(Kind::Land), "land-v1");
}
