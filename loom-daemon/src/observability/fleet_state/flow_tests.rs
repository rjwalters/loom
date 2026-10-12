//! Tests for the per-repo `ready_wait` queue-flow tracker (#11645).

use std::collections::BTreeSet;

use chrono::{DateTime, Duration, TimeZone, Utc};

use super::{QueueFlowTracker, QUEUE_FLOW_WINDOW_SECS};

fn t(min: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 12, 0, 0, 0).unwrap() + Duration::minutes(min)
}

fn set(issues: &[u32]) -> BTreeSet<u32> {
    issues.iter().copied().collect()
}

#[test]
fn first_sight_seeds_the_baseline_without_counting_arrivals() {
    let mut tracker = QueueFlowTracker::new(QUEUE_FLOW_WINDOW_SECS);
    let repo = "acme/a";
    // A queue of three issues exists before we ever watch it.
    let queue = set(&[1, 2, 3]);
    tracker.observe(repo, &queue, t(0));

    let flow = tracker.flow(repo, t(0)).unwrap();
    // The depth is the set size; arrivals/drains are zero — the queue
    // predates our watch, so it is not counted as having "arrived".
    assert_eq!(queue.len(), 3, "seeded depth");
    assert_eq!(flow.arrivals, 0);
    assert_eq!(flow.drains, 0);
    assert_eq!(flow.window_secs, u64::try_from(QUEUE_FLOW_WINDOW_SECS).unwrap());
    assert_eq!(flow.since, t(0));
}

#[test]
fn arrivals_and_drains_match_a_seeded_queue() {
    let mut tracker = QueueFlowTracker::new(QUEUE_FLOW_WINDOW_SECS);
    let repo = "acme/a";

    tracker.observe(repo, &set(&[1, 2, 3]), t(0)); // baseline
    tracker.observe(repo, &set(&[2, 3, 4, 5]), t(10)); // +4,+5  -1
    tracker.observe(repo, &set(&[5, 6]), t(20)); // +6  -2,-3,-4

    let flow = tracker.flow(repo, t(20)).unwrap();
    assert_eq!(flow.arrivals, 3, "4, 5, 6 entered");
    assert_eq!(flow.drains, 4, "1, 2, 3, 4 left");
    // since stays the first-sight instant.
    assert_eq!(flow.since, t(0));
    // Within the window, so observed span is reader-computed from `since`.
    assert!(t(20) - flow.since < tracker_window());
}

#[test]
fn events_older_than_the_window_expire() {
    // A tight 1-hour window so the test does not need 6 h of steps.
    let mut tracker = QueueFlowTracker::new(60 * 60);
    let repo = "acme/a";

    tracker.observe(repo, &set(&[]), t(0)); // baseline: empty queue
    tracker.observe(repo, &set(&[1]), t(5)); // +1 at 00:05
    tracker.observe(repo, &set(&[1, 2]), t(50)); // +2 at 00:50

    // At 00:55 both arrivals are inside the 1 h window.
    assert_eq!(tracker.flow(repo, t(55)).unwrap().arrivals, 2);

    // At 01:06 the 00:05 arrival has aged out; only the 00:50 one remains.
    let flow = tracker.flow(repo, t(66)).unwrap();
    assert_eq!(flow.arrivals, 1, "the 00:05 arrival expired");
}

#[test]
fn an_idle_repo_reports_the_same_flow_between_passes() {
    let mut tracker = QueueFlowTracker::new(QUEUE_FLOW_WINDOW_SECS);
    let repo = "acme/a";
    tracker.observe(repo, &set(&[1, 2]), t(0));
    tracker.observe(repo, &set(&[1, 2, 3]), t(5)); // one arrival

    let a = tracker.flow(repo, t(10)).unwrap();
    tracker.observe(repo, &set(&[1, 2, 3]), t(10)); // no change
    let b = tracker.flow(repo, t(10)).unwrap();
    // Identical facts — no delta churn for an unchanged queue.
    assert_eq!(a, b);
}

#[test]
fn repos_are_tracked_independently() {
    let mut tracker = QueueFlowTracker::new(QUEUE_FLOW_WINDOW_SECS);
    tracker.observe("acme/a", &set(&[1]), t(0));
    tracker.observe("acme/b", &set(&[9]), t(0));
    tracker.observe("acme/a", &set(&[1, 2]), t(5)); // a: +2
    tracker.observe("acme/b", &set(&[]), t(5)); // b: -9

    assert_eq!(tracker.flow("acme/a", t(5)).unwrap().arrivals, 1);
    assert_eq!(tracker.flow("acme/a", t(5)).unwrap().drains, 0);
    assert_eq!(tracker.flow("acme/b", t(5)).unwrap().arrivals, 0);
    assert_eq!(tracker.flow("acme/b", t(5)).unwrap().drains, 1);
    assert!(tracker.flow("acme/c", t(5)).is_none(), "never observed");
}

fn tracker_window() -> Duration {
    Duration::seconds(QUEUE_FLOW_WINDOW_SECS)
}
