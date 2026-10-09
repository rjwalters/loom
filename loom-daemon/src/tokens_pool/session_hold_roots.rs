//! One set of workspace roots for operator holds (issue #10661, item 3).
//!
//! An operator hold ([`super::session_hold`]) is per account, but an account
//! name can resolve to a different profile directory in each workspace root.
//! The session reconcile pass reads holds (and `enabled=false`) in every
//! root's profile for the account; the `accounts session` CLI lifts them and
//! reports them. Before #10661 the two read different sets: the pass used the
//! registry's roots or, with an empty or unreadable registry, the daemon's
//! own **fallback root** (its `LOOM_WORKSPACE` or working directory), while
//! the CLI used the registered roots plus its own workspace. A hold in an
//! unregistered fallback root was then honoured by the pass but neither shown
//! nor lifted by the CLI.
//!
//! [`hold_roots`] is now the one definition: every registered root, then the
//! daemon's fallback root when it is not one of them. The pass calls it with
//! its fallback root. The CLI runs in another process and cannot see that
//! root, so the daemon records it on disk when it starts the reconcile loop
//! ([`record_fallback_root`], at [`fallback_root_file`]) and the CLI reads it
//! back ([`cli_peer_roots`]). The CLI also keeps its own workspace, the root
//! it resolved the account in.
//!
//! Both failure directions are safe: a missing or unreadable record means the
//! CLI sees fewer roots (the pre-#10661 behaviour), and a stale one (the
//! daemon moved or stopped) adds a root, which only shows more holds and lifts
//! more of them on an operator start, never fewer.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Overrides where the daemon records its fallback root (tests; a host whose
/// daemon and operator do not share a home directory).
pub const FALLBACK_ROOT_FILE_ENV: &str = "LOOM_SESSION_FALLBACK_ROOT_FILE";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FallbackRoot {
    schema_version: u32,
    root: PathBuf,
}

/// The roots whose Codex profiles carry an account's operator hold: every
/// registered root, in registry order, then `fallback` unless it is already
/// one of them.
#[must_use]
pub fn hold_roots(registered: &[PathBuf], fallback: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = registered.to_vec();
    if let Some(fallback) = fallback {
        if !roots.iter().any(|root| root == fallback) {
            roots.push(fallback.to_path_buf());
        }
    }
    roots
}

/// The other roots the CLI lifts and reads holds in, given its own workspace
/// `own` (the account's profile there is always included by the lifecycle):
/// [`hold_roots`] without `own`.
#[must_use]
pub fn cli_peer_roots(own: &Path, registered: &[PathBuf], fallback: Option<&Path>) -> Vec<PathBuf> {
    hold_roots(registered, fallback)
        .into_iter()
        .filter(|root| root != own)
        .collect()
}

/// [`FALLBACK_ROOT_FILE_ENV`], else `~/.loom/session-reconcile-fallback-root.json`.
#[must_use]
pub fn fallback_root_file() -> Option<PathBuf> {
    match std::env::var_os(FALLBACK_ROOT_FILE_ENV) {
        Some(file) if !file.is_empty() => Some(PathBuf::from(file)),
        _ => dirs::home_dir().map(|home| {
            home.join(".loom")
                .join("session-reconcile-fallback-root.json")
        }),
    }
}

/// Record the daemon's fallback root in `file` (atomic replace).
pub fn record_fallback_root(file: &Path, root: &Path) -> Result<()> {
    let dir = file
        .parent()
        .with_context(|| format!("{} has no parent directory", file.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let name = file
        .file_name()
        .with_context(|| format!("{} has no file name", file.display()))?
        .to_string_lossy()
        .into_owned();
    let value = FallbackRoot {
        schema_version: 1,
        root: root.to_path_buf(),
    };
    super::session_hold::write_json_atomic(dir, &name, &value)
}

/// The fallback root recorded in `file`; `None` when absent or unreadable.
#[must_use]
pub fn read_fallback_root(file: &Path) -> Option<PathBuf> {
    let bytes = std::fs::read(file).ok()?;
    let value: FallbackRoot = serde_json::from_slice(&bytes).ok()?;
    (value.schema_version == 1 && value.root.is_absolute()).then_some(value.root)
}

/// Daemon side: record `root` at [`fallback_root_file`], warning (never
/// failing) when it cannot be written.
pub fn record_daemon_fallback_root(root: &Path) {
    let Some(file) = fallback_root_file() else {
        return;
    };
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    if let Err(error) = record_fallback_root(&file, &root) {
        log::warn!(
            "session_reconcile: could not record the daemon's fallback root ({error:#}); \
             `accounts session status`/`start` will not see an operator hold kept only in \
             {}",
            root.display()
        );
    }
}

/// CLI side: the recorded daemon fallback root, if any.
#[must_use]
pub fn recorded_daemon_fallback_root() -> Option<PathBuf> {
    read_fallback_root(&fallback_root_file()?)
}

#[cfg(test)]
#[path = "session_hold_roots_tests.rs"]
mod tests;
