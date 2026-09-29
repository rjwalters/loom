//! Unit tests for the "worktree directory already exists" arm (#8195 slice 12).
//!
//! Every case runs against a **real throwaway repo with a real linked worktree**
//! — the decision's first question is *"does `git worktree list` know this
//! directory?"*, and a mock of git would be a mock of the thing under test. That
//! is also the only way the two defects this slice retires are reachable at all:
//! one needs a repo reached through a **symlink**, the other needs two worktrees
//! whose paths are **prefixes of each other**.
//!
//! The destructive step is injected ([`decide`]'s `reset` closure), so the reset
//! arm's decision, sentinel write and message set are exercised without a real
//! `git reset --hard`. Its own behaviour is [`super::super::reset`]'s 25-assertion
//! suite plus `test-worktree-race-rescue.sh`; what is asserted HERE is that this
//! arm reaches it with the reference the verdict was reached against, and only
//! when the verdict is "stale".

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::*;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn tmpdir(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "loom-wt-existing-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).expect("create tmpdir");
    fs::canonicalize(&base).expect("canonicalize tmpdir")
}

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Loom Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "Loom Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .env("LC_ALL", "C")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// A repo at `<dir>/<name>` with one `main` commit and no remote.
///
/// No `origin` deliberately: [`super::super::stale_ref`] then finds no
/// `origin/<branch>` and falls back to `base_ref`, which keeps these cases about
/// the arm's own decision rather than about the reference resolution (which has
/// its own suite).
fn repo(dir: &Path, name: &str) -> PathBuf {
    let work = dir.join(name);
    assert!(Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .arg(&work)
        .status()
        .expect("git init")
        .success());
    git(&work, &["config", "user.name", "Loom Test"]);
    git(&work, &["config", "user.email", "t@example.invalid"]);
    fs::write(work.join("base.txt"), "base\n").expect("write");
    git(&work, &["add", "base.txt"]);
    git(&work, &["commit", "-q", "-m", "base"]);
    work
}

/// `git worktree add <path> -b <branch>` — a REGISTERED linked worktree.
fn add_worktree(repo: &Path, path: &Path, branch: &str) {
    git(
        repo,
        &[
            "worktree",
            "add",
            path.to_str().expect("utf-8"),
            "-b",
            branch,
        ],
    );
}

fn opts(repo: &Path, worktree: &Path, branch: &str) -> Options {
    Options {
        worktree: worktree.to_path_buf(),
        repo: repo.to_path_buf(),
        issue: "42".to_string(),
        branch: branch.to_string(),
        default_branch: "main".to_string(),
        base_ref: "main".to_string(),
        base_display: "main".to_string(),
        base_branch: String::new(),
        quiet: true,
        ignore_pids: vec![std::process::id()],
    }
}

/// [`decide`] with a reset that records what it was asked to do and reports
/// success. Returns the outcome and the target ref it saw (if any).
fn decide_recording(opts: &Options) -> (Outcome, Option<String>) {
    let seen = std::cell::RefCell::new(None);
    let outcome = decide(opts, &|req| {
        *seen.borrow_mut() = Some(req.target_ref.clone());
        true
    });
    (outcome, seen.into_inner())
}

fn sentinel_exists(worktree: &Path) -> bool {
    worktree.join(super::super::sentinel::FILE_NAME).is_file()
}

// ---------------------------------------------------------------------------
// 1. The registration probe — the defect this slice retires
// ---------------------------------------------------------------------------

/// The plain case: a live registered worktree is not refused.
#[test]
fn a_registered_worktree_is_not_refused() {
    let tmp = tmpdir("registered");
    let repo_dir = repo(&tmp, "repo");
    let wt = tmp.join("wt");
    add_worktree(&repo_dir, &wt, "feature/issue-42");

    let (outcome, _) = decide_recording(&opts(&repo_dir, &wt, "feature/issue-42"));
    assert_ne!(outcome, Outcome::Unregistered);
}

/// A directory that exists but is NOT a worktree is refused — and, critically,
/// gets no `.loom-managed` sentinel: that marker is what authorizes `rm -rf`
/// tooling, and writing one into crash debris is how the retired substring
/// match's false-positive side ended (#3334).
#[test]
fn an_unregistered_directory_is_refused_and_gets_no_sentinel() {
    let tmp = tmpdir("unregistered");
    let repo_dir = repo(&tmp, "repo");
    let debris = repo_dir.join(".loom/worktrees/issue-42");
    fs::create_dir_all(&debris).expect("mkdir debris");

    let (outcome, reset_target) = decide_recording(&opts(&repo_dir, &debris, "feature/issue-42"));
    assert_eq!(outcome, Outcome::Unregistered);
    assert_eq!(outcome.code(), 1);
    assert!(!sentinel_exists(&debris), "sentinel written into crash debris");
    assert!(
        reset_target.is_none(),
        "the reset must not be reached for an unregistered directory"
    );
}

/// **The #7858-class false negative.** `git worktree list` prints
/// symlink-RESOLVED paths; the retired `grep -q "$WORKTREE_PATH"` compared them
/// against the unresolved path the script built by concatenation. Reached through
/// a symlinked repo root — which is every macOS checkout under `/tmp` — the live
/// worktree's path was absent from that output and the arm refused it, advising
/// `rm -rf` on a worktree that may hold uncommitted work.
///
/// This test fails if the probe ever goes back to a textual comparison.
#[test]
fn a_live_worktree_reached_through_a_symlink_is_still_registered() {
    let tmp = tmpdir("symlink");
    let repo_dir = repo(&tmp, "repo");
    let wt = tmp.join("wt");
    add_worktree(&repo_dir, &wt, "feature/issue-42");

    // The caller reaches both the repo and the worktree through a symlink, the
    // way `/tmp` -> `/private/tmp` does on macOS for every shell fixture.
    let link = tmp.join("link");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&tmp, &link).expect("symlink");
    let via_link_repo = link.join("repo");
    let via_link_wt = link.join("wt");

    let (outcome, _) = decide_recording(&opts(&via_link_repo, &via_link_wt, "feature/issue-42"));
    assert_ne!(
        outcome,
        Outcome::Unregistered,
        "a live worktree reached through a symlink was refused — the retired substring match's bug"
    );
}

/// **The false-positive side.** `…/issue-4` is a substring of the registered
/// `…/issue-44`, so the retired `grep -q` answered "registered" for an
/// unregistered directory, then ran the drift check and the reset with
/// `git -C <unregistered dir>` — which resolves to the PARENT repo — and wrote a
/// sentinel into it.
#[test]
fn a_prefix_of_a_registered_worktree_path_is_not_registered() {
    let tmp = tmpdir("prefix");
    let repo_dir = repo(&tmp, "repo");
    let registered = tmp.join("issue-44");
    add_worktree(&repo_dir, &registered, "feature/issue-44");

    let lookalike = tmp.join("issue-4");
    fs::create_dir_all(&lookalike).expect("mkdir lookalike");

    let (outcome, _) = decide_recording(&opts(&repo_dir, &lookalike, "feature/issue-4"));
    assert_eq!(
        outcome,
        Outcome::Unregistered,
        "issue-4 was accepted because it is a substring of the registered issue-44"
    );
    assert!(!sentinel_exists(&lookalike));
}

/// A path containing a **space** is the #7858 vector itself. The porcelain
/// reader must read the path whole rather than word-splitting it, in both
/// directions: the registered worktree-with-a-space is accepted, and an
/// unregistered sibling next to it is still refused.
#[test]
fn paths_containing_spaces_are_matched_whole() {
    let tmp = tmpdir("spaces");
    let repo_dir = repo(&tmp, "my repo");
    let wt = tmp.join("work tree 42");
    add_worktree(&repo_dir, &wt, "feature/issue-42");

    let (registered, _) = decide_recording(&opts(&repo_dir, &wt, "feature/issue-42"));
    assert_ne!(
        registered,
        Outcome::Unregistered,
        "a registered worktree whose path contains spaces was refused"
    );

    let debris = tmp.join("work tree 42 leftovers");
    fs::create_dir_all(&debris).expect("mkdir");
    let (unregistered, _) = decide_recording(&opts(&repo_dir, &debris, "feature/issue-42"));
    assert_eq!(
        unregistered,
        Outcome::Unregistered,
        "an unregistered dir whose path shares a prefix with a registered one was accepted"
    );
    assert!(!sentinel_exists(&debris));
}

/// A repo path containing a regex metacharacter. The retired needle went to
/// `grep`, not `grep -F`, so `+` and `.` were pattern syntax; the port compares
/// canonicalized paths, where they are just bytes.
#[test]
fn a_regex_metacharacter_in_the_path_is_not_a_pattern() {
    let tmp = tmpdir("metachar");
    let repo_dir = repo(&tmp, "c++.repo");
    let wt = tmp.join("wt+1");
    add_worktree(&repo_dir, &wt, "feature/issue-42");

    let (outcome, _) = decide_recording(&opts(&repo_dir, &wt, "feature/issue-42"));
    assert_ne!(outcome, Outcome::Unregistered);
}

// ---------------------------------------------------------------------------
// 2. The verdict
// ---------------------------------------------------------------------------

/// Commits ahead of the reference => preserve, and back-fill the sentinel
/// (#3548: a resumed worktree that lost its marker must stay cleanup-eligible).
#[test]
fn commits_ahead_preserve_the_worktree_and_backfill_the_sentinel() {
    let tmp = tmpdir("ahead");
    let repo_dir = repo(&tmp, "repo");
    let wt = tmp.join("wt");
    add_worktree(&repo_dir, &wt, "feature/issue-42");
    fs::write(wt.join("work.txt"), "work\n").expect("write");
    git(&wt, &["add", "work.txt"]);
    git(&wt, &["commit", "-q", "-m", "real work"]);

    let (outcome, reset_target) = decide_recording(&opts(&repo_dir, &wt, "feature/issue-42"));
    assert_eq!(outcome, Outcome::Preserved);
    assert_eq!(outcome.code(), 0);
    assert!(sentinel_exists(&wt), "#3548 sentinel back-fill did not happen");
    assert!(
        reset_target.is_none(),
        "a worktree with commits ahead must never reach the reset"
    );
}

/// Uncommitted changes and NO commits ahead => preserve. The reading is taken by
/// the arm itself, so a worktree dirtied between the caller's own check and this
/// call is still preserved rather than reset.
#[test]
fn uncommitted_changes_preserve_the_worktree() {
    let tmp = tmpdir("dirty");
    let repo_dir = repo(&tmp, "repo");
    let wt = tmp.join("wt");
    add_worktree(&repo_dir, &wt, "feature/issue-42");
    fs::write(wt.join("base.txt"), "locally edited\n").expect("write");

    let (outcome, reset_target) = decide_recording(&opts(&repo_dir, &wt, "feature/issue-42"));
    assert_eq!(outcome, Outcome::Preserved);
    assert!(reset_target.is_none(), "a dirty worktree must never reach the reset");
}

/// An untracked file counts too — `git status --porcelain` reports it, and the
/// shell's non-empty test did not distinguish. Asserted separately because
/// `git diff HEAD` (the #6334 guard's own signal) does NOT report untracked
/// files, so the two questions genuinely differ and this one must stay the
/// broader `status --porcelain` reading.
#[test]
fn an_untracked_file_also_preserves_the_worktree() {
    let tmp = tmpdir("untracked");
    let repo_dir = repo(&tmp, "repo");
    let wt = tmp.join("wt");
    add_worktree(&repo_dir, &wt, "feature/issue-42");
    fs::write(wt.join("scratch.txt"), "notes\n").expect("write");

    let (outcome, _) = decide_recording(&opts(&repo_dir, &wt, "feature/issue-42"));
    assert_eq!(outcome, Outcome::Preserved);
}

/// Clean and level with the reference => stale, and the reset is reached with
/// **that** reference as its target (#8287: the verdict's reference and the
/// reset's target are one value, not two derivations).
#[test]
fn a_stale_worktree_resets_to_the_reference_the_verdict_used() {
    let tmp = tmpdir("stale");
    let repo_dir = repo(&tmp, "repo");
    let wt = tmp.join("wt");
    add_worktree(&repo_dir, &wt, "feature/issue-42");

    let (outcome, reset_target) = decide_recording(&opts(&repo_dir, &wt, "feature/issue-42"));
    assert_eq!(outcome, Outcome::Reset);
    assert_eq!(outcome.code(), 0);
    assert_eq!(reset_target.as_deref(), Some("main"));
    assert!(sentinel_exists(&wt), "the reset arm must back-fill the sentinel too");
}

/// A refused or failed reset is still exit 0 with the worktree left alone — the
/// shell's *"Could not reset stale worktree (continuing to use as-is)"*. This is
/// the #6334 guard's refusal landing, and it must not become a refusal to hand
/// the worktree over.
#[test]
fn a_refused_reset_leaves_the_worktree_usable() {
    let tmp = tmpdir("reset-refused");
    let repo_dir = repo(&tmp, "repo");
    let wt = tmp.join("wt");
    add_worktree(&repo_dir, &wt, "feature/issue-42");

    let outcome = decide(&opts(&repo_dir, &wt, "feature/issue-42"), &|_| false);
    assert_eq!(outcome, Outcome::ResetFailed);
    assert_eq!(outcome.code(), 0);
    assert!(
        sentinel_exists(&wt),
        "the sentinel is written before the reset attempt and survives its refusal"
    );
}

/// Every usable outcome is exit 0 and only the two refusals are exit 1 — the
/// mapping `worktree.sh` branches on.
#[test]
fn only_the_refusals_are_nonzero() {
    assert_eq!(Outcome::Preserved.code(), 0);
    assert_eq!(Outcome::Reset.code(), 0);
    assert_eq!(Outcome::ResetFailed.code(), 0);
    assert_eq!(Outcome::Unregistered.code(), 1);
    assert_eq!(Outcome::SentinelFailed.code(), 1);
}
