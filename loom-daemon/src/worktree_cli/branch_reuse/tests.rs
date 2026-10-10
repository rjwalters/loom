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

// ---------------------------------------------------------------------------
// #9319: the refusal names the worktree still holding the branch.
// ---------------------------------------------------------------------------

/// Today's line, spelled out — the string the no-holder path must still equal.
const BARE_REFUSAL: &str = "Local branch 'feature/issue-42' has already landed on main (already-merged PR #999) - refusing to reuse it. Delete it and re-run: git branch -D feature/issue-42 && ./.loom/scripts/worktree.sh 42";

/// A linked worktree on `feature/issue-42`, under a directory with a space in
/// its name, optionally carrying the `.loom-managed` sentinel.
fn linked_worktree(fx: &Fx, managed: bool) -> PathBuf {
    let wt = fx.root.join("wt s").join("pr-9");
    fs::create_dir_all(wt.parent().unwrap()).unwrap();
    git(
        &fx.repo,
        &[
            "worktree",
            "add",
            "-q",
            wt.to_str().unwrap(),
            "feature/issue-42",
        ],
    );
    if managed {
        fs::write(wt.join(".loom-managed"), "issue=42\n").unwrap();
    }
    wt
}

fn branch_exists(repo: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "-q", "refs/heads/feature/issue-42"])
        .output()
        .expect("git")
        .status
        .success()
}

/// Run the first `steps` `&&`-joined commands of a printed remedy, verbatim,
/// through a real shell in the main workspace — the way an operator would.
fn run_remedy(repo: &Path, message: &str, steps: usize) -> std::process::Output {
    let commands = message.rsplit_once("re-run: ").expect("a remedy").1;
    let head: Vec<&str> = commands.split(" && ").take(steps).collect();
    Command::new("sh")
        .arg("-c")
        .arg(head.join(" && "))
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("sh")
}

fn same_dir(a: &Path, b: &Path) -> bool {
    fs::canonicalize(a).unwrap() == fs::canonicalize(b).unwrap()
}

#[test]
fn with_no_holder_the_refusal_line_is_unchanged() {
    let fx = fixture("holder-none", true);
    let o = opts(&fx.repo, false);
    assert_eq!(holder::find(&fx.repo, &o.branch), None);
    assert_eq!(refusal_message(&o, Some("999"), None), BARE_REFUSAL);
    assert_eq!(
        refusal_message(&o, None, None),
        BARE_REFUSAL.replace(" (already-merged PR #999)", "")
    );
}

#[test]
fn a_managed_holder_gets_a_remedy_that_works_verbatim() {
    let fx = fixture("holder-managed", true);
    let wt = linked_worktree(&fx, true);
    let o = opts(&fx.repo, false);

    // The defect: the bare remedy's first command is refused by git.
    let bare = run_remedy(&fx.repo, BARE_REFUSAL, 1);
    assert!(!bare.status.success(), "git deleted a branch a worktree holds");
    assert!(String::from_utf8_lossy(&bare.stderr).contains("used by worktree"));

    let held = holder::find(&fx.repo, &o.branch).expect("the pr worktree holds the branch");
    assert_eq!(held.kind, holder::Kind::Managed);
    assert!(same_dir(&held.path, &wt));

    let msg = refusal_message(&o, Some("999"), Some(&held));
    assert!(msg.contains(&held.path.display().to_string()), "{msg}");
    assert!(
        msg.ends_with(&format!(
            "git worktree remove {} --force && git branch -D feature/issue-42 && ./.loom/scripts/worktree.sh 42",
            holder::shell_word(&held.path)
        )),
        "{msg}"
    );

    // The path contains a space (`re po`'s sibling `wt s`), so this only
    // passes if the printed path is quoted.
    let ran = run_remedy(&fx.repo, &msg, 2);
    assert!(ran.status.success(), "{}", String::from_utf8_lossy(&ran.stderr));
    assert!(!wt.exists(), "the stale worktree is still on disk");
    assert!(!branch_exists(&fx.repo), "the landed branch survived the remedy");
}

#[test]
fn the_refusal_itself_removes_nothing_when_a_worktree_holds_the_branch() {
    let fx = fixture("holder-nondestructive", true);
    let wt = linked_worktree(&fx, true);
    let tip = rev(&fx.repo, "feature/issue-42");
    assert_eq!(run_with(&opts(&fx.repo, false), &|_| merged_at(&tip, "999")), 1);
    assert_eq!(run_with(&opts(&fx.repo, true), &|_| merged_at(&tip, "999")), 1);
    assert!(wt.join(".loom-managed").is_file(), "the holding worktree was touched");
    assert_eq!(rev(&fx.repo, "feature/issue-42"), tip, "branch tip moved");
}

#[test]
fn a_user_provisioned_holder_is_named_but_never_offered_for_removal() {
    let fx = fixture("holder-user", true);
    let wt = linked_worktree(&fx, false);
    let o = opts(&fx.repo, false);
    let held = holder::find(&fx.repo, &o.branch).expect("holder");
    assert_eq!(held.kind, holder::Kind::Unmanaged);
    assert!(same_dir(&held.path, &wt));

    let msg = refusal_message(&o, Some("999"), Some(&held));
    assert!(msg.contains(&held.path.display().to_string()), "{msg}");
    assert!(msg.contains("still holds it"), "{msg}");
    assert!(!msg.contains("--force"), "{msg}");
    assert!(!msg.contains("worktree remove"), "{msg}");
    // What it does print is still true: nothing after `re-run:` is a removal.
    assert!(
        msg.ends_with("re-run: git branch -D feature/issue-42 && ./.loom/scripts/worktree.sh 42")
    );
}

#[test]
fn the_main_workspace_as_holder_is_told_to_switch_not_to_remove() {
    let fx = fixture("holder-main", true);
    git(&fx.repo, &["checkout", "-q", "feature/issue-42"]);
    let o = opts(&fx.repo, false);
    let held = holder::find(&fx.repo, &o.branch).expect("holder");
    assert_eq!(held.kind, holder::Kind::Main);
    assert!(same_dir(&held.path, &fx.repo));

    let msg = refusal_message(&o, Some("999"), Some(&held));
    assert!(msg.contains("main workspace"), "{msg}");
    assert!(!msg.contains("worktree remove"), "{msg}");
    assert!(!msg.contains("--force"), "{msg}");

    let ran = run_remedy(&fx.repo, &msg, 2);
    assert!(ran.status.success(), "{}", String::from_utf8_lossy(&ran.stderr));
    assert!(!branch_exists(&fx.repo));
}

#[test]
fn a_worktree_mid_rebase_on_the_branch_is_still_the_holder() {
    // git reports this worktree as `detached`, so a `branch refs/heads/…`
    // porcelain match alone would miss it — and `git branch -D` would still
    // refuse. The answer has to come from `branch_holders`.
    let fx = fixture("holder-rebase", true);
    let wt = linked_worktree(&fx, true);
    fs::write(fx.repo.join("slice.txt"), "main\n").unwrap();
    git(&fx.repo, &["add", "slice.txt"]);
    git(&fx.repo, &["commit", "-q", "-m", "conflicting main change"]);
    let rebase = Command::new("git")
        .arg("-C")
        .arg(&wt)
        .args([
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "rebase",
            "main",
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_EDITOR", "true")
        .output()
        .expect("git");
    assert!(!rebase.status.success(), "the rebase must stop on a conflict");
    let porcelain = Command::new("git")
        .arg("-C")
        .arg(&fx.repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .expect("git");
    let porcelain = String::from_utf8_lossy(&porcelain.stdout).into_owned();
    assert!(
        !porcelain.contains("branch refs/heads/feature/issue-42"),
        "precondition: the worktree must read as detached\n{porcelain}"
    );

    let held = holder::find(&fx.repo, "feature/issue-42").expect("a mid-rebase holder");
    assert_eq!(held.kind, holder::Kind::Managed);
    assert!(same_dir(&held.path, &wt));
}

#[test]
fn a_failed_holder_probe_degrades_to_the_unchanged_line() {
    // `git worktree list` cannot run in a directory that is not there.
    let gone = std::env::temp_dir().join(format!("loom-wt-reuse-gone-{}", std::process::id()));
    let _ = fs::remove_dir_all(&gone);
    let o = opts(&gone, false);
    let held = holder::find(&gone, &o.branch);
    assert_eq!(held, None);
    assert_eq!(refusal_message(&o, Some("999"), held.as_ref()), BARE_REFUSAL);
}

#[test]
fn the_document_gains_held_by_worktree_and_keeps_every_other_field() {
    let o = opts(Path::new("/nowhere"), true);
    // A path holding a space, a double quote and a backslash — all of which a
    // hand-spliced document would have broken on.
    let held = holder::Holder {
        path: PathBuf::from("/tmp/wt s/pr-\"9\\x"),
        kind: holder::Kind::Managed,
    };
    let text = refusal_document(&o, Some("999"), Some(&held)).to_string();
    let v: serde_json::Value = serde_json::from_str(&text).expect("must parse");
    assert_eq!(v["heldByWorktree"], "/tmp/wt s/pr-\"9\\x");
    assert_eq!(v["success"], false);
    assert_eq!(v["error"], "branch-already-landed");
    assert_eq!(v["issueNumber"], 42);
    assert_eq!(v["branch"], "feature/issue-42");
    assert_eq!(v["prNumber"], 999);
    assert_eq!(v.as_object().unwrap().len(), 6, "an unexpected key: {text}");

    // With no holder the document is the pre-#9319 one, key for key: the
    // field is absent (so `.heldByWorktree` still reads as null).
    let none = refusal_document(&o, None, None);
    assert!(none["heldByWorktree"].is_null());
    assert_eq!(none.as_object().unwrap().len(), 5, "{none}");
    assert!(none["prNumber"].is_null());
}

#[test]
fn a_path_holding_a_single_quote_is_still_one_shell_word() {
    let word = holder::shell_word(Path::new("/tmp/it's here"));
    assert_eq!(word, r"'/tmp/it'\''s here'");
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!("printf %s {word}"))
        .output()
        .expect("sh");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "/tmp/it's here");
}
