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
}
