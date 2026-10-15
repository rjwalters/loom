//! The tick deadline's gather walk (#12074): truncation, rotation, coverage.

use super::*;

/// Walk `n` repos from `start`, with the deadline passing once `budget` repos
/// have been gathered. Returns the gathered order and the walk.
fn walk(n: usize, start: usize, budget: usize) -> (Vec<usize>, GatherWalk) {
    let mut w = GatherWalk::new(n, start, Instant::now());
    let mut gathered = Vec::new();
    for idx in w.order().collect::<Vec<_>>() {
        let passed = gathered.len() >= budget;
        if w.admit(idx, passed) {
            gathered.push(idx);
        }
    }
    (gathered, w)
}

#[test]
fn no_deadline_gathers_every_repo_and_keeps_the_start() {
    let (gathered, w) = walk(5, 2, usize::MAX);
    assert_eq!(gathered, vec![2, 3, 4, 0, 1]);
    let stats = w.stats(Instant::now());
    assert_eq!((stats.repos_gathered, stats.repos_skipped, stats.truncated), (5, 0, false));
    assert_eq!(w.next_start(), 2);
}

#[test]
fn a_passed_deadline_truncates_and_the_next_tick_starts_at_the_skipped() {
    let (gathered, w) = walk(5, 0, 2);
    assert_eq!(gathered, vec![0, 1]);
    let stats = w.stats(Instant::now());
    assert_eq!((stats.repos_gathered, stats.repos_skipped, stats.truncated), (2, 3, true));
    assert_eq!(w.next_start(), 2, "the next tick resumes at the first repo skipped");
}

/// A run of truncated ticks covers every repo, never re-reading the first
/// few forever.
#[test]
fn successive_truncated_ticks_cover_every_repo() {
    let mut seen = std::collections::BTreeSet::new();
    let mut start = 0;
    for _ in 0..4 {
        let (gathered, w) = walk(7, start, 2);
        seen.extend(gathered);
        start = w.next_start();
    }
    assert_eq!(seen.len(), 7, "{seen:?}");
}

/// A tick whose deadline passed before the gather began still gathers one
/// repo, so it makes progress (its reads fail fast to the last good listing).
#[test]
fn a_deadline_passed_up_front_still_gathers_one_repo() {
    let (gathered, w) = walk(4, 3, 0);
    assert_eq!(gathered, vec![3]);
    assert_eq!(w.next_start(), 0);
    assert!(w.stats(Instant::now()).truncated);
}

#[test]
fn an_empty_walk_is_harmless() {
    let (gathered, w) = walk(0, 9, 0);
    assert!(gathered.is_empty());
    assert_eq!(w.next_start(), 0);
}

#[test]
fn deadline_passed_reads_the_instant() {
    let now = Instant::now();
    assert!(!deadline_passed(None, now));
    assert!(!deadline_passed(Some(now + Duration::from_secs(1)), now));
    assert!(deadline_passed(Some(now), now));
}

#[test]
fn the_deadline_config_defaults_and_overrides() {
    assert_eq!(tick_deadline_from(&serde_json::json!({})), DEFAULT_TICK_DEADLINE);
    let set = serde_json::json!({"autonomous": {"workFinder": {"tickDeadlineSecs": 30}}});
    assert_eq!(tick_deadline_from(&set), Duration::from_secs(30));
    let zero = serde_json::json!({"autonomous": {"workFinder": {"tickDeadlineSecs": 0}}});
    assert_eq!(tick_deadline_from(&zero), DEFAULT_TICK_DEADLINE);
}
