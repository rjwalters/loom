//! Unit tests for the staleness-reference decision (#8287 / #8354).
//!
//! Every case runs against a **real throwaway git repo** — a bare `origin` plus
//! a working clone — because the whole decision is a question about which of
//! two refs exists and what `rev-list` says about them, and a mock of git would
//! be a mock of the thing under test. That is the same shape the shell-level
//! fixtures in `defaults/scripts/tests/test-worktree-existing-dir-drift-check.sh`
//! use, and those two suites deliberately overlap: this one owns the decision,
//! that one owns `worktree.sh` actually honouring it end to end.
//!
//! The forge rung is the one thing injected ([`resolve_with`]) — the same seam
//! [`branch_landed::probe_with`] exposes, so no case here touches a network.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::*;
use crate::worktree_cli::branch_landed::ForgeStatus;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn tmpdir(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "loom-stale-ref-{tag}-{}-{:?}",
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
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn rev(dir: &Path, spec: &str) -> String {
    String::from_utf8_lossy(&git(dir, &["rev-parse", spec]).stdout)
        .trim()
        .to_string()
}

/// A working clone at `<dir>/work` with a bare `origin`, one `main` commit, and
/// a `<branch>` carrying one commit of its own.
///
/// `push_branch` decides whether `origin/<branch>` exists at all — the
/// difference between "an in-flight PR branch" and "a local branch nobody ever
/// pushed".
fn repo(dir: &Path, branch: &str, push_branch: bool) -> PathBuf {
    let origin = dir.join("origin.git");
    let work = dir.join("work");
    assert!(Command::new("git")
        .args(["init", "-q", "-b", "main", "--bare"])
        .arg(&origin)
        .status()
        .expect("git init --bare")
        .success());
    assert!(Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .arg(&work)
        .status()
        .expect("git init")
        .success());
    git(&work, &["config", "user.name", "Loom Test"]);
    git(&work, &["config", "user.email", "t@example.invalid"]);
    git(&work, &["remote", "add", "origin", origin.to_str().expect("utf-8")]);
    fs::write(work.join("base.txt"), "base\n").expect("write");
    git(&work, &["add", "base.txt"]);
    git(&work, &["commit", "-q", "-m", "base"]);
    git(&work, &["push", "-q", "origin", "main"]);
    git(&work, &["checkout", "-q", "-b", branch]);
    fs::write(work.join("work.txt"), "work\n").expect("write");
    git(&work, &["add", "work.txt"]);
    git(&work, &["commit", "-q", "-m", "work"]);
    if push_branch {
        git(&work, &["push", "-q", "origin", branch]);
    }
    work
}

/// The incident shape: the LOCAL branch is rewound to the base while
/// `origin/<branch>` keeps the branch's real commit. "0 commits ahead of main,
/// no uncommitted changes" by the pre-#8287 criterion, with the PR's content
/// reachable only through the remote ref.
fn rewind_local_to_base(work: &Path) {
    git(work, &["reset", "-q", "--hard", "origin/main"]);
}

fn opts(work: &Path, branch: &str) -> Options {
    Options {
        worktree: work.to_path_buf(),
        branch: branch.to_string(),
        default_branch: "main".into(),
        base_ref: "origin/main".into(),
        base_display: "main".into(),
    }
}

/// A forge that reports nothing merged — the live-PR case.
fn no_merged_pr(_: &str) -> ForgeProbe {
    ForgeProbe {
        status: ForgeStatus::NotFound,
        head_sha: None,
        number: None,
    }
}

/// A forge that reports a merged PR whose head is `sha` — the #5657 case.
fn merged_at(sha: &str) -> impl Fn(&str) -> ForgeProbe + '_ {
    move |_| ForgeProbe {
        status: ForgeStatus::Found,
        head_sha: Some(sha.to_string()),
        number: Some("999".into()),
    }
}

// ---------------------------------------------------------------------------
// The decision
// ---------------------------------------------------------------------------

/// The #8147/#8190 incident, stated as the property that closes it: with a
/// live, unmerged `origin/<branch>`, the reference is that remote ref — never
/// the base — so the reset below it cannot discard the PR's only local trace.
#[test]
fn a_live_unmerged_remote_branch_is_the_reference() {
    let dir = tmpdir("live-remote");
    let work = repo(&dir, "feature/issue-401", true);
    rewind_local_to_base(&work);

    let r = resolve_with(&opts(&work, "feature/issue-401"), &no_merged_pr);

    assert_eq!(r.reference, "origin/feature/issue-401");
    assert_eq!(r.display, "origin/feature/issue-401");
    assert_eq!(r.ahead, 0, "the rewound local branch is not ahead of it");
    assert_eq!(r.behind, 1, "…it is one commit behind the pushed tip");
}

/// The #5657 skip, on this code path: an `origin/<branch>` whose tip is the
/// head of an already-merged PR is dead history, so the base is still the
/// correct reference. Without this, a reused branch name would reset a worktree
/// onto merged work as though it were pending.
#[test]
fn an_already_merged_remote_tip_falls_back_to_the_base() {
    let dir = tmpdir("merged-remote");
    let work = repo(&dir, "feature/issue-402", true);
    let tip = rev(&work, "refs/remotes/origin/feature/issue-402");
    rewind_local_to_base(&work);

    let r = resolve_with(&opts(&work, "feature/issue-402"), &merged_at(&tip));

    assert_eq!(r.reference, "origin/main");
    assert_eq!(r.display, "main", "the base keeps its human-facing spelling");
    assert_eq!(r.ahead, 0);
    assert_eq!(r.behind, 0);
}

/// The landed check must judge the REMOTE tip, not the stale LOCAL one. Both
/// refs exist here by construction (that is the code path's premise), and
/// `branch_landed`'s resolution ladder prefers a local ref — so a key of the
/// bare branch name would ask about the rewound local tip, which IS an ancestor
/// of `origin/main` and therefore "landed", wrongly falling back to the base
/// while the real PR content sits on the remote.
#[test]
fn the_landed_check_keys_on_the_remote_tip_not_the_stale_local_one() {
    let dir = tmpdir("remote-key");
    let work = repo(&dir, "feature/issue-403", true);
    let local_tip_after_rewind = rev(&work, "refs/remotes/origin/main");
    rewind_local_to_base(&work);
    assert_eq!(
        rev(&work, "HEAD"),
        local_tip_after_rewind,
        "fixture precondition: the local tip IS origin/main, i.e. 'landed'"
    );

    // A forge that answers "merged, head = the LOCAL tip". Keyed on the remote
    // tip (as the port does) this is a head MISMATCH, so the remote ref is
    // kept; keyed on the local tip it would be a match and the decision would
    // collapse back to the base.
    let r = resolve_with(&opts(&work, "feature/issue-403"), &merged_at(&local_tip_after_rewind));
    assert_eq!(r.reference, "origin/feature/issue-403");
}

/// No `origin/<branch>` at all — an unpushed local branch — is the pre-#8287
/// world unchanged: the base is the reference, and the forge is never asked
/// (there is nothing to ask about, and this path must not pay a round-trip).
#[test]
fn an_unpushed_branch_keeps_the_base_and_never_asks_the_forge() {
    let dir = tmpdir("unpushed");
    let work = repo(&dir, "feature/issue-404", false);

    let r = resolve_with(&opts(&work, "feature/issue-404"), &|_| {
        panic!("the forge must not be consulted without a remote branch")
    });

    assert_eq!(r.reference, "origin/main");
    assert_eq!(r.display, "main");
    assert_eq!(r.ahead, 1, "the local branch still carries its own commit");
    assert_eq!(r.behind, 0);
}

/// A forge that cannot answer (`unavailable`) must NOT fall back to the base.
/// `Unknown` is not `Landed`, and the fail-closed direction here is *preserve
/// the remote content*: over-preserving costs a rebase, under-preserving costs
/// a PR. Pinned separately from the `not-found` case because collapsing
/// `Unknown` into "landed" is the classic defect at this boundary.
#[test]
fn an_unanswerable_forge_still_keeps_the_live_remote_reference() {
    let dir = tmpdir("forge-down");
    let work = repo(&dir, "feature/issue-405", true);
    rewind_local_to_base(&work);

    let r = resolve_with(&opts(&work, "feature/issue-405"), &|_| ForgeProbe::unavailable());

    assert_eq!(r.reference, "origin/feature/issue-405");
}

// ---------------------------------------------------------------------------
// Stacked children (#3729)
// ---------------------------------------------------------------------------

/// The reason the default branch is a PARAMETER and not the literal
/// `origin/main`: a stacked child measures against its parent. With no live
/// remote branch the caller's `--base-ref` / `--base-display` are passed
/// through verbatim, counts included.
#[test]
fn a_stacked_child_measures_against_its_parent_base_ref() {
    let dir = tmpdir("stacked");
    let work = repo(&dir, "feature/issue-500", false);
    // Give the "parent" a pushed branch the child sits on top of.
    git(&work, &["branch", "-q", "feature/issue-499", "HEAD~1"]);
    git(&work, &["push", "-q", "origin", "feature/issue-499"]);

    let r = resolve_with(
        &Options {
            base_ref: "origin/feature/issue-499".into(),
            base_display: "origin/feature/issue-499".into(),
            ..opts(&work, "feature/issue-500")
        },
        &|_| panic!("no remote branch for the child; the forge is not asked"),
    );

    assert_eq!(r.reference, "origin/feature/issue-499");
    assert_eq!(r.display, "origin/feature/issue-499");
    assert_eq!(r.ahead, 1, "the child carries one commit past its parent");
    assert_eq!(r.behind, 0);
}

// ---------------------------------------------------------------------------
// The wire contract
// ---------------------------------------------------------------------------

/// Four space-separated tokens on one line, in the order `worktree.sh`'s
/// `read -r stale_ref stale_display local_commits_ahead local_commits_behind`
/// consumes them. The shell has no way to notice a reordering, so it is pinned
/// here.
#[test]
fn render_emits_ref_display_ahead_behind_in_that_order() {
    let r = Resolved {
        reference: "origin/feature/issue-7".into(),
        display: "origin/feature/issue-7".into(),
        ahead: 0,
        behind: 3,
    };
    assert_eq!(r.render(), "origin/feature/issue-7 origin/feature/issue-7 0 3");
    let line = r.render();
    let fields: Vec<&str> = line.split(' ').collect();
    assert_eq!(fields.len(), 4, "exactly four fields, never a fifth");
}

/// An unresolvable reference reports 0/0 rather than erroring — the shell's
/// `|| local_commits_ahead="0"` verbatim. Safe only because
/// `loom_worktree_reset_or_rescue` re-derives commits-ahead itself immediately
/// before the destructive reset (#6334); pinned so a future "surely this should
/// fail loudly" change has to argue with that pairing explicitly.
#[test]
fn an_unresolvable_reference_counts_as_zero_rather_than_failing() {
    let dir = tmpdir("bad-ref");
    let work = repo(&dir, "feature/issue-406", false);

    let r = resolve_with(
        &Options {
            base_ref: "origin/definitely-missing".into(),
            base_display: "definitely-missing".into(),
            ..opts(&work, "feature/issue-406")
        },
        &|_| panic!("no remote branch; the forge is not asked"),
    );

    assert_eq!(r.reference, "origin/definitely-missing");
    assert_eq!(r.ahead, 0);
    assert_eq!(r.behind, 0);
}

/// A directory that is not a repo at all cannot panic and cannot report a
/// non-zero exit: this sits on `worktree.sh`'s re-invocation path, where the
/// only thing worse than a wrong reference is an aborted run.
#[test]
fn a_nonexistent_worktree_degrades_to_the_base_and_exit_zero() {
    let dir = tmpdir("absent");
    let absent = dir.join("no such worktree");

    let r = resolve_with(&opts(&absent, "feature/issue-407"), &|_| {
        panic!("no repo, so no remote branch, so no forge call")
    });
    assert_eq!(r.reference, "origin/main");
    assert_eq!(r.ahead, 0);
    assert_eq!(r.behind, 0);
    assert_eq!(run(&opts(&absent, "feature/issue-407")), 0);
}
