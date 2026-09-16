//! Issue #7812: `clean --aggressive` must reap a worktree whose work has
//! LANDED under rewritten commit SHAs, and must never reap one whose landed
//! state could not be determined.
//!
//! Before #7812 the decision was `is_ancestor_of_origin_main` — raw
//! reachability — which is false for every squash merge (#5189) and equally
//! false for a rebase merge, since GitHub's rebase merge "always updates the
//! committer information and creates new commit SHAs". These fixtures build
//! both rewrites for real (`git merge --squash` and a cherry-pick, the local
//! equivalents of the two forge strategies) and drive the live decision tree
//! over them.
//!
//! Every worktree here is **detached**, deliberately: a detached worktree has
//! no `feature/issue-N` branch, so neither the open-PR probe nor the
//! issue-state probe runs and the test never touches `gh` or the network.
//! What remains is exactly the rung under test — the offline
//! `git merge-tree --write-tree` tree-equality comparison.

use std::path::Path;

use loom_daemon::worktree_ops::aggressive::{
    clean_aggressive, evaluate_aggressive_candidate, Decision, Reason, WorktreeInfo,
    LOOM_MANAGED_SENTINEL,
};
use loom_daemon::worktree_ops::landed::Landed;

fn git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Loom Test")
        .env("GIT_AUTHOR_EMAIL", "loom@example.com")
        .env("GIT_COMMITTER_NAME", "Loom Test")
        .env("GIT_COMMITTER_EMAIL", "loom@example.com")
        .status()
        .expect("git must spawn");
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

/// A repo with an `origin` remote and a seeded `main`, plus a managed
/// (detached) worktree at `.loom/worktrees/pr-9999` sitting on `head_rev`.
struct Fixture {
    _origin: tempfile::TempDir,
    repo: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let origin = tempfile::tempdir().unwrap();
        git(origin.path(), &["init", "-q", "--bare"]);
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q", "--initial-branch=main"]);
        git(repo.path(), &["config", "user.email", "loom@example.com"]);
        git(repo.path(), &["config", "user.name", "Loom Test"]);
        // Keep the managed worktrees out of the index: `git add -A` below
        // would otherwise register `.loom/worktrees/pr-9999` as an embedded
        // repository and break the push.
        std::fs::write(repo.path().join(".gitignore"), ".loom/\n").unwrap();
        git(repo.path(), &["add", ".gitignore"]);
        git(repo.path(), &["commit", "-q", "-m", "seed"]);
        git(repo.path(), &["remote", "add", "origin", origin.path().to_str().unwrap()]);
        git(repo.path(), &["push", "-q", "origin", "main"]);
        Self {
            _origin: origin,
            repo,
        }
    }

    fn path(&self) -> &Path {
        self.repo.path()
    }

    /// Attach a managed, detached worktree at `.loom/worktrees/pr-9999`,
    /// parked on `rev`.
    fn attach_worktree(&self, rev: &str) -> std::path::PathBuf {
        let wt = self.path().join(".loom").join("worktrees").join("pr-9999");
        git(
            self.path(),
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                wt.to_str().unwrap(),
                rev,
            ],
        );
        std::fs::write(wt.join(LOOM_MANAGED_SENTINEL), "").unwrap();
        wt
    }

    /// Build a `feature/issue-4242` branch carrying one real change, and
    /// return its tip SHA. Leaves the checkout back on `main`.
    fn feature_branch(&self) -> String {
        git(self.path(), &["checkout", "-q", "-b", "feature/issue-4242"]);
        std::fs::write(self.path().join("feature.txt"), "landed content\n").unwrap();
        git(self.path(), &["add", "-A"]);
        git(self.path(), &["commit", "-q", "-m", "issue 4242 work"]);
        let tip = String::from_utf8(
            std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(self.path())
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        git(self.path(), &["checkout", "-q", "main"]);
        tip
    }

    fn push_main(&self) {
        git(self.path(), &["push", "-q", "-f", "origin", "main"]);
        git(self.path(), &["fetch", "-q", "origin"]);
    }
}

/// #7812 AC: a SQUASH-merged worktree is reaped. `git merge --squash` folds
/// the branch into one brand-new commit on `main`, so the worktree's HEAD is
/// provably not an ancestor of `origin/main` — the exact shape that made
/// aggressive mode refuse to reap every merged worktree (#5189).
#[test]
fn squash_merged_worktree_is_reaped() {
    let fx = Fixture::new();
    let tip = fx.feature_branch();
    let wt = fx.attach_worktree(&tip);
    git(fx.path(), &["merge", "-q", "--squash", "feature/issue-4242"]);
    git(fx.path(), &["commit", "-q", "-m", "squash-merge (simulated)"]);
    fx.push_main();

    // Precondition: raw reachability — the pre-#7812 criterion — says NO.
    let reachable = std::process::Command::new("git")
        .args(["merge-base", "--is-ancestor", &tip, "origin/main"])
        .current_dir(fx.path())
        .status()
        .unwrap()
        .success();
    assert!(!reachable, "fixture must reproduce the squash-merge shape");

    let stats = clean_aggressive(fx.path(), /* dry_run */ false, false, false, 0);
    assert_eq!(stats.removed, 1, "a squash-merged worktree must be reaped");
    assert!(!wt.exists(), "worktree directory must be gone");
}

/// #7812 AC: a REBASE-merged worktree is reaped too — the capability #7754's
/// rebase-merge proposal could not deliver. A cherry-pick onto `main` is the
/// local equivalent: identical content, brand-new SHA, so neither raw
/// reachability nor a tip-SHA match can see it.
#[test]
fn rebase_merged_worktree_is_reaped() {
    let fx = Fixture::new();
    let tip = fx.feature_branch();
    let wt = fx.attach_worktree(&tip);
    // An unrelated commit on main first, so the cherry-pick below cannot
    // fast-forward into the identical SHA — a rebase merge replays the change
    // onto a moved base, which is precisely what rewrites the SHA.
    std::fs::write(fx.path().join("other.txt"), "meanwhile on main\n").unwrap();
    git(fx.path(), &["add", "-A"]);
    git(fx.path(), &["commit", "-q", "-m", "unrelated main commit"]);
    git(fx.path(), &["cherry-pick", &tip]);
    fx.push_main();

    let rebased_tip = String::from_utf8(
        std::process::Command::new("git")
            .args(["rev-parse", "origin/main"])
            .current_dir(fx.path())
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert_ne!(rebased_tip.trim(), tip, "the rebase must rewrite the SHA");

    let stats = clean_aggressive(fx.path(), /* dry_run */ false, false, false, 0);
    assert_eq!(stats.removed, 1, "a rebase-merged worktree must be reaped");
    assert!(!wt.exists(), "worktree directory must be gone");
}

/// #7812 AC: genuinely unlanded work is still protected — the landed
/// primitive only ever makes the *landed* class reapable.
#[test]
fn unlanded_worktree_is_not_reaped() {
    let fx = Fixture::new();
    let tip = fx.feature_branch();
    let wt = fx.attach_worktree(&tip);
    // main never receives the change.

    let stats = clean_aggressive(fx.path(), /* dry_run */ false, false, false, 0);
    assert_eq!(stats.removed, 0, "unlanded work must never be reaped");
    assert_eq!(stats.skipped_unreachable, 1);
    assert!(wt.exists(), "worktree directory must survive");
}

/// #7812 AC: `unknown` never reaps — not even under `--force`. Unrelated
/// histories make `git merge-tree --write-tree` fail outright ("refusing to
/// merge unrelated histories", exit 128), and a detached worktree has no
/// branch for the forge probe — so BOTH rungs are unavailable and the answer
/// is `unknown`, not `not-landed`.
///
/// This is the fail-closed contract: `--force` may legitimately override a
/// known-unlanded worktree, but it must not be able to override "we could not
/// tell", which is what a forge outage looks like.
#[test]
fn undeterminable_worktree_is_never_reaped_even_with_force() {
    let fx = Fixture::new();
    // `main` here is a single empty seed commit, so the orphan checkout starts
    // with an empty working tree — nothing to `git rm` first.
    git(fx.path(), &["checkout", "-q", "--orphan", "unrelated"]);
    std::fs::write(fx.path().join("orphan.txt"), "unrelated history\n").unwrap();
    git(fx.path(), &["add", "-A"]);
    git(fx.path(), &["commit", "-q", "-m", "unrelated root"]);
    let orphan_tip = String::from_utf8(
        std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(fx.path())
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    git(fx.path(), &["checkout", "-q", "main"]);
    let wt = fx.attach_worktree(&orphan_tip);

    let stats = clean_aggressive(
        fx.path(),
        /* dry_run */ false,
        /* force */ true,
        /* safe */ false,
        0,
    );
    assert_eq!(stats.removed, 0, "`unknown` must never reap, even with --force");
    assert_eq!(
        stats.forced_unreachable, 0,
        "the force override must not fire on an undetermined answer"
    );
    assert!(wt.exists(), "worktree directory must survive");
}

// ---------------------------------------------------------------------------
// Decision-tree cases for the three-way answer.
//
// These live here rather than in `aggressive.rs`'s own `#[cfg(test)]` block
// only because that file sits at its file-size-ratchet baseline
// (scripts/file-size-baseline.txt) and may not grow; the decision function is
// `pub`, so testing it from here is equivalent. The pre-existing in-file cases
// were rewritten in place to pass `Landed::{Reachable,Rewritten,NotLanded}`.
// ---------------------------------------------------------------------------

/// A canonical managed worktree on `feature/issue-42`, matching the in-file
/// test module's `wt()` helper.
fn wt() -> WorktreeInfo {
    WorktreeInfo {
        path: std::path::PathBuf::from("/repo/.loom/worktrees/issue-42"),
        head: Some("abc123".to_string()),
        branch: Some("refs/heads/feature/issue-42".to_string()),
        detached: false,
        locked: false,
        bare: false,
    }
}

/// #7812: `Landed::Unknown` — neither the forge nor the offline tree
/// comparison could answer — KEEPS, and unlike a known-unlanded worktree
/// it keeps even under `--force`. A forge outage is not evidence of
/// unmerged work, but reaping on it loses work irrecoverably, so this is
/// the one removal-blocking state `--force` cannot override.
#[test]
fn landed_unknown_is_kept_even_with_force() {
    let w = wt();
    let (d, r) = evaluate_aggressive_candidate(
        &w,
        false,
        Some((false, true)),
        false,
        true,
        true,
        false,
        Landed::Unknown,
        Some(999_999), // old enough that the age gate cannot be what keeps it
        86400,
        true, // --force
        false,
        Some(&|| "CLOSED".to_string()),
    );
    assert_eq!(d, Decision::Keep, "`unknown` must never reap");
    assert_eq!(r, Reason::LandedUnknown);
}

/// #7812 contrast: a KNOWN-unlanded worktree stays force-removable — the
/// fail-closed arm above is specific to `Unknown`, and does not quietly
/// re-freeze the documented `--force` override (#5735).
#[test]
fn not_landed_is_still_force_removable() {
    let w = wt();
    let (d, r) = evaluate_aggressive_candidate(
        &w,
        false,
        Some((false, true)),
        false,
        true,
        true,
        false,
        Landed::NotLanded,
        Some(999_999),
        86400,
        true,
        false,
        Some(&|| "CLOSED".to_string()),
    );
    assert_eq!(d, Decision::Remove);
    assert_eq!(r, Reason::ForceOverrideUnreachable);
}
