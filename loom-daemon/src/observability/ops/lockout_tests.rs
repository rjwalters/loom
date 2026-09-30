//! Issue #9674 coverage for `super::lockout`.

use chrono::{Duration, Utc};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::{
    duration_secs, frozen_by_repo, observe_global, reset, sample, FrozenBacklog, LockoutTracker,
    RepoLockout,
};
use crate::types::QueueDisposition as Qd;

/// Serializes the tests that touch the process-global tracker: `reset()`
/// wipes every repo's clock, so two such tests running in parallel would
/// flake on each other. The pure-function tests above never lock — they do
/// not read the global at all.
fn global_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn t(secs: i64) -> chrono::DateTime<Utc> {
    Utc::now() + Duration::seconds(secs)
}

#[test]
fn only_open_pr_rows_aggregate_and_unsized_contributes_no_points() {
    let locked = frozen_by_repo([
        ("a/b", Qd::OpenPr, Some(3)),
        ("a/b", Qd::OpenPr, None), // unsized: counted, contributes no points
        ("a/b", Qd::OpenPr, Some(8)),
        ("a/b", Qd::Dispatched, Some(13)), // not blocked by the guard
        ("a/b", Qd::OpenPrBackoff, Some(5)), // the backoff sibling is not the skip
        ("c/d", Qd::DeferredCapacity, Some(2)), // unblocked repo: absent
    ]);
    assert_eq!(locked.len(), 1, "a repo with no OpenPr row is absent, not zero");
    let backlog = locked["a/b"];
    assert_eq!(
        backlog,
        FrozenBacklog {
            candidates: 3,
            points: 11
        }
    );
}

#[test]
fn empty_rows_aggregate_to_no_lockouts() {
    assert!(frozen_by_repo([]).is_empty());
}

#[test]
fn tracker_starts_clock_at_first_observation_and_grows() {
    let mut tracker = LockoutTracker::default();
    let locked = frozen_by_repo([("a/b", Qd::OpenPr, Some(3))]);
    tracker.observe(&locked, &[], t(0));
    assert_eq!(tracker.duration_secs("a/b", t(0)), Some(0));
    assert_eq!(tracker.duration_secs("a/b", t(90)), Some(90));
    // Still locked: the clock does NOT reset on re-observation.
    tracker.observe(&locked, &[], t(120));
    assert_eq!(tracker.duration_secs("a/b", t(120)), Some(120));
    // The observed repo is not the tracker's only input; `locked` is unchanged.
    assert_eq!(locked.len(), 1);
}

#[test]
fn successful_clear_resets_and_relock_restarts_clock() {
    let mut tracker = LockoutTracker::default();
    let locked = frozen_by_repo([("a/b", Qd::OpenPr, Some(3))]);
    tracker.observe(&locked, &[], t(0));
    // Successfully listed, no OpenPr rows: the lock cleared.
    tracker.observe(&frozen_by_repo([]), &[], t(60));
    assert!(!tracker.is_locked("a/b"));
    // A re-lock starts a fresh clock.
    tracker.observe(&locked, &[], t(120));
    assert_eq!(tracker.duration_secs("a/b", t(150)), Some(30));
}

#[test]
fn failed_listing_never_reads_as_cleared() {
    let mut tracker = LockoutTracker::default();
    let locked = frozen_by_repo([("a/b", Qd::OpenPr, Some(3))]);
    tracker.observe(&locked, &[], t(0));
    // `a/b` is missing from the sample because its listing failed.
    tracker.observe(&frozen_by_repo([]), &["a/b".to_string()], t(60));
    assert!(tracker.is_locked("a/b"), "a transient gh failure is not a clear");
    assert_eq!(tracker.duration_secs("a/b", t(90)), Some(90));
}

#[test]
fn duration_is_a_floor_never_negative() {
    let mut tracker = LockoutTracker::default();
    let locked = frozen_by_repo([("a/b", Qd::OpenPr, None)]);
    // A clock observed in the future of the read (clock skew between the two
    // Utc::now() reads that produced `now` and the sample stamp) clamps to 0.
    tracker.observe(&locked, &[], t(100));
    assert_eq!(tracker.duration_secs("a/b", t(50)), Some(0));
}

#[test]
fn global_tracker_observes_and_reads() {
    let _guard = global_lock();
    reset();
    let now = t(0);
    let locked = frozen_by_repo([
        ("fleet/x", Qd::OpenPr, Some(5)),
        ("fleet/y", Qd::OpenPr, Some(2)),
    ]);
    observe_global(&locked, &[], now);
    assert_eq!(duration_secs("fleet/x", now + Duration::seconds(45)), Some(45));
    assert_eq!(duration_secs("fleet/y", now + Duration::seconds(45)), Some(45));
    assert_eq!(duration_secs("fleet/z", now), None);
    // The other global consumer (the disposition sampler) re-observes the
    // same lock: idempotent, clock keeps its origin.
    observe_global(&locked, &[], now + Duration::seconds(30));
    assert_eq!(duration_secs("fleet/x", now + Duration::seconds(45)), Some(45));
    reset();
    assert_eq!(duration_secs("fleet/x", now), None, "reset clears");
}

/// `sample` — the one call both span-export seams make — folds the weight,
/// advances the global clock, and resolves durations into pure data.
#[test]
fn sample_returns_pure_lockouts_with_resolved_durations() {
    let _guard = global_lock();
    reset();
    let now = t(0);
    let lockouts = sample(
        [
            ("solo/repo", Qd::OpenPr, Some(3)),
            ("solo/repo", Qd::OpenPr, None),
            ("solo/repo", Qd::DeferredCapacity, Some(8)),
        ],
        &[],
        now,
    );
    let first = lockouts["solo/repo"];
    assert_eq!(
        first.backlog,
        FrozenBacklog {
            candidates: 2,
            points: 3
        }
    );
    assert_eq!(first.duration_secs, Some(0));
    // A second sample at a later instant keeps the clock's origin.
    let later = t(75);
    let again = sample([("solo/repo", Qd::OpenPr, Some(3))], &[], later);
    assert_eq!(
        again["solo/repo"],
        RepoLockout {
            backlog: FrozenBacklog {
                candidates: 1,
                points: 3
            },
            duration_secs: Some(75),
        }
    );
    // Unlocked repos are absent; a failed listing keeps the clock.
    let cleared = sample([], &["solo/repo".to_string()], t(100));
    assert!(!cleared.contains_key("solo/repo"));
    assert_eq!(duration_secs("solo/repo", t(130)), Some(130));
    reset();
}

/// The map `sample` returns is what the span builders consume, so its shape
/// (and nothing else) is what they depend on.
#[test]
fn sample_of_nothing_is_an_empty_map() {
    let _guard = global_lock();
    reset();
    let lockouts: HashMap<String, RepoLockout> = sample([], &[], t(0));
    assert!(lockouts.is_empty());
    reset();
}
