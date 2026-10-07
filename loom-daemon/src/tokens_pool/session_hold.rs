//! On-disk operator intent for a session container (issue #10453): the
//! **operator hold** `accounts session stop` leaves behind, and the
//! workspace/image of the last operator `accounts session start`.
//!
//! Both live as sidecar files directly inside the account's profile
//! (`CODEX_HOME`) directory, next to [`super::session_lifecycle::SESSION_MARKER_FILE`]:
//!
//! * [`HOLD_FILE`] (`.session-hold.json`) — present means **held**:
//!   `{"schema_version":1,"reason":"operator stop","held_at_unix_ms":<ms>}`.
//!   Written by `SessionLifecycle::stop` *before* `docker stop`. Only an
//!   operator `start`/`start_with_workspace`/`shell` removes it — by
//!   **deleting** it, before that start touches Docker ([`lift_holds`]). The
//!   session reconcile pass never writes or removes it; it only reads it,
//!   before any `docker` call and again right before it would mutate a
//!   container.
//! * [`LAST_START_FILE`] (`.session-last-start.json`) —
//!   `{"schema_version":1,"workspace":"<abs path>","image":"<ref>","started_at_unix_ms":<ms>}`,
//!   written by an operator start once the container is up, so a container
//!   that disappears across a daemon restart is recreated against the
//!   `--mount-workspace` the operator chose rather than a guessed parent.
//!
//! **A hold is never out-dated, only deleted.** Whether an account is held
//! is the existence of a hold file — no timestamp comparison — so a wall
//! clock stepping back between a start and a stop (NTP step, VM resume)
//! cannot make a fresh hold read as older than the start it follows. The
//! timestamps are informational; [`write_hold`] still stamps
//! `max(now, last start + 1)` so the record reads in order.
//!
//! Profile directories normally sit under one host-wide profile root, so a
//! sidecar is per **account**, not per registered root. When two roots do
//! resolve the same account name to different directories, a hold in any of
//! them holds the account ([`held_across`]), and an operator start deletes
//! the hold in every one it can see (its own root and the registered peers,
//! [`account_profiles`]). A hold file that exists but cannot be parsed —
//! including an empty one left by a crash mid-write — counts as held (a
//! deliberate stop fails safe: down).
//!
//! The stop/reconcile race (a pass whose hold check ran just before `stop`
//! wrote the hold, so its `docker start` lands between `stop`'s `docker
//! stop` and `docker rm`) is closed in `SessionLifecycle::stop`, which
//! retries the stop+rm once when `rm` finds the container running again.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Sidecar marking an account's session as held down by the operator.
pub const HOLD_FILE: &str = ".session-hold.json";
/// Sidecar recording the last operator start's workspace and image.
pub const LAST_START_FILE: &str = ".session-last-start.json";
/// The only hold reason written today; shown verbatim by status and logs.
pub const HOLD_REASON_OPERATOR_STOP: &str = "operator stop";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hold {
    pub schema_version: u32,
    pub reason: String,
    pub held_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastStart {
    pub schema_version: u32,
    pub workspace: PathBuf,
    pub image: String,
    pub started_at_unix_ms: u64,
}

/// Typed error a hold-respecting start returns when the account turned out
/// to be held after its container was inspected (the stop/reconcile race).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperatorHeld;

impl std::fmt::Display for OperatorHeld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "held ({HOLD_REASON_OPERATOR_STOP})")
    }
}

impl std::error::Error for OperatorHeld {}

#[must_use]
pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

pub(super) fn write_json_atomic<T: Serialize>(dir: &Path, file: &str, value: &T) -> Result<()> {
    let path = dir.join(file);
    let temp = dir.join(format!("{file}.tmp-{}", std::process::id()));
    std::fs::write(&temp, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("failed to stage {}", path.display()))?;
    std::fs::rename(&temp, &path).with_context(|| format!("failed to commit {}", path.display()))
}

/// Record an operator hold in `profile` (atomic replace), stamped
/// `max(at_ms, that directory's last start + 1)`.
pub fn write_hold(profile: &Path, at_ms: u64) -> Result<()> {
    let floor = read_last_start(profile).map_or(0, |s| s.started_at_unix_ms.saturating_add(1));
    let hold = Hold {
        schema_version: 1,
        reason: HOLD_REASON_OPERATOR_STOP.into(),
        held_at_unix_ms: at_ms.max(floor),
    };
    write_json_atomic(profile, HOLD_FILE, &hold)
}

/// Delete the hold in every one of `profiles` — the operator-start half of
/// the hold contract, done **before** the start touches Docker so a failure
/// here leaves the container exactly as it was (down and held), never
/// running-but-held.
pub fn lift_holds(profiles: &[PathBuf]) -> Result<()> {
    for profile in profiles {
        let path = profile.join(HOLD_FILE);
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).with_context(|| {
                    format!("failed to lift the session hold {}", path.display())
                });
            }
            _ => {}
        }
    }
    // An operator start also overrides the reconciler's own "removed for a
    // denied mount" record (#10364): the operator has taken the decision.
    super::session_drift_removal::clear(profiles);
    Ok(())
}

/// Record what an operator start brought up. Informational for recreation:
/// callers treat a failure as a warning, never as a failed start.
pub fn record_operator_start(
    profile: &Path,
    workspace: &Path,
    image: &str,
    at_ms: u64,
) -> Result<()> {
    let start = LastStart {
        schema_version: 1,
        workspace: workspace.to_path_buf(),
        image: image.to_string(),
        started_at_unix_ms: at_ms,
    };
    write_json_atomic(profile, LAST_START_FILE, &start)
}

#[must_use]
pub fn read_last_start(profile: &Path) -> Option<LastStart> {
    let bytes = std::fs::read(profile.join(LAST_START_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The newest operator start recorded in any of `profiles`.
#[must_use]
pub fn latest_start(profiles: &[PathBuf]) -> Option<LastStart> {
    profiles
        .iter()
        .filter_map(|p| read_last_start(p))
        .max_by_key(|s| s.started_at_unix_ms)
}

/// Whether the account whose profile directories are `profiles` is held: a
/// hold file (readable or not) exists in any of them.
#[must_use]
pub fn held_across(profiles: &[PathBuf]) -> bool {
    profiles.iter().any(|p| p.join(HOLD_FILE).exists())
}

/// `own` plus the profile directory `name` resolves to in each of
/// `peer_roots` (other registered workspaces), deduplicated. A root whose
/// inventory cannot be read contributes nothing.
#[must_use]
pub fn account_profiles(own: &Path, peer_roots: &[PathBuf], name: &str) -> Vec<PathBuf> {
    use super::account_registry::{account_inventory_quiet, AccountProvider};
    let mut profiles = vec![own.to_path_buf()];
    for root in peer_roots {
        let Ok(inventory) = account_inventory_quiet(root, AccountProvider::Codex) else {
            continue;
        };
        for account in inventory.into_iter().filter(|a| a.id.name == name) {
            if !profiles.contains(&account.credential_reference) {
                profiles.push(account.credential_reference);
            }
        }
    }
    profiles
}

#[cfg(test)]
#[path = "session_hold_tests.rs"]
mod tests;
