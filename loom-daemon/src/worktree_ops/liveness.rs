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
//! - `orphan_process_reaper.rs`'s pid/pgid-aware Gate 2 (#5135), via
//!   [`active_locked_issue_roots`] — the same claim-lock data,
//!   read back with enough detail (owner pid + start time) to discriminate a
//!   live sweep's own process tree from an unrelated orphan, instead of
//!   treating "a claim exists" as blanket protection for the whole worktree.

use std::path::Path;

use chrono::{DateTime, Utc};

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

/// Minimal read-side view of a claim lock's `owner.json`, just the two
/// fields needed to build a [`LiveSweepRoot`].
///
/// Deliberately its own type rather than a re-export of `sweep_registry`'s
/// private `LockOwner` — same rationale as `live_claim::LockOwnerView`:
/// this module only needs two fields, and keeping the read-side shape local
/// means a future writer-side field addition cannot break the probe
/// (unknown fields are ignored by serde).
#[derive(Debug, serde::Deserialize)]
struct LockOwnerRootView {
    owner_pid: u32,
    #[serde(default)]
    acquired_at: String,
}

/// A live sweep's own process-tree root for one issue: the claim lock's
/// owner pid (the spawned sweep child, #3808) and when that claim was
/// acquired — used as a proxy for "when did this sweep's own process tree
/// begin" (Issue #5135).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveSweepRoot {
    /// The live sweep's own root pid.
    pub pid: u32,
    /// When the live sweep's claim was acquired (lock `acquired_at`).
    pub started_at: DateTime<Utc>,
}

/// Per-issue live-sweep root info, read back from `.loom/locks/issue-<N>/owner.json`
/// (Issue #5135) — the pid/pgid data [`crate::sweep_registry::LockOwner`]
/// already persists on every lock acquisition (#4980), but that nothing
/// previously read back out for the orphan-process reaper's Gate 2.
///
/// Only issues whose recorded `owner_pid` is a **confirmed-live, non-zombie**
/// process are included ([`crate::live_claim::pid_is_live_process`]) — a lock
/// whose owner has already died (stale, not yet reaped by
/// [`crate::sweep_registry::SweepRegistry::reconstruct`]) has no live process
/// tree to compare a candidate pid against, so callers MUST fall back to full
/// protection rather than treat "no live root" as "nothing to protect" (a
/// dead root's descendant walk is trivially empty, which would make every
/// process in the worktree look like an orphan).
///
/// An unparseable/missing `owner.json`, or an unparseable `acquired_at`, is
/// likewise excluded — never license to reap, only ever a reason to fall back
/// to the issue-scoped protection callers already apply via
/// [`active_locked_issues`] / [`active_spawn_loop_issues`].
#[must_use]
pub fn active_locked_issue_roots(
    repo_root: &Path,
) -> std::collections::HashMap<u32, LiveSweepRoot> {
    let dir = locks_dir(repo_root);
    let mut out = std::collections::HashMap::new();
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
        let Some(rest) = name.strip_prefix("issue-") else {
            continue;
        };
        let Ok(issue) = rest.parse::<u32>() else {
            continue;
        };

        let owner_path = entry.path().join("owner.json");
        let Ok(raw) = std::fs::read_to_string(&owner_path) else {
            continue;
        };
        let Ok(owner) = serde_json::from_str::<LockOwnerRootView>(&raw) else {
            continue;
        };
        if !crate::live_claim::pid_is_live_process(owner.owner_pid) {
            continue;
        }
        let Ok(acquired_at) = DateTime::parse_from_rfc3339(&owner.acquired_at) else {
            continue;
        };
        out.insert(
            issue,
            LiveSweepRoot {
                pid: owner.owner_pid,
                started_at: acquired_at.with_timezone(&Utc),
            },
        );
    }
    out
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

    fn write_owner(dir: &std::path::Path, issue: u32, owner_pid: u32, acquired_at: &str) {
        let lock = locks_dir(dir).join(format!("issue-{issue}"));
        std::fs::create_dir_all(&lock).unwrap();
        std::fs::write(
            lock.join("owner.json"),
            format!(
                r#"{{"issue":{issue},"owner_pid":{owner_pid},"acquired_at":"{acquired_at}","sweep_id":"sweep-issue-{issue}-test"}}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn active_locked_issue_roots_reads_live_owner_pid_and_start_time() {
        let dir = tempdir().unwrap();
        write_owner(dir.path(), 50, std::process::id(), "2026-01-01T00:00:00Z");

        let roots = active_locked_issue_roots(dir.path());
        let root = roots.get(&50).expect("issue 50 must resolve a live root");
        assert_eq!(root.pid, std::process::id());
        assert_eq!(root.started_at.to_rfc3339(), "2026-01-01T00:00:00+00:00");
    }

    #[test]
    fn active_locked_issue_roots_excludes_dead_owner_pid() {
        let dir = tempdir().unwrap();
        // A pid far outside any plausible live range.
        write_owner(dir.path(), 51, 2_147_483_640, "2026-01-01T00:00:00Z");

        let roots = active_locked_issue_roots(dir.path());
        assert!(
            !roots.contains_key(&51),
            "a stale (dead-owner) lock must never resolve a live root"
        );
    }

    #[test]
    fn active_locked_issue_roots_excludes_unparseable_acquired_at() {
        let dir = tempdir().unwrap();
        write_owner(dir.path(), 52, std::process::id(), "not-a-timestamp");

        let roots = active_locked_issue_roots(dir.path());
        assert!(
            !roots.contains_key(&52),
            "an unparseable acquired_at must never resolve a live root"
        );
    }

    #[test]
    fn active_locked_issue_roots_empty_when_no_locks_dir() {
        let dir = tempdir().unwrap();
        assert!(active_locked_issue_roots(dir.path()).is_empty());
    }
}
