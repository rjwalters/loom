//! Worktree removal safety checks.
//!
//! Rust port of `loom_tools.common.worktree_safety` (the process-detection
//! piece `clean.py` needs — `find_processes_using_directory`) plus the small
//! git-status helpers `clean.py` inlines (`check_uncommitted_changes`). This
//! is the load-bearing safety layer the epic body calls out: "same tier as
//! the `.loom-managed` sentinel" — a worktree with an active process must
//! never be torn down even when every other gate says "stale".
//!
//! `common/worktree_safety.py` + its test file are deleted by this issue
//! (`clean.py` was its sole importer); this module is the Rust replacement
//! for the one function `clean.py` actually used.

use std::path::Path;
use std::process::Command;

/// Find PIDs with their current working directory inside `directory`
/// (recursively — matches the Python `_find_processes_lsof` / `_find_processes_proc`
/// behavior of matching the directory itself or any descendant).
///
/// macOS/BSD: shells out to `lsof +D <dir> -F pt` and keeps only `cwd`-typed
/// entries. Linux: scans `/proc/*/cwd` symlinks. Any other platform falls
/// back to the `lsof` path. Detection failures (missing tool, permission
/// errors) degrade to an empty list rather than propagating an error — the
/// caller treats "unknown" the same as "no active processes", matching the
/// Python original's fail-open-to-empty behavior (the marker file + issue
/// state checks are the primary gates; this is defense in depth).
#[must_use]
pub fn find_processes_using_directory(directory: &Path) -> Vec<u32> {
    let directory = directory
        .canonicalize()
        .unwrap_or_else(|_| directory.to_path_buf());
    let mut pids = if cfg!(target_os = "linux") {
        find_processes_proc(&directory)
    } else {
        find_processes_lsof(&directory)
    };
    let current_pid = std::process::id();
    pids.retain(|p| *p != current_pid);
    pids
}

fn find_processes_lsof(directory: &Path) -> Vec<u32> {
    let output = match Command::new("lsof")
        .arg("+d")
        .arg(directory)
        .arg("-F")
        .arg("pt")
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    if !output.status.success() {
        return Vec::new();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut pids: Vec<u32> = Vec::new();
    let mut current_pid: Option<u32> = None;
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix('p') {
            current_pid = rest.parse().ok();
        } else if let Some(rest) = line.strip_prefix('t') {
            if rest == "cwd" {
                if let Some(pid) = current_pid {
                    if !pids.contains(&pid) {
                        pids.push(pid);
                    }
                }
            }
        }
    }
    pids
}

#[cfg(target_os = "linux")]
fn find_processes_proc(directory: &Path) -> Vec<u32> {
    let proc = Path::new("/proc");
    if !proc.is_dir() {
        return Vec::new();
    }
    let dir_str = directory.to_string_lossy().to_string();
    let mut pids = Vec::new();
    let Ok(entries) = std::fs::read_dir(proc) else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let Ok(pid) = name_str.parse::<u32>() else {
            continue;
        };
        let cwd_link = entry.path().join("cwd");
        if let Ok(cwd) = std::fs::read_link(&cwd_link) {
            let cwd_str = cwd.to_string_lossy();
            if cwd_str == dir_str || cwd_str.starts_with(&format!("{dir_str}/")) {
                pids.push(pid);
            }
        }
    }
    pids
}

#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
fn find_processes_proc(_directory: &Path) -> Vec<u32> {
    Vec::new()
}

/// True if `worktree_path` has any uncommitted changes (staged or unstaged).
/// Mirrors `clean.py::check_uncommitted_changes`: a non-directory path is
/// treated as "no changes" (nothing to lose), and any git invocation failure
/// is treated the same way (fail toward "safe to proceed", matching the
/// Python original — the caller layers other gates on top of this one).
#[must_use]
pub fn check_uncommitted_changes(worktree_path: &Path) -> bool {
    if !worktree_path.is_dir() {
        return false;
    }
    let unstaged = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["diff", "--quiet"])
        .status();
    let staged = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["diff", "--cached", "--quiet"])
        .status();
    let unstaged_dirty = unstaged.map(|s| !s.success()).unwrap_or(false);
    let staged_dirty = staged.map(|s| !s.success()).unwrap_or(false);
    unstaged_dirty || staged_dirty
}

/// Untracked files Loom writes into a managed worktree itself. These are
/// bookkeeping, not user work, so they must never make a worktree look dirty
/// to [`has_untracked_files`] — most repos gitignore them (the `loom-managed`
/// block in `.gitignore`), but a repo that predates or has edited that block
/// would otherwise see every managed worktree as permanently unreclaimable.
const LOOM_OWN_UNTRACKED_FILES: [&str; 2] = [".loom-managed", ".loom-in-use"];

/// True if `worktree_path` contains untracked, non-gitignored files that are
/// not Loom's own sentinels (issue #5939).
///
/// [`check_uncommitted_changes`] deliberately only asks `git diff` /
/// `git diff --cached`, which are both blind to untracked files — and
/// `git worktree remove --force` deletes those. For an `issue-<N>` worktree
/// that gap is bounded by the closed-issue gate; for a `pr-<N>` worktree,
/// whose branch and contents come from outside Loom, it is not, so the PR path
/// layers this on top (see [`super::clean::classify_pr_worktree`]).
///
/// A `git` failure (including "not a git worktree at all", the #5177 orphaned
/// directory case the removal path exists to clean up) reports `false` — the
/// same fail-toward-proceed convention as [`check_uncommitted_changes`], and
/// the reason the sentinel/containment gates still bound that path.
#[must_use]
pub fn has_untracked_files(worktree_path: &Path) -> bool {
    if !worktree_path.is_dir() {
        return false;
    }
    let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["ls-files", "--others", "--exclude-standard"])
        .output()
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .any(|l| !LOOM_OWN_UNTRACKED_FILES.contains(&l))
}

/// [`check_uncommitted_changes`] widened to also catch untracked files
/// ([`has_untracked_files`]) — the `uncommitted` probe the `pr-<N>` path wires
/// in (issue #5939).
///
/// Kept as a separate function rather than folded into
/// [`check_uncommitted_changes`] on purpose: the `issue-<N>` gate chain was
/// reviewed and shipped with the narrower definition, and widening it there is
/// a behavior change to a path this work does not touch.
#[must_use]
pub fn check_uncommitted_or_untracked_changes(worktree_path: &Path) -> bool {
    check_uncommitted_changes(worktree_path) || has_untracked_files(worktree_path)
}

/// Parsed `.loom-in-use` marker contents (best-effort; unknown/missing
/// fields render as `"unknown"`, matching `clean.py::clean_worktrees`'s
/// `marker_data.get(..., "unknown")` reads).
#[derive(Debug, Default, Clone)]
pub struct InUseMarker {
    pub task_id: String,
    pub pid: String,
}

/// Read a worktree's `.loom-in-use` marker file, if present.
#[must_use]
pub fn read_in_use_marker(worktree_path: &Path) -> Option<InUseMarker> {
    let marker_path = worktree_path.join(".loom-in-use");
    if !marker_path.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(&marker_path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    let obj = value.as_object();
    Some(InUseMarker {
        task_id: obj
            .and_then(|o| o.get("shepherd_task_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string(),
        pid: obj
            .and_then(|o| o.get("pid"))
            .map(|v| match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_else(|| "unknown".to_string()),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn missing_directory_has_no_uncommitted_changes() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert!(!check_uncommitted_changes(&missing));
    }

    #[test]
    fn find_processes_excludes_current_pid() {
        // Whatever `find_processes_using_directory` reports for a scratch
        // dir, it must never include our own pid (we're "using" our own
        // cwd trivially, and the Python original explicitly filters it).
        let dir = tempdir().unwrap();
        let pids = find_processes_using_directory(dir.path());
        assert!(!pids.contains(&std::process::id()));
    }

    #[test]
    fn reads_in_use_marker_with_defaults() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(".loom-in-use"), r#"{"pid": 123}"#).unwrap();
        let marker = read_in_use_marker(dir.path()).unwrap();
        assert_eq!(marker.pid, "123");
        assert_eq!(marker.task_id, "unknown");
    }

    #[test]
    fn missing_marker_is_none() {
        let dir = tempdir().unwrap();
        assert!(read_in_use_marker(dir.path()).is_none());
    }

    #[test]
    fn corrupt_marker_falls_back_to_unknown_defaults() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(".loom-in-use"), "not json").unwrap();
        let marker = read_in_use_marker(dir.path()).unwrap();
        assert_eq!(marker.pid, "unknown");
        assert_eq!(marker.task_id, "unknown");
    }

    // ------------------------------------------------------------------
    // Untracked-file gate (#5939 review): `git diff` is blind to untracked
    // files, and `git worktree remove --force` deletes them.
    // ------------------------------------------------------------------

    fn init_repo_with_commit(dir: &Path) {
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "t"],
        ] {
            assert!(Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(&args)
                .status()
                .unwrap()
                .success());
        }
        std::fs::write(dir.join("tracked.txt"), "x").unwrap();
        assert!(Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["add", "."])
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["commit", "-q", "-m", "init"])
            .status()
            .unwrap()
            .success());
    }

    #[test]
    fn a_clean_worktree_has_no_untracked_files() {
        let dir = tempdir().unwrap();
        init_repo_with_commit(dir.path());
        assert!(!has_untracked_files(dir.path()));
        assert!(!check_uncommitted_or_untracked_changes(dir.path()));
    }

    #[test]
    fn an_untracked_file_is_work_that_would_be_lost() {
        let dir = tempdir().unwrap();
        init_repo_with_commit(dir.path());
        std::fs::write(dir.path().join("scratch-notes.md"), "unsaved").unwrap();
        assert!(has_untracked_files(dir.path()));
        // The narrower legacy probe cannot see it — the exact gap this closes.
        assert!(!check_uncommitted_changes(dir.path()));
        assert!(check_uncommitted_or_untracked_changes(dir.path()));
    }

    #[test]
    fn looms_own_sentinels_are_not_untracked_user_work() {
        let dir = tempdir().unwrap();
        init_repo_with_commit(dir.path());
        std::fs::write(dir.path().join(".loom-managed"), "").unwrap();
        std::fs::write(dir.path().join(".loom-in-use"), "{}").unwrap();
        assert!(
            !has_untracked_files(dir.path()),
            "every managed worktree carries these; counting them would make the pr-<N> \
             reaper a permanent no-op in a repo without the loom .gitignore block"
        );
    }

    #[test]
    fn gitignored_build_artifacts_are_not_untracked_user_work() {
        let dir = tempdir().unwrap();
        init_repo_with_commit(dir.path());
        std::fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("target/big.bin"), "0").unwrap();
        // `.gitignore` itself is untracked here, so commit it before asserting.
        assert!(Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["add", ".gitignore"])
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["commit", "-q", "-m", "ignore"])
            .status()
            .unwrap()
            .success());
        assert!(!has_untracked_files(dir.path()));
    }

    #[test]
    fn a_non_git_directory_reports_no_untracked_files() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("loose.txt"), "x").unwrap();
        assert!(
            !has_untracked_files(dir.path()),
            "the #5177 orphaned-directory path must stay reclaimable"
        );
    }
}
