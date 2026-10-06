//! On-disk operator intent for a session container (issue #10453): the
//! **operator hold** `accounts session stop` leaves behind, and the
//! workspace/image of the last operator `accounts session start`.
//!
//! Both live as sidecar files directly inside the account's profile
//! (`CODEX_HOME`) directory, next to [`super::session_lifecycle::SESSION_MARKER_FILE`]:
//!
//! * [`HOLD_FILE`] (`.session-hold.json`) — present means **held**:
//!   `{"schema_version":1,"reason":"operator stop","held_at_unix_ms":<ms>}`.
//!   Written by `SessionLifecycle::stop` *before* `docker stop`, removed by
//!   an operator `start`/`start_with_workspace`/`shell`. The session reconcile
//!   pass never writes or removes it; it only reads it, before any `docker`
//!   call and again right before it would mutate a container.
//! * [`LAST_START_FILE`] (`.session-last-start.json`) —
//!   `{"schema_version":1,"workspace":"<abs path>","image":"<ref>","started_at_unix_ms":<ms>}`,
//!   written by an operator start once the container is up, so a container
//!   that disappears across a daemon restart is recreated against the
//!   `--mount-workspace` the operator chose rather than a guessed parent.
//!
//! Profile directories normally sit under one host-wide profile root, so a
//! sidecar is per **account**, not per registered root. When two roots do
//! resolve the same account name to different directories, the reader takes
//! every directory into account ([`held_across`], [`latest_start`]): the
//! account is held iff the newest hold is not older than the newest operator
//! start anywhere. A hold file that exists but cannot be parsed counts as
//! held (a deliberate stop fails safe: down).

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

fn write_json_atomic<T: Serialize>(dir: &Path, file: &str, value: &T) -> Result<()> {
    let path = dir.join(file);
    let temp = dir.join(format!("{file}.tmp-{}", std::process::id()));
    std::fs::write(&temp, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("failed to stage {}", path.display()))?;
    std::fs::rename(&temp, &path).with_context(|| format!("failed to commit {}", path.display()))
}

/// Record an operator hold in `profile` (atomic replace).
pub fn write_hold(profile: &Path, at_ms: u64) -> Result<()> {
    let hold = Hold {
        schema_version: 1,
        reason: HOLD_REASON_OPERATOR_STOP.into(),
        held_at_unix_ms: at_ms,
    };
    write_json_atomic(profile, HOLD_FILE, &hold)
}

/// Lift the hold in `profile` and record what the operator started, in that
/// order of visibility: the start record lands first, so a reader never sees
/// "no hold, no newer start" for an account another root still holds.
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
    write_json_atomic(profile, LAST_START_FILE, &start)?;
    match std::fs::remove_file(profile.join(HOLD_FILE)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).context("failed to lift the session hold")
        }
        _ => Ok(()),
    }
}

/// The hold's timestamp in `profile`: `None` when there is no hold file,
/// `u64::MAX` when one exists but cannot be read (fail safe: held).
fn hold_at(profile: &Path) -> Option<u64> {
    let path = profile.join(HOLD_FILE);
    if !path.exists() {
        return None;
    }
    Some(
        std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Hold>(&b).ok())
            .map_or(u64::MAX, |h| h.held_at_unix_ms),
    )
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

/// Whether the account whose profile directories are `profiles` is held:
/// the newest hold is at least as new as the newest operator start.
#[must_use]
pub fn held_across(profiles: &[PathBuf]) -> bool {
    let Some(hold) = profiles.iter().filter_map(|p| hold_at(p)).max() else {
        return false;
    };
    let start = latest_start(profiles).map_or(0, |s| s.started_at_unix_ms);
    hold >= start
}

#[cfg(test)]
#[path = "session_hold_tests.rs"]
mod tests;
