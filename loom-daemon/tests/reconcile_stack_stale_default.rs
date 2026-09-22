//! Issue #8583: the rebase destination must be the **fetched remote** default
//! branch tip, never the local branch of the same name.
//!
//! Every fixture here is a disposable pair of real repositories built under
//! `tempfile::tempdir()` — a bare "origin" and a clone — with synthetic
//! identities (`loom@example.com`). Nothing touches the network, a real
//! forge, `gh`, or any live branch.
//!
//! The shape reproduced throughout is the one that exists at the exact moment
//! reconciliation runs: the parent PR squash-merged **on the forge**, so the
//! remote default branch has moved and the local one has not.
//!
//! ```text
//!   origin/main:  A ── M            (M = the parent, squash-merged)
//!   local  main:  A                 (stale — nothing pulled it)
//!   parent  P:    A ── P1           (the pre-squash commit)
//!   child   C:    A ── P1 ── C1
//! ```
//!
//! `rebase --onto <local main> P C` exits **0** here when C1 touches files P1
//! did not: the child's own commit replays cleanly onto A and the merged
//! parent's implementation is silently gone. That silent case is the first
//! test below; the visible one (a child that edits parent-introduced content,
//! which conflicts instead) is the second.

use std::path::{Path, PathBuf};
use std::process::Command;

use loom_daemon::reconcile_stack::{self, PlanRequest, Prerequisite};

fn git_out(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(["-c", "protocol.file.allow=always"])
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Loom Test")
        .env("GIT_AUTHOR_EMAIL", "loom@example.com")
        .env("GIT_COMMITTER_NAME", "Loom Test")
        .env("GIT_COMMITTER_EMAIL", "loom@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git must spawn")
}

fn git(dir: &Path, args: &[&str]) {
    let out = git_out(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn stdout(dir: &Path, args: &[&str]) -> String {
    let out = git_out(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn write(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).unwrap();
}

fn commit_all(dir: &Path, message: &str) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", message]);
}

const PARENT: &str = "feature/issue-8001";
const CHILD: &str = "feature/issue-8002";

/// A bare origin, a stale clone, and a second clone standing in for the forge
/// that performed the squash merge.
struct Fixture {
    tmp: tempfile::TempDir,
    origin: PathBuf,
    /// The clone reconciliation runs in. Its local default branch is stale.
    repo: PathBuf,
    default_branch: String,
    /// The squash-merge commit that only `origin` knows about.
    remote_tip: String,
    /// What the clone's local default branch still points at.
    stale_local_tip: String,
}

impl Fixture {
    /// `child_edit` receives the child worktree and writes the child's own
    /// change — an independent file (the silent case) or an edit to
    /// parent-introduced content (the conflicting case).
    fn build(default_branch: &str, child_edit: impl Fn(&Path)) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        let repo = tmp.path().join("repo");
        let forge = tmp.path().join("forge");

        git(tmp.path(), &["init", "-q", "--bare", origin.to_str().unwrap()]);

        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", &format!("--initial-branch={default_branch}")]);
        git(&repo, &["config", "user.email", "loom@example.com"]);
        git(&repo, &["config", "user.name", "Loom Test"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        write(&repo, "base.txt", "base\n");
        commit_all(&repo, "A: base");
        git(&repo, &["remote", "add", "origin", origin.to_str().unwrap()]);
        git(&repo, &["push", "-q", "-u", "origin", default_branch]);

        // Parent branch: the pre-squash commit, pushed then (below) merged.
        git(&repo, &["checkout", "-q", "-b", PARENT]);
        write(&repo, "parent.txt", "parent\n");
        commit_all(&repo, "P1: parent implementation");
        git(&repo, &["push", "-q", "-u", "origin", PARENT]);

        // Child branch: stacked on the parent, one commit of its own.
        git(&repo, &["checkout", "-q", "-b", CHILD]);
        child_edit(&repo);
        commit_all(&repo, "C1: child work");
        git(&repo, &["push", "-q", "-u", "origin", CHILD]);
        git(&repo, &["checkout", "-q", default_branch]);

        // The squash merge happens ON THE FORGE, in a clone this one never
        // hears about — which is precisely why the local default branch is
        // stale at reconciliation time.
        git(
            tmp.path(),
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                forge.to_str().unwrap(),
            ],
        );
        git(&forge, &["config", "user.email", "loom@example.com"]);
        git(&forge, &["config", "user.name", "Loom Test"]);
        git(&forge, &["checkout", "-q", default_branch]);
        write(&forge, "parent.txt", "parent\n");
        commit_all(&forge, "M: P1 squash-merged (#8001)");
        git(&forge, &["push", "-q", "origin", default_branch]);
        // delete_branch_on_merge: the parent branch disappears from origin.
        git(&forge, &["push", "-q", "origin", "--delete", PARENT]);

        let remote_tip = stdout(&forge, &["rev-parse", "HEAD"]);
        let stale_local_tip = stdout(&repo, &["rev-parse", default_branch]);
        assert_ne!(
            remote_tip, stale_local_tip,
            "fixture must start with a STALE local default branch"
        );

        Fixture {
            tmp,
            origin,
            repo,
            default_branch: default_branch.to_string(),
            remote_tip,
            stale_local_tip,
        }
    }

    fn independent_child(default_branch: &str) -> Self {
        Self::build(default_branch, |wt| write(wt, "child.txt", "child\n"))
    }

    fn overlapping_child(default_branch: &str) -> Self {
        // The child edits content the PARENT introduced — the visible half of
        // the defect, where a stale destination conflicts instead of silently
        // dropping work.
        Self::build(default_branch, |wt| {
            write(wt, "parent.txt", "parent\nchild-extends-parent\n");
        })
    }

    /// A path beside the fixture's repositories, for linked worktrees.
    fn sibling(&self, name: &str) -> PathBuf {
        self.tmp.path().join(name)
    }

    fn request(&self) -> PlanRequest<'_> {
        PlanRequest {
            repo_dir: &self.repo,
            remote: "origin",
            default_branch: &self.default_branch,
            child_branch: CHILD,
            parent_branch: PARENT,
        }
    }

    /// Subjects of `git log <default>..<child>` after a reconcile — the
    /// commits that would show up in the child PR.
    fn child_commits_above_remote_tip(&self) -> Vec<String> {
        stdout(
            &self.repo,
            &[
                "log",
                "--format=%s",
                &format!("{}..{CHILD}", self.remote_tip),
            ],
        )
        .lines()
        .map(str::to_string)
        .collect()
    }

    fn file_at_child(&self, name: &str) -> Option<String> {
        let out = git_out(&self.repo, &["show", &format!("{CHILD}:{name}")]);
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// Pin the parent's pre-merge tip the way `merge-pr.sh`'s merge-ordering
    /// guard does (#7982), then delete every local trace of the branch.
    fn pin_and_drop_parent(&self) {
        let tip = stdout(&self.repo, &["rev-parse", PARENT]);
        git(&self.repo, &["checkout", "-q", &self.default_branch]);
        git(&self.repo, &["branch", "-D", PARENT]);
        let _ =
            git_out(&self.repo, &["update-ref", "-d", &format!("refs/remotes/origin/{PARENT}")]);
        git(&self.repo, &["update-ref", &format!("refs/loom/parent/{PARENT}"), &tip]);
    }
}

// ───────────────────────────────────────────────────────────────────────────
// The silent case: an independent child file.
// ───────────────────────────────────────────────────────────────────────────

/// The regression the issue is about. Before the fix this test's rebase
/// succeeded and `parent.txt` was gone from the child branch.
#[test]
fn independent_child_keeps_the_merged_parents_implementation() {
    let fx = Fixture::independent_child("main");

    let plan = reconcile_stack::plan(&fx.request()).expect("plan must succeed");

    assert_eq!(
        plan.target_commit, fx.remote_tip,
        "the destination must be the FETCHED remote tip"
    );
    assert_ne!(
        plan.target_commit, fx.stale_local_tip,
        "the destination must never be the stale local default branch"
    );
    // The branch NAME survives for `gh pr edit --base`; the mutation target
    // is the commit. Keeping both is an acceptance criterion, not an
    // implementation detail.
    assert_eq!(plan.default_branch, "main");

    reconcile_stack::rebase(&plan).expect("rebase must succeed");

    assert_eq!(
        fx.file_at_child("parent.txt").as_deref(),
        Some("parent\n"),
        "the just-merged parent's implementation must survive on the child"
    );
    assert_eq!(fx.file_at_child("child.txt").as_deref(), Some("child\n"));
    assert_eq!(
        fx.child_commits_above_remote_tip(),
        vec!["C1: child work".to_string()],
        "ONLY the child's own commits are replayed"
    );
}

/// The same fixture, driven the old way, proving the fixture really does
/// reproduce the defect rather than merely describing it: rebasing onto the
/// stale LOCAL branch exits 0 and drops `parent.txt`.
#[test]
fn rebasing_onto_the_stale_local_default_silently_drops_parent_work() {
    let fx = Fixture::independent_child("main");

    let out = git_out(&fx.repo, &["rebase", "--onto", "main", PARENT, CHILD]);
    assert!(
        out.status.success(),
        "the defect is that this SUCCEEDS: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(fx.file_at_child("parent.txt"), None, "this is the silent data loss #8583 fixes");
}

// ───────────────────────────────────────────────────────────────────────────
// The visible case: a child that edits parent-introduced content.
// ───────────────────────────────────────────────────────────────────────────

#[test]
fn child_editing_parent_content_replays_cleanly_onto_the_fetched_tip() {
    let fx = Fixture::overlapping_child("main");

    let plan = reconcile_stack::plan(&fx.request()).expect("plan must succeed");
    assert_eq!(plan.target_commit, fx.remote_tip);

    reconcile_stack::rebase(&plan).expect("rebase onto the real merged parent must not conflict");

    assert_eq!(
        fx.file_at_child("parent.txt").as_deref(),
        Some("parent\nchild-extends-parent\n"),
        "the child's edit sits on top of the merged parent's content"
    );
    assert_eq!(fx.child_commits_above_remote_tip(), vec!["C1: child work".to_string()]);
}

#[test]
fn child_editing_parent_content_conflicts_against_the_stale_local_default() {
    let fx = Fixture::overlapping_child("main");

    let out = git_out(&fx.repo, &["rebase", "--onto", "main", PARENT, CHILD]);
    assert!(
        !out.status.success(),
        "the stale-destination rebase must be the thing that conflicts"
    );
    // Leave no rebase in progress for the fixture's Drop.
    let _ = git_out(&fx.repo, &["rebase", "--abort"]);
}

// ───────────────────────────────────────────────────────────────────────────
// Destination resolution under every shape the local default branch can take.
// ───────────────────────────────────────────────────────────────────────────

#[test]
fn destination_is_the_fetched_tip_when_the_local_default_is_absent() {
    let fx = Fixture::independent_child("main");
    // Detach, then delete the local default branch entirely.
    let head = stdout(&fx.repo, &["rev-parse", "HEAD"]);
    git(&fx.repo, &["checkout", "-q", "--detach", &head]);
    git(&fx.repo, &["branch", "-D", "main"]);
    assert!(!git_out(&fx.repo, &["rev-parse", "--verify", "main"])
        .status
        .success());

    let plan = reconcile_stack::plan(&fx.request()).expect("a missing local default is not fatal");
    assert_eq!(plan.target_commit, fx.remote_tip);

    reconcile_stack::rebase(&plan).expect("rebase must succeed with no local default branch");
    assert_eq!(fx.file_at_child("parent.txt").as_deref(), Some("parent\n"));
}

#[test]
fn destination_is_the_fetched_tip_when_the_local_default_has_diverged() {
    let fx = Fixture::independent_child("main");
    git(&fx.repo, &["checkout", "-q", "main"]);
    write(&fx.repo, "local-only.txt", "divergent\n");
    commit_all(&fx.repo, "local divergence never pushed");
    let divergent = stdout(&fx.repo, &["rev-parse", "main"]);

    let plan = reconcile_stack::plan(&fx.request()).expect("plan must succeed");
    assert_eq!(plan.target_commit, fx.remote_tip);
    assert_ne!(plan.target_commit, divergent);

    reconcile_stack::rebase(&plan).expect("rebase must succeed");
    assert_eq!(
        fx.file_at_child("local-only.txt"),
        None,
        "the divergent local commit must not be dragged onto the child"
    );
    assert_eq!(fx.file_at_child("parent.txt").as_deref(), Some("parent\n"));
    assert_eq!(
        stdout(&fx.repo, &["rev-parse", "main"]),
        divergent,
        "reconciliation must not move the operator's local default branch"
    );
}

#[test]
fn destination_is_the_fetched_tip_when_the_local_default_is_checked_out_elsewhere() {
    let fx = Fixture::independent_child("main");
    // A linked worktree holding the DEFAULT branch: the shape that makes a
    // "just check out main and reset --hard" fix impossible, and the reason
    // the destination has to be a commit rather than a branch checkout.
    let head = stdout(&fx.repo, &["rev-parse", "HEAD"]);
    git(&fx.repo, &["checkout", "-q", "--detach", &head]);
    let other = fx.sibling("default-wt");
    git(&fx.repo, &["worktree", "add", "-q", other.to_str().unwrap(), "main"]);

    let plan = reconcile_stack::plan(&fx.request()).expect("plan must succeed");
    assert_eq!(plan.target_commit, fx.remote_tip);
    reconcile_stack::rebase(&plan).expect("rebase must succeed");
    assert_eq!(fx.file_at_child("parent.txt").as_deref(), Some("parent\n"));
    assert_eq!(
        stdout(&fx.repo, &["rev-parse", "main"]),
        fx.stale_local_tip,
        "the other worktree's checked-out branch is left exactly where it was"
    );
}

#[test]
fn a_custom_default_branch_name_is_honored_end_to_end() {
    let fx = Fixture::independent_child("trunk");

    let plan = reconcile_stack::plan(&fx.request()).expect("plan must succeed");
    assert_eq!(plan.default_branch, "trunk", "the NAME is what `gh pr edit --base` needs");
    assert_eq!(plan.target_ref, "refs/remotes/origin/trunk");
    assert_eq!(plan.target_commit, fx.remote_tip);

    reconcile_stack::rebase(&plan).expect("rebase must succeed");
    assert_eq!(fx.file_at_child("parent.txt").as_deref(), Some("parent\n"));
}

// ───────────────────────────────────────────────────────────────────────────
// Refusals: every one must happen BEFORE the child branch is touched.
// ───────────────────────────────────────────────────────────────────────────

#[test]
fn a_fetch_failure_refuses_instead_of_reusing_stale_local_state() {
    let fx = Fixture::independent_child("main");
    let before = stdout(&fx.repo, &["rev-parse", CHILD]);
    // Point origin at a path that is not a repository: the fetch fails the
    // way a network outage does, and the pre-#8583 `|| true` would have
    // carried straight on into the stale-destination rebase.
    git(&fx.repo, &["remote", "set-url", "origin", "/nonexistent/loom-8583"]);

    let err = reconcile_stack::plan(&fx.request()).expect_err("a failed fetch must refuse");
    assert_eq!(err.prerequisite, Prerequisite::Fetch);
    assert!(
        err.message.contains("Refusing"),
        "diagnostics must say it refused: {}",
        err.message
    );
    assert_eq!(
        stdout(&fx.repo, &["rev-parse", CHILD]),
        before,
        "nothing may be mutated by a refused plan"
    );
}

#[test]
fn a_missing_remote_target_is_reported_as_its_own_prerequisite() {
    let fx = Fixture::independent_child("main");
    let before = stdout(&fx.repo, &["rev-parse", CHILD]);

    let req = PlanRequest {
        default_branch: "no-such-default",
        ..fx.request()
    };
    let err = reconcile_stack::plan(&req).expect_err("a missing remote branch must refuse");
    assert_eq!(err.prerequisite, Prerequisite::RemoteTarget);
    assert_eq!(stdout(&fx.repo, &["rev-parse", CHILD]), before);
}

#[test]
fn a_stale_parent_pin_refuses_rather_than_replaying_the_wrong_range() {
    let fx = Fixture::independent_child("main");
    let before = stdout(&fx.repo, &["rev-parse", CHILD]);

    // A pin left behind by an EARLIER merge of a reused `feature/issue-N`
    // name: it resolves, but it is not an ancestor of this child (#8010).
    git(&fx.repo, &["checkout", "-q", "main"]);
    write(&fx.repo, "someone-elses-slice.txt", "a different issue\n");
    commit_all(&fx.repo, "an unrelated commit the child never had");
    let not_an_ancestor = stdout(&fx.repo, &["rev-parse", "HEAD"]);
    git(&fx.repo, &["branch", "-D", PARENT]);
    let _ = git_out(&fx.repo, &["update-ref", "-d", &format!("refs/remotes/origin/{PARENT}")]);
    git(
        &fx.repo,
        &[
            "update-ref",
            &format!("refs/loom/parent/{PARENT}"),
            &not_an_ancestor,
        ],
    );

    let err = reconcile_stack::plan(&fx.request()).expect_err("a stale pin must refuse");
    assert_eq!(err.prerequisite, Prerequisite::ParentAncestry);
    assert!(err.message.contains("NOT an ancestor"), "{}", err.message);
    assert!(
        err.message.contains("silently add commits"),
        "the consequence must be named: {}",
        err.message
    );
    assert_eq!(stdout(&fx.repo, &["rev-parse", CHILD]), before);
}

#[test]
fn an_unresolvable_parent_refuses_before_the_rebase_can_misfire() {
    let fx = Fixture::independent_child("main");
    let before = stdout(&fx.repo, &["rev-parse", CHILD]);

    git(&fx.repo, &["checkout", "-q", "main"]);
    git(&fx.repo, &["branch", "-D", PARENT]);
    let _ = git_out(&fx.repo, &["update-ref", "-d", &format!("refs/remotes/origin/{PARENT}")]);

    let err = reconcile_stack::plan(&fx.request()).expect_err("no parent ref anywhere must refuse");
    assert_eq!(err.prerequisite, Prerequisite::ParentRef);
    assert!(err.message.contains("refs/loom/parent/"), "{}", err.message);
    assert_eq!(stdout(&fx.repo, &["rev-parse", CHILD]), before);
}

#[test]
fn the_pinned_parent_fallback_still_works_when_the_branch_is_gone() {
    let fx = Fixture::independent_child("main");
    fx.pin_and_drop_parent();

    let plan =
        reconcile_stack::plan(&fx.request()).expect("the #7982 pin fallback must still work");
    assert_eq!(
        plan.parent_ref,
        format!("refs/loom/parent/{PARENT}"),
        "the pin is the upstream when the branch name is gone"
    );
    assert_eq!(plan.parent_pin_ref.as_deref(), Some(plan.parent_ref.as_str()));
    assert_eq!(plan.target_commit, fx.remote_tip);

    reconcile_stack::rebase(&plan).expect("rebase must succeed through the pinned-ref path");
    assert_eq!(fx.file_at_child("parent.txt").as_deref(), Some("parent\n"));
    assert_eq!(fx.child_commits_above_remote_tip(), vec!["C1: child work".to_string()]);
}

// ───────────────────────────────────────────────────────────────────────────
// Worktree routing and active-work deferral.
// ───────────────────────────────────────────────────────────────────────────

#[test]
fn the_rebase_runs_inside_the_worktree_holding_the_child_branch() {
    let fx = Fixture::independent_child("main");
    let child_wt = fx.sibling("child-wt");
    git(&fx.repo, &["worktree", "add", "-q", child_wt.to_str().unwrap(), CHILD]);

    let plan = reconcile_stack::plan(&fx.request()).expect("plan must succeed");
    assert!(
        plan.child_worktree.is_some(),
        "the managed worktree holding the child branch must be found (#3776)"
    );
    assert_eq!(plan.git_dir, plan.child_worktree.clone().unwrap());

    reconcile_stack::rebase(&plan)
        .expect("git refuses to rebase a branch checked out elsewhere — it must run THERE");
    assert_eq!(fx.file_at_child("parent.txt").as_deref(), Some("parent\n"));
}

/// Active child work is still deferred: an uncommitted edit in the worktree
/// holding the child branch refuses the whole reconcile, before the fetched
/// destination can be applied to anything.
#[test]
fn uncommitted_child_work_defers_the_reconcile() {
    let fx = Fixture::independent_child("main");
    let child_wt = fx.sibling("child-wt");
    git(&fx.repo, &["worktree", "add", "-q", child_wt.to_str().unwrap(), CHILD]);
    write(&child_wt, "in-progress.txt", "builder is still typing\n");
    let before = stdout(&fx.repo, &["rev-parse", CHILD]);

    let err = reconcile_stack::plan(&fx.request()).expect_err("a dirty child worktree must refuse");
    assert_eq!(err.prerequisite, Prerequisite::DirtyWorktree);
    assert_eq!(stdout(&fx.repo, &["rev-parse", CHILD]), before);
    assert_eq!(
        std::fs::read_to_string(child_wt.join("in-progress.txt")).unwrap(),
        "builder is still typing\n",
        "the in-progress work is left untouched"
    );
}

/// Unrelated state in the main checkout — tracked files, untracked files, and
/// other branch refs — must come through a reconcile bit-for-bit unchanged.
#[test]
fn unrelated_main_checkout_state_is_left_alone() {
    let fx = Fixture::independent_child("main");
    git(&fx.repo, &["branch", "unrelated/keep-me", "main"]);
    let unrelated_before = stdout(&fx.repo, &["rev-parse", "unrelated/keep-me"]);
    // Ignored, like real operator scratch: an *unignored* untracked file is
    // a dirty tree, which this script has always refused outright.
    std::fs::write(fx.repo.join(".git/info/exclude"), "scratch.txt\n").unwrap();
    write(&fx.repo, "scratch.txt", "operator scratch\n");

    let plan = reconcile_stack::plan(&fx.request()).expect("plan must succeed");
    reconcile_stack::rebase(&plan).expect("rebase must succeed");

    assert_eq!(stdout(&fx.repo, &["rev-parse", "unrelated/keep-me"]), unrelated_before);
    assert_eq!(
        stdout(&fx.repo, &["rev-parse", "main"]),
        fx.stale_local_tip,
        "the local default branch ref is NOT reset by reconciliation"
    );
    assert_eq!(
        std::fs::read_to_string(fx.repo.join("scratch.txt")).unwrap(),
        "operator scratch\n"
    );
    // The origin fixture is only read from; assert it is still intact.
    assert!(fx.origin.join("HEAD").exists());
}
