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
        comments: 0,
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
        item(
            5,
            &[
                "loom:blocked",
                "tier:goal-advancing",
                "loom:operator-priority",
            ],
        ),
        // Already in the ready listing: the work finder has its row.
        item(6, &["loom:blocked", "loom:issue"]),
        // A PR in the REST issues listing.
        RestIssue {
            comments: 0,
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
    // Starred is reported; `urgent` is always false since #9244.
    assert!(row.operator_priority && !row.urgent);
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

// ---- which repo a listing names (W12 part 2, B1) ---------------------------

/// A fork-shaped checkout: `origin` is the fork, `upstream` the repo `gh`
/// resolves the slug to.
fn fork_checkout(dir: &std::path::Path) {
    for args in [
        &["init", "-q"][..],
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/fork-owner/app.git",
        ],
        &[
            "remote",
            "add",
            "upstream",
            "https://github.com/acme/app.git",
        ],
    ] {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }
}

/// A `gh` that records its arguments and answers an empty `200`.
fn recording_gh(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let log = dir.join("gh-args.log");
    let gh = dir.join("fake-gh.sh");
    std::fs::write(
        &gh,
        format!(
            "#!/bin/sh\necho \"$*\" >> '{}'\nprintf 'HTTP/2.0 200 OK\\r\\n\\r\\n[]\\n'\n",
            log.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    (gh, log)
}

fn listed(log: &std::path::Path) -> String {
    std::fs::read_to_string(log).unwrap_or_default()
}

#[test]
#[serial_test::serial]
fn the_captains_listing_names_the_published_slug_on_a_fork_checkout() {
    std::env::remove_var("LOOM_REPO");
    let checkout = tempfile::tempdir().unwrap();
    fork_checkout(checkout.path());
    let tools = tempfile::tempdir().unwrap();
    let (gh, log) = recording_gh(tools.path());

    // The captain names the slug it publishes under.
    super::list_open_with(&gh, checkout.path(), Some("acme/app"), "loom:blocked", "queue_blocked")
        .unwrap();
    let args = listed(&log);
    assert!(args.contains("repos/acme/app/issues"), "{args}");
    assert!(!args.contains("fork-owner"), "{args}");

    // Left to the helper, the same root lists `origin`: the fork. That is
    // what the captain published under the upstream's name before.
    std::fs::remove_file(&log).unwrap();
    super::list_open_with(&gh, checkout.path(), None, "loom:blocked", "queue_blocked").unwrap();
    assert!(listed(&log).contains("repos/fork-owner/app/issues"), "{}", listed(&log));
}

#[test]
#[serial_test::serial]
fn loom_repo_does_not_redirect_the_captains_listing() {
    let checkout = tempfile::tempdir().unwrap();
    fork_checkout(checkout.path());
    let tools = tempfile::tempdir().unwrap();
    let (gh, log) = recording_gh(tools.path());
    std::env::set_var("LOOM_REPO", "stray/other");
    let result = super::list_open_with(
        &gh,
        checkout.path(),
        Some("acme/app"),
        "loom:operator-priority",
        "star_liveness",
    );
    std::env::remove_var("LOOM_REPO");
    result.unwrap();
    let args = listed(&log);
    assert!(args.contains("repos/acme/app/issues"), "{args}");
    assert!(!args.contains("stray/other"), "{args}");
}
