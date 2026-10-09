//! The reconciler's own record that it removed an account's session
//! container for a denied mount and left nothing in its place (issue #10364
//! Part B): [`REMOVED_FILE`] (`.session-drift-removed.json`) in the
//! account's profile directory, beside `.session-last-start.json`.
//!
//! It exists to bound remove → recreate → remove. While the record stands,
//! the reconcile pass neither recreates the container nor removes it a
//! second time; it is on disk so a daemon restart does not re-arm the cycle.
//! It is cleared only when the denial no longer applies (the reconciler
//! clears it) or when an operator starts the session
//! ([`super::session_hold::lift_holds`] deletes it with the hold).
//!
//! It is **not** an operator hold: `accounts session status` does not show
//! the account as held, and the reconciler still never writes or lifts
//! [`super::session_hold::HOLD_FILE`].

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Sidecar recording a fail-closed drift removal.
pub const REMOVED_FILE: &str = ".session-drift-removed.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriftRemoval {
    pub schema_version: u32,
    /// The `--mount-workspace` the removed container would be recreated with.
    pub workspace: PathBuf,
    /// The mounted paths that were positively denied.
    pub denied: Vec<PathBuf>,
    /// Why no container was accepted in its place.
    pub reason: String,
    pub removed_at_unix_ms: u64,
}

/// Record a removal in `profile` (atomic replace).
///
/// # Errors
/// When the sidecar cannot be written; the caller must then not remove.
pub fn record(profile: &Path, removal: &DriftRemoval) -> Result<()> {
    super::session_hold::write_json_atomic(profile, REMOVED_FILE, removal)
}

/// The removal recorded in any of `profiles`. A record that exists but does
/// not parse still stands (with nothing known about it), so a torn write
/// cannot re-arm the cycle.
#[must_use]
pub fn read(profiles: &[PathBuf]) -> Option<DriftRemoval> {
    profiles.iter().find_map(|profile| {
        let bytes = std::fs::read(profile.join(REMOVED_FILE)).ok()?;
        Some(serde_json::from_slice(&bytes).unwrap_or_else(|_| DriftRemoval {
            schema_version: 0,
            workspace: PathBuf::new(),
            denied: Vec::new(),
            reason: "unreadable removal record".into(),
            removed_at_unix_ms: 0,
        }))
    })
}

/// Delete the record in every one of `profiles` (best effort: a record that
/// cannot be deleted keeps standing, which is the safe direction).
pub fn clear(profiles: &[PathBuf]) {
    for profile in profiles {
        let _ = std::fs::remove_file(profile.join(REMOVED_FILE));
    }
}
