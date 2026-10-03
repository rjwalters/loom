//! Issue #10134: the approval-staleness guard must not strip `loom:pr` after a
//! base-only update-branch merely because the host's clone has not seen the
//! new merge commit yet.
//!
//! Every case here models the incident's two-repository shape with real git:
//! a **forge** repository where the head moves (what `gh pr update-branch`
//! does server-side), and a **host clone** — a full, non-shallow clone of the
//! forge made BEFORE the move, so the new head is absent from it, exactly like
//! a daemon root that has not fetched since. `origin` of the host clone is the
//! forge, so the fetch [`super::git_objects::ensure_commit`] performs is a real
//! fetch.
//!
//! The base move deliberately touches the SAME FILE the PR changes (different
//! lines, so the merge is clean). That is the case only the clean-merge kind
//! can carry: the PR's merge-base-relative patch text legitimately changes, so
//! the forge-only rebase kind refutes, and before #10134 the clean-merge kind
//! answered `Indeterminate` for want of the object — clearing the verdict.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::tests::{fake_gh, git_ok, merge_tree_available, write, BASE_MAIN};
use super::*;
use std::path::PathBuf;
use tempfile::{tempdir, TempDir};

const PR: u32 = 10134;

/// Ten lines, so the PR and the base can each edit a different end of the same
/// file and still merge cleanly.
fn lines(top: &str, bottom: &str) -> String {
    let mut s = format!("{top}\n");
    for i in 2..10 {
        s.push_str(&format!("line {i}\n"));
    }
    s.push_str(&format!("{bottom}\n"));
    s
}

struct Fixture {
    _forge_dir: TempDir,
    forge: PathBuf,
    _host_dir: TempDir,
    host: PathBuf,
    reviewed: String,
}

/// Forge repo:  base0 --- base1 (main: edits the BOTTOM of shared.txt)
///                  \
///                   reviewed (feature: edits the TOP of shared.txt)
///
/// The host clone is taken here, before any head move.
fn fixture() -> Fixture {
    let forge_dir = tempdir().unwrap();
    let forge = forge_dir.path().to_path_buf();
    git_ok(&forge, &["init", "--quiet", "."]);
    git_ok(&forge, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    write(&forge, "shared.txt", &lines("top-0", "bottom-0"));
    git_ok(&forge, &["add", "-A"]);
    git_ok(&forge, &["commit", "--quiet", "-m", "base0"]);

    git_ok(&forge, &["checkout", "--quiet", "-b", "feature"]);
    write(&forge, "shared.txt", &lines("top-PR", "bottom-0"));
    git_ok(&forge, &["add", "-A"]);
    git_ok(&forge, &["commit", "--quiet", "-m", "the PR's own change"]);
    let reviewed = git_ok(&forge, &["rev-parse", "HEAD"]);

    git_ok(&forge, &["checkout", "--quiet", "main"]);
    write(&forge, "shared.txt", &lines("top-0", "bottom-main-1"));
    git_ok(&forge, &["add", "-A"]);
    git_ok(&forge, &["commit", "--quiet", "-m", "main moved (same file)"]);
    git_ok(&forge, &["checkout", "--quiet", "feature"]);

    let host_dir = tempdir().unwrap();
    let host = host_dir.path().join("clone");
    let out = std::process::Command::new("git")
        .args(["clone", "--quiet", "--no-local"])
        .arg(format!("file://{}", forge.display()))
        .arg(&host)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    Fixture {
        _forge_dir: forge_dir,
        forge,
        _host_dir: host_dir,
        host,
        reviewed,
    }
}

fn has(repo: &Path, sha: &str) -> bool {
    git_objects::has_commit(repo, sha)
}

/// The update-branch shape, made on the forge: merge main into the PR branch.
fn update_branch_on_forge(f: &Fixture) -> String {
    git_ok(
        &f.forge,
        &[
            "merge",
            "--quiet",
            "--no-ff",
            "-m",
            "Merge main into feature",
            "main",
        ],
    );
    git_ok(&f.forge, &["rev-parse", "HEAD"])
}

/// `gh` for the whole `detect` path: the tree compare reports a difference
/// (the base really moved), the merged parent is on `main`, and the forge-only
/// patch comparison REFUTES (the shared file's patch context changed) — so
/// only the clean-merge kind can carry the verdict.
fn gh_for_detect(dir: &Path, log: &Path, f: &Fixture, head: &str) -> PathBuf {
    let base1 = git_ok(&f.forge, &["rev-parse", "main"]);
    let before = r#"{"files": [{"filename": "shared.txt", "status": "modified", "sha": "aaaa", "patch": "@@ -1 +1 @@ top-PR (old context)"}]}"#;
    let after = r#"{"files": [{"filename": "shared.txt", "status": "modified", "sha": "bbbb", "patch": "@@ -1 +1 @@ top-PR (new context)"}]}"#;
    fake_gh(
        dir,
        log,
        &[
            (
                &format!("compare/{}...{head}", f.reviewed),
                r#"{"status": "ahead", "files": [{"filename": "shared.txt"}]}"#,
            ),
            (&format!("compare/{base1}...main"), r#"{"status": "identical", "files": []}"#),
            (&format!("compare/main...{}", f.reviewed), before),
            (&format!("compare/main...{head}"), after),
            BASE_MAIN,
        ],
    )
}

// ---------------------------------------------------------------------------
// AC 1 — a base-only merge keeps the verdict, even when the new commit is not
// in the local clone yet.
// ---------------------------------------------------------------------------

#[test]
fn base_only_merge_absent_from_the_clone_is_fetched_and_carries_the_verdict() {
    let f = fixture();
    if !merge_tree_available(&f.host) {
        return;
    }
    let head = update_branch_on_forge(&f);
    assert!(!has(&f.host, &head), "fixture: the new head must be absent from the host clone");

    let ghdir = tempdir().unwrap();
    let log = ghdir.path().join("gh.log");
    let gh = gh_for_detect(ghdir.path(), &log, &f, &head);
    let a = assess_with(true, true, &gh, Some(&f.host), PR, &f.reviewed, &head);
    assert_eq!(a.equivalence, Equivalence::Equivalent(EquivalenceKind::CleanMerge), "{a:?}");
    assert_eq!(a.unavailable_note(), None);
    assert!(has(&f.host, &head), "the head must have been fetched into the clone");
}

/// GitHub may reach a PR head only through `refs/pull/<n>/head` (no branch
/// points at it). The fetch must still bring it in.
#[test]
fn a_head_reachable_only_through_the_pull_ref_is_fetched() {
    let f = fixture();
    if !merge_tree_available(&f.host) {
        return;
    }
    let head = update_branch_on_forge(&f);
    // Move the branch back, leaving the merge reachable only via the PR ref.
    git_ok(&f.forge, &["update-ref", &format!("refs/pull/{PR}/head"), &head]);
    git_ok(&f.forge, &["reset", "--quiet", "--hard", &f.reviewed]);

    let ghdir = tempdir().unwrap();
    let log = ghdir.path().join("gh.log");
    let gh = gh_for_detect(ghdir.path(), &log, &f, &head);
    assert_eq!(
        assess_with(true, true, &gh, Some(&f.host), PR, &f.reviewed, &head).equivalence,
        Equivalence::Equivalent(EquivalenceKind::CleanMerge)
    );
}

// ---------------------------------------------------------------------------
// AC 2 — a commit that changes the PR's own diff still clears the verdict.
// ---------------------------------------------------------------------------

#[test]
fn a_real_content_commit_absent_from_the_clone_is_still_changed() {
    let f = fixture();
    if !merge_tree_available(&f.host) {
        return;
    }
    write(&f.forge, "shared.txt", &lines("top-PR-edited-after-review", "bottom-0"));
    git_ok(&f.forge, &["add", "-A"]);
    git_ok(&f.forge, &["commit", "--quiet", "-m", "a real change"]);
    let head = git_ok(&f.forge, &["rev-parse", "HEAD"]);

    let ghdir = tempdir().unwrap();
    let log = ghdir.path().join("gh.log");
    let gh = gh_for_detect(ghdir.path(), &log, &f, &head);
    let a = assess_with(true, true, &gh, Some(&f.host), PR, &f.reviewed, &head);
    assert_eq!(a.equivalence, Equivalence::Changed, "{a:?}");
    assert_eq!(a.unavailable_note(), None, "a determinate answer carries no reason");
}

/// A base merge with a hand edit folded in is not the automatic merge: once
/// fetched, the clean-merge kind positively refutes it.
#[test]
fn a_base_merge_with_a_hand_edit_absent_from_the_clone_is_refuted() {
    let f = fixture();
    if !merge_tree_available(&f.host) {
        return;
    }
    git_ok(&f.forge, &["merge", "--quiet", "--no-ff", "--no-commit", "main"]);
    write(&f.forge, "sneaky.txt", "unreviewed\n");
    git_ok(&f.forge, &["add", "-A"]);
    git_ok(&f.forge, &["commit", "--quiet", "-m", "Merge main (with an edit)"]);
    let head = git_ok(&f.forge, &["rev-parse", "HEAD"]);

    let base1 = git_ok(&f.forge, &["rev-parse", "main"]);
    let ghdir = tempdir().unwrap();
    let log = ghdir.path().join("gh.log");
    let gh = fake_gh(
        ghdir.path(),
        &log,
        &[(&format!("compare/{base1}...main"), r#"{"status": "identical", "files": []}"#)],
    );
    assert_eq!(
        clean_merge::assess(&gh, Some(&f.host), &f.host, Some(PR), &f.reviewed, &head, "main"),
        (Evidence::Refuted, None)
    );
}

// ---------------------------------------------------------------------------
// AC 3 — when the commit cannot be fetched, the answer still fails closed, but
// says why.
// ---------------------------------------------------------------------------

#[test]
fn an_unfetchable_head_fails_closed_and_says_why() {
    let f = fixture();
    if !merge_tree_available(&f.host) {
        return;
    }
    let head = update_branch_on_forge(&f);
    // The forge becomes unreachable from the host.
    git_ok(
        &f.host,
        &[
            "remote",
            "set-url",
            "origin",
            "file:///nonexistent/loom-10134",
        ],
    );

    let ghdir = tempdir().unwrap();
    let log = ghdir.path().join("gh.log");
    let gh = gh_for_detect(ghdir.path(), &log, &f, &head);

    let (ev, why) =
        clean_merge::assess(&gh, Some(&f.host), &f.host, Some(PR), &f.reviewed, &head, "main");
    assert_eq!(ev, Evidence::Indeterminate);
    let why = why.expect("an unfetchable head must name a reason");
    assert!(why.contains("could not be fetched"), "{why}");
    assert!(why.contains(&head), "{why}");

    // Through detect: the patch kind refutes here, so the overall answer is a
    // determinate Changed — the case where that refutation is reached only
    // because the clean-merge kind could not run. Make the patch kind
    // unavailable too to see the whole fail-closed note.
    let gh_down = fake_gh(
        ghdir.path(),
        &log,
        &[
            (
                &format!("compare/{}...{head}", f.reviewed),
                r#"{"status": "ahead", "files": [{"filename": "shared.txt"}]}"#,
            ),
            BASE_MAIN,
        ],
    );
    let a = assess_with(true, true, &gh_down, Some(&f.host), PR, &f.reviewed, &head);
    assert_eq!(a.equivalence, Equivalence::Indeterminate);
    let note = a
        .unavailable_note()
        .expect("Indeterminate must carry a note");
    assert!(note.contains("clean-merge:") && note.contains("could not be fetched"), "{note}");
    assert!(note.contains("rebase-patch-identical:"), "{note}");
}

#[test]
fn unreadable_base_ref_is_named_in_the_note() {
    let dir = tempdir().unwrap();
    let log = dir.path().join("gh.log");
    let gh = fake_gh(dir.path(), &log, &[]);
    let a = assess_with(
        true,
        true,
        &gh,
        Some(dir.path()),
        PR,
        "1111111111111111111111111111111111111111",
        "2222222222222222222222222222222222222222",
    );
    assert_eq!(a.equivalence, Equivalence::Indeterminate);
    let note = a.unavailable_note().unwrap();
    assert!(note.contains("base branch"), "{note}");
}

#[test]
fn fetch_diagnostics_never_leak_url_credentials() {
    let raw =
        "fatal: unable to access 'https://x-access-token:ghs_SECRET@github.com/o/r.git/':\n  \
               Could not resolve host";
    let red = git_objects::redact(raw);
    assert!(!red.contains("ghs_SECRET"), "{red}");
    assert!(!red.contains('\n'));
    assert!(red.contains("https://***@github.com/o/r.git/"), "{red}");
    // A plain URL is left alone.
    assert_eq!(git_objects::redact("see https://github.com/o/r"), "see https://github.com/o/r");
    // Capped.
    assert!(git_objects::redact(&"x ".repeat(1000)).chars().count() <= 401);
}
