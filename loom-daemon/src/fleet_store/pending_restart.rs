//! Restart-required fleet-config changes, pending until the daemon that was
//! running at render time actually restarts (Issue #9597).
//!
//! `fleet-config render` classifies every changed key as live-reloadable or
//! restart-required ([`crate::fleet_store::reload`]). A live-reloadable key
//! needs nothing further — the consuming loop already re-reads it fresh (see
//! `reload`'s doc comment for the per-knob evidence). A restart-required key
//! has no such consumer; the only way "landed" becomes "effective" is the
//! daemon that was running at render time ending and a new one starting in
//! its place — which is exactly a **process identity change**. This module
//! therefore tracks pending restarts by PID rather than a timestamp:
//! `render` records which pid answered `DaemonStatus` when the drift was
//! written, and `status` considers the pending state resolved the moment the
//! currently-answering pid differs from the recorded one (never mind by how
//! long — a manual `restart` and a supervisor relaunch are both a fresh
//! process either way).
//!
//! Written only when `render` could reach a running daemon at all — with
//! nothing running, there is no stale process to be pending against, and the
//! next daemon to start reads the freshly-rendered file directly (no marker
//! needed).
//!
//! A host-level file (`<loom_dir>/fleet-config-pending-restart.json`) rather
//! than a field on the IPC `DaemonStatusReport`, mirroring
//! [`crate::fleet_sync::FleetSyncStatus`]'s own rationale: this is a fact
//! about the *host*, and `status` must be able to show a stale marker even
//! when the daemon it refers to is not the one currently answering (or is not
//! answering at all).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::fetch::write_atomic;

/// Name of the host-level marker under `<loom_dir>`.
pub const FILENAME: &str = "fleet-config-pending-restart.json";

/// One render's restart-required paths, pinned to the daemon pid observed at
/// render time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingRestart {
    /// The restart-required dotted paths this render found changed.
    pub paths: Vec<String>,
    /// The daemon pid that answered `DaemonStatus` when this was recorded —
    /// the pid a subsequent `status` compares itself against.
    pub observed_pid: u32,
    /// When the render ran.
    pub rendered_at: DateTime<Utc>,
}

/// Resolve `<loom_dir>` the same way [`crate::fleet_sync`] does: the parent
/// of `LOOM_SOCKET_PATH` when set (so a test daemon pointed at a tempdir
/// socket never writes into the operator's real `~/.loom`), else `~/.loom`.
fn resolve_loom_dir() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("LOOM_SOCKET_PATH") {
        return PathBuf::from(path).parent().map(Path::to_path_buf);
    }
    dirs::home_dir().map(|h| h.join(".loom"))
}

/// Path of the host-level marker, or `None` when no loom dir resolves.
#[must_use]
pub fn marker_path() -> Option<PathBuf> {
    resolve_loom_dir().map(|d| d.join(FILENAME))
}

/// Record `paths` against `observed_pid`. Best-effort on the caller's part —
/// this returns `Err` on a write failure so `render` can log it, never
/// panics, and never fails the render itself.
pub fn record(paths: Vec<String>, observed_pid: u32, rendered_at: DateTime<Utc>) -> Result<()> {
    let Some(path) = marker_path() else {
        return Ok(());
    };
    let marker = PendingRestart {
        paths,
        observed_pid,
        rendered_at,
    };
    let mut body = serde_json::to_vec_pretty(&marker)?;
    body.push(b'\n');
    write_atomic(&path, &body).with_context(|| format!("writing {}", path.display()))
}

/// The marker at `path`, `None` when absent/unreadable.
#[must_use]
fn read(path: &Path) -> Option<PendingRestart> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// The marker on disk at the default location, `None` when absent/unset.
#[must_use]
pub fn probe() -> Option<PendingRestart> {
    read(&marker_path()?)
}

/// Remove the marker (best-effort — a missing file is not an error).
pub fn clear() {
    if let Some(path) = marker_path() {
        let _ = std::fs::remove_file(path);
    }
}

/// `status`'s read side: the marker, when it is still pending against
/// `current_pid` — reconciling (clearing) it first when the daemon has since
/// restarted. `current_pid: None` (the daemon is unreachable right now)
/// leaves an existing marker exactly as it is: there is nothing to compare
/// against, and clearing on "can't tell" would silently drop real state.
#[must_use]
pub fn reconcile(current_pid: Option<u32>) -> Option<PendingRestart> {
    let marker = probe()?;
    match current_pid {
        Some(pid) if pid == marker.observed_pid => Some(marker),
        Some(_) => {
            clear();
            None
        }
        None => Some(marker),
    }
}

#[cfg(test)]
mod tests {
    use serial_test::serial;

    use super::*;

    fn with_loom_dir<T>(f: impl FnOnce(&Path) -> T) -> T {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("loom-daemon.sock");
        let prior = std::env::var("LOOM_SOCKET_PATH").ok();
        std::env::set_var("LOOM_SOCKET_PATH", &socket);
        let out = f(dir.path());
        match prior {
            Some(p) => std::env::set_var("LOOM_SOCKET_PATH", p),
            None => std::env::remove_var("LOOM_SOCKET_PATH"),
        }
        out
    }

    #[test]
    #[serial(loom_socket_path_env)]
    fn record_then_probe_roundtrips() {
        with_loom_dir(|_| {
            record(vec!["autonomous.hostBreaker.enabled".to_string()], 4242, Utc::now()).unwrap();
            let marker = probe().unwrap();
            assert_eq!(marker.paths, vec!["autonomous.hostBreaker.enabled".to_string()]);
            assert_eq!(marker.observed_pid, 4242);
        });
    }

    #[test]
    #[serial(loom_socket_path_env)]
    fn absent_marker_probes_to_none() {
        with_loom_dir(|_| assert!(probe().is_none()));
    }

    #[test]
    #[serial(loom_socket_path_env)]
    fn reconcile_clears_once_the_pid_changes() {
        with_loom_dir(|_| {
            record(vec!["fleet.autoApply".to_string()], 100, Utc::now()).unwrap();
            // Same pid still answering: still pending.
            assert!(reconcile(Some(100)).is_some());
            assert!(probe().is_some(), "still on disk while pending");
            // A different pid answered: the daemon restarted; resolved.
            assert!(reconcile(Some(101)).is_none());
            assert!(probe().is_none(), "cleared once resolved");
        });
    }

    #[test]
    #[serial(loom_socket_path_env)]
    fn reconcile_leaves_an_unreachable_daemon_marker_untouched() {
        with_loom_dir(|_| {
            record(vec!["fleet.autoApply".to_string()], 100, Utc::now()).unwrap();
            assert!(reconcile(None).is_some(), "nothing to compare against — stays pending");
            assert!(probe().is_some());
        });
    }
}
