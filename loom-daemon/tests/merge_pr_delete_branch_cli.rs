//! `loom-daemon merge-pr delete-branch` (#8191): the CLI wrapper over
//! `worktree_cli::branch_delete::maybe_delete_local_branch` that
//! `merge-pr.sh`'s `_maybe_delete_local_branch` now calls instead of
//! reimplementing the squash-aware `-d`/`-D` rule inline.
//!
//! `branch_delete.rs` already carries thorough unit coverage of the
//! underlying decision (`loom-daemon/src/worktree_cli/branch_delete/tests.rs`,
//! reused verbatim from the #8195 slice 3 port `worktree.sh remove` landed
//! first). What is new here, and untested until now, is the CLI seam itself:
//! argument wiring (`--repo-root`, `--branch`, `--expected-head-sha`,
//! `--default-branch`, `--no-cleanup-primary`) and the `LEVEL<TAB>message`
//! replay protocol `merge-pr.sh`'s shell wrapper parses back into its own
//! `info`/`warning`/`success` calls. These run the REAL built binary against
//! REAL git repositories — no mocks — because the whole point is proving the
//! wiring, not the decision logic a unit test already covers.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;
use std::process::{Command, Output};

fn git(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git could not be executed")
}

fn git_ok(dir: &Path, args: &[&str]) {
    let out = git(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn rev_parse(dir: &Path, rev: &str) -> String {
    let out = git(dir, &["rev-parse", rev]);
    assert!(out.status.success(), "rev-parse {rev} failed");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A repo with one commit on `main`, `main` as HEAD.
fn init_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git_ok(dir, &["init", "-q"]);
    git_ok(dir, &["config", "user.email", "test@example.com"]);
    git_ok(dir, &["config", "user.name", "Test"]);
    std::fs::write(dir.join("README.md"), "hello\n").unwrap();
    git_ok(dir, &["add", "-A"]);
    git_ok(dir, &["commit", "-q", "-m", "initial"]);
    git_ok(dir, &["branch", "-M", "main"]);
}

fn run_delete_branch(repo: &Path, branch: &str, expected_head_sha: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args([
            "merge-pr",
            "delete-branch",
            "--repo-root",
            repo.to_str().unwrap(),
            "--branch",
            branch,
            "--expected-head-sha",
            expected_head_sha,
            "--default-branch",
            "main",
        ])
        .output()
        .expect("could not exec loom-daemon merge-pr delete-branch")
}

/// One `(LEVEL, message)` pair per stdout line, tab-split exactly the way
/// `merge-pr.sh`'s `while IFS=$'\t' read -r level text` consumes it.
fn parse_lines(out: &Output) -> Vec<(String, String)> {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|line| {
            let mut parts = line.splitn(2, '\t');
            let level = parts.next().unwrap_or_default().to_string();
            let text = parts.next().unwrap_or_default().to_string();
            (level, text)
        })
        .collect()
}

/// Always exits 0 — "never fails the cleanup pipeline" is the CLI's own
/// contract, not just the underlying rule's, and `merge-pr.sh`'s wrapper
/// treats any non-zero exit as "the guard itself could not run" and skips the
/// whole replay, so a decision path that returned non-zero would silently
/// vanish rather than reach the operator.
#[test]
fn always_exits_zero_whatever_the_outcome() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);

    // (a) branch does not exist.
    let out = run_delete_branch(&repo, "feature/issue-nonexistent", "deadbeef");
    assert!(out.status.success(), "nonexistent-branch case must exit 0");

    // (b) default branch refusal.
    let out = run_delete_branch(&repo, "main", &rev_parse(&repo, "main"));
    assert!(out.status.success(), "default-branch refusal must exit 0");
}

/// A tip matching `--expected-head-sha` force-deletes (`-D`) and reports it as
/// a `SUCCESS` line — the exact wiring `merge-pr.sh`'s `success "$text"` arm
/// consumes.
#[test]
fn tip_match_force_deletes_and_emits_a_success_line() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);

    git_ok(&repo, &["checkout", "-q", "-b", "feature/issue-100"]);
    std::fs::write(repo.join("README.md"), "hello\nchange\n").unwrap();
    git_ok(&repo, &["commit", "-q", "-am", "issue 100 work"]);
    let match_sha = rev_parse(&repo, "feature/issue-100");
    git_ok(&repo, &["checkout", "-q", "main"]);

    let out = run_delete_branch(&repo, "feature/issue-100", &match_sha);
    assert!(out.status.success());
    let lines = parse_lines(&out);
    assert!(
        lines.iter().any(|(level, text)| level == "SUCCESS"
            && text.contains("Local branch 'feature/issue-100' deleted")
            && text.contains("safe force-delete")),
        "expected a SUCCESS force-delete line, got: {lines:?}"
    );
    let show_ref = git(
        &repo,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            "refs/heads/feature/issue-100",
        ],
    );
    assert!(!show_ref.status.success(), "the branch must actually be gone");
}

/// The repo's default branch is never a delete target, reported as a
/// `WARNING` line — never a `SUCCESS`, and the ref must survive.
#[test]
fn default_branch_is_refused_with_a_warning_line() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let base_sha = rev_parse(&repo, "main");

    let out = run_delete_branch(&repo, "main", &base_sha);
    assert!(out.status.success());
    let lines = parse_lines(&out);
    assert!(
        lines.iter().any(|(level, text)| level == "WARNING"
            && text.contains("it is the repository's default branch")),
        "expected a WARNING default-branch refusal, got: {lines:?}"
    );
    let show_ref = git(&repo, &["show-ref", "--verify", "--quiet", "refs/heads/main"]);
    assert!(show_ref.status.success(), "main must survive");
}

/// A branch that does not exist locally is a quiet `INFO` no-op — never a
/// `WARNING`, matching the pre-#8191 shell's "quiet info no-op" contract.
#[test]
fn nonexistent_branch_is_a_quiet_info_line() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);

    let out = run_delete_branch(&repo, "feature/issue-does-not-exist", "deadbeef");
    assert!(out.status.success());
    let lines = parse_lines(&out);
    assert_eq!(lines.len(), 1, "expected exactly one line, got: {lines:?}");
    assert_eq!(lines[0].0, "INFO");
    assert!(lines[0]
        .1
        .contains("does not exist — skipping branch delete"));
}

/// A branch checked out in another worktree is refused with the
/// checked-out-specific `WARNING`, never force-deleted.
#[test]
fn branch_checked_out_in_another_worktree_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    git_ok(&repo, &["branch", "feature/issue-300"]);
    let wt = tmp.path().join("wt2");
    git_ok(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            wt.to_str().unwrap(),
            "feature/issue-300",
        ],
    );

    let out = run_delete_branch(&repo, "feature/issue-300", "");
    assert!(out.status.success());
    let lines = parse_lines(&out);
    assert!(
        lines.iter().any(|(level, text)| level == "WARNING"
            && text.contains("checked out (current HEAD or another worktree)")),
        "expected the checked-out-elsewhere warning, got: {lines:?}"
    );
    let show_ref = git(
        &repo,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            "refs/heads/feature/issue-300",
        ],
    );
    assert!(show_ref.status.success(), "the branch must survive");
}
