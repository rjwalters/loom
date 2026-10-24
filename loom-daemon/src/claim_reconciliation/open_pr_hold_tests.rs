//! Issue #11852: claim reconciliation keeps `loom:building` while the issue's
//! PR is open, and restores `loom:issue` on the first pass after that PR
//! closes unmerged.

use super::super::open_pr_listing::test_support::{pulls_arm_cmd, row};
use super::super::*;
use crate::sweep_journal::{self, JournalEntry, SweepJournal};
use chrono::Utc;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;

/// A fake `gh` whose open-PR listing is whatever `pulls_file` holds at call
/// time (so a test can close the PR between passes), and whose building
/// listing is issue 99 with a stale claim.
fn fake_gh(
    dir: &std::path::Path,
    gh_log: &std::path::Path,
    pulls_file: &std::path::Path,
) -> std::path::PathBuf {
    let fake = dir.join("fake-gh-open-pr-hold.sh");
    let script = format!(
        r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
{pulls}case "$*" in
  api*--include*) printf 'HTTP/2.0 200 OK\r\n\r\n'
    echo '[{{"number":99,"state":"open","labels":[{{"name":"loom:building"}}],"updated_at":"{now}"}}]' ;;
  *"issues/99 --jq .state"*) echo open ;;
  *"issues/99 --jq"*) echo '["loom:building","loom:curated"]' ;;
esac
exit 0
"#,
        log = gh_log.display(),
        pulls = pulls_arm_cmd(&format!("cat '{}'", pulls_file.display())),
        now = Utc::now().to_rfc3339(),
    );
    std::fs::write(&fake, script).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    fake
}

#[test]
#[serial]
fn an_open_pr_holds_the_claim_and_an_unmerged_close_restores_the_queue_label() {
    // The repo the listing is read for, so the PR's same-repo head counts
    // (H14 drops an untrusted author's PR only from a fork).
    std::env::set_var("LOOM_REPO", "o/r");
    let dir = tempdir().unwrap();
    let repo_root = dir.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();
    let repo_str = repo_root.display().to_string();
    let journal_path = dir.path().join("sweeps.json");
    std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);
    // A dead pid: every pass decides to reclaim at once.
    let mut journal = SweepJournal::default();
    journal.entries.push(JournalEntry {
        repo: repo_str.clone(),
        issue: 99,
        pid: 0,
        started_at: Utc::now(),
    });
    sweep_journal::save(&journal_path, &journal).unwrap();
    let gh_log = dir.path().join("gh.log");
    let pulls_file = dir.path().join("pulls.json");
    let open = row(500, &["loom:review-requested"])
        .body("Closes #99")
        .repo("o/r")
        .json();
    std::fs::write(&pulls_file, serde_json::json!([open]).to_string()).unwrap();
    let gh = fake_gh(dir.path(), &gh_log, &pulls_file);

    // Pass 1: PR #500 is open — the claim stays, nothing is written.
    let (checked, reclaimed) = forge::reconcile_workspace(&gh, &repo_root, false);
    assert_eq!((checked, reclaimed), (1, 0), "a held claim is not a reclaim");
    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(!calls.contains("issue edit"), "no label write while the PR is open: {calls}");
    let after = sweep_journal::load(&journal_path);
    assert!(sweep_journal::find(&after, &repo_str, 99).is_some(), "the evidence survives");

    // The PR closes unmerged (the issue stays open). Pass 2 restores loom:issue.
    std::fs::write(&pulls_file, "[]").unwrap();
    let (checked, reclaimed) = forge::reconcile_workspace(&gh, &repo_root, false);
    std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
    std::env::remove_var("LOOM_REPO");
    assert_eq!((checked, reclaimed), (1, 1));
    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        calls.contains("issue edit 99 --remove-label loom:building --add-label loom:issue"),
        "the next pass after an unmerged close re-queues the issue: {calls}"
    );
}

/// Fail-open: an unreadable open-PR listing reclaims as before.
#[test]
#[serial]
fn an_unreadable_listing_reclaims_as_before() {
    std::env::remove_var("LOOM_REPO");
    let dir = tempdir().unwrap();
    let pulls_file = dir.path().join("pulls.json");
    std::fs::write(&pulls_file, "not json").unwrap();
    let gh = fake_gh(dir.path(), &dir.path().join("gh.log"), &pulls_file);
    assert_eq!(super::open_linked_pr(&gh, dir.path(), 99), None);
}
