//! Daemon-startup tmux restore, gated on `LOOM_NO_RESTORE` (issue #8463).
//!
//! # Why this is a sibling module of `terminal`
//!
//! Both halves of the #8463 fix — the `LOOM_NO_RESTORE` predicate and the
//! startup restore/cleanup block it gates — would otherwise have to grow
//! `terminal.rs` (1918 code lines) and `daemon_service.rs` (1250), which are
//! both frozen at their current size by the File Size Ratchet
//! (`.loom/docs/file-size-policy.md`). Extracting them here is the remedy that
//! policy prescribes: new code goes in a new sibling module, leaving a single
//! dispatch line behind at each call site. Both parents shrink as a result.
//!
//! # What the gate is for
//!
//! `LOOM_NO_RESTORE=1` is set unconditionally by the integration-test harness
//! (`loom-daemon/tests/common/mod.rs`) and by no real daemon startup path. It
//! keeps a freshly-spawned test daemon from reading a live fleet's real
//! terminals off the shared `-L loom` tmux socket. Before #8463 it only
//! suppressed the *lazy* restore-on-empty-list path in
//! [`crate::terminal::TerminalManager::list_terminals`] (and the pruning that
//! accompanies it); the daemon's own startup path called
//! `restore_from_tmux*` directly, a second unguarded entry point to the same
//! import. With no workspace config present — the normal case for a throwaway
//! test fixture — it fell through to the *unfiltered* "legacy" restore and
//! silently imported every real `loom-*` session into the test daemon's
//! registry regardless of the flag.

use std::collections::HashSet;

use anyhow::Result;

use crate::terminal::TerminalManager;

/// Whether `LOOM_NO_RESTORE=1` (or a case-insensitive `true`) is set in this
/// process's environment.
///
/// Single source of truth for the flag, shared by every tmux-restore call
/// site: [`restore_and_clean_at_startup`] below and
/// [`crate::terminal::TerminalManager::list_terminals`].
pub fn no_restore_env() -> bool {
    std::env::var("LOOM_NO_RESTORE").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// Import surviving `loom-*` tmux sessions into `tm` at daemon startup, then
/// sweep the sessions that are not in the registry.
///
/// Both steps are skipped together when [`no_restore_env`] holds. Skipping
/// `clean_stale_sessions` is required for safety, not just symmetry: that
/// sweep kills every `loom-*` tmux session NOT present in the registry, so
/// leaving it enabled while the restore is skipped would treat a live fleet's
/// real sessions as "stale" and destroy them.
///
/// `configured_ids` is the workspace's configured terminal IDs (issue #1952).
/// When present, only matching sessions are imported; when absent, the legacy
/// import-everything behavior is used.
pub fn restore_and_clean_at_startup(
    tm: &mut TerminalManager,
    configured_ids: Option<&HashSet<String>>,
) -> Result<()> {
    if no_restore_env() {
        log::debug!("LOOM_NO_RESTORE=1 — skipping tmux restore and stale-session cleanup");
        return Ok(());
    }

    // Use config-based filtering if workspace config is available
    if configured_ids.is_some() {
        tm.restore_from_tmux_with_filter(configured_ids)?;
    } else {
        // Fall back to legacy behavior (import all) when no config available
        log::warn!("No workspace config found - using legacy restore (all sessions)");
        tm.restore_from_tmux()?;
    }
    log::info!("Restored {} terminals", tm.list_terminals().len());

    match tm.clean_stale_sessions() {
        Ok(0) => log::debug!("No stale tmux sessions to clean"),
        Ok(count) => log::info!("Cleaned {count} stale tmux session(s) from previous run"),
        Err(e) => log::warn!("Failed to clean stale tmux sessions: {e}"),
    }

    Ok(())
}
