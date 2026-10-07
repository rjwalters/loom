//! Promotion statistics (#10525): the item-clustered primary test, the day
//! consistency check and the adaptation check, on fixed fixtures and the
//! fixed bootstrap seed.

use super::as_of;
use super::shadow::{comparison, ledger_with, BACKTEST_CASES, PASSING_PAIRS};
use crate::eta::heuristics::{LAND_V1, LAND_V2};
use crate::eta::offline::evaluate::{Estimate, IssueSums};
use crate::eta::shadow::{self, DayWins, PromotionDecision};
use crate::eta::shadow_stats::{
    adaptation_check, apply_adaptation, day_consistency, item_test, item_test_of, AdaptationStatus,
    AdaptationTimes, INDEPENDENCE_UNIT, MIN_DISTINCT_ITEMS,
};
use crate::eta::Kind;

/// `n` items; item `i` has `refreshes` observations of difference `delta(i)`.
fn items(n: usize, refreshes: usize, delta: impl Fn(usize) -> f64) -> IssueSums {
    (0..n)
        .map(|i| (format!("o/r#{i}"), (delta(i) * refreshes as f64, refreshes)))
        .collect()
}

/// A small deterministic wobble so the interval has width.
fn wobble(i: usize) -> f64 {
    ((i * 37) % 11) as f64 - 5.0
}

#[test]
fn a_consistent_negative_difference_over_enough_items_passes() {
    let t = item_test(&items(120, 1, |i| -20.0 + wobble(i)));
    assert!(t.passed, "{}", t.detail);
    assert_eq!(t.distinct_items, 120);
    assert_eq!(t.unit, INDEPENDENCE_UNIT);
    assert!(t.sampling.contains("seed 0x10193"), "{}", t.sampling);
    let (lo, hi) = t.ci95.unwrap();
    assert!(lo < hi && hi < 0.0, "{lo} {hi}");
    // Reproducible: the seed is fixed.
    assert_eq!(t, item_test(&items(120, 1, |i| -20.0 + wobble(i))));
}

#[test]
fn a_positive_or_straddling_difference_fails() {
    let worse = item_test(&items(120, 1, |i| 20.0 + wobble(i)));
    assert!(!worse.passed);
    assert!(worse.detail.contains("does not exclude 0"), "{}", worse.detail);
    let straddle = item_test(&items(120, 1, |i| if i % 2 == 0 { 30.0 } else { -31.0 }));
    assert!(!straddle.passed, "{}", straddle.detail);
}

#[test]
fn the_distinct_item_floor_is_inclusive_and_binds() {
    let below = item_test(&items(MIN_DISTINCT_ITEMS - 1, 1, |i| -20.0 + wobble(i)));
    assert!(!below.passed);
    assert!(below.detail.contains("99 distinct item(s)"), "{}", below.detail);
    let at = item_test(&items(MIN_DISTINCT_ITEMS, 1, |i| -20.0 + wobble(i)));
    assert!(at.passed, "{}", at.detail);
}

#[test]
fn repeated_refreshes_add_no_items_and_no_confidence() {
    let once = item_test(&items(120, 1, |i| -20.0 + wobble(i)));
    let thirty = item_test(&items(120, 30, |i| -20.0 + wobble(i)));
    assert_eq!(thirty.distinct_items, once.distinct_items);
    assert_eq!(thirty.observations, 30 * once.observations);
    assert_eq!(thirty.ci95, once.ci95, "uniform refreshes leave the interval unchanged");
    assert_eq!(thirty.mean_delta_pinball4_sec, once.mean_delta_pinball4_sec);

    // One item refreshed a thousand times is still one item.
    let one = item_test(&items(1, 1_000, |_| -500.0));
    assert!(!one.passed);
    assert_eq!(one.distinct_items, 1);
    assert_eq!(one.observations, 1_000);
}

#[test]
fn one_heavily_refreshed_correlated_item_cannot_carry_the_test() {
    // 99 items where the candidate is a little worse, and one item refreshed
    // 5,000 times where it is far better. The pooled point estimate favours
    // the candidate, but resampling whole items omits that one item in about
    // a third of draws, so the interval cannot exclude 0.
    let mut sums = items(99, 1, |_| 5.0);
    sums.insert("o/r#hot".into(), (-100.0 * 5_000.0, 5_000));
    let t = item_test(&sums);
    assert!(t.mean_delta_pinball4_sec.unwrap() < 0.0);
    assert!(!t.passed, "{}", t.detail);
}

#[test]
fn missing_or_nonfinite_evidence_fails() {
    let empty = item_test(&IssueSums::new());
    assert!(!empty.passed);
    assert_eq!(empty.distinct_items, 0);
    let none = item_test_of(None, 500, false);
    assert!(!none.passed);
    let nan = item_test_of(
        Some(Estimate {
            value: Some(f64::NAN),
            lo: Some(f64::NAN),
            hi: Some(f64::NAN),
            n: 500,
        }),
        500,
        false,
    );
    assert!(!nan.passed);
    assert!(nan.detail.contains("no finite"), "{}", nan.detail);
}

#[test]
fn a_flipped_estimate_is_read_from_the_candidates_side() {
    let e = Estimate {
        value: Some(5.0),
        lo: Some(2.0),
        hi: Some(8.0),
        n: 300,
    };
    let as_is = item_test_of(Some(e), 150, false);
    assert!(!as_is.passed);
    let flipped = item_test_of(Some(e), 150, true);
    assert!(flipped.passed, "{}", flipped.detail);
    assert_eq!(flipped.ci95, Some((-8.0, -2.0)));
}

#[test]
fn the_day_check_needs_seven_decided_days_and_a_strict_majority() {
    let days = |wins: usize, days: usize| DayWins {
        days,
        wins,
        ..DayWins::default()
    };
    assert!(day_consistency(&days(6, 6))
        .unwrap_err()
        .contains("6 decided day(s), 7 required"));
    assert!(day_consistency(&days(4, 7)).is_ok(), "4/7 is a majority");
    assert!(day_consistency(&days(4, 8))
        .unwrap_err()
        .contains("not a majority"));
}

#[test]
fn both_halves_record_the_item_test_on_the_decision() {
    let stats = ledger_with(PASSING_PAIRS, 100.0, 60.0, PASSING_PAIRS / 2).stats(
        Kind::Land,
        LAND_V1,
        LAND_V2,
    );
    let d = shadow::evaluate(
        Kind::Land,
        LAND_V1,
        LAND_V2,
        Some(&comparison(1000.0, 800.0, BACKTEST_CASES)),
        &stats,
        as_of(),
    );
    assert!(d.promote, "{}", d.reason);
    let backtest = d.backtest.item_test.as_ref().unwrap();
    assert_eq!(backtest.distinct_items, BACKTEST_CASES);
    assert_eq!(backtest.mean_delta_pinball4_sec, Some(-200.0));
    let live = d.live.stats.item_test.as_ref().unwrap();
    assert_eq!(live.distinct_items, PASSING_PAIRS);
    assert_eq!(live.mean_delta_pinball4_sec, Some(-40.0));
    assert_eq!(live.min_distinct_items, MIN_DISTINCT_ITEMS);
    let adaptation = d.adaptation.as_ref().unwrap();
    assert_eq!(adaptation.status, AdaptationStatus::NotMeasured);
    assert!(adaptation.detail.contains("#10528"), "{}", adaptation.detail);

    // The record round-trips, and one logged before these fields parses.
    let wire = serde_json::to_value(&d).unwrap();
    assert_eq!(serde_json::from_value::<PromotionDecision>(wire.clone()).unwrap(), d);
    let mut old = wire;
    old.as_object_mut().unwrap().remove("adaptation");
    old["backtest"].as_object_mut().unwrap().remove("item_test");
    old["live"]["stats"]
        .as_object_mut()
        .unwrap()
        .remove("item_test");
    let back: PromotionDecision = serde_json::from_value(old).unwrap();
    assert_eq!((back.adaptation, back.backtest.item_test), (None, None));
}

fn times(t_p50_sec: f64, t_cov_sec: f64) -> Option<AdaptationTimes> {
    Some(AdaptationTimes {
        t_p50_sec,
        t_cov_sec,
    })
}

#[test]
fn adaptation_is_not_gating_until_both_sides_are_measured() {
    for (current, candidate) in [
        (None, None),
        (times(3_600.0, 7_200.0), None),
        (None, times(3_600.0, 7_200.0)),
        (times(3_600.0, 7_200.0), times(f64::NAN, 7_200.0)),
    ] {
        assert_eq!(adaptation_check(current, candidate).status, AdaptationStatus::NotMeasured);
    }
}

#[test]
fn a_slower_adapting_candidate_is_refused_even_when_both_gates_pass() {
    let passing = || {
        let stats = ledger_with(PASSING_PAIRS, 100.0, 60.0, PASSING_PAIRS / 2).stats(
            Kind::Land,
            LAND_V1,
            LAND_V2,
        );
        shadow::evaluate(
            Kind::Land,
            LAND_V1,
            LAND_V2,
            Some(&comparison(1000.0, 800.0, BACKTEST_CASES)),
            &stats,
            as_of(),
        )
    };
    let mut slower = passing();
    let check = adaptation_check(times(3_600.0, 7_200.0), times(3_600.0, 9_000.0));
    assert_eq!(check.status, AdaptationStatus::Regressed);
    apply_adaptation(&mut slower, check);
    assert!(!slower.promote);
    assert!(slower.reason.contains("t_cov 9000s vs 7200s"), "{}", slower.reason);

    let mut same = passing();
    let check = adaptation_check(times(3_600.0, 7_200.0), times(3_000.0, 7_200.0));
    assert_eq!(check.status, AdaptationStatus::NoRegression);
    apply_adaptation(&mut same, check);
    assert!(same.promote, "{}", same.reason);
}
