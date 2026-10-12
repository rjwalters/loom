//! The clean-merge kind through tree-identical commits and earlier clean
//! merges (Issue #10875).
//!
//! #10857's history between a released critical-file hold and the head it
//! re-armed on was `released → re-run-CI no-op → no-op → merge(main) → no-op →
//! no-op`. Before #10875 the clean-merge kind accepted only the bare
//! `head = merge(reviewed, base)`, so that shape could never be proven even
//! when the merge was clean. These cases pin both halves: the routine fleet
//! shapes now prove, and every content change on the way — a real commit
//! (whatever its message claims), a hand-edited or conflicted merge — still
//! refutes. Real git, same as `tests.rs`'s clean-merge cases.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::tests::{fake_gh, git_ok, merge_tree_available, write, BASE_MAIN};
use super::*;
use tempfile::{tempdir, TempDir};

/// `reviewed` on `feature` (touches `f.txt`); `main` moved once (`m.txt`).
fn fixture() -> (TempDir, String, String) {
    let dir = tempdir().unwrap();
    let repo = dir.path();
    git_ok(repo, &["init", "--quiet", "."]);
    git_ok(repo, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    write(repo, "m.txt", "main-0\n");
    write(repo, "f.txt", "feature-0\n");
    git_ok(repo, &["add", "-A"]);
    git_ok(repo, &["commit", "--quiet", "-m", "base0"]);
    git_ok(repo, &["checkout", "--quiet", "-b", "feature"]);
    write(repo, "f.txt", "feature-1\n");
    git_ok(repo, &["add", "-A"]);
    git_ok(repo, &["commit", "--quiet", "-m", "the PR's own change"]);
    let reviewed = git_ok(repo, &["rev-parse", "HEAD"]);
    let base1 = move_main(repo, "main-1\n");
    (dir, reviewed, base1)
}

/// Commit `content` to `m.txt` on `main`, return to `feature`, return the sha.
fn move_main(repo: &Path, content: &str) -> String {
    git_ok(repo, &["checkout", "--quiet", "main"]);
    write(repo, "m.txt", content);
    git_ok(repo, &["add", "-A"]);
    git_ok(repo, &["commit", "--quiet", "-m", "main moved"]);
    let sha = git_ok(repo, &["rev-parse", "HEAD"]);
    git_ok(repo, &["checkout", "--quiet", "feature"]);
    sha
}

/// A tree-identical commit, worded like the fleet's own re-date push.
fn redate(repo: &Path) -> String {
    git_ok(
        repo,
        &[
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "chore: re-date required checks (#8248 guard, automated by #8508)",
        ],
    );
    git_ok(repo, &["rev-parse", "HEAD"])
}

fn merge_main(repo: &Path) -> String {
    git_ok(repo, &["merge", "--quiet", "--no-ff", "-m", "Merge main", "main"]);
    git_ok(repo, &["rev-parse", "HEAD"])
}

/// `gh` confirming every listed sha is on `main`.
fn gh_on_main(dir: &Path, bases: &[&str]) -> PathBuf {
    let routes: Vec<(String, String)> = bases
        .iter()
        .map(|b| (format!("compare/{b}...main"), r#"{"status": "identical", "files": []}"#.into()))
        .collect();
    let mut refs: Vec<(&str, &str)> = routes
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_str()))
        .collect();
    refs.push(BASE_MAIN);
    fake_gh(dir, &dir.join("gh.log"), &refs)
}

fn evidence(gh: &Path, repo: &Path, reviewed: &str, head: &str) -> Evidence {
    clean_merge::evidence(gh, Some(repo), repo, reviewed, head, "main")
}

#[test]
fn a_redate_on_top_of_a_clean_merge_proves() {
    let (dir, reviewed, base1) = fixture();
    let repo = dir.path();
    if !merge_tree_available(repo) {
        return;
    }
    merge_main(repo);
    let head = redate(repo);
    let ghdir = tempdir().unwrap();
    let gh = gh_on_main(ghdir.path(), &[&base1]);
    assert_eq!(evidence(&gh, repo, &reviewed, &head), Evidence::Proven);
}

/// #10857's shape, with a clean merge: no-ops on both sides of it.
#[test]
fn no_ops_on_both_sides_of_a_clean_merge_prove() {
    let (dir, reviewed, base1) = fixture();
    let repo = dir.path();
    if !merge_tree_available(repo) {
        return;
    }
    redate(repo);
    redate(repo);
    merge_main(repo);
    redate(repo);
    let head = redate(repo);
    let ghdir = tempdir().unwrap();
    let gh = gh_on_main(ghdir.path(), &[&base1]);
    assert_eq!(evidence(&gh, repo, &reviewed, &head), Evidence::Proven);
}

#[test]
fn two_clean_merges_with_a_redate_between_prove() {
    let (dir, reviewed, base1) = fixture();
    let repo = dir.path();
    if !merge_tree_available(repo) {
        return;
    }
    merge_main(repo);
    redate(repo);
    let base2 = move_main(repo, "main-2\n");
    let head = merge_main(repo);
    let ghdir = tempdir().unwrap();
    let gh = gh_on_main(ghdir.path(), &[&base1, &base2]);
    assert_eq!(evidence(&gh, repo, &reviewed, &head), Evidence::Proven);
}

/// A content commit worded exactly like a re-date: the message proves nothing.
#[test]
fn a_content_commit_with_a_redate_message_after_the_merge_refutes() {
    let (dir, reviewed, base1) = fixture();
    let repo = dir.path();
    if !merge_tree_available(repo) {
        return;
    }
    merge_main(repo);
    write(repo, "f.txt", "feature-1-and-more\n");
    git_ok(repo, &["add", "-A"]);
    git_ok(
        repo,
        &[
            "commit",
            "--quiet",
            "-m",
            "chore: re-date required checks (tree-identical no-op)",
        ],
    );
    let head = git_ok(repo, &["rev-parse", "HEAD"]);
    let ghdir = tempdir().unwrap();
    let gh = gh_on_main(ghdir.path(), &[&base1]);
    assert_eq!(evidence(&gh, repo, &reviewed, &head), Evidence::Refuted);
}

#[test]
fn a_content_commit_beneath_the_merge_refutes() {
    let (dir, reviewed, base1) = fixture();
    let repo = dir.path();
    if !merge_tree_available(repo) {
        return;
    }
    write(repo, "f.txt", "feature-2\n");
    git_ok(repo, &["add", "-A"]);
    git_ok(repo, &["commit", "--quiet", "-m", "a real change"]);
    redate(repo);
    merge_main(repo);
    let head = redate(repo);
    let ghdir = tempdir().unwrap();
    let gh = gh_on_main(ghdir.path(), &[&base1]);
    assert_eq!(evidence(&gh, repo, &reviewed, &head), Evidence::Refuted);
}

/// #10857's actual 17:23 shape: the merge between the no-ops resolved a
/// conflict by hand. No clean automatic merge exists, so it stays refuted and
/// the hold re-arms (fail closed), exactly as before #10875.
#[test]
fn no_ops_around_a_conflict_resolved_merge_refute() {
    let (dir, reviewed, _base1) = fixture();
    let repo = dir.path();
    if !merge_tree_available(repo) {
        return;
    }
    // The PR also edits the line main moves next.
    write(repo, "m.txt", "feature-touched-main-file\n");
    git_ok(repo, &["add", "-A"]);
    git_ok(repo, &["commit", "--quiet", "--amend", "--no-edit"]);
    let reviewed_conflicting = git_ok(repo, &["rev-parse", "HEAD"]);
    assert_ne!(reviewed, reviewed_conflicting);
    let base2 = move_main(repo, "main-2\n");
    redate(repo);
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["merge", "--quiet", "--no-ff", "main"])
        .output()
        .unwrap();
    write(repo, "m.txt", "resolved-by-hand\n");
    git_ok(repo, &["add", "-A"]);
    git_ok(repo, &["commit", "--quiet", "-m", "Merge main"]);
    let head = redate(repo);
    let ghdir = tempdir().unwrap();
    let gh = gh_on_main(ghdir.path(), &[&base2]);
    assert_eq!(evidence(&gh, repo, &reviewed_conflicting, &head), Evidence::Refuted);
}

/// No-ops alone are the tree kind's proof; this kind does not claim them.
#[test]
fn no_ops_without_a_merge_are_left_to_the_tree_kind() {
    let (dir, reviewed, base1) = fixture();
    let repo = dir.path();
    if !merge_tree_available(repo) {
        return;
    }
    redate(repo);
    let head = redate(repo);
    let ghdir = tempdir().unwrap();
    let gh = gh_on_main(ghdir.path(), &[&base1]);
    assert_eq!(evidence(&gh, repo, &reviewed, &head), Evidence::Refuted);
}

/// A merged parent the forge cannot place on the base is no answer.
#[test]
fn an_unconfirmable_base_parent_under_a_redate_is_indeterminate() {
    let (dir, reviewed, _base1) = fixture();
    let repo = dir.path();
    if !merge_tree_available(repo) {
        return;
    }
    redate(repo);
    merge_main(repo);
    let head = redate(repo);
    let ghdir = tempdir().unwrap();
    let gh = gh_on_main(ghdir.path(), &[]);
    let (ev, why) = clean_merge::assess(&gh, Some(repo), repo, None, &reviewed, &head, "main");
    assert_eq!(ev, Evidence::Indeterminate);
    assert!(why.unwrap().contains("could not confirm"));
}

/// End to end through [`assess_with`]: the tree differs (main moved), so only
/// the composed clean-merge kind can carry it, and it names itself.
#[test]
fn detect_carries_a_redated_clean_merge_as_clean_merge() {
    let (dir, reviewed, base1) = fixture();
    let repo = dir.path();
    if !merge_tree_available(repo) {
        return;
    }
    redate(repo);
    merge_main(repo);
    let head = redate(repo);
    let ghdir = tempdir().unwrap();
    let log = ghdir.path().join("gh.log");
    let compare = format!("compare/{reviewed}...{head}");
    let on_main = format!("compare/{base1}...main");
    let gh = fake_gh(
        ghdir.path(),
        &log,
        &[
            (&compare, r#"{"status": "ahead", "files": [{"filename": "m.txt"}]}"#),
            (&on_main, r#"{"status": "identical", "files": []}"#),
            BASE_MAIN,
        ],
    );
    super::tests::assert_assessed(
        Equivalence::Equivalent(EquivalenceKind::CleanMerge),
        true,
        true,
        &gh,
        repo,
        &log,
        &reviewed,
        &head,
    );
}
