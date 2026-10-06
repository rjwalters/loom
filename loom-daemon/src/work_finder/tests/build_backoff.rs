//! The build back-off (#9410) in pass 2 of a multi-workspace tick: plain
//! candidates wait, starred and red-main-fix candidates bypass, in-flight
//! sweeps are untouched, and `false` is the pre-#9410 tick exactly. Since
//! #10624 the hold is per workspace: a held repo never defers a sibling.

use super::*;

fn starred(n: u32) -> WorkItem {
    WorkItem::new(n, vec![OPERATOR_PRIORITY_LABEL.to_string()])
}

fn fix(n: u32) -> WorkItem {
    WorkItem::new(n, vec!["loom:issue".into()])
        .with_body(Some("Fixes red main.\n\n<!-- loom:main-red-fix -->\n".into()))
}

const RED: RedMainLane = RedMainLane {
    verified_red: true,
    gate_disabled: false,
    other_hold: false,
};

fn run(
    multi: &mut [(FakeSource, RecordingDispatcher)],
    max: usize,
    per_repo: Option<usize>,
    lanes: &[RedMainLane],
    held: &[bool],
) -> TickReport {
    tick_multi_with_build_backoff(
        multi,
        &[100; 4][..multi.len()],
        max.into(),
        &[false; 4][..multi.len()],
        None,
        usize::MAX,
        false,
        None,
        per_repo,
        lanes,
        held,
    )
}

// -- AC4 ----------------------------------------------------------------------

#[test]
fn engaged_defers_every_plain_candidate_and_leaves_in_flight_alone() {
    let busy = || RecordingDispatcher {
        in_flight: [7].into_iter().collect(),
        ..RecordingDispatcher::default()
    };
    let items = || vec![issue(1), issue(2), issue(7)];
    let mut multi = vec![
        (FakeSource::once(items()), busy()),
        (FakeSource::once(vec![issue(10)]), RecordingDispatcher::default()),
    ];
    let report = run(&mut multi, 10, None, &[], &[true, true]);

    assert_eq!(report.dispatched, 0);
    assert!(multi.iter().all(|(_, d)| d.dispatched.is_empty()));
    assert_eq!(report.deferred_build_backoff, 3, "#1, #2 and #10");
    assert!(report.build_backoff_held);
    // In flight: still in flight, counted, never deferred or cancelled.
    assert_eq!(multi[0].1.in_flight, [7].into_iter().collect());
    assert_eq!(report.occupancy, Some(1));
    assert_eq!(report.skipped_in_flight, 1);

    let rows = ready_queue::finish(&report.queue, &[]);
    let deferred: Vec<u32> = rows
        .iter()
        .filter(|r| r.disposition == Qd::DeferredBuildBackoff)
        .map(|r| r.issue)
        .collect();
    assert_eq!(deferred.len(), report.deferred_build_backoff);
    assert!(rows
        .iter()
        .any(|r| r.issue == 7 && r.disposition == Qd::InFlight));
    let summary = tick_summary(&report, 10, chrono::Utc::now(), &[], None);
    assert_eq!((summary.deferred_build_backoff, summary.build_backoff_held), (3, true));
    assert!(summary
        .reason_summary()
        .contains("3 deferred-build-backoff"));
    assert!(summary.reason_summary().contains("BUILD-BACKOFF-HELD"));
}

// -- AC5 ----------------------------------------------------------------------

#[test]
fn starred_and_red_main_fix_candidates_bypass_the_backoff() {
    let mut multi = vec![(
        FakeSource::once(vec![issue(1), starred(5), fix(9)]),
        RecordingDispatcher::default(),
    )];
    let report = run(&mut multi, 10, None, &[RED], &[true]);
    assert_eq!(multi[0].1.dispatched, vec![5, 9]);
    assert_eq!((report.dispatched, report.deferred_build_backoff), (2, 1));

    // Without a verified-red main the marker is inert: the fix waits too.
    let mut multi = vec![(FakeSource::once(vec![fix(9)]), RecordingDispatcher::default())];
    let report = run(&mut multi, 10, None, &[], &[true]);
    assert_eq!((report.dispatched, report.deferred_build_backoff), (0, 1));
}

#[test]
fn a_bypassing_star_still_meets_the_cap_and_the_overflow_slot() {
    let full = |n: u32| RecordingDispatcher {
        in_flight: (1000..1000 + n).collect(),
        ..RecordingDispatcher::default()
    };
    // Cap full: the first star takes the single overflow slot, the second
    // is deferred by the cap (not the back-off), the plain one by the back-off.
    let mut multi = vec![(FakeSource::once(vec![starred(1), starred(2), issue(3)]), full(2))];
    let report = run(&mut multi, 2, None, &[], &[true]);
    assert_eq!((report.dispatched, report.dispatched_overflow), (1, 1));
    assert_eq!(report.deferred_capacity, 1);
    assert_eq!(report.deferred_build_backoff, 1);
}

#[test]
fn not_held_is_the_repo_cap_tick_exactly() {
    let items = || vec![issue(1), starred(2), issue(3), fix(4)];
    let mut a = vec![(FakeSource::once(items()), RecordingDispatcher::default())];
    let mut b = vec![(FakeSource::once(items()), RecordingDispatcher::default())];
    let via_backoff = run(&mut a, 2, Some(1), &[RED], &[false]);
    let via_repo_cap = tick_multi_with_repo_cap(
        &mut b,
        &[100],
        2.into(),
        &[false],
        None,
        usize::MAX,
        false,
        None,
        Some(1),
        &[RED],
    );
    // Identical but for the admission records' wall-clock stamps.
    let unstamped = |mut r: TickReport| {
        let epoch = chrono::DateTime::<chrono::Utc>::UNIX_EPOCH;
        for a in &mut r.admissions {
            (a.started_at, a.ended_at) = (epoch, epoch);
        }
        format!("{r:?}")
    };
    assert_eq!(via_backoff.admissions.len(), 1);
    assert_eq!(unstamped(via_backoff.clone()), unstamped(via_repo_cap));
    assert_eq!(a[0].1.dispatched, b[0].1.dispatched);
    assert_eq!((via_backoff.deferred_build_backoff, via_backoff.build_backoff_held), (0, false));
}

#[test]
fn the_saturation_brake_outranks_the_backoff() {
    let mut multi = vec![(FakeSource::once(vec![issue(1)]), RecordingDispatcher::default())];
    let report = tick_multi_with_build_backoff(
        &mut multi,
        &[100],
        10.into(),
        &[false],
        None,
        usize::MAX,
        true,
        None,
        None,
        &[],
        &[true],
    );
    assert_eq!((report.deferred_saturation, report.deferred_build_backoff), (1, 0));
}

// -- #10624: per-repo isolation ----------------------------------------------

#[test]
fn a_held_repo_defers_only_its_own_candidates() {
    let mut multi = vec![
        (
            FakeSource::once(vec![issue(1), starred(2), issue(3)]),
            RecordingDispatcher::default(),
        ),
        (FakeSource::once(vec![issue(10), issue(11)]), RecordingDispatcher::default()),
    ];
    let report = run(&mut multi, 10, None, &[], &[true, false]);
    assert_eq!(multi[0].1.dispatched, vec![2], "only the star bypasses repo A's hold");
    assert_eq!(multi[1].1.dispatched, vec![10, 11], "repo B has no debt of its own");
    assert_eq!((report.dispatched, report.deferred_build_backoff), (3, 2));
    assert!(report.build_backoff_held, "any held workspace marks the tick");

    let rows = ready_queue::finish(&report.queue, &[]);
    let deferred: Vec<u32> = rows
        .iter()
        .filter(|r| r.disposition == Qd::DeferredBuildBackoff)
        .map(|r| r.issue)
        .collect();
    assert_eq!(deferred, vec![1, 3]);
}

#[test]
fn an_empty_or_all_false_hold_slice_holds_nothing() {
    for held in [&[][..], &[false, false][..]] {
        let mut multi = vec![
            (FakeSource::once(vec![issue(1)]), RecordingDispatcher::default()),
            (FakeSource::once(vec![issue(2)]), RecordingDispatcher::default()),
        ];
        let report = run(&mut multi, 10, None, &[], held);
        assert_eq!((report.dispatched, report.deferred_build_backoff), (2, 0), "{held:?}");
        assert!(!report.build_backoff_held);
    }
}

/// A short slice breaks the parallel-slice contract: caught in debug builds
/// (release fails open through `build_backoff::defers`' `.get()`).
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "build_backoff_held has 1 flags for 2 workspaces")]
fn a_short_hold_slice_is_a_caller_bug_in_debug_builds() {
    let mut multi = vec![
        (FakeSource::once(vec![issue(1)]), RecordingDispatcher::default()),
        (FakeSource::once(vec![issue(2)]), RecordingDispatcher::default()),
    ];
    let _ = run(&mut multi, 10, None, &[], &[false]);
}

/// #10624: a held repo with nothing to defer does not tag the tick, so the
/// health line and the tick result do not report a hold on every idle tick.
#[test]
fn a_held_repo_with_no_candidates_does_not_tag_the_tick() {
    let mut multi = vec![
        (FakeSource::once(vec![]), RecordingDispatcher::default()),
        (FakeSource::once(vec![issue(10)]), RecordingDispatcher::default()),
    ];
    let report = run(&mut multi, 10, None, &[], &[true, false]);
    assert_eq!((report.dispatched, report.deferred_build_backoff), (1, 0));
    assert!(report.build_backoff_held, "the repo is still held");
    let summary = tick_summary(&report, 10, chrono::Utc::now(), &[], None);
    assert!(!summary.reason_summary().contains("BUILD-BACKOFF-HELD"));
    assert_eq!(crate::observability::ops::dispatch::tick_result(&report), "dispatched");

    // Same hold, the other repo at capacity: the result is `capacity_full`.
    let mut multi = vec![
        (FakeSource::once(vec![]), RecordingDispatcher::default()),
        (FakeSource::once(vec![issue(10), issue(11)]), RecordingDispatcher::default()),
    ];
    let report = run(&mut multi, 0, None, &[], &[true, false]);
    assert_eq!(report.deferred_build_backoff, 0);
    assert!(report.deferred_capacity > 0, "{report:?}");
    assert_eq!(crate::observability::ops::dispatch::tick_result(&report), "capacity_full");
}
