//! Dispatch ordering: cross-repo priority (#3946) and the #9244 keys
//! (`loom:operator-priority` first, starred-at, red-main fix). Moved out of
//! `work_finder/tests.rs`, which is size-frozen.

use super::*;

fn issue_at(n: u32, created_at: &str) -> WorkItem {
    WorkItem::with_created_at(n, vec!["loom:issue".into()], Some(created_at.to_string()))
}

fn labelled_at(n: u32, extra: &str, created_at: &str) -> WorkItem {
    WorkItem::with_created_at(
        n,
        vec!["loom:issue".into(), extra.into()],
        Some(created_at.to_string()),
    )
}

/// A dispatcher whose host already runs its overflow sweep, so a cap of 1
/// admits exactly one starred issue and the test sees pure ordering.
fn no_overflow() -> RecordingDispatcher {
    RecordingDispatcher {
        overflow_live: true,
        ..RecordingDispatcher::default()
    }
}

fn starred_at(n: u32, created_at: &str, starred: Option<&str>) -> WorkItem {
    labelled_at(n, OPERATOR_PRIORITY_LABEL, created_at)
        .with_operator_priority_at(starred.map(str::to_string))
}

#[test]
fn test_tick_multi_higher_priority_repo_dispatches_first_under_cap() {
    // ACCEPTANCE (#3946): the LOWER-priority repo (index 0, priority 100) has
    // OLDER and MORE candidates than the HIGHER-priority repo (index 1,
    // priority 0). Under a global cap of 2, the higher-priority repo's
    // candidates MUST dispatch first anyway — a deep/old product backlog
    // never starves a small high-priority tool repo.
    let mut multi = vec![
        (
            FakeSource::once(vec![
                issue_at(1, "2023-01-01T00:00:00Z"),
                issue_at(2, "2023-01-02T00:00:00Z"),
                issue_at(3, "2023-01-03T00:00:00Z"),
                issue_at(4, "2023-01-04T00:00:00Z"),
            ]),
            RecordingDispatcher::default(),
        ),
        (
            FakeSource::once(vec![
                issue_at(50, "2025-01-01T00:00:00Z"),
                issue_at(51, "2025-01-02T00:00:00Z"),
            ]),
            RecordingDispatcher::default(),
        ),
    ];
    let report = tick_multi(&mut multi, &[100, 0], 2, &[false, false]);

    assert_eq!(report.dispatched, 2, "the global cap of 2 is filled");
    assert_eq!(report.deferred_capacity, 4, "the low-priority repo's 4 are deferred");
    assert!(multi[0].1.dispatched.is_empty());
    assert_eq!(multi[1].1.dispatched, vec![50, 51]);
}

#[test]
fn starred_in_a_priority_100_repo_beats_unstarred_in_a_priority_0_repo() {
    // #9244 key 1: starring outranks the workspace tier, fleet-wide.
    let mut multi = vec![
        (
            FakeSource::once(vec![starred_at(7, "2026-01-01T00:00:00Z", None)]),
            RecordingDispatcher::default(),
        ),
        (
            FakeSource::once(vec![issue_at(50, "2020-01-01T00:00:00Z")]),
            RecordingDispatcher::default(),
        ),
    ];
    let report = tick_multi(&mut multi, &[100, 0], 1, &[false, false]);
    assert_eq!(report.dispatched, 1);
    assert_eq!(multi[0].1.dispatched, vec![7], "the starred issue dispatches first");
    assert!(multi[1].1.dispatched.is_empty());
}

#[test]
fn two_starred_issues_order_by_starred_at_not_age() {
    // #9244 key 2: #1 is older but was starred LATER than #2. (A live
    // overflow sweep keeps the second starred issue from going over the cap.)
    let mut multi = vec![(
        FakeSource::once(vec![
            starred_at(1, "2020-01-01T00:00:00Z", Some("2026-09-02T00:00:00Z")),
            starred_at(2, "2026-01-01T00:00:00Z", Some("2026-09-01T00:00:00Z")),
        ]),
        no_overflow(),
    )];
    let report = tick_multi(&mut multi, &[100], 1, &[false]);
    assert_eq!(report.dispatched, 1);
    assert_eq!(multi[0].1.dispatched, vec![2], "starred first lands first");
}

#[test]
fn a_missing_starred_at_falls_back_to_created_at() {
    // #9244 key 2 fallback: #3 has no starred-at, so it orders by its
    // createdAt (2026-08-01), which is earlier than #4's starred-at.
    let mut multi = vec![(
        FakeSource::once(vec![
            starred_at(4, "2020-01-01T00:00:00Z", Some("2026-09-01T00:00:00Z")),
            starred_at(3, "2026-08-01T00:00:00Z", None),
        ]),
        no_overflow(),
    )];
    let report = tick_multi(&mut multi, &[100], 1, &[false]);
    assert_eq!(report.dispatched, 1);
    assert_eq!(multi[0].1.dispatched, vec![3]);
}

#[test]
fn loom_urgent_no_longer_changes_order() {
    // #9244: `loom:urgent` is tolerated but is not a key. The older
    // non-urgent issue dispatches first; the urgent one is not an error.
    let mut multi = vec![(
        FakeSource::once(vec![
            labelled_at(9, URGENT_LABEL, "2026-01-01T00:00:00Z"),
            issue_at(1, "2023-01-01T00:00:00Z"),
        ]),
        RecordingDispatcher::default(),
    )];
    let report = tick_multi(&mut multi, &[100], 1, &[false]);
    assert_eq!((report.dispatched, report.errors), (1, 0));
    assert_eq!(multi[0].1.dispatched, vec![1], "age, not urgency, decides");
    // The label is still parsed harmlessly.
    assert!(labelled_at(9, URGENT_LABEL, "2026-01-01T00:00:00Z").is_urgent());
}

#[test]
fn test_tick_multi_oldest_first_within_same_tier() {
    // Same tier, nothing starred: oldest-first by createdAt. Cap 1 ⇒ the
    // oldest (#7, 2022) dispatches before the newer (#2, 2024).
    let mut multi = vec![(
        FakeSource::once(vec![
            issue_at(2, "2024-06-01T00:00:00Z"),
            issue_at(7, "2022-06-01T00:00:00Z"),
        ]),
        RecordingDispatcher::default(),
    )];
    let report = tick_multi(&mut multi, &[100], 1, &[false]);
    assert_eq!(report.dispatched, 1);
    assert_eq!(multi[0].1.dispatched, vec![7], "the older issue dispatches first");
}

#[test]
fn test_tick_multi_missing_priority_entry_defaults() {
    // A short `priorities` slice treats the unspecified workspaces as the
    // default tier rather than panicking.
    let mut multi = vec![
        (FakeSource::once(vec![issue(1)]), RecordingDispatcher::default()),
        (FakeSource::once(vec![issue(10)]), RecordingDispatcher::default()),
    ];
    let report = tick_multi(&mut multi, &[], 10, &[false, false]);
    assert_eq!(report.dispatched, 2);
    assert_eq!(multi[0].1.dispatched, vec![1]);
    assert_eq!(multi[1].1.dispatched, vec![10]);
}

#[test]
fn single_workspace_tick_moves_starred_first_and_keeps_listing_order_otherwise() {
    // The single-workspace tick sorts by the lane keys only: the starred
    // issue jumps the queue, and the unstarred ones keep their listing order
    // (newest-first here) exactly as before #9244.
    let mut source = FakeSource::once(vec![
        issue_at(3, "2026-03-01T00:00:00Z"),
        issue_at(1, "2026-01-01T00:00:00Z"),
        starred_at(9, "2026-09-01T00:00:00Z", None),
        issue_at(2, "2026-02-01T00:00:00Z"),
    ]);
    let mut dispatcher = RecordingDispatcher::default();
    let report = tick(&mut source, &mut dispatcher, 10, false).unwrap();
    assert_eq!(report.dispatched, 4);
    assert_eq!(dispatcher.dispatched, vec![9, 3, 1, 2]);

    // Nothing starred: byte-for-byte the listing order.
    let mut source = FakeSource::once(vec![issue(5), issue(4), issue(6)]);
    let mut dispatcher = RecordingDispatcher::default();
    tick(&mut source, &mut dispatcher, 10, false).unwrap();
    assert_eq!(dispatcher.dispatched, vec![5, 4, 6]);
}

#[test]
fn test_candidate_cmp_ordering() {
    use std::cmp::Ordering;
    let mk = |prio, created: Option<&str>, num| PriorityCandidate {
        workspace_priority: prio,
        created_at: created.map(str::to_string),
        number: num,
        ..PriorityCandidate::default()
    };
    let star = |c: PriorityCandidate, at: Option<&str>| PriorityCandidate {
        operator_priority: true,
        operator_priority_at: at.map(str::to_string),
        ..c
    };
    let fix = |c: PriorityCandidate| PriorityCandidate {
        main_red_fix: true,
        ..c
    };

    // 1. Starred beats everything: a starred prio-100 newer issue outranks an
    //    unstarred prio-0 older red-main fix.
    let starred = star(mk(100, Some("2026-01-01T00:00:00Z"), 99), None);
    let other = fix(mk(0, Some("2000-01-01T00:00:00Z"), 1));
    assert_eq!(candidate_cmp(&starred, &other), Ordering::Less);

    // 2. Among starred: starred-at ascending, with createdAt as fallback.
    let early = star(mk(100, Some("2020-01-01T00:00:00Z"), 5), Some("2026-09-01T00:00:00Z"));
    let late = star(mk(0, Some("2000-01-01T00:00:00Z"), 6), Some("2026-09-02T00:00:00Z"));
    assert_eq!(candidate_cmp(&early, &late), Ordering::Less);
    let fallback = star(mk(100, Some("2026-08-01T00:00:00Z"), 7), None);
    assert_eq!(candidate_cmp(&fallback, &early), Ordering::Less);

    // 3. A red-main fix beats a better workspace tier.
    let fix100 = fix(mk(100, Some("2026-01-01T00:00:00Z"), 8));
    let plain0 = mk(0, Some("2000-01-01T00:00:00Z"), 1);
    assert_eq!(candidate_cmp(&fix100, &plain0), Ordering::Less);

    // 4. Workspace priority dominates age.
    let high = mk(0, Some("2025-01-01T00:00:00Z"), 999);
    let low = mk(100, Some("2000-01-01T00:00:00Z"), 1);
    assert_eq!(candidate_cmp(&high, &low), Ordering::Less);

    // 5. Same tier: oldest-first, and a dated issue before an undated one.
    let old = mk(100, Some("2020-01-01T00:00:00Z"), 80);
    let new = mk(100, Some("2024-01-01T00:00:00Z"), 2);
    assert_eq!(candidate_cmp(&old, &new), Ordering::Less);
    assert_eq!(candidate_cmp(&new, &mk(100, None, 1)), Ordering::Less);

    // 6. Fully-tied keys fall through to the number tiebreak.
    assert_eq!(candidate_cmp(&mk(100, None, 3), &mk(100, None, 8)), Ordering::Less);
}

#[test]
fn test_work_item_is_urgent_still_parses() {
    assert!(!issue(1).is_urgent());
    assert!(WorkItem::new(1, vec!["loom:issue".into(), "loom:urgent".into()]).is_urgent());
}
