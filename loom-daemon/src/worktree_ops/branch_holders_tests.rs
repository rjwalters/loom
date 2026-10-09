use super::super::clean::{
    clean_branches, exit_code, force_delete_cmd, record_branch_delete_failure, run_checked,
    CleanOptions, CleanupStats,
};
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
    assert_eq!(stats.held_branches, 1);
    assert_eq!(stats.kept_branches, 0);
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
    record_branch_delete_failure(
        &mut stats,
        "feature/issue-7657",
        "branch feature/issue-7657",
        "error: cannot delete branch 'feature/issue-7657' used by worktree at '/r/.loom/worktrees/pr-7904'",
    );
    assert_eq!(stats.errors, 0);
    assert_eq!(stats.held_branches, 1);
    assert_eq!(stats.kept_branches, 0);
    assert_eq!(exit_code(stats.errors), 0);

    record_branch_delete_failure(
        &mut stats,
        "x",
        "branch x",
        "error: unable to write ref: permission denied",
    );
    assert_eq!(stats.errors, 1);
    assert_eq!(exit_code(stats.errors), 1);
}

// --- #10851 follow-ups ---------------------------------------------------

/// Commit `contents` to `file` in `dir` (which must be on a branch).
fn commit_file(dir: &Path, file: &str, contents: &str, msg: &str) {
    std::fs::write(dir.join(file), contents).unwrap();
    assert!(git_ok(dir, &["add", file]));
    assert!(git_ok(dir, &["commit", "-q", "-m", msg]));
}

#[test]
fn update_refs_branches_are_held() {
    // main <- stack1 <- feature/issue-1, with `stack1`'s commit conflicting
    // with a later commit on main, so `rebase --update-refs` stops mid-way.
    let (_tmp, repo, wt) = repo_with_dirty_worktree_on_stale_branch();
    std::fs::remove_file(wt.join("dirty.txt")).unwrap();
    commit_file(&wt, "a.txt", "stack1\n", "stack1");
    assert!(git_ok(&wt, &["branch", "stack1"]));
    commit_file(&wt, "b.txt", "top\n", "top");
    commit_file(&repo, "a.txt", "main\n", "main change");

    let rebase = Command::new("git")
        .args(["-c", "user.email=t@example.com", "-c", "user.name=t"])
        .args(["rebase", "--update-refs", "main"])
        .env("GIT_EDITOR", "true")
        .current_dir(&wt)
        .output()
        .unwrap();
    assert!(!rebase.status.success(), "rebase must conflict");

    let holders = branch_holders(&repo);
    assert_eq!(holders.get("feature/issue-1"), Some(&wt), "head-name");
    assert_eq!(holders.get("stack1"), Some(&wt), "update-refs");

    let mut stats = CleanupStats::default();
    clean_branches(&repo, &mut stats, &force_opts());
    assert_eq!(stats.errors, 0, "{:?}", stats.error_details);
    assert!(branch_exists(&repo, "stack1"));
    assert!(stats.held_branches >= 1);
    assert_eq!(stats.kept_branches, 0);
}

#[test]
fn update_refs_parser_takes_every_third_line() {
    let null = "0000000000000000000000000000000000000000";
    let oid = "e6392a6aee28f6b06b399465a5598f90d24d6230";
    let fixture = format!(
        "refs/heads/stack1\n{oid}\n{null}\nrefs/tags/v1\n{oid}\n{null}\nrefs/heads/stack2\n{oid}\n{null}\n"
    );
    assert_eq!(update_refs_branches(&fixture), vec!["stack1", "stack2"]);
    assert!(update_refs_branches("").is_empty());
}

#[test]
fn force_delete_runs_git_under_the_c_locale() {
    let cmd = force_delete_cmd(Path::new("/r"), "b");
    let lc_all = cmd
        .get_envs()
        .find(|(k, _)| *k == "LC_ALL")
        .and_then(|(_, v)| v);
    assert_eq!(lc_all, Some(std::ffi::OsStr::new("C")));
}

#[test]
fn localized_refusal_is_still_a_skip() {
    // An operator locale of German. The env is set on the child only, so the
    // test needs no serialization. `LC_ALL` is deliberately left unset here:
    // setting it would replace `force_delete_cmd`'s own `LC_ALL=C` (pinned by
    // the test above). Passes trivially when no `de` catalog is installed.
    let (_tmp, repo, wt) = repo_with_dirty_worktree_on_stale_branch();
    let mut cmd = force_delete_cmd(&repo, "feature/issue-1");
    cmd.env("LANGUAGE", "de")
        .env("LANG", "de_DE.UTF-8")
        .env("LC_MESSAGES", "de_DE.UTF-8");
    let cause = run_checked(cmd).expect_err("a held branch must be refused");
    assert_eq!(held_by_worktree(&cause), Some(wt), "{cause}");

    let mut stats = CleanupStats::default();
    record_branch_delete_failure(&mut stats, "feature/issue-1", "branch feature/issue-1", &cause);
    assert_eq!(stats.errors, 0, "{:?}", stats.error_details);
    assert_eq!(stats.held_branches, 1);
}

#[test]
fn bisect_in_linked_worktree_holds_its_start_branch() {
    let (_tmp, repo, wt) = repo_with_dirty_worktree_on_stale_branch();
    std::fs::remove_file(wt.join("dirty.txt")).unwrap();
    for i in 0..3 {
        commit_file(&wt, "b.txt", &format!("{i}\n"), &format!("c{i}"));
    }
    assert!(git_ok(&wt, &["bisect", "start", "HEAD", "HEAD~3"]));

    // Precondition: git reports the worktree as detached, so only the
    // `BISECT_START` probe can see that it holds the branch.
    let porcelain = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(&repo)
        .output()
        .unwrap();
    let porcelain = String::from_utf8_lossy(&porcelain.stdout).into_owned();
    let block = porcelain
        .split("\n\n")
        .find(|b| b.starts_with(&format!("worktree {}", wt.display())))
        .unwrap_or_else(|| panic!("no porcelain entry for wt: {porcelain}"));
    assert!(block.lines().any(|l| l == "detached"), "{block}");

    assert_eq!(branch_holders(&repo).get("feature/issue-1"), Some(&wt));

    // git's own refusal parses to the same worktree. Newer git (2.56) appends
    // ` for bisect`; older git ends at the worktree path. Both must parse.
    // The suffixed form is pinned by `held_by_worktree_parses_the_bisect_refusal`.
    let cause = run_checked(force_delete_cmd(&repo, "feature/issue-1"))
        .expect_err("git must refuse a branch held for bisect");
    assert_eq!(held_by_worktree(&cause), Some(wt.clone()));

    let mut stats = CleanupStats::default();
    clean_branches(&repo, &mut stats, &force_opts());
    assert_eq!(stats.errors, 0, "{:?}", stats.error_details);
    assert_eq!(stats.held_branches, 1);
    assert!(branch_exists(&repo, "feature/issue-1"));
}

#[test]
fn held_by_worktree_parses_the_bisect_refusal() {
    assert_eq!(
        held_by_worktree("error: cannot delete branch 'b' used by worktree at '/r/wt' for bisect"),
        Some(PathBuf::from("/r/wt"))
    );
}
