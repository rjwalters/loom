//! Branch -> holding-worktree map shared by every branch-deletion site (#10766).

use std::path::{Path, PathBuf};
use std::process::Command;

/// Branch -> the worktree that holds it (issue #10766). One shared helper for
/// every branch-deletion site, so "a kept worktree still has this branch" is
/// decided in one place.
///
/// A worktree holds a branch when it has it checked out (`branch refs/heads/x`
/// in `git worktree list --porcelain`) **or** when it is mid-rebase / mid-bisect:
/// git reports such a worktree as `detached`, yet `git branch -D` still refuses
/// the branch ("used by worktree at ..."), because the rebase will move it back
/// on completion. That detached-but-held case is what failed
/// `loom-fleet-clean` on robb-studio (gf180-trng `pr-161`, loom `pr-7904` /
/// `pr-10252`).
pub(crate) fn branch_holders(repo_root: &Path) -> std::collections::HashMap<String, PathBuf> {
    let mut holders = std::collections::HashMap::new();
    let Ok(out) = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo_root)
        .output()
    else {
        return holders;
    };
    if !out.status.success() {
        return holders;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut current: Option<PathBuf> = None;
    for line in stdout.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            let path = PathBuf::from(p.trim());
            for b in in_progress_branches(&path) {
                holders.entry(b).or_insert_with(|| path.clone());
            }
            current = Some(path);
        } else if let Some(name) = line.strip_prefix("branch refs/heads/") {
            if let Some(path) = &current {
                holders.insert(name.trim().to_string(), path.clone());
            }
        }
    }
    holders
}

/// Branches a worktree is holding while "detached": the branch being rebased
/// (`rebase-merge/head-name`, `rebase-apply/head-name`) and a bisect's start
/// branch (`BISECT_START`).
fn in_progress_branches(worktree: &Path) -> Vec<String> {
    let Ok(out) = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(worktree)
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let git_dir = if Path::new(&raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        worktree.join(raw)
    };
    let mut names = Vec::new();
    for rel in ["rebase-merge/head-name", "rebase-apply/head-name"] {
        if let Ok(c) = std::fs::read_to_string(git_dir.join(rel)) {
            if let Some(b) = c.trim().strip_prefix("refs/heads/") {
                names.push(b.to_string());
            }
        }
    }
    if let Ok(c) = std::fs::read_to_string(git_dir.join("BISECT_START")) {
        let c = c.trim();
        if !c.is_empty() {
            names.push(c.strip_prefix("refs/heads/").unwrap_or(c).to_string());
        }
    }
    names
}

/// The `kept:` log line for a branch skipped because a worktree holds it.
#[must_use]
pub(crate) fn kept_line(worktree: &Path, branch: &str) -> String {
    format!("kept: worktree {} holds {branch}", worktree.display())
}

/// If `branch` is held by a worktree, log the `kept:` line and return `true`
/// (the caller must skip the deletion).
pub(crate) fn skip_if_held(repo_root: &Path, branch: &str) -> bool {
    match branch_holders(repo_root).get(branch) {
        Some(path) => {
            println!("  {}", kept_line(path, branch));
            true
        }
        None => false,
    }
}

/// Worktree path from git's "cannot delete branch 'x' used by worktree at
/// '<path>'" refusal, if that is what `cause` is. Backstop for holders the
/// porcelain/state-file probe did not see.
pub(super) fn held_by_worktree(cause: &str) -> Option<PathBuf> {
    let rest = cause.split("used by worktree at '").nth(1)?;
    Some(PathBuf::from(rest.split('\'').next()?))
}

#[cfg(test)]
#[path = "branch_holders_tests.rs"]
mod tests;
