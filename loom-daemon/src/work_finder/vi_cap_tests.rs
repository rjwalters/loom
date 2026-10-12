use super::*;
use crate::priority_pick::Level;

fn vi(n: u32, created_at: &str) -> WorkItem {
    WorkItem::with_created_at(
        n,
        vec!["loom:issue".into(), VERY_IMPORTANT_LABEL.into()],
        Some(created_at.to_string()),
    )
}

fn levels(ready: &[WorkItem]) -> Vec<(u32, Level)> {
    ready
        .iter()
        .map(|i| (i.number, Level::of(&i.labels)))
        .collect()
}

/// ACCEPTANCE (#11509): N+2 very-important issues in one workspace (N =
/// [`VERY_IMPORTANT_CAP`] = 1): exactly the oldest stays very-important, the
/// others rank as important. The fixtures carry no actor information.
#[test]
fn n_plus_two_very_important_issues_keep_exactly_the_oldest() {
    let mut ready = vec![
        vi(7, "2026-03-01T00:00:00Z"),
        vi(5, "2026-01-01T00:00:00Z"),
        vi(6, "2026-02-01T00:00:00Z"),
    ];
    let demoted = apply(3, &mut ready, None);
    assert_eq!(
        levels(&ready),
        vec![
            (7, Level::Important),
            (5, Level::VeryImportant),
            (6, Level::Important)
        ]
    );
    let expect = |number| ViDemotion {
        workspace_idx: 3,
        number,
        holder: 5,
    };
    assert_eq!(demoted, vec![expect(7), expect(6)]);
    assert!(ready[0].labels.contains(&IMPORTANT_LABEL.to_string()));
    assert!(!ready[0].labels.contains(&VERY_IMPORTANT_LABEL.to_string()));
}

/// A very-important issue being built is not in the ready listing but is
/// still open: it keeps the slot, so no ready issue is promoted behind it.
#[test]
fn an_open_older_very_important_issue_outside_the_ready_listing_holds_the_slot() {
    let building = WorkItem::with_created_at(
        1,
        vec!["loom:building".into(), VERY_IMPORTANT_LABEL.into()],
        Some("2025-12-01T00:00:00Z".into()),
    );
    let mut ready = vec![vi(5, "2026-01-01T00:00:00Z")];
    let demoted = apply(0, &mut ready, Some(&[building]));
    assert_eq!(levels(&ready), vec![(5, Level::Important)]);
    assert_eq!(demoted[0].holder, 1);
    // Once it closes (no longer listed), the ready issue holds the slot.
    let mut ready = vec![vi(5, "2026-01-01T00:00:00Z")];
    assert!(apply(0, &mut ready, Some(&[])).is_empty());
    assert_eq!(levels(&ready), vec![(5, Level::VeryImportant)]);
}

/// A single very-important issue, and an issue that already had both
/// labels, are handled without duplicate labels or spurious demotions.
#[test]
fn one_very_important_issue_is_untouched_and_labels_stay_unique() {
    let mut ready = vec![vi(5, "2026-01-01T00:00:00Z")];
    assert!(apply(0, &mut ready, None).is_empty());
    let mut both = vi(9, "2026-02-01T00:00:00Z");
    both.labels.push(IMPORTANT_LABEL.into());
    let mut ready = vec![vi(5, "2026-01-01T00:00:00Z"), both];
    apply(0, &mut ready, None);
    let important = ready[1]
        .labels
        .iter()
        .filter(|l| *l == IMPORTANT_LABEL)
        .count();
    assert_eq!(important, 1);
}
