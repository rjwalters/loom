//! Read-only cross-check of the daemon's PER-ISSUE claim lock against
//! `worktree.sh` (#8553).
//!
//! # Why this is a DIFFERENT lock from [`super::lock`]
//!
//! [`super::lock`] guards the repo-global, short-lived critical section around
//! one `git worktree add` invocation. This module reads a different, per-issue,
//! long-lived lock: `.loom/locks/issue-<N>/owner.json`, the daemon's
//! sweep-dispatch claim held for a sweep's ENTIRE lifetime by
//! `sweep_registry::locks::acquire_lock`. `worktree.sh` never acquires or
//! releases this lock — only the daemon's `dispatch`/`reap`/`cancel` paths do.
//! This module only ever READS it, to stop a second, independently-driven
//! session from checking out into the SAME `.loom/worktrees/issue-<N>` path a
//! live sweep already owns. The incident behind this: a dispatched sweep held
//! the lock while a second, manually-driven session ran `worktree.sh` for the
//! same issue and got the identical path with no warning — the two agents then
//! interleaved writes in one working tree for hours, each one's cleanup
//! silently reverting the other's committed and uncommitted work.
//!
//! Deliberately duplicates the minimal on-disk schema rather than importing
//! `sweep_registry::locks::LockOwner` (`pub(crate)`, and carries fields —
//! `pgid`, `model`, `effort` — this read-only check has no use for): the two
//! callers evolve independently, and the on-disk shape, not a shared Rust
//! type, is the real contract between them.
//!
//! FAIL-OPEN, matching every other guard in this family: a missing lock dir,
//! an unreadable/unparsable `owner.json`, or a dead `owner_pid` all report
//! "not live". A garbage lock file must never wedge a legitimate worktree
//! creation.

use std::path::Path;

use serde::Deserialize;

use super::lock::{locks_dir, pid_alive};

#[derive(Debug, Deserialize)]
struct Owner {
    #[serde(default)]
    owner_pid: u32,
    #[serde(default)]
    acquired_at: String,
    #[serde(default)]
    sweep_id: String,
}

/// A confirmed-live issue claim lock, as reported by [`check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveIssueLock {
    pub sweep_id: String,
    pub owner_pid: u32,
    pub acquired_at: String,
}

impl LiveIssueLock {
    /// True when `caller_sweep_id` (the CLI passes `$LOOM_SWEEP_ID`) names the
    /// SAME sweep that holds this lock (#8702).
    ///
    /// A dispatched sweep's own `worktree.sh` calls -- the first build and
    /// every resume/re-dispatch after -- are descendants of the sweep that
    /// itself took this lock (`record_child_pid_in_lock` sets `owner_pid` to
    /// the sweep's own child process), so without this exemption a sweep is
    /// refused by its own claim on every call after the first.
    #[must_use]
    pub fn owned_by(&self, caller_sweep_id: Option<&str>) -> bool {
        caller_sweep_id.is_some_and(|id| id == self.sweep_id)
    }

    /// A short "3m12s"-style age for the CLI's warning/refusal text.
    /// Presentation only — never gates the live/dead verdict in [`check`].
    /// Falls back to `"unknown age"` when `acquired_at` does not parse as
    /// RFC 3339 (a lock written by a future/older schema).
    #[must_use]
    pub fn age_desc(&self) -> String {
        let Ok(acquired) = chrono::DateTime::parse_from_rfc3339(&self.acquired_at) else {
            return "unknown age".to_string();
        };
        let secs = (chrono::Utc::now() - acquired.with_timezone(&chrono::Utc))
            .num_seconds()
            .max(0);
        if secs >= 60 {
            format!("{}m{}s", secs / 60, secs % 60)
        } else {
            format!("{secs}s")
        }
    }
}

/// The confirmed-live claim lock for `issue`, or `None` when the lock is
/// absent, unreadable, unparsable, or its recorded owner is dead — see the
/// module docs for the fail-open rationale.
#[must_use]
pub fn check(repo: &Path, issue: u32) -> Option<LiveIssueLock> {
    let owner_path = locks_dir(repo)
        .join(format!("issue-{issue}"))
        .join("owner.json");
    let text = std::fs::read_to_string(&owner_path).ok()?;
    let owner: Owner = serde_json::from_str(&text).ok()?;
    if owner.owner_pid == 0 || !pid_alive(owner.owner_pid) {
        return None;
    }
    Some(LiveIssueLock {
        sweep_id: owner.sweep_id,
        owner_pid: owner.owner_pid,
        acquired_at: owner.acquired_at,
    })
}

#[cfg(test)]
mod tests;
