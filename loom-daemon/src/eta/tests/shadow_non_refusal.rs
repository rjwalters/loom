//! The live non-refusal check a primary flip must pass (#10949): of the
//! tracker passes in the last 24 h where `current` answered, the candidate
//! must have answered at least 95%.

use super::as_of;
use super::shadow::{comparison, ledger_with, BACKTEST_CASES, PASSING_PAIRS};
use crate::eta::heuristics::{LAND_V1, LAND_V2};
use crate::eta::shadow::{self, GateStatus, PromotionDecision, ShadowLedger};
use crate::eta::shadow_non_refusal::{self as non_refusal, MAX_HOURS, MIN_PASSES, MIN_RATE};
use crate::eta::tracker::PassAnswers;
use crate::eta::Kind;
use chrono::{DateTime, Duration, Utc};

fn current_land(_: Kind) -> String {
    LAND_V1.to_string()
}

fn passes(n: usize, current: bool, candidate: bool) -> Vec<PassAnswers> {
    let pass = PassAnswers {
        kind: Kind::Land,
        states: vec![
            (LAND_V1.to_string(), current),
            (LAND_V2.to_string(), candidate),
        ],
    };
    vec![pass; n]
}

/// `answered` of `n` passes the candidate answered, all with `current`
/// answering, recorded at `at`.
fn record(ledger: &mut ShadowLedger, at: DateTime<Utc>, n: usize, answered: usize) {
    ledger.record_answer_hours(&current_land, &passes(answered, true, true), at);
    ledger.record_answer_hours(&current_land, &passes(n - answered, true, false), at);
}

/// A ledger whose comparison has been watched for `hours` already: one
/// answered pass that long ago, so later buckets sit inside a matured span.
fn matured(now: DateTime<Utc>, hours: i64) -> ShadowLedger {
    let mut ledger = ShadowLedger::default();
    record(&mut ledger, now - Duration::hours(hours), 1, 1);
    ledger
}

fn check(ledger: &ShadowLedger, now: DateTime<Utc>) -> non_refusal::NonRefusal {
    ledger.non_refusal(Kind::Land, LAND_V1, LAND_V2, now)
}

#[test]
fn passes_at_95_percent_and_fails_just_below() {
    let now = as_of();
    let mut ledger = matured(now, 30);
    record(&mut ledger, now - Duration::hours(2), 100, 95);
    let c = check(&ledger, now);
    assert_eq!(c.status, GateStatus::Passed, "{}", c.detail);
    assert_eq!((c.current_answered, c.candidate_answered), (100, 95));
    assert_eq!(c.rate, Some(0.95));
    assert!((c.min_rate - MIN_RATE).abs() < f64::EPSILON);

    let mut ledger = matured(now, 30);
    record(&mut ledger, now - Duration::hours(2), 100, 94);
    let c = check(&ledger, now);
    assert_eq!(c.status, GateStatus::Failed);
    assert!(c.detail.contains("94.0%"), "{}", c.detail);
}

#[test]
fn passes_where_current_refused_do_not_count_against_the_candidate() {
    // The fitted family's 2026-10-08 failure mode is the candidate refusing
    // where current answers. An item neither can estimate is not that.
    let now = as_of();
    let mut ledger = matured(now, 30);
    record(&mut ledger, now - Duration::hours(1), MIN_PASSES, MIN_PASSES);
    ledger.record_answer_hours(&current_land, &passes(500, false, false), now);
    let c = check(&ledger, now);
    assert_eq!(c.status, GateStatus::Passed, "{}", c.detail);
    assert_eq!(c.current_answered, MIN_PASSES);
}

#[test]
fn only_the_trailing_24_hours_count() {
    // Weeks of `no_model` before a fit was published must not drown the
    // day after it was: the window is recent by construction.
    let now = as_of();
    let mut ledger = ShadowLedger::default();
    record(&mut ledger, now - Duration::hours(30), 1_000, 0);
    record(&mut ledger, now - Duration::hours(3), 200, 200);
    let c = check(&ledger, now);
    assert_eq!(c.status, GateStatus::Passed, "{}", c.detail);
    assert_eq!(c.current_answered, 200);
    assert!(c.since <= now - Duration::hours(24));
    assert!(c.since > now - Duration::hours(25));
    assert_eq!(c.until, now);
}

#[test]
fn a_fresh_ledger_with_enough_passes_is_refused_until_a_day_is_observed() {
    let now = as_of();
    let mut ledger = ShadowLedger::default();
    record(&mut ledger, now, MIN_PASSES, MIN_PASSES);
    let c = check(&ledger, now);
    assert_eq!(c.status, GateStatus::Failed);
    assert_eq!(c.observed_hours, Some(0));
    assert!(c.detail.contains("observed for 0 h"), "{}", c.detail);

    // 23 h watched is still short; 24 h is enough.
    let mut ledger = matured(now, 23);
    record(&mut ledger, now, MIN_PASSES, MIN_PASSES);
    assert_eq!(check(&ledger, now).status, GateStatus::Failed);
    let mut ledger = matured(now, 24);
    record(&mut ledger, now, MIN_PASSES, MIN_PASSES);
    let c = check(&ledger, now);
    assert_eq!(c.status, GateStatus::Passed, "{}", c.detail);
    assert_eq!(c.observed_hours, Some(24));
    assert!(c.observed_since.is_some());
}

#[test]
fn a_legacy_ledger_without_buckets_has_no_observation_start() {
    let c = check(&ShadowLedger::default(), as_of());
    assert_eq!((c.observed_since, c.observed_hours), (None, None));
    assert_eq!(c.status, GateStatus::Failed);
}

#[test]
fn too_few_passes_fail_closed_and_an_empty_ledger_has_none() {
    let now = as_of();
    let c = check(&ShadowLedger::default(), now);
    assert_eq!(c.status, GateStatus::Failed);
    assert_eq!(c.rate, None);

    let mut ledger = matured(now, 30);
    record(&mut ledger, now, MIN_PASSES - 1, MIN_PASSES - 1);
    let c = check(&ledger, now);
    assert_eq!(c.status, GateStatus::Failed);
    assert!(c.detail.contains("required"), "{}", c.detail);
}

#[test]
fn hourly_buckets_are_capped_and_cleared_with_the_comparison() {
    let now = as_of();
    let mut ledger = ShadowLedger::default();
    for h in 0..(MAX_HOURS as i64 + 10) {
        record(&mut ledger, now - Duration::hours(h), 1, 1);
    }
    let key = format!("land|{LAND_V1}|{LAND_V2}");
    assert_eq!(ledger.answer_hours[&key].len(), MAX_HOURS);

    let stats = ledger.stats(Kind::Land, LAND_V1, LAND_V2);
    ledger.clear(&stats.key);
    assert!(!ledger.answer_hours.contains_key(&key));
}

#[test]
fn a_ledger_written_before_the_hours_existed_still_parses() {
    let old = r#"{"pairs":{},"keys":{},"days":{},"items":{}}"#;
    let ledger: ShadowLedger = serde_json::from_str(old).unwrap();
    assert!(ledger.answer_hours.is_empty());
    let round: ShadowLedger =
        serde_json::from_str(&serde_json::to_string(&ledger).unwrap()).unwrap();
    assert_eq!(round, ledger);
}

/// A decision both gates pass.
fn passing_decision() -> PromotionDecision {
    let ledger = ledger_with(PASSING_PAIRS, 100.0, 60.0, PASSING_PAIRS / 2);
    let stats = ledger.stats(Kind::Land, LAND_V1, LAND_V2);
    let backtest = comparison(1000.0, 800.0, BACKTEST_CASES);
    shadow::evaluate(Kind::Land, LAND_V1, LAND_V2, Some(&backtest), &stats, as_of())
}

#[test]
fn a_failed_check_refuses_a_flip_both_gates_passed() {
    let mut decision = passing_decision();
    assert!(decision.promote, "{}", decision.reason);
    let failed = check(&ShadowLedger::default(), as_of());
    non_refusal::apply(&mut decision, failed);
    assert!(!decision.promote);
    assert!(
        decision.reason.starts_with("live non-refusal check failed"),
        "{}",
        decision.reason
    );
    assert!(decision.non_refusal.is_some());
}

#[test]
fn a_passed_check_is_recorded_and_changes_nothing() {
    let now = as_of();
    let mut ledger = matured(now, 30);
    record(&mut ledger, now, MIN_PASSES, MIN_PASSES);
    let mut decision = passing_decision();
    let reason = decision.reason.clone();
    non_refusal::apply(&mut decision, check(&ledger, now));
    assert!(decision.promote);
    assert_eq!(decision.reason, reason);

    // The record carries the check and the evidence link through the log.
    decision.evidence = Some("https://example.invalid/evidence".to_string());
    let line = serde_json::to_string(&decision).unwrap();
    let back: PromotionDecision = serde_json::from_str(&line).unwrap();
    assert_eq!(back, decision);
    assert!(line.contains("\"non_refusal\""));
    assert!(line.contains("\"evidence\""));
}
