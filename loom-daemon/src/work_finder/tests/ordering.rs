//! Dispatch ordering: cross-repo workspace priority (#3946) and the
//! priority level (#11103, re-keyed by #11508: level, oldest, number; the
//! star and starred-at keys are gone). #11547 restored a leading verified
//! red-main-fix key, live only while the repo's `main` is verified red. Moved out of
//! `work_finder/tests.rs`, which is size-frozen.

use super::*;

/// AC (#11508, #11547): `candidate_cmp`'s keys are exactly main_red_fix ->
/// level -> created_at -> number, over a fixed fixture. Workspace priority
/// is not a key.
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
        workspace_weight: 0,
        ..mk(Level::Important, None, 4)
    };
    let b = PriorityCandidate {
        workspace_weight: 100,
        ..mk(Level::Important, None, 4)
    };
    assert_eq!(candidate_cmp(&a, &b), Ordering::Equal);

    // 5. The fixture, shuffled, sorts to exactly this order.
    let mut fixture = [plain, new, imp, mk(Level::Default, None, 1), vi, old];
    fixture.sort_by(candidate_cmp);
    let numbers: Vec<u32> = fixture.iter().map(|c| c.number).collect();
    assert_eq!(numbers, vec![99, 5, 1, 80, 2, 1]);

    // 6. #11547: a verified red-main fix (default level, newest) sorts ahead
    //    of an older very-important candidate.
    let red_fix = PriorityCandidate {
        main_red_fix: true,
        ..mk(Level::Default, Some("2026-09-01T00:00:00Z"), 200)
    };
    let older_vi = mk(Level::VeryImportant, Some("2020-01-01T00:00:00Z"), 3);
    assert_eq!(candidate_cmp(&red_fix, &older_vi), Ordering::Less);

    // 7. Green main: a marker-bearing item keyed with `repo_red = false` has
    //    `main_red_fix == false` and sorts exactly as an unmarked one.
    let marked = WorkItem::with_created_at(
        200,
        vec!["loom:issue".into()],
        Some("2026-09-01T00:00:00Z".into()),
    )
    .with_body(Some("<!-- loom:main-red-fix -->".into()));
    let green = ready_queue::key_of(0, 0, &marked, false);
    assert!(!green.main_red_fix);
    assert_eq!(candidate_cmp(&green, &older_vi), Ordering::Greater);
    let plain_200 = mk(Level::Default, Some("2026-09-01T00:00:00Z"), 200);
    let others = [
        older_vi,
        mk(Level::Important, Some("2026-10-01T00:00:00Z"), 4),
        mk(Level::Default, Some("2020-01-01T00:00:00Z"), 80),
        mk(Level::Default, None, 1),
    ];
    for other in &others {
        assert_eq!(candidate_cmp(&green, other), candidate_cmp(&plain_200, other));
    }
    assert!(ready_queue::key_of(0, 0, &marked, true).main_red_fix);
}
