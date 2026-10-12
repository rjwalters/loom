//! Dispatch ordering: cross-repo workspace priority (#3946) and the
//! priority level (#11103, re-keyed by #11508: level, oldest, number; the
//! star, starred-at and red-main-fix keys are gone). Moved out of
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
fn very_important_in_a_priority_100_repo_beats_a_priority_0_repo() {
    // #11103: a workspace with very-important work wins the draw.
    let mut multi = vec![
        (
            FakeSource::once(vec![labelled_at(
                7,
                "loom:very-important",
                "2026-01-01T00:00:00Z",
            )]),
            RecordingDispatcher::default(),
        ),
        (
            FakeSource::once(vec![issue_at(50, "2020-01-01T00:00:00Z")]),
            RecordingDispatcher::default(),
        ),
    ];
    let report = tick_multi(&mut multi, &[100, 0], 1, &[false, false]);
    assert_eq!(report.dispatched, 1);
    assert_eq!(multi[0].1.dispatched, vec![7], "the very-important issue dispatches first");
    assert!(multi[1].1.dispatched.is_empty());
}

#[test]
fn two_important_issues_order_by_age() {
    // Two issues at the same level order oldest `createdAt` first; a legacy
    // star on the newest changes nothing (#11508).
    let mut multi = vec![(
        FakeSource::once(vec![
            labelled_at(2, "loom:important", "2026-01-01T00:00:00Z"),
            labelled_at(1, "loom:important", "2020-01-01T00:00:00Z"),
            labelled_at(3, "loom:important", "2019-08-01T00:00:00Z"),
            labelled_at(4, OPERATOR_PRIORITY_LABEL, "2010-01-01T00:00:00Z"),
        ]),
        RecordingDispatcher::default(),
    )];
    let report = tick_multi(&mut multi, &[100], 1, &[false]);
    assert_eq!(report.dispatched, 1);
    assert_eq!(multi[0].1.dispatched, vec![3], "the oldest lands first");
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
fn single_workspace_tick_moves_important_first_then_oldest_first() {
    // #11103: the single-workspace tick uses the in-workspace order — level,
    // oldest `createdAt`, number — not the listing order (newest-first
    // here). The legacy star (#8) is a plain issue since #11508.
    let mut source = FakeSource::once(vec![
        issue_at(3, "2026-03-01T00:00:00Z"),
        issue_at(1, "2026-01-01T00:00:00Z"),
        labelled_at(9, "loom:important", "2026-09-01T00:00:00Z"),
        labelled_at(8, OPERATOR_PRIORITY_LABEL, "2026-08-01T00:00:00Z"),
        issue_at(2, "2026-02-01T00:00:00Z"),
    ]);
    let mut dispatcher = RecordingDispatcher::default();
    let report = tick(&mut source, &mut dispatcher, 10, false).unwrap();
    assert_eq!(report.dispatched, 5);
    assert_eq!(dispatcher.dispatched, vec![9, 1, 2, 3, 8]);

    // No level and no `createdAt`: issue number ascending.
    let mut source = FakeSource::once(vec![issue(5), issue(4), issue(6)]);
    let mut dispatcher = RecordingDispatcher::default();
    tick(&mut source, &mut dispatcher, 10, false).unwrap();
    assert_eq!(dispatcher.dispatched, vec![4, 5, 6]);
}

/// AC (#11508): `candidate_cmp`'s keys are exactly level -> created_at ->
/// number, over a fixed fixture. Workspace priority is not a key.
#[test]
fn test_candidate_cmp_ordering() {
    use crate::priority_pick::Level;
    use std::cmp::Ordering;
    let mk = |level, created: Option<&str>, num| PriorityCandidate {
        level,
        created_at: created.map(str::to_string),
        number: num,
        ..PriorityCandidate::default()
    };

    // 1. Level beats age and number: a newer very-important outranks an
    //    older important, which outranks an older default.
    let vi = mk(Level::VeryImportant, Some("2026-01-01T00:00:00Z"), 99);
    let imp = mk(Level::Important, Some("2020-01-01T00:00:00Z"), 5);
    let plain = mk(Level::Default, Some("2000-01-01T00:00:00Z"), 1);
    assert_eq!(candidate_cmp(&vi, &imp), Ordering::Less);
    assert_eq!(candidate_cmp(&imp, &plain), Ordering::Less);

    // 2. Same level: oldest-first, and a dated issue before an undated one.
    let old = mk(Level::Default, Some("2020-01-01T00:00:00Z"), 80);
    let new = mk(Level::Default, Some("2024-01-01T00:00:00Z"), 2);
    assert_eq!(candidate_cmp(&old, &new), Ordering::Less);
    assert_eq!(candidate_cmp(&new, &mk(Level::Default, None, 1)), Ordering::Less);

    // 3. Fully-tied keys fall through to the number tiebreak.
    assert_eq!(
        candidate_cmp(&mk(Level::Default, None, 3), &mk(Level::Default, None, 8)),
        Ordering::Less
    );

    // 4. Workspace priority is not a key: two candidates differing only in it
    //    compare equal.
    let a = PriorityCandidate {
        workspace_priority: 0,
        ..mk(Level::Important, None, 4)
    };
    let b = PriorityCandidate {
        workspace_priority: 100,
        ..mk(Level::Important, None, 4)
    };
    assert_eq!(candidate_cmp(&a, &b), Ordering::Equal);

    // 5. The fixture, shuffled, sorts to exactly this order.
    let mut fixture = [plain, new, imp, mk(Level::Default, None, 1), vi, old];
    fixture.sort_by(candidate_cmp);
    let numbers: Vec<u32> = fixture.iter().map(|c| c.number).collect();
    assert_eq!(numbers, vec![99, 5, 1, 80, 2, 1]);
}
