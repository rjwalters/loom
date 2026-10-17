//! Worktree footprint (#5939): pins the `worktrees` `--json` contract and
//! the human `Worktree footprint:` section. Both are pure functions of a
//! `[WorktreeDiskSummary]`, so nothing here touches a real filesystem —
//! `worktree_disk_status`'s own tests cover the collection side.
use super::{build_status_json_value, worktree_disk_lines};
use crate::cli::status::sample_report::sample_report;
use loom_daemon::worktree_disk_status::WorktreeDiskSummary;
use std::path::PathBuf;

fn no_update() -> loom_daemon::self_update::SelfUpdateStatus {
    loom_daemon::self_update::SelfUpdateStatus {
        built_commit: "abc1234".to_string(),
        source_commit: None,
        update_available: None,
        commits_behind: None,
        hours_behind: None,
    }
}

/// The shape #5939 measured on `loom-worker-1`: 14 `issue-*`, 110 `pr-*`,
/// 39 GB total, disk-bound at a third of the configured concurrency.
fn measured_host() -> WorktreeDiskSummary {
    WorktreeDiskSummary {
        root: PathBuf::from("/repos/loom"),
        total_count: 125,
        issue_count: 14,
        pr_count: 110,
        other_count: 1,
        total_bytes: Some(39 * 1024 * 1024 * 1024),
    }
}

#[test]
fn renders_the_headline_count_size_and_class_split() {
    let lines = worktree_disk_lines(Some(&[measured_host()])).unwrap();
    assert_eq!(
        lines[0],
        "Worktree footprint: 125 worktree(s), 39.0 GB on disk \
         (issue-* 14, pr-* 110, other 1)"
    );
    // A per-repo row carries the same figures, so a multi-repo host shows
    // WHICH repo is carrying the weight.
    let row = lines.iter().find(|l| l.contains("/repos/loom")).unwrap();
    assert!(row.contains("125"), "{row}");
    assert!(row.contains("39.0 GB"), "{row}");
    assert!(row.contains("110"), "{row}");
}

#[test]
fn unrecognized_naming_classes_get_an_explicit_note() {
    let lines = worktree_disk_lines(Some(&[measured_host()])).unwrap();
    let note = lines.iter().find(|l| l.contains("note:")).unwrap();
    assert!(note.contains("neither `issue-<N>` nor `pr-<N>`"), "{note}");
}

#[test]
fn no_note_when_every_worktree_matches_a_known_class() {
    let summary = WorktreeDiskSummary {
        other_count: 0,
        ..measured_host()
    };
    let lines = worktree_disk_lines(Some(&[summary])).unwrap();
    assert!(!lines.iter().any(|l| l.contains("note:")), "{lines:?}");
}

#[test]
fn totals_are_summed_across_managed_repos() {
    let a = WorktreeDiskSummary {
        root: PathBuf::from("/repos/a"),
        total_count: 3,
        issue_count: 2,
        pr_count: 1,
        other_count: 0,
        total_bytes: Some(1024),
    };
    let b = WorktreeDiskSummary {
        root: PathBuf::from("/repos/b"),
        total_count: 2,
        issue_count: 0,
        pr_count: 2,
        other_count: 0,
        total_bytes: Some(3072),
    };
    let lines = worktree_disk_lines(Some(&[a, b])).unwrap();
    assert_eq!(
        lines[0],
        "Worktree footprint: 5 worktree(s), 4.0 KB on disk (issue-* 2, pr-* 3, other 0)"
    );
}

#[test]
fn an_unmeasurable_repo_renders_unknown_not_a_false_zero() {
    let summary = WorktreeDiskSummary {
        root: PathBuf::from("/repos/unreadable"),
        total_count: 0,
        issue_count: 0,
        pr_count: 0,
        other_count: 0,
        total_bytes: None,
    };
    let lines = worktree_disk_lines(Some(&[summary])).unwrap();
    assert!(lines[0].contains("unknown on disk"), "{}", lines[0]);
    let row = lines
        .iter()
        .find(|l| l.contains("/repos/unreadable"))
        .unwrap();
    assert!(row.contains("unknown"), "{row}");
}

#[test]
fn nothing_renders_when_no_census_was_collected() {
    assert!(worktree_disk_lines(None).is_none());
}

#[test]
fn json_carries_the_per_repo_census() {
    let value = build_status_json_value(
        &sample_report(),
        None,
        &no_update(),
        None,
        None,
        Some(&[measured_host()]),
    );
    let w = &value["worktrees"][0];
    assert_eq!(w["root"], "/repos/loom");
    assert_eq!(w["total_count"], 125);
    assert_eq!(w["issue_count"], 14);
    assert_eq!(w["pr_count"], 110);
    assert_eq!(w["other_count"], 1);
    assert_eq!(w["total_bytes"], 39_u64 * 1024 * 1024 * 1024);
}

#[test]
fn json_worktrees_is_null_when_not_collected() {
    let value = build_status_json_value(&sample_report(), None, &no_update(), None, None, None);
    assert!(
        value["worktrees"].is_null(),
        "\"not collected\" must stay distinguishable from \"collected, and empty\""
    );
}
