//! #10382: the claim / merge-sequence pass family spends no GraphQL on its
//! changed-file and by-head reads. A fake `gh` refuses (exit 97, logging
//! `FORBIDDEN`) the old `gh pr view --json files` and `gh pr list --head`
//! calls and serves only their REST replacements, so a regression back to
//! either GraphQL call fails here — and the passes' decisions are unchanged.
//!
//! A sibling file: `claim_reconciliation.rs` and `tests.rs` are frozen by the
//! file-size ratchet (`scripts/file-size-baseline.txt`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::open_pr_listing::test_support::{pulls_arm, row};
use super::{forge, merge_sequence};
use crate::sweep_journal;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

const SHA: &str = "2222222222222222222222222222222222222222";

/// A fake `gh`: the old GraphQL calls are refused; `pulls_arm` serves the open
/// listing; `head_arm` is the by-head answer (a shell command); `/files?` pages
/// give PRs 1 and 2 a shared file and every other PR its own; any other `api`
/// read lists issue #80 as `loom:building` (no ETag, so nothing is cached).
fn fake_gh(dir: &Path, pulls: &str, head_arm: &str) -> (PathBuf, PathBuf) {
    let log = dir.join("gh.log");
    let bin = dir.join("fake-gh-rest-only.sh");
    let now = chrono::Utc::now().to_rfc3339();
    let script = format!(
        r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> '{log}'
case "$*" in
  'pr view'*'--json files'*|'pr list'*'--head'*) echo FORBIDDEN >> '{log}'; exit 97 ;;
esac
{pulls}case "$*" in
  *'pulls?head='*) {head_arm} ;;
  *'/files?'*)
    n="${{3%/files*}}"; n="${{n##*/}}"
    printf 'HTTP/2.0 200 OK\r\n\r\n'
    case "$n" in 1|2) echo '[{{"filename":"src/shared.rs"}}]' ;; *) echo "[{{\"filename\":\"docs/$n.md\"}}]" ;; esac
    exit 0 ;;
esac
if [ "$1" = "api" ] && [[ "$*" != */comments* ]]; then
  printf 'HTTP/2.0 200 OK\r\n\r\n'
  echo '[{{"number":80,"state":"open","labels":[{{"name":"loom:building"}}],"updated_at":"{now}"}}]'
fi
exit 0
"#,
        log = log.display(),
    );
    std::fs::write(&bin, script).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    (bin, log)
}

/// A by-head arm answering `200` with `rows` (a JSON array).
fn head_rows(rows: &str) -> String {
    format!("printf 'HTTP/2.0 200 OK\\r\\n\\r\\n'; echo '{rows}'; exit 0")
}

fn calls(log: &Path) -> String {
    std::fs::read_to_string(log).unwrap_or_default()
}

#[test]
#[serial]
fn the_merge_sequence_plan_reads_changed_files_over_rest_only() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let seq_row = |n: u32| {
        row(n, &[])
            .sha(SHA)
            .created(&format!("2026-10-0{n}T00:00:00Z"))
            .updated("2026-10-03T00:00:00Z")
    };
    let pulls = pulls_arm(&[seq_row(3), seq_row(2), seq_row(1)]);
    let (gh, log) = fake_gh(dir.path(), &pulls, "exit 1");
    let report = merge_sequence::plan_report(&gh, &root).unwrap();
    let log = calls(&log);
    assert!(!log.contains("FORBIDDEN"), "a GraphQL read was attempted: {log}");
    assert_eq!(report.open_prs, 3);
    for n in 1..=3 {
        assert!(log.contains(&format!("pulls/{n}/files?per_page=100&page=1")), "{log}");
    }
    // Unchanged decision: only the two PRs sharing a file form a group.
    let groups: Vec<Vec<u32>> = report.groups.iter().map(|g| g.order.clone()).collect();
    assert_eq!(groups, vec![vec![1, 2]], "{report:?}");
}

/// Seed issue `issue`'s checkpoint at `curator-done`, long past the grace.
fn seed_curator_done(root: &Path, issue: u32) {
    let dir = root.join(".loom").join("sweep-checkpoint");
    std::fs::create_dir_all(&dir).unwrap();
    let body = format!(
        r#"{{"phase":"curator-done","task_id":"sweep-{issue}","timestamp":"2026-01-01T00:00:00Z","pr_number":null}}"#
    );
    std::fs::write(dir.join(format!("issue-{issue}.json")), body).unwrap();
}

/// One claim pass over a fresh-labelled `curator-done` #80 whose by-head read
/// answers with `head_arm`: `(reclaimed, gh log)`.
fn claim_pass(head_arm: &str) -> (usize, String) {
    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, dir.path().join("sweeps.json"));
    seed_curator_done(&root, 80);
    let (gh, log) = fake_gh(dir.path(), "", head_arm);
    let (checked, reclaimed) = forge::reconcile_workspace(&gh, &root, false);
    std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
    assert_eq!(checked, 1);
    let log = calls(&log);
    assert!(!log.contains("FORBIDDEN"), "a GraphQL read was attempted: {log}");
    (reclaimed, log)
}

#[test]
#[serial]
fn the_no_progress_probe_and_reclaim_warning_read_by_head_over_rest() {
    // GitHub ignores a `head` it cannot resolve and lists every open PR: a
    // row on another branch must still read as "no PR on this branch".
    let (reclaimed, log) = claim_pass(&head_rows(
        r#"[{"number":9,"state":"open","head":{"ref":"feature/issue-800"}}]"#,
    ));
    assert_eq!(reclaimed, 1, "#4462 fast reclaim still fires: {log}");
    let by_head: Vec<&str> = log.lines().filter(|l| l.contains("pulls?head=")).collect();
    // The no-progress probe, then the post-reclaim diagnostic (#8116).
    assert_eq!(by_head.len(), 2, "{log}");
    for l in by_head {
        assert!(l.contains(":feature/issue-80&state=open"), "{l}");
    }
}

#[test]
#[serial]
fn an_open_pr_on_the_branch_blocks_the_fast_reclaim() {
    let (reclaimed, log) = claim_pass(&head_rows(
        r#"[{"number":4242,"state":"open","head":{"ref":"feature/issue-80"}}]"#,
    ));
    assert_eq!(reclaimed, 0, "{log}");
}

/// #4462 / #7863: an inconclusive by-head read is never "confirmed no PR".
#[test]
#[serial]
fn a_failed_by_head_read_never_fast_reclaims() {
    let (reclaimed, log) = claim_pass("echo 'gh: HTTP 502' >&2; exit 1");
    assert_eq!(reclaimed, 0, "a fresh label falls through to the age gate: {log}");
    assert!(log.contains("pulls?head="), "{log}");
}
