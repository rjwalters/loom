//! Priority changes dispatch order, never admission (#11508, operator ruling
//! on #11103's rollout): `loom:important` / `loom:very-important` order the
//! queue (level, oldest, number), but no priority overflows a cap, skips the
//! repo-sharding slice, or makes an issue eligible. The legacy star
//! (`loom:operator-priority`) carries no weight at all. The verified red-main
//! fix (#9244) is not a priority and keeps its slice bypass.

use super::*;

fn vi(n: u32) -> WorkItem {
    WorkItem::new(n, vec!["loom:issue".into(), "loom:very-important".into()])
}

fn important(n: u32) -> WorkItem {
    WorkItem::new(n, vec!["loom:issue".into(), "loom:important".into()])
}

/// The retired operator star (#11510): the daemon no longer knows it, so it
/// is a plain label. The one dispatch-path fixture asserting that.
const RETIRED_STAR: &str = "loom:operator-priority";

fn starred(n: u32) -> WorkItem {
    WorkItem::new(n, vec!["loom:issue".into(), RETIRED_STAR.to_string()])
}

/// A dispatcher whose occupancy is fixed, so a cap can be "full" without any
/// in-flight dedup side effect on the candidates under test.
fn full(occupancy: u32) -> RecordingDispatcher {
    RecordingDispatcher {
        in_flight: (1000..1000 + occupancy).collect(),
        ..RecordingDispatcher::default()
    }
}

#[test]
fn levels_order_the_queue_on_both_tick_paths_and_the_star_is_ignored() {
    // One slot: the very-important issue wins it, whatever its number or age.
    let items = || vec![issue(1), starred(2), important(3), vi(4)];
    let mut dispatcher = full(0);
    tick(&mut FakeSource::once(items()), &mut dispatcher, 1, false).unwrap();
    assert_eq!(dispatcher.dispatched, vec![4]);

    let mut multi = vec![(FakeSource::once(items()), full(0))];
    tick_multi(&mut multi, &[100], 1, &[false]);
    assert_eq!(multi[0].1.dispatched, vec![4]);

    // Room for all: level, then number (no createdAt); the star is plain.
    let mut dispatcher = full(0);
    tick(&mut FakeSource::once(items()), &mut dispatcher, 10, false).unwrap();
    assert_eq!(dispatcher.dispatched, vec![4, 3, 1, 2]);
}

#[test]
fn no_priority_overflows_the_global_cap() {
    // Cap 2, occupancy 2: nothing is admitted, on either path, and the slot
    // the star used to take stays unused.
    let items = || vec![vi(1), important(2), starred(3), issue(4)];
    let mut multi = vec![(FakeSource::once(items()), full(2))];
    let report = tick_multi(&mut multi, &[100], 2, &[false]);
    assert_eq!((report.dispatched, report.dispatched_overflow), (0, 0));
    assert_eq!(report.deferred_capacity, 4);
    assert!(multi[0].1.dispatched_overflow.is_empty());
    assert_eq!(report.overflow_free, Some(true));

    let mut dispatcher = full(2);
    let report = tick(&mut FakeSource::once(items()), &mut dispatcher, 2, false).unwrap();
    assert_eq!((report.dispatched, report.deferred_capacity), (0, 4));
    assert!(dispatcher.dispatched_overflow.is_empty());

    // Headroom to spare above the configured cap changes nothing either.
    let terms = CapTerms {
        configured: 2,
        headroom: 5,
    };
    let mut multi = vec![(FakeSource::once(items()), full(2))];
    let report = tick_multi_with_repo_cap(
        &mut multi,
        &[100],
        terms,
        &[false],
        None,
        usize::MAX,
        false,
        None,
        None,
        &[],
    );
    assert_eq!((report.dispatched, report.deferred_capacity), (0, 4));
}

#[test]
fn a_very_important_candidate_is_deferred_by_the_per_repo_cap() {
    // Global room (1 of 4) but this repo is at its per-repo cap of 1.
    let mut multi = vec![(FakeSource::once(vec![vi(1), issue(2)]), full(1))];
    let report = tick_multi_with_repo_cap(
        &mut multi,
        &[100],
        4.into(),
        &[false],
        None,
        usize::MAX,
        false,
        None,
        Some(1),
        &[],
    );
    assert_eq!((report.dispatched, report.dispatched_overflow), (0, 0));
    assert_eq!(report.deferred_repo_cap, 2);
}

#[test]
fn a_very_important_candidate_outside_this_hosts_slice_is_deferred() {
    // Repo 0 is this host's slice; repo 1 (holding the VI issue) is not.
    let mut multi = vec![
        (FakeSource::once(vec![issue(1)]), full(0)),
        (FakeSource::once(vec![vi(5)]), RecordingDispatcher::default()),
    ];
    let slice = [true, false];
    let report = tick_multi_with_repo_cap(
        &mut multi,
        &[100, 100],
        4.into(),
        &[false, false],
        None,
        usize::MAX,
        false,
        Some(&slice),
        None,
        &[],
    );
    assert_eq!(multi[0].1.dispatched, vec![1]);
    assert!(multi[1].1.dispatched.is_empty());
    assert_eq!(report.deferred_out_of_slice, 1);
    let rows = ready_queue::finish(&report.queue, &[]);
    let row = rows.iter().find(|r| r.issue == 5).unwrap();
    assert_eq!(row.disposition, Qd::DeferredOutOfSlice);
}

/// The red-main-fix lane (#9244) is not a priority and keeps its slice
/// bypass (#11508, Judge): another host's slice must not hold the fix that
/// turns `main` green. It is kept in its draw position and does not make
/// this host's slice non-empty. On a green `main` the marker grants nothing.
#[test]
fn a_verified_red_main_fix_outside_this_hosts_slice_is_not_deferred() {
    let fix = |n: u32| {
        WorkItem::new(n, vec!["loom:issue".into()])
            .with_body(Some("Fixes red main.\n\n<!-- loom:main-red-fix -->\n".into()))
    };
    let red = RedMainLane {
        verified_red: true,
        gate_disabled: false,
        other_hold: false,
    };
    let tick = |multi: &mut Vec<(FakeSource, RecordingDispatcher)>, lanes: &[RedMainLane]| {
        let slice = [true, false];
        tick_multi_with_repo_cap(
            multi,
            &[100, 100],
            4.into(),
            &[false, false],
            None,
            usize::MAX,
            false,
            Some(&slice),
            None,
            lanes,
        )
    };
    // Repo 1 (out of slice) is verified red: its fix dispatches, its VI waits.
    let mut multi = vec![
        (FakeSource::once(vec![issue(1)]), full(0)),
        (FakeSource::once(vec![vi(5), fix(9)]), RecordingDispatcher::default()),
    ];
    let report = tick(&mut multi, &[RedMainLane::default(), red]);
    assert_eq!(multi[0].1.dispatched, vec![1]);
    assert_eq!(multi[1].1.dispatched, vec![9]);
    assert_eq!(report.deferred_out_of_slice, 1);
    let rows = ready_queue::finish(&report.queue, &[]);
    let disp = |n: u32| rows.iter().find(|r| r.issue == n).unwrap().disposition;
    assert_eq!(disp(5), Qd::DeferredOutOfSlice);

    // Repo 1 green: the marker is inert and the fix is deferred like the VI.
    let mut multi = vec![
        (FakeSource::once(vec![issue(1)]), full(0)),
        (FakeSource::once(vec![vi(5), fix(9)]), RecordingDispatcher::default()),
    ];
    let report = tick(&mut multi, &[]);
    assert_eq!(multi[0].1.dispatched, vec![1]);
    assert!(multi[1].1.dispatched.is_empty());
    assert_eq!(report.deferred_out_of_slice, 2);
}

#[test]
fn the_published_row_keys_are_main_red_fix_level_created_at_number() {
    let mut multi = vec![(FakeSource::once(vec![issue(1), vi(2)]), full(0))];
    let report = tick_multi(&mut multi, &[100], 10, &[false]);
    let rows = ready_queue::finish(&report.queue, &[]);
    assert_eq!(rows[0].issue, 2, "rank 1 is the very-important issue");
    assert_eq!(rows[0].level, crate::priority_pick::Level::VeryImportant, "#11510");
    assert!(!rows[0].main_red_fix, "no verified red-main fix here");
    assert_eq!(
        ready_queue::ordering_names(),
        vec![
            "main_red_fix".to_string(),
            "level".into(),
            "created_at".into(),
            "number".into()
        ]
    );
}
