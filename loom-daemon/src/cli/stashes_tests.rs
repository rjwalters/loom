//! #8876: a `stashes` run that could read no issue state fails; any other
//! run keeps its exit status. Real `git` in a tempdir, a fake forge.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use loom_daemon::stash_retirement::{list_quarantine_stashes, IssueStateLookup};

use super::{run_with, RunOptions};

/// Open for every issue in the table, unknown for the rest.
struct OpenFor(HashMap<u64, bool>, usize);

impl OpenFor {
    fn issues(issues: &[u64]) -> Self {
        Self(issues.iter().map(|n| (*n, false)).collect(), 0)
    }
}

impl IssueStateLookup for OpenFor {
    fn is_closed(&mut self, issue: u64) -> Option<bool> {
        self.1 += 1;
        self.0.get(&issue).copied()
    }
}

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(status.status.success(), "git {args:?}: {status:?}");
}

/// A repo with one quarantine stash per label, newest last.
fn repo_with_stashes(labels: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("file.txt"), "base\n").unwrap();
    git(repo, &["add", "file.txt"]);
    git(repo, &["commit", "-q", "-m", "base"]);
    for (i, label) in labels.iter().enumerate() {
        std::fs::write(repo.join("file.txt"), format!("edit {i}\n")).unwrap();
        git(repo, &["stash", "push", "-q", "-m", label]);
    }
    dir
}

fn opts(issue_filter: Option<u64>, execute: bool, json: bool) -> RunOptions {
    RunOptions {
        issue_filter,
        execute,
        show_paths: false,
        json,
    }
}

const THREE: [&str; 3] = [
    "loom-quarantine: issue=969",
    "loom-quarantine: run=sweep-x issue=992",
    "loom-quarantine: issue=1001",
];

#[test]
fn a_run_whose_every_lookup_fails_exits_non_zero_and_drops_nothing() {
    let repo = repo_with_stashes(&THREE);
    // `list`, `retire`, `retire --execute`, and `--json`: all the same.
    for (execute, json) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut blind = OpenFor::issues(&[]);
        let err = run_with(repo.path(), &opts(None, execute, json), &mut blind)
            .expect_err("a blind run must not exit 0");
        let message = err.to_string();
        assert!(message.starts_with("issue state unreadable for all 3 stash(es)"), "{message}");
        assert_eq!(blind.1, 3, "each distinct issue was looked up once");
    }
    assert_eq!(list_quarantine_stashes(repo.path()).unwrap().len(), 3);
}

#[test]
fn one_unreadable_issue_among_readable_ones_still_exits_zero() {
    let repo = repo_with_stashes(&THREE);
    let mut mixed = OpenFor::issues(&[969, 1001]);
    run_with(repo.path(), &opts(None, false, false), &mut mixed).unwrap();
    let mut mixed = OpenFor::issues(&[969, 1001]);
    run_with(repo.path(), &opts(None, true, true), &mut mixed).unwrap();
}

#[test]
fn stashes_with_no_issue_token_do_not_make_a_run_blind() {
    let repo = repo_with_stashes(&["loom-quarantine: unattributed", "loom-quarantine: x"]);
    let mut blind = OpenFor::issues(&[]);
    run_with(repo.path(), &opts(None, false, false), &mut blind).unwrap();
    assert_eq!(blind.1, 0, "no issue token, no lookup");
}

#[test]
fn an_issue_filter_scopes_the_blind_check_to_that_issue() {
    let repo = repo_with_stashes(&THREE);
    // Only #992 is unreadable. Scoped to it, the run read nothing.
    let mut lookup = OpenFor::issues(&[969, 1001]);
    assert!(run_with(repo.path(), &opts(Some(992), false, false), &mut lookup).is_err());
    // Scoped to a readable issue, or to one with no stashes, it is fine.
    let mut lookup = OpenFor::issues(&[969, 1001]);
    run_with(repo.path(), &opts(Some(969), false, false), &mut lookup).unwrap();
    let mut lookup = OpenFor::issues(&[]);
    run_with(repo.path(), &opts(Some(5), false, true), &mut lookup).unwrap();
}

#[test]
fn a_repo_with_no_stashes_exits_zero() {
    let repo = repo_with_stashes(&[]);
    let mut blind = OpenFor::issues(&[]);
    run_with(repo.path(), &opts(None, true, false), &mut blind).unwrap();
}
