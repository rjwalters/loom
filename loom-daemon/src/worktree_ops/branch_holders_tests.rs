use super::super::clean::{clean_branches, exit_code, CleanOptions, CleanupStats};
use super::*;

// --- worktree-held branches are skipped, not errors (#10766) ----------

fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(["-c", "user.email=t@example.com", "-c", "user.name=t"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
        .status
        .success()
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    git_ok(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
}

/// Repo with a stale (no remote) branch `feature/issue-1` checked out in a
/// dirty kept worktree `.loom/worktrees/pr-1`.
fn repo_with_dirty_worktree_on_stale_branch() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().canonicalize().unwrap();
    assert!(git_ok(&repo, &["init", "-q", "-b", "main"]));
    std::fs::write(repo.join("a.txt"), "base\n").unwrap();
    assert!(git_ok(&repo, &["add", "."]));
    assert!(git_ok(&repo, &["commit", "-q", "-m", "init"]));
    let wt = crate::worktree_root::worktree_root(&repo).join("pr-1");
    assert!(git_ok(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature/issue-1",
            wt.to_str().unwrap()
        ]
    ));
    std::fs::write(wt.join("dirty.txt"), "uncommitted").unwrap();
    (tmp, repo, wt)
}

fn force_opts() -> CleanOptions {
    CleanOptions {
        force: true,
        ..CleanOptions::default()
    }
}

#[test]
fn clean_branches_keeps_a_branch_held_by_a_dirty_worktree_without_error() {
    let (_tmp, repo, wt) = repo_with_dirty_worktree_on_stale_branch();
    let mut stats = CleanupStats::default();
    clean_branches(&repo, &mut stats, &force_opts());
    assert_eq!(stats.errors, 0, "{:?}", stats.error_details);
    assert_eq!(exit_code(stats.errors), 0);
    assert_eq!(stats.kept_branches, 1);
    assert!(branch_exists(&repo, "feature/issue-1"));
    assert!(wt.join("dirty.txt").exists());
    assert_eq!(
        kept_line(&wt, "feature/issue-1"),
        format!("kept: worktree {} holds feature/issue-1", wt.display())
    );
}

#[test]
fn clean_branches_keeps_a_branch_whose_worktree_is_detached_mid_rebase() {
    // The robb-studio failure: git lists a mid-rebase worktree as `detached`,
    // so the porcelain `branch` line is absent, yet `git branch -D` refuses.
    let (_tmp, repo, wt) = repo_with_dirty_worktree_on_stale_branch();
    std::fs::remove_file(wt.join("dirty.txt")).unwrap();
    std::fs::write(wt.join("a.txt"), "feature\n").unwrap();
    assert!(git_ok(&wt, &["commit", "-q", "-am", "feature"]));
    std::fs::write(repo.join("a.txt"), "main\n").unwrap();
    assert!(git_ok(&repo, &["commit", "-q", "-am", "main change"]));
    assert!(!git_ok(&wt, &["rebase", "main"]), "rebase must conflict");
    std::fs::write(wt.join("dirty.txt"), "uncommitted").unwrap();

    let holders = branch_holders(&repo);
    assert_eq!(holders.get("feature/issue-1"), Some(&wt));

    let mut stats = CleanupStats::default();
    clean_branches(&repo, &mut stats, &force_opts());
    assert_eq!(stats.errors, 0, "{:?}", stats.error_details);
    assert_eq!(exit_code(stats.errors), 0);
    assert!(branch_exists(&repo, "feature/issue-1"));
    assert!(wt.join("dirty.txt").exists());
}

#[test]
fn a_worktree_held_refusal_from_git_is_a_skip_but_other_failures_still_error() {
    let mut stats = CleanupStats::default();
    crate::worktree_ops::clean::record_branch_delete_failure(
        &mut stats,
        "feature/issue-7657",
        "branch feature/issue-7657",
        "error: cannot delete branch 'feature/issue-7657' used by worktree at '/r/.loom/worktrees/pr-7904'",
    );
    assert_eq!(stats.errors, 0);
    assert_eq!(stats.kept_branches, 1);
    assert_eq!(exit_code(stats.errors), 0);

    crate::worktree_ops::clean::record_branch_delete_failure(
        &mut stats,
        "x",
        "branch x",
        "error: unable to write ref: permission denied",
    );
    assert_eq!(stats.errors, 1);
    assert_eq!(exit_code(stats.errors), 1);
}
