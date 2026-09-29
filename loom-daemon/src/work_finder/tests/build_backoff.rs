//! The build back-off (#9410) in pass 2 of a multi-workspace tick: plain
//! candidates wait, starred and red-main-fix candidates bypass, in-flight
//! sweeps are untouched, and `false` is the pre-#9410 tick exactly.

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
    held: bool,
) -> TickReport {
    tick_multi_with_build_backoff(
        multi,
        &[100; 4][..multi.len()],
        max.into(),
        &[false; 4][..multi.len()],
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
    let report = run(&mut multi, 10, None, &[], true);

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
    let report = run(&mut multi, 10, None, &[RED], true);
    assert_eq!(multi[0].1.dispatched, vec![5, 9]);
    assert_eq!((report.dispatched, report.deferred_build_backoff), (2, 1));

    // Without a verified-red main the marker is inert: the fix waits too.
    let mut multi = vec![(FakeSource::once(vec![fix(9)]), RecordingDispatcher::default())];
    let report = run(&mut multi, 10, None, &[], true);
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
    let report = run(&mut multi, 2, None, &[], true);
    assert_eq!((report.dispatched, report.dispatched_overflow), (1, 1));
    assert_eq!(report.deferred_capacity, 1);
    assert_eq!(report.deferred_build_backoff, 1);
}

#[test]
fn not_held_is_the_repo_cap_tick_exactly() {
    let items = || vec![issue(1), starred(2), issue(3), fix(4)];
    let mut a = vec![(FakeSource::once(items()), RecordingDispatcher::default())];
    let mut b = vec![(FakeSource::once(items()), RecordingDispatcher::default())];
    let via_backoff = run(&mut a, 2, Some(1), &[RED], false);
    let via_repo_cap = tick_multi_with_repo_cap(
        &mut b,
        &[100],
        2.into(),
        &[false],
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
        usize::MAX,
        true,
        None,
        None,
        &[],
        true,
    );
    assert_eq!((report.deferred_saturation, report.deferred_build_backoff), (1, 0));
}
