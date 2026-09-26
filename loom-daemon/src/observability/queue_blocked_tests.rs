//! Forge-side `loom:blocked` queue rows (Issue #8957).

use std::collections::HashMap;

use super::{append, blocked_rows, UNRANKED};
use crate::forge_listing::RestIssue;
use crate::observability::queue_snapshot::build_record;
use crate::telemetry::queue_snapshot::{QueueRepoRef, MAX_ROWS};
use crate::telemetry::RepoVisibility;
use crate::types::{QueueDisposition, WorkFinderTickSummary};

fn item(number: u32, labels: &[&str]) -> RestIssue {
    RestIssue {
        number,
        title: Some("secret title".into()),
        labels: labels.iter().map(|l| (*l).to_string()).collect(),
        created_at: Some("2026-09-01T00:00:00Z".into()),
        updated_at: None,
        closed_at: None,
        state: "open".into(),
        body: Some("Blocked on the vendor contract; see comment".into()),
        author: Some("someone".into()),
        is_pull_request: false,
    }
}

fn repo(visibility: RepoVisibility) -> QueueRepoRef {
    QueueRepoRef {
        repo: "acme/app".into(),
        visibility,
    }
}

#[test]
fn a_blocked_issue_without_loom_issue_becomes_an_unranked_blocked_row() {
    let listing = vec![
        item(5, &["loom:blocked", "tier:goal-advancing", "loom:urgent"]),
        // Already in the ready listing: the work finder has its row.
        item(6, &["loom:blocked", "loom:issue"]),
        // A PR in the REST issues listing.
        RestIssue {
            is_pull_request: true,
            ..item(7, &["loom:blocked"])
        },
    ];
    let rows = blocked_rows(&repo(RepoVisibility::Public), &listing);
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.issue, 5);
    assert_eq!(row.rank, UNRANKED);
    assert_eq!(row.repo, "acme/app");
    assert_eq!(row.disposition, QueueDisposition::LabelledBlocked);
    assert_eq!(row.state, "blocked");
    assert_eq!(row.reason, "blocked: labelled loom:blocked");
    assert!(row.urgent);
    assert_eq!(row.tier.as_deref(), Some("tier:goal-advancing"));
    assert_eq!(row.detail, None);
    let wire = serde_json::to_value(row).unwrap();
    assert_eq!(wire["disposition"], "labelled_blocked");
}

#[test]
fn detail_is_only_allowlisted_hold_labels_never_free_text() {
    let listing = vec![item(
        9,
        &[
            "loom:blocked",
            "loom:operator",
            "loom:needs-capability",
            "custom:secret",
        ],
    )];
    let rows = blocked_rows(&repo(RepoVisibility::Private), &listing);
    assert_eq!(rows[0].detail.as_deref(), Some("loom:operator, loom:needs-capability"));
    assert_eq!(rows[0].visibility, RepoVisibility::Private);
    let wire = serde_json::to_string(&rows[0]).unwrap();
    for leaked in [
        "secret title",
        "vendor contract",
        "someone",
        "custom:secret",
    ] {
        assert!(!wire.contains(leaked), "{leaked} leaked into {wire}");
    }
}

#[test]
fn appended_rows_count_as_blocked_and_respect_the_row_cap() {
    let mut record = build_record(&WorkFinderTickSummary::default(), &HashMap::new());
    let listing: Vec<RestIssue> = (1..=3).map(|n| item(n, &["loom:blocked"])).collect();
    let rows = blocked_rows(&repo(RepoVisibility::Public), &listing);
    append(&mut record, rows.clone());
    assert_eq!(record.rows.len(), 3);
    assert_eq!(record.counts.blocked, 3);
    // A second root resolving to the same repo does not duplicate rows.
    append(&mut record, rows);
    assert_eq!(record.rows.len(), 3);

    let many: Vec<RestIssue> = (1..=u32::try_from(MAX_ROWS).unwrap() + 2)
        .map(|n| item(n + 100, &["loom:blocked"]))
        .collect();
    append(&mut record, blocked_rows(&repo(RepoVisibility::Public), &many));
    assert_eq!(record.rows.len(), MAX_ROWS);
    assert_eq!(record.rows_truncated, 5);
    assert_eq!(record.counts.blocked, 3 + MAX_ROWS + 2);
}
