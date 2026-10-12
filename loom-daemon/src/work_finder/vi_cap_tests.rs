use super::*;
use crate::priority_pick::Level;

fn vi(n: u32, created_at: &str) -> WorkItem {
    WorkItem::with_created_at(
        n,
        vec!["loom:issue".into(), VERY_IMPORTANT_LABEL.into()],
        Some(created_at.to_string()),
    )
}

/// A very-important issue carrying exactly `labels` besides the VI label.
fn vi_with(n: u32, created_at: &str, labels: &[&str]) -> WorkItem {
    let mut all: Vec<String> = labels.iter().map(|l| (*l).to_string()).collect();
    all.push(VERY_IMPORTANT_LABEL.into());
    WorkItem::with_created_at(n, all, Some(created_at.to_string()))
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

/// ACCEPTANCE (#11559): an older blocked very-important issue, whether it is
/// only in the open listing or also a (not yet skip-filtered) ready row, does
/// not hold the slot; the newer live one does and is named as the holder.
#[test]
fn a_blocked_older_very_important_issue_does_not_hold_the_slot() {
    let blocked = || vi_with(1, "2025-12-01T00:00:00Z", &["loom:issue", "loom:blocked"]);
    // Only in the open listing: the newer ready issue holds and demotes the
    // newest one.
    let mut ready = vec![vi(5, "2026-01-01T00:00:00Z"), vi(6, "2026-02-01T00:00:00Z")];
    let demoted = apply(0, &mut ready, Some(&[blocked()]));
    assert_eq!(levels(&ready), vec![(5, Level::VeryImportant), (6, Level::Important)]);
    assert_eq!(
        demoted,
        vec![ViDemotion {
            workspace_idx: 0,
            number: 6,
            holder: 5
        }]
    );
    // As a ready row (open listing unavailable): it is demoted itself, and
    // the live issue is the holder.
    let mut ready = vec![blocked(), vi(5, "2026-01-01T00:00:00Z")];
    let demoted = apply(0, &mut ready, None);
    assert_eq!(levels(&ready), vec![(1, Level::Important), (5, Level::VeryImportant)]);
    assert_eq!(
        demoted,
        vec![ViDemotion {
            workspace_idx: 0,
            number: 1,
            holder: 5
        }]
    );
}

/// ACCEPTANCE (#11559): an older very-important issue not yet approved
/// (`loom:triage`, or `loom:curated` without `loom:issue`) does not hold the
/// slot; the newer approved issue keeps the level and nothing is demoted.
#[test]
fn an_unapproved_older_very_important_issue_does_not_hold_the_slot() {
    for state in ["loom:triage", "loom:curated"] {
        let unapproved = vi_with(1, "2025-12-01T00:00:00Z", &[state]);
        let mut ready = vec![vi(5, "2026-01-01T00:00:00Z")];
        assert!(apply(0, &mut ready, Some(&[unapproved])).is_empty(), "{state}");
        assert_eq!(levels(&ready), vec![(5, Level::VeryImportant)], "{state}");
    }
}

/// ACCEPTANCE (#11559): live in-flight work still holds the slot, but not
/// once it carries a hold label (`loom:operator`, `loom:needs-capability`).
#[test]
fn building_holds_the_slot_unless_it_also_carries_a_hold_label() {
    let building = vi_with(1, "2025-12-01T00:00:00Z", &["loom:building"]);
    let mut ready = vec![vi(5, "2026-01-01T00:00:00Z")];
    let demoted = apply(0, &mut ready, Some(&[building]));
    assert_eq!(levels(&ready), vec![(5, Level::Important)]);
    assert_eq!(demoted[0].holder, 1);

    for held in [
        vi_with(1, "2025-12-01T00:00:00Z", &["loom:building", "loom:operator"]),
        vi_with(1, "2025-12-01T00:00:00Z", &["loom:issue", "loom:needs-capability"]),
    ] {
        let labels = held.labels.clone();
        let mut ready = vec![vi(5, "2026-01-01T00:00:00Z")];
        assert!(apply(0, &mut ready, Some(&[held])).is_empty(), "{labels:?}");
        assert_eq!(levels(&ready), vec![(5, Level::VeryImportant)], "{labels:?}");
    }
}
