//! Shared "which issues have a live sweep right now" helpers.
//!
//! Rust port of the `clean.py` / `orphan_recovery.py` helpers that read
//! `.loom/locks/issue-<N>/` (the spawn loop's atomic claim-lock directories)
//! and union them with `.loom/spawn-loop-state.json::running`. Used by:
//!
//! - `clean.rs`'s standard worktree pass (skip issues with a live claim)
//!   and its `--daemon` crash-recovery label revert.
//! - `aggressive.rs`'s `active_shepherd` gate.
//! - `orphan_recovery.rs`'s [`crate::worktree_ops::orphan_recovery::gather_liveness_evidence`].

use std::path::Path;

use super::spawn_loop_state::{read_spawn_loop_state, SpawnLoopState};

/// Path to the spawn loop's atomic claim-lock directory
/// (`<repo_root>/.loom/locks/`). Each in-flight sweep child holds a
/// `.loom/locks/issue-<N>/` directory (mkdir-atomic primitive); the lock's
/// presence is the lock, an `owner.json` inside is diagnostic-only.
#[must_use]
pub fn locks_dir(repo_root: &Path) -> std::path::PathBuf {
    repo_root.join(".loom").join("locks")
}

/// Issue numbers with a present `.loom/locks/issue-<N>/` claim-lock dir.
#[must_use]
pub fn active_locked_issues(repo_root: &Path) -> std::collections::HashSet<u32> {
    let dir = locks_dir(repo_root);
    let mut out = std::collections::HashSet::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(rest) = name.strip_prefix("issue-") {
            if let Ok(n) = rest.parse::<u32>() {
                out.insert(n);
            }
        }
    }
    out
}

/// Union of `.loom/spawn-loop-state.json::running[].issue` and
/// `.loom/locks/issue-<N>/` — the set of issues currently "in flight" per
/// the local-workspace evidence (no forge/daemon calls). Mirrors
/// `clean.py::_active_spawn_loop_issues`.
#[must_use]
pub fn active_spawn_loop_issues(repo_root: &Path) -> std::collections::HashSet<u32> {
    let state: SpawnLoopState = read_spawn_loop_state(repo_root);
    let mut active: std::collections::HashSet<u32> = state
        .running
        .iter()
        .filter(|t| t.issue != 0)
        .map(|t| t.issue)
        .collect();
    active.extend(active_locked_issues(repo_root));
    active
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn no_locks_dir_yields_empty_set() {
        let dir = tempdir().unwrap();
        assert!(active_locked_issues(dir.path()).is_empty());
    }

    #[test]
    fn locks_dir_contributes_issue_numbers() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(locks_dir(dir.path()).join("issue-7")).unwrap();
        std::fs::create_dir_all(locks_dir(dir.path()).join("issue-9")).unwrap();
        // Non-issue entries (e.g. the transient repo-global worktree-add
        // lock) must be ignored.
        std::fs::create_dir_all(locks_dir(dir.path()).join("worktree-add")).unwrap();
        let active = active_locked_issues(dir.path());
        assert_eq!(active, std::collections::HashSet::from([7, 9]));
    }

    #[test]
    fn active_spawn_loop_issues_unions_state_and_locks() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        std::fs::write(
            dir.path().join(".loom").join("spawn-loop-state.json"),
            r#"{"running": [{"issue": 5, "pid": 1}]}"#,
        )
        .unwrap();
        std::fs::create_dir_all(locks_dir(dir.path()).join("issue-6")).unwrap();
        let active = active_spawn_loop_issues(dir.path());
        assert_eq!(active, std::collections::HashSet::from([5, 6]));
    }
}
