//! Unit tests for the local-branch reuse arm (#8195 slice 14).
//!
//! Every fixture repo lives under a directory named `re po` — a path with a
//! space in it, #7858's data-loss class. It is not decoration: these tests are
//! the port's own regression case for it, as the issue's AC requires, and they
//! would fail (git would resolve nothing, so every verdict would collapse to
//! `Unknown`) if any `git -C` argument here were ever word-split.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::*;
use crate::worktree_cli::branch_landed::{ForgeProbe, ForgeStatus};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn rev(dir: &Path, r: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", r])
        .output()
        .expect("git");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

struct Fx {
    root: PathBuf,
    repo: PathBuf,
}

impl Drop for Fx {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// `main` pushed to a bare origin, plus a local `feature/issue-42` carrying one
/// commit of its own. `advance_main` adds a commit to `main` (and pushes it),
/// so the branch no longer contains all of the base ref's history.
fn fixture(tag: &str, advance_main: bool) -> Fx {
    let root = std::env::temp_dir().join(format!("loom-wt-reuse-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let repo = root.join("re po"); // a space: #7858's class
    fs::create_dir_all(&repo).unwrap();
    git(&root, &["init", "-q", "--bare", "-b", "main", "origin.git"]);
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            root.join("origin.git").to_str().unwrap(),
        ],
    );
    git(&repo, &["push", "-q", "origin", "main"]);
    git(&repo, &["checkout", "-q", "-b", "feature/issue-42"]);
    fs::write(repo.join("slice.txt"), "slice\n").unwrap();
    git(&repo, &["add", "slice.txt"]);
    git(&repo, &["commit", "-q", "-m", "slice work"]);
    git(&repo, &["checkout", "-q", "main"]);
    if advance_main {
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "later main"]);
        git(&repo, &["push", "-q", "origin", "main"]);
    }
    git(&repo, &["fetch", "-q", "origin"]);
    Fx { root, repo }
}

fn opts(repo: &Path, json: bool) -> Options {
    Options {
        repo: repo.to_path_buf(),
        branch: "feature/issue-42".to_string(),
        issue: "42".to_string(),
        default_branch: "main".to_string(),
        base_ref: "origin/main".to_string(),
        base_display: "main".to_string(),
        json_output: if json { "true" } else { "false" }.to_string(),
    }
}

fn merged_at(sha: &str, number: &str) -> ForgeProbe {
    ForgeProbe {
        status: ForgeStatus::Found,
        head_sha: Some(sha.to_string()),
        number: Some(number.to_string()),
    }
}

// ---------------------------------------------------------------------------
// The decision table, with no repo at all.
// ---------------------------------------------------------------------------

#[test]
fn only_a_landed_verdict_on_a_non_degenerate_tip_refuses() {
    assert_eq!(decide(Verdict::Landed, false), Decision::Refuse);
    // The degenerate tip: identical to origin/<default>'s current tip, which
    // is trivially its own ancestor. Refusing it would break every ordinary
    // `worktree.sh <N>` re-run.
    assert_eq!(decide(Verdict::Landed, true), Decision::Reuse);
    // Fail open in BOTH non-proof directions — the pinned contract.
    assert_eq!(decide(Verdict::NotLanded, false), Decision::Reuse);
    assert_eq!(decide(Verdict::Unknown, false), Decision::Reuse);
}

// ---------------------------------------------------------------------------
// End to end, against real repos under a path containing a space.
// ---------------------------------------------------------------------------

#[test]
fn a_merged_pr_head_matching_the_tip_refuses() {
    let fx = fixture("landed", true);
    let tip = rev(&fx.repo, "feature/issue-42");
    let code = run_with(&opts(&fx.repo, false), &|_| merged_at(&tip, "999"));
    assert_eq!(code, 1, "an already-landed local branch must be refused");
}

#[test]
fn the_refusal_leaves_the_branch_and_the_worktree_alone() {
    // #8280's refusal is not destructive: the retained suite asserts the
    // branch survives, and nothing in this module may ever delete one.
    let fx = fixture("nondestructive", true);
    let tip = rev(&fx.repo, "feature/issue-42");
    assert_eq!(run_with(&opts(&fx.repo, false), &|_| merged_at(&tip, "999")), 1);
    assert_eq!(rev(&fx.repo, "feature/issue-42"), tip, "branch tip moved");
}

#[test]
fn an_unavailable_forge_still_reuses() {
    // Test 5 of test-worktree-stale-merged-branch.sh, in Rust: a forge outage
    // must never block worktree creation.
    let fx = fixture("failopen", true);
    let code = run_with(&opts(&fx.repo, false), &|_| ForgeProbe::unavailable());
    assert_eq!(code, 0);
}

#[test]
fn a_merged_pr_whose_head_moved_on_still_reuses() {
    // #7872's merged-head-mismatch rung: the branch carries commits the merge
    // never saw, so the merged PR is not proof about this tip.
    let fx = fixture("moved", true);
    let stale = rev(&fx.repo, "feature/issue-42~1");
    let code = run_with(&opts(&fx.repo, false), &|_| merged_at(&stale, "999"));
    assert_eq!(code, 0);
}

#[test]
fn a_fresh_branch_at_the_default_tip_is_never_refused() {
    // The degenerate case end to end: a local branch pointing exactly at
    // origin/main. Even with the forge claiming it merged, reuse must proceed.
    let fx = fixture("degenerate", false);
    git(&fx.repo, &["branch", "-f", "feature/issue-42", "origin/main"]);
    let tip = rev(&fx.repo, "feature/issue-42");
    let code = run_with(&opts(&fx.repo, false), &|_| merged_at(&tip, "999"));
    assert_eq!(code, 0, "an unused branch at the base tip must stay reusable");
}

#[test]
fn divergence_is_detected_only_when_the_branch_lacks_base_history() {
    let behind = fixture("behind", true);
    assert!(
        !contains_base_history(&behind.repo, &opts(&behind.repo, false)),
        "a branch cut before main advanced does not contain origin/main"
    );
    let current = fixture("current", false);
    assert!(
        contains_base_history(&current.repo, &opts(&current.repo, false)),
        "a branch cut from the current tip does contain origin/main"
    );
}

#[test]
fn the_upstream_correction_runs_on_the_reuse_path() {
    // Step 2 of the arm (#6095/#6100): a local branch left tracking
    // origin/main is re-pointed at its own remote branch. This is the property
    // the retired `_worktree_upstream_check local-branch` wrapper existed for,
    // asserted here now that the call site is in-process.
    let fx = fixture("upstream", false);
    git(&fx.repo, &["push", "-q", "origin", "feature/issue-42"]);
    git(
        &fx.repo,
        &[
            "branch",
            "--set-upstream-to=origin/main",
            "feature/issue-42",
        ],
    );
    assert_eq!(run_with(&opts(&fx.repo, false), &|_| ForgeProbe::unavailable()), 0);
    let up = Command::new("git")
        .arg("-C")
        .arg(&fx.repo)
        .args(["rev-parse", "--abbrev-ref", "feature/issue-42@{u}"])
        .output()
        .expect("git");
    assert_eq!(
        String::from_utf8_lossy(&up.stdout).trim(),
        "origin/feature/issue-42",
        "the reuse arm did not correct the branch's upstream"
    );
}

// ---------------------------------------------------------------------------
// The JSON document.
// ---------------------------------------------------------------------------

#[test]
fn the_refusal_document_is_valid_json_for_a_refname_holding_a_quote() {
    // The defect this slice fixes: `git check-ref-format` permits `"` in a
    // refname, and the retired shell spliced $BRANCH_NAME into the document by
    // hand. Built here rather than through the process so the assertion is on
    // the document, not on stdout capture.
    let doc = serde_json::json!({
        "success": false,
        "error": "branch-already-landed",
        "issueNumber": json_issue("42"),
        "branch": "feature/a\"b\\c",
        "prNumber": json_pr(Some("999")),
    })
    .to_string();
    let v: serde_json::Value = serde_json::from_str(&doc).expect("must parse");
    assert_eq!(v["branch"], "feature/a\"b\\c");
    assert_eq!(v["issueNumber"], 42);
    assert_eq!(v["prNumber"], 999);
    assert_eq!(v["error"], "branch-already-landed");
}

#[test]
fn field_types_match_the_retired_unquoted_splices() {
    // issueNumber and prNumber were spliced UNQUOTED, so both are JSON
    // numbers, and an absent PR number was the literal `null`. A consumer
    // keying on either must not start seeing strings.
    assert!(json_issue("42").is_number());
    assert!(json_pr(Some("999")).is_number());
    assert!(json_pr(None).is_null());
    assert!(json_pr(Some("")).is_null());
    // A non-numeric forge answer becomes null, never a quoted string.
    assert!(json_pr(Some("abc")).is_null());
    // A non-numeric issue can no longer reach here (worktree.sh validates it
    // first), but it must still produce parseable JSON if it ever did.
    assert!(json_issue("x").is_string());
}
