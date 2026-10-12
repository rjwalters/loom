//! Coverage for the per-root hold clock (Issue #11564).

use std::path::PathBuf;

use chrono::{DateTime, Duration, TimeZone, Utc};

use super::{HaltClock, RootHalt};
use crate::work_finder::halt_cause::HaltCause;

fn t(minutes: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 9, 3, 5, 0).unwrap() + Duration::minutes(minutes)
}

fn root(name: &str) -> PathBuf {
    PathBuf::from(format!("/w/{name}"))
}

fn tick(clock: &mut HaltClock, at: DateTime<Utc>, roots: &[(PathBuf, bool, Option<HaltCause>)]) {
    clock.observe(roots.iter().map(|(r, held, cause)| (r, *held, *cause)), at);
}

fn halt(clock: &HaltClock, name: &str) -> Option<RootHalt> {
    clock
        .halts()
        .unwrap()
        .into_iter()
        .find(|h| h.root == root(name))
}

#[test]
fn no_halts_before_the_first_tick() {
    assert_eq!(HaltClock::new().halts(), None);
    let mut clock = HaltClock::new();
    tick(&mut clock, t(0), &[(root("a"), false, None)]);
    assert_eq!(clock.halts(), Some(Vec::new()));
}

#[test]
fn a_hold_seen_on_the_first_tick_is_a_lower_bound_and_keeps_its_start() {
    let mut clock = HaltClock::new();
    let held = [(root("a"), true, Some(HaltCause::InstallIncompatible))];
    tick(&mut clock, t(0), &held);
    tick(&mut clock, t(60), &held);
    let h = halt(&clock, "a").unwrap();
    assert_eq!(h.cause, Some(HaltCause::InstallIncompatible));
    assert_eq!(h.since, t(0));
    assert!(h.since_lower_bound);
}

#[test]
fn a_hold_that_starts_later_is_exact() {
    let mut clock = HaltClock::new();
    tick(&mut clock, t(0), &[(root("a"), false, None)]);
    tick(&mut clock, t(1), &[(root("a"), true, Some(HaltCause::MainRed))]);
    let h = halt(&clock, "a").unwrap();
    assert_eq!(h.since, t(1));
    assert!(!h.since_lower_bound);
}

#[test]
fn a_cause_change_restarts_since_and_clears_the_lower_bound() {
    let mut clock = HaltClock::new();
    tick(&mut clock, t(0), &[(root("a"), true, Some(HaltCause::MainRed))]);
    tick(&mut clock, t(5), &[(root("a"), true, Some(HaltCause::GatePending))]);
    let h = halt(&clock, "a").unwrap();
    assert_eq!(h.cause, Some(HaltCause::GatePending));
    assert_eq!(h.since, t(5));
    assert!(!h.since_lower_bound);
}

#[test]
fn a_cleared_or_vanished_root_is_dropped_and_a_new_hold_restarts() {
    let mut clock = HaltClock::new();
    let a = [
        (root("a"), true, Some(HaltCause::Drain)),
        (root("b"), true, None),
    ];
    tick(&mut clock, t(0), &a);
    // `a` clears, `b` leaves the root list.
    tick(&mut clock, t(1), &[(root("a"), false, None)]);
    assert_eq!(clock.halts(), Some(Vec::new()));
    tick(&mut clock, t(2), &[(root("a"), true, Some(HaltCause::Drain))]);
    let h = halt(&clock, "a").unwrap();
    assert_eq!(h.since, t(2));
    assert!(!h.since_lower_bound);
}

#[test]
fn a_causeless_hold_is_still_a_hold() {
    let mut clock = HaltClock::new();
    tick(&mut clock, t(0), &[(root("a"), true, None)]);
    let h = halt(&clock, "a").unwrap();
    assert_eq!(h.cause, None);
}
