//! Tests for the PR-debt ledger (#9392, #11911). Every test uses its own
//! [`DebtLedger`]; none touches the global one or the network.

use super::*;

const HOUR: Duration = Duration::from_secs(3600);

fn root(n: u8) -> PathBuf {
    PathBuf::from(format!("/tmp/loom-11911-pr-debt-{n}"))
}

fn row(number: u32, is_pull_request: bool) -> RestIssue {
    RestIssue {
        comments: 0,
        number,
        title: None,
        labels: Vec::new(),
        created_at: None,
        updated_at: None,
        closed_at: None,
        state: "open".to_string(),
        body: None,
        author: None,
        author_association: None,
        is_pull_request,
        open_sub_issues: None,
        labels_at: None,
    }
}

/// A PR row in queue `label` that also carries each of `extra`.
fn queue_row(number: u32, label: &str, extra: &[&str]) -> RestIssue {
    let mut labels = vec![label.to_string()];
    labels.extend(extra.iter().map(ToString::to_string));
    RestIssue {
        labels,
        ..row(number, true)
    }
}

#[test]
fn ledger_sums_counts_and_roots_with_debt_per_axis() {
    let ledger = DebtLedger::default();
    ledger.record(&root(1), DebtAxis::Merge, 4);
    ledger.record(&root(2), DebtAxis::Merge, 0);
    ledger.record(&root(3), DebtAxis::Merge, 2);
    ledger.record(&root(1), DebtAxis::Review, 5);
    let debt = ledger.host_debt(HOUR);
    assert_eq!(
        debt.merge,
        Some(AxisDebt {
            total: 6,
            roots_with_debt: 2
        }),
        "a zero entry is observed but is not a root with debt"
    );
    assert_eq!(debt.changes, None, "an axis nobody recorded is unobserved");
    // A later record replaces the root's entry rather than adding to it.
    ledger.record(&root(1), DebtAxis::Merge, 1);
    assert_eq!(ledger.host_debt(HOUR).merge.unwrap().total, 3);
    assert_eq!(ledger.repo_debt(&root(3), HOUR).merge.unwrap().total, 2);
    assert_eq!(ledger.repo_debt(&root(3), HOUR).review, None);
}

#[test]
fn entries_older_than_stale_after_are_unobserved() {
    let ledger = DebtLedger::default();
    let t0 = Instant::now();
    ledger.record_at(&root(1), DebtAxis::Review, 7, t0);
    ledger.record_at(&root(2), DebtAxis::Merge, 3, t0 + Duration::from_secs(1000));
    let at = |secs| ledger.host_debt_at(t0 + Duration::from_secs(secs), STALE_AFTER);
    assert_eq!(at(1800).review.unwrap().total, 7, "exactly STALE_AFTER old is fresh");
    let later = at(1801);
    assert_eq!(later.review, None, "one axis stale: that axis is None");
    assert_eq!(later.merge.unwrap().total, 3, "the fresh axis is unaffected");
    assert_eq!(at(2801), HostDebt::default(), "every axis stale: all None");
}

#[test]
fn a_root_that_leaves_the_registry_stops_counting() {
    let ledger = DebtLedger::default();
    ledger.record(&root(1), DebtAxis::Merge, 5);
    ledger.record(&root(2), DebtAxis::Merge, 5);
    ledger.retain_roots(&[root(2)]);
    assert_eq!(ledger.host_debt(HOUR).merge.unwrap().total, 5);
}

/// The step tick's all-open listing records every axis, from the rows that
/// carry its label, leaving parked and held PRs and plain issues out.
#[test]
fn an_open_listing_records_every_axis_from_its_own_rows() {
    let ledger = DebtLedger::default();
    let mut closed = queue_row(9, "loom:pr", &[]);
    closed.state = "closed".to_string();
    let rows = vec![
        queue_row(1, "loom:review-requested", &[]),
        queue_row(2, "loom:review-requested", &["loom:blocked"]),
        queue_row(3, "loom:changes-requested", &["loom:treating"]),
        queue_row(4, "loom:pr", &[]),
        queue_row(5, "loom:pr", &["loom:operator"]),
        RestIssue {
            labels: vec!["loom:pr".to_string()],
            ..row(6, false)
        },
        closed,
        queue_row(7, "loom:issue", &[]),
    ];
    record_open_listing(&ledger, &root(1), &rows);
    let debt = ledger.repo_debt(&root(1), HOUR);
    assert_eq!(debt.review.unwrap().total, 1, "the parked review PR is left out");
    assert_eq!(debt.changes.unwrap().total, 1, "a live Doctor claim still counts");
    assert_eq!(debt.merge.unwrap().total, 1, "held, closed and issue rows are left out");

    // A listing with nothing in a queue records that axis as observed zero.
    record_open_listing(&ledger, &root(1), &[queue_row(7, "loom:issue", &[])]);
    let debt = ledger.repo_debt(&root(1), HOUR);
    for axis in DebtAxis::ALL {
        assert_eq!(debt.axis(axis), Some(AxisDebt::default()), "{axis:?}");
    }
}

#[test]
fn held_and_parked_prs_are_no_debt() {
    let merge = vec![
        queue_row(1, "loom:pr", &[]),
        queue_row(2, "loom:pr", &["loom:operator"]),
        queue_row(3, "loom:pr", &["loom:operator-only"]),
        queue_row(4, "loom:pr", &["loom:blocked"]),
        queue_row(5, "loom:pr", &["loom:auto-merge-ok"]),
        queue_row(6, "loom:pr", &["loom:frozen"]),
    ];
    assert_eq!(count_axis_rows(DebtAxis::Merge, &merge), 2, "#1 and #5 only");
    let changes = vec![
        queue_row(1, "loom:changes-requested", &["loom:needs-fix"]),
        queue_row(2, "loom:changes-requested", &["loom:blocked"]),
        queue_row(3, "loom:changes-requested", &["loom:operator"]),
        queue_row(4, "loom:changes-requested", &["loom:treating"]),
    ];
    assert_eq!(count_axis_rows(DebtAxis::Changes, &changes), 2, "#1 and #4");
}

/// #11861: a Champion hold out at Doctor (or handed back to Judge) keeps
/// `loom:blocked` and is still that role's work; the Doctor-cycle-cap park,
/// with the same labels and no hold record, is not.
#[test]
fn a_champion_hold_on_its_round_trip_is_debt() {
    let record = crate::park_record::render_park(
        &[],
        Some(crate::champion_hold::HOLD_BY),
        Some("2026-10-14T00:00:00Z"),
        Some(crate::champion_hold::MERGE_RISK_HOLD_REASON),
    );
    for (axis, label) in [
        (DebtAxis::Changes, "loom:changes-requested"),
        (DebtAxis::Review, "loom:review-requested"),
    ] {
        let held = RestIssue {
            body: Some(format!("A change.\n\n{record}\n")),
            ..queue_row(1, label, &["loom:blocked"])
        };
        assert_eq!(count_axis_rows(axis, std::slice::from_ref(&held)), 1, "{label}");
        let capped = queue_row(2, label, &["loom:blocked"]);
        assert_eq!(count_axis_rows(axis, &[capped]), 0, "{label}");
    }
}

#[test]
fn park_sets_are_the_work_finders() {
    for axis in [DebtAxis::Changes, DebtAxis::Review] {
        assert_eq!(axis_park_labels(axis), crate::work_finder::PARK_LABELS.as_slice());
    }
    assert_eq!(axis_park_labels(DebtAxis::Merge), &MERGE_HOLD_LABELS[..]);
    assert!(MERGE_HOLD_LABELS.contains(&crate::work_finder::FROZEN_LABEL));
    for axis in DebtAxis::ALL {
        assert_eq!(DebtAxis::for_label(axis.label()), Some(axis));
    }
    assert_eq!(DebtAxis::for_label("loom:curated"), None);
}
