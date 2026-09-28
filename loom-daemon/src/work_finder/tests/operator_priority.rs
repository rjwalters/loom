//! `loom:operator-priority` (#9244 slice A): the starred listing, skip labels
//! on starred items, the starred-at cache, and the overflow slot.

use super::*;
use std::time::{Duration, Instant};

use crate::work_finder::operator_priority::{
    latest_labeled_at, merge_starred, StarredAtCache, StarredAtSource, STARRED_AT_RETRY,
};

fn starred(n: u32, extra: &[&str]) -> WorkItem {
    let mut labels = vec![OPERATOR_PRIORITY_LABEL.to_string()];
    labels.extend(extra.iter().map(|l| (*l).to_string()));
    WorkItem::new(n, labels)
}

/// A dispatcher whose occupancy is fixed, so a cap can be "full" without any
/// in-flight dedup side effect on the candidates under test.
fn full(occupancy: u32) -> RecordingDispatcher {
    RecordingDispatcher {
        in_flight: (1000..1000 + occupancy).collect(),
        ..RecordingDispatcher::default()
    }
}

// ---- The second listing (§4) ---------------------------------------------

#[test]
fn merge_dedupes_by_number_and_drops_claimed_starred_rows() {
    let ready = vec![starred(1, &["loom:issue"]), issue(2)];
    let listed = vec![
        starred(1, &["loom:issue"]),    // also in loom:issue: deduped
        starred(3, &["loom:triage"]),   // kept
        starred(4, &["loom:curated"]),  // kept
        starred(5, &[]),                // no workflow label: kept
        starred(6, &[BUILDING_LABEL]),  // being built: dropped
        starred(7, &["loom:curating"]), // being curated: dropped
    ];
    let merged: Vec<u32> = merge_starred(ready, listed)
        .iter()
        .map(|i| i.number)
        .collect();
    assert_eq!(merged, vec![1, 2, 3, 4, 5]);
}

#[test]
fn starred_triage_curated_and_unlabelled_issues_are_dispatched() {
    // Both tick paths: the merged rows are ordinary candidates.
    let rows = || {
        merge_starred(
            vec![],
            vec![
                starred(3, &["loom:triage"]),
                starred(4, &["loom:curated"]),
                starred(5, &[]),
            ],
        )
    };
    let mut dispatcher = RecordingDispatcher::default();
    tick(&mut FakeSource::once(rows()), &mut dispatcher, 10, false).unwrap();
    assert_eq!(dispatcher.dispatched, vec![3, 4, 5]);

    let mut multi = vec![(FakeSource::once(rows()), RecordingDispatcher::default())];
    let report = tick_multi(&mut multi, &[100], 10, &[false]);
    assert_eq!(report.dispatched, 3);
    assert_eq!(multi[0].1.dispatched, vec![3, 4, 5]);
}

#[test]
fn starred_items_with_skip_park_or_decision_labels_are_never_dispatched() {
    let parked = || {
        vec![
            starred(10, &["loom:blocked"]),
            starred(11, &["loom:operator-only", "loom:operator-mechanical"]),
            // The decision sub-kind alone, base label dropped: still skipped.
            starred(12, &["loom:operator-decision"]),
            starred(13, &[OPERATOR_HOLD_LABEL]),
        ]
    };
    let mut dispatcher = RecordingDispatcher::default();
    let report = tick(&mut FakeSource::once(parked()), &mut dispatcher, 10, false).unwrap();
    assert!(dispatcher.dispatched.is_empty());
    assert_eq!(report.skipped_labeled, 4);

    let mut multi = vec![(FakeSource::once(parked()), full(0))];
    let report = tick_multi(&mut multi, &[100], 10, &[false]);
    assert!(multi[0].1.dispatched.is_empty());
    assert_eq!(report.skipped_labeled, 4);
    let rows = ready_queue::finish(&report.queue, &[]);
    assert!(rows.iter().all(|r| r.disposition == Qd::Parked), "{rows:?}");
    assert!(rows
        .iter()
        .any(|r| r.detail.as_deref() == Some("loom:operator-decision")));
}

// ---- Starred-at (§3) -------------------------------------------------------

#[derive(Default)]
struct FakeTimeline {
    answers: std::collections::HashMap<u32, String>,
    fail: bool,
    calls: Vec<u32>,
}

impl StarredAtSource for FakeTimeline {
    fn starred_at(&mut self, issue: u32) -> Result<Option<String>> {
        self.calls.push(issue);
        if self.fail {
            anyhow::bail!("gh: rate limited");
        }
        Ok(self.answers.get(&issue).cloned())
    }
}

#[test]
fn starred_at_is_read_once_per_starred_issue_and_never_for_unstarred() {
    let mut src = FakeTimeline::default();
    src.answers.insert(1, "2026-09-01T00:00:00Z".into());
    let mut cache = StarredAtCache::default();
    let t0 = Instant::now();

    let mut items = vec![starred(1, &["loom:issue"]), issue(2), starred(3, &[])];
    cache.resolve(&mut items, &mut src, t0);
    assert_eq!(src.calls, vec![1, 3], "only starred issues are read");
    assert_eq!(items[0].operator_priority_at.as_deref(), Some("2026-09-01T00:00:00Z"));
    assert_eq!(items[1].operator_priority_at, None);
    assert_eq!(items[2].operator_priority_at, None, "no event: falls back later");

    // Next tick: a known value is never re-read; an unknown one waits for
    // the retry window.
    let mut items = vec![starred(1, &["loom:issue"]), issue(2), starred(3, &[])];
    cache.resolve(&mut items, &mut src, t0 + Duration::from_secs(60));
    assert_eq!(src.calls, vec![1, 3], "no read per candidate per tick");
    assert_eq!(items[0].operator_priority_at.as_deref(), Some("2026-09-01T00:00:00Z"));

    cache.resolve(&mut items, &mut src, t0 + STARRED_AT_RETRY);
    assert_eq!(src.calls, vec![1, 3, 3], "the unknown one is retried after the window");
}

#[test]
fn removing_the_star_invalidates_the_cached_starred_at() {
    let mut src = FakeTimeline::default();
    src.answers.insert(1, "2026-09-01T00:00:00Z".into());
    let mut cache = StarredAtCache::default();
    let now = Instant::now();

    cache.resolve(&mut [starred(1, &[])], &mut src, now);
    assert_eq!(cache.len(), 1);
    // Unstarred on the next tick: the entry is dropped, nothing is read.
    cache.resolve(&mut [issue(1)], &mut src, now);
    assert!(cache.is_empty());
    assert_eq!(src.calls, vec![1]);
    // Re-starred: the new event is read.
    src.answers.insert(1, "2026-09-05T00:00:00Z".into());
    let mut items = [starred(1, &[])];
    cache.resolve(&mut items, &mut src, now);
    assert_eq!(src.calls, vec![1, 1]);
    assert_eq!(items[0].operator_priority_at.as_deref(), Some("2026-09-05T00:00:00Z"));
}

#[test]
fn a_failed_starred_at_read_falls_back_instead_of_failing() {
    let mut src = FakeTimeline {
        fail: true,
        ..FakeTimeline::default()
    };
    let mut cache = StarredAtCache::default();
    let mut items = [starred(1, &[])];
    cache.resolve(&mut items, &mut src, Instant::now());
    assert_eq!(items[0].operator_priority_at, None);
}

#[test]
fn latest_labeled_at_takes_the_most_recent_star() {
    let stdout =
        "2026-09-01T00:00:00Z\n\"2026-09-03T10:00:00Z\"\nnot-a-date\n2026-09-02T00:00:00Z\n";
    assert_eq!(latest_labeled_at(stdout).as_deref(), Some("2026-09-03T10:00:00Z"));
    assert_eq!(latest_labeled_at(""), None);
}

// ---- Overflow slot (§5) ----------------------------------------------------

#[test]
fn overflow_admits_exactly_one_starred_sweep_over_the_global_cap() {
    // Cap 2, occupancy 2: two starred and one plain candidate. Exactly one
    // starred issue goes over the limit; the rest wait.
    let mut multi = vec![(
        FakeSource::once(vec![starred(1, &["loom:issue"]), starred(2, &[]), issue(3)]),
        full(2),
    )];
    let report = tick_multi(&mut multi, &[100], 2, &[false]);
    assert_eq!((report.dispatched, report.dispatched_overflow), (1, 1));
    assert_eq!(multi[0].1.dispatched_overflow, vec![1]);
    assert_eq!(report.deferred_capacity, 2, "the second starred and the plain one wait");
    let rows = ready_queue::finish(&report.queue, &[]);
    let row = rows.iter().find(|r| r.issue == 1).unwrap();
    assert_eq!((row.disposition, row.detail.as_deref()), (Qd::Dispatched, Some("overflow")));

    // Single-workspace path: same contract.
    let mut dispatcher = full(2);
    let src = &mut FakeSource::once(vec![starred(1, &[]), starred(2, &[]), issue(3)]);
    let report = tick(src, &mut dispatcher, 2, false).unwrap();
    assert_eq!((report.dispatched_overflow, report.deferred_capacity), (1, 2));
    assert_eq!(dispatcher.dispatched_overflow, vec![1]);
}

#[test]
fn overflow_refuses_while_an_overflow_sweep_is_live_or_the_host_is_already_over() {
    let live = RecordingDispatcher {
        overflow_live: true,
        ..full(2)
    };
    let mut multi = vec![(FakeSource::once(vec![starred(1, &[])]), live)];
    let report = tick_multi(&mut multi, &[100], 2, &[false]);
    assert_eq!((report.dispatched, report.deferred_capacity), (0, 1));

    // The dynamic cap dropped below occupancy (3 running, cap 2): no overflow.
    let mut multi = vec![(FakeSource::once(vec![starred(1, &[])]), full(3))];
    let report = tick_multi(&mut multi, &[100], 2, &[false]);
    assert_eq!((report.dispatched, report.deferred_capacity), (0, 1));

    let mut dispatcher = RecordingDispatcher {
        overflow_live: true,
        ..full(2)
    };
    let report = tick(&mut FakeSource::once(vec![starred(1, &[])]), &mut dispatcher, 2, false);
    assert_eq!(report.unwrap().dispatched, 0);
}

#[test]
fn unstarred_work_never_uses_the_overflow_slot() {
    let mut multi = vec![(FakeSource::once(vec![issue(1), issue(2)]), full(2))];
    let report = tick_multi(&mut multi, &[100], 2, &[false]);
    assert_eq!((report.dispatched, report.dispatched_overflow), (0, 0));
    assert_eq!(report.deferred_capacity, 2);
}

#[test]
fn overflow_covers_the_per_repo_cap_too() {
    // Global cap has room (occupancy 1 of 4) but this repo is at its
    // per-repo cap of 1: the starred issue goes over it, the plain one waits.
    let mut multi = vec![(FakeSource::once(vec![starred(1, &[]), issue(2)]), full(1))];
    let report = tick_multi_with_repo_cap(
        &mut multi,
        &[100],
        4,
        &[false],
        usize::MAX,
        false,
        None,
        Some(1),
        &[],
    );
    assert_eq!((report.dispatched, report.dispatched_overflow), (1, 1));
    assert_eq!(multi[0].1.dispatched_overflow, vec![1]);
    assert_eq!(report.deferred_repo_cap, 1);
}

#[test]
fn overflow_still_yields_to_the_saturation_brake_and_the_ramp_cap() {
    let mut multi = vec![(FakeSource::once(vec![starred(1, &[])]), full(2))];
    let report =
        tick_multi_with_saturation_brake(&mut multi, &[100], 2, &[false], usize::MAX, true);
    assert_eq!((report.dispatched, report.deferred_saturation), (0, 1));

    let mut multi = vec![(FakeSource::once(vec![starred(1, &[])]), full(2))];
    let report = tick_multi_with_admission_cap(&mut multi, &[100], 2, &[false], 0);
    assert_eq!((report.dispatched, report.deferred_ramp_cap), (0, 1));

    let mut dispatcher = full(2);
    let src = &mut FakeSource::once(vec![starred(1, &[])]);
    let report = tick_with_saturation_brake(src, &mut dispatcher, 2, false, usize::MAX, true);
    assert_eq!(report.unwrap().deferred_saturation, 1);
}

/// A full dispatcher on a `local-dev` host (#9034), for the host-class gate.
struct LocalDev(RecordingDispatcher);

impl WorkDispatcher for LocalDev {
    fn in_flight(&self) -> HashSet<u32> {
        self.0.in_flight()
    }
    fn heavy_local_policy(&self) -> host_class::HeavyLocalPolicy {
        host_class::HeavyLocalPolicy {
            class: host_class::HostClass::LocalDev,
            allow_heavy_local: false,
        }
    }
    fn dispatch(&mut self, issue: u32, complexity: Option<&str>) -> Result<bool> {
        self.0.dispatch(issue, complexity)
    }
}

#[test]
fn overflow_still_yields_to_the_host_class_and_pool_gates() {
    let heavy = || vec![starred(1, &[host_class::LOOM_HEAVY_LABEL])];
    let mut multi = vec![(FakeSource::once(heavy()), LocalDev(full(2)))];
    let report = tick_multi(&mut multi, &[100], 2, &[false]);
    assert_eq!((report.dispatched, report.skipped_host_class), (0, 1));

    // A pool / pre-flight hold arrives as a per-root halt with no verified
    // red main behind it: the starred issue waits like everything else.
    let lane = RedMainLane {
        other_hold: true,
        ..RedMainLane::default()
    };
    let mut dispatcher = full(0);
    let src = &mut FakeSource::once(vec![starred(1, &[])]);
    let report = tick_with_lanes(src, &mut dispatcher, (2, usize::MAX), true, false, lane).unwrap();
    assert!(report.halted && dispatcher.dispatched.is_empty());
}
