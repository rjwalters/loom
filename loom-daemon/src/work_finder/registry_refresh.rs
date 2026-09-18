//! Purely-local workspace-registry refresh for the multi-workspace tick loop
//! (issue #8121).
//!
//! `loom-daemon workspace add <path> --priority N` writes the machine-level
//! registry (`~/.loom/workspaces.json`) and reports success immediately, but
//! the daemon only *acts* on that write when its tick loop reloads the
//! registry and provisions the new root's
//! [`SweepRegistry`](crate::sweep_registry::SweepRegistry) through the shared
//! [`WorkspacePool`]. Before #8121 both steps sat **after** the GitHub
//! rate-limit circuit breaker's early-`continue` in
//! [`spawn_multi_work_finder_task`](super::spawn_multi_work_finder_task), so a
//! workspace registered while the breaker was suppressing gh polling stayed
//! un-provisioned — invisible to `loom-daemon status` and to dispatch — for
//! the breaker's entire suppression window, even though a registry edit
//! consumes none of the API budget the breaker exists to protect.
//!
//! This module holds the extracted refresh the tick loop now calls
//! unconditionally, **before** that breaker check. It lives in its own file
//! rather than in `work_finder.rs` because that file is over the file-size
//! ratchet threshold (`.loom/docs/file-size-policy.md`) and may not grow.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::workspace_pool::WorkspacePool;
use crate::workspace_registry::{filter_missing_roots, WorkspaceRegistry};

/// Reload the machine-level [`WorkspaceRegistry`] and provision every
/// currently-registered (and still-existing-on-disk) root's
/// [`SweepRegistry`](crate::sweep_registry::SweepRegistry) via the shared
/// [`WorkspacePool`] (issue #8121).
///
/// Both steps are purely local — one filesystem read of
/// `~/.loom/workspaces.json`, then in-memory registry construction plus
/// config-file reads for any root not already pooled — with **zero GitHub API
/// calls**. [`WorkspacePool::get_or_provision`] is idempotent (a cheap map
/// lookup for an already-pooled root), so calling it for every root on every
/// tick costs nothing extra once a workspace is warm.
///
/// The caller in
/// [`spawn_multi_work_finder_task`](super::spawn_multi_work_finder_task)'s
/// tick loop MUST call this **before** the rate-limit circuit breaker's
/// early-`continue`: prior to #8121 the registry reload (and therefore
/// provisioning) sat after that check, so `loom-daemon workspace add <path>
/// --priority N` — which writes the registry unconditionally and reports
/// success immediately — could sit un-provisioned (invisible to `loom-daemon
/// status` and dispatch) for the breaker's entire suppression window, even
/// though nothing about a registry edit touches the GitHub API budget the
/// breaker protects.
pub(super) fn refresh_local_workspace_state(
    pool: &WorkspacePool,
    fallback_root: &Path,
    missing_roots_warned: &mut HashSet<PathBuf>,
) -> (WorkspaceRegistry, Vec<PathBuf>) {
    let registry = WorkspaceRegistry::load_default().unwrap_or_else(|e| {
        log::warn!("work_finder: could not load workspace registry ({e}); using cwd");
        WorkspaceRegistry::default()
    });
    let roots = registry.effective_roots(fallback_root);
    // Skip registered roots whose directory no longer exists on disk
    // (#4326 — e.g. a leaked/stale registry entry) so a dangling entry cannot
    // occupy top dispatch priority or burn the tick. This is warn-and-skip,
    // never auto-remove: the entry stays registered (`loom-daemon status`
    // flags it, `workspace remove` clears it).
    let roots = filter_missing_roots(roots, missing_roots_warned);
    for root in &roots {
        // Hot-apply (#8121): provisioning a brand-new root right here — the
        // `SweepRegistry` + reaper + watchdog construction in
        // `WorkspacePool::get_or_provision` — is what makes a freshly
        // registered workspace observable/dispatchable without waiting for
        // (or being blocked by) the rate-limit breaker in the tick loop.
        let _ = pool.get_or_provision(root);
    }
    (registry, roots)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
