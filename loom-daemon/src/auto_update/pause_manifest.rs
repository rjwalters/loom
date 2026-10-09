//! The pause manifest, schema v1 (issue #10830; design
//! `docs/design/daemon-roll-pause-resume.md` §6).
//!
//! A roll records every in-flight agent here before it stops any of them (H4,
//! `pause_roll`, #10831), and the next start resumes or requeues from it (H5,
//! `pause_resume`, #10832).
//!
//! # Compatibility rules (§6), and how they are enforced
//!
//! * **Additive within a version.** Every struct ignores unknown fields (no
//!   `deny_unknown_fields`), and every field a later writer may omit is
//!   `#[serde(default)]`.
//! * **Unknown enum values.** Each enum carries an `Unknown(String)` variant
//!   (`#[serde(from = "String")]`), so an unknown value still parses. Reading an
//!   item through [`ManifestItem::effective_disposition`] turns any unknown
//!   `kind`, `disposition`, `status` or `resume_handle.runtime` into `requeue`
//!   with reason `unknown-<field>-<value>`. The item is never dropped.
//! * **Newer `schema_version`.** [`load`] returns [`LoadOutcome::UnknownVersion`]
//!   and resumes nothing; the caller falls back to plain restart recovery.
//! * **The frozen v1 core** ([`FROZEN_V1_CORE`]) is the set of fields a
//!   rolled-back binary must still be able to read. A test pins that each of
//!   them round-trips; removing or renaming one is a schema break.
//!
//! # Writes
//!
//! [`save`] writes a temp file in the same directory, `fsync`s it, then
//! `rename`s it over the target, so a crash leaves the old manifest or the new
//! one, never a partial file.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The schema this binary writes and fully understands.
pub const SCHEMA_VERSION: u32 = 1;
/// File name under the auto-update state dir.
pub const MANIFEST_FILE: &str = "roll-pause-manifest.json";

/// The fields a rolled-back binary must be able to read (§6, "the frozen v1
/// core"), as JSON paths. `items[].` paths apply to every item.
pub const FROZEN_V1_CORE: &[&str] = &[
    "schema_version",
    "manifest_id",
    "phase",
    "written_by.version",
    "roll.to_version",
    "roll.to_artifact_sha256",
    "roll.max_age_secs",
    "items[].id",
    "items[].kind",
    "items[].repo",
    "items[].disposition",
    "items[].status",
    "items[].reason",
    "items[].issue",
    "items[].pid",
    "items[].pid_started_at",
    "items[].pgid",
    "items[].scope_unit",
    "items[].agent_started_at",
    "items[].resume_handle.runtime",
    "items[].resume_handle.session_id",
    "items[].resume_handle.session_store",
    "items[].resume_handle.cwd",
    "items[].resume_handle.container",
    "items[].worktree.path",
    "items[].claim",
    "items[].lease_comment_id",
];

/// Path of the live manifest: `$LOOM_AUTO_UPDATE_STATE_DIR` when set, else
/// `~/.loom/` (the directory `auto-update-artifact-roll.json` uses).
#[must_use]
pub fn manifest_path() -> Option<PathBuf> {
    super::state_dir().map(|d| d.join(MANIFEST_FILE))
}

macro_rules! open_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(from = "String", into = "String")]
        pub enum $name {
            $($variant,)+
            /// A value this binary does not know (written by a newer one).
            Unknown(String),
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                match s.as_str() {
                    $($text => $name::$variant,)+
                    _ => $name::Unknown(s),
                }
            }
        }

        impl From<$name> for String {
            fn from(v: $name) -> String {
                v.as_str().to_string()
            }
        }

        impl $name {
            /// The wire spelling.
            #[must_use]
            pub fn as_str(&self) -> &str {
                match self {
                    $($name::$variant => $text,)+
                    $name::Unknown(s) => s,
                }
            }
        }
    };
}

open_enum!(
    /// Manifest lifecycle phase.
    Phase { Pausing => "pausing", Paused => "paused", Resuming => "resuming", Resumed => "resumed", Abandoned => "abandoned" }
);
open_enum!(
    /// What kind of agent an item is.
    ItemKind { Sweep => "sweep", RoleRun => "role_run" }
);
open_enum!(
    /// What the roll intends for an item.
    Disposition { Resume => "resume", Requeue => "requeue" }
);
open_enum!(
    /// Where an item is in the pause/resume lifecycle.
    ItemStatus {
        Planned => "planned", Stopping => "stopping", Paused => "paused", Requeued => "requeued",
        Resumed => "resumed", Completed => "completed", Exited => "exited", Failed => "failed",
    }
);
open_enum!(
    /// The agent runtime.
    Runtime { Claude => "claude", Codex => "codex" }
);
open_enum!(
    /// What triggered the roll.
    TargetSource { Floor => "floor", RepoAhead => "repo_ahead", ConfigRestart => "config_restart", AutoUpdate => "autoupdate" }
);

/// Who wrote the manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WrittenBy {
    pub version: String,
    #[serde(default)]
    pub artifact_sha256: Option<String>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub supervisor: Option<String>,
}

/// The roll the manifest belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Roll {
    #[serde(default)]
    pub from_version: Option<String>,
    pub to_version: String,
    #[serde(default)]
    pub to_artifact_sha256: Option<String>,
    #[serde(default)]
    pub target_source: Option<TargetSource>,
    #[serde(default)]
    pub staged_at: Option<DateTime<Utc>>,
    pub pause_started_at: DateTime<Utc>,
    #[serde(default)]
    pub pause_completed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub pause_budget_secs: Option<u64>,
    #[serde(default)]
    pub min_resumable_age_secs: Option<u64>,
    pub max_age_secs: u64,
}

/// An agent's resume handle: how to relaunch its saved session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeHandle {
    pub runtime: Runtime,
    /// `None` means not resumable (`session-not-resumable`).
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub session_store: Option<String>,
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// `{name, account}` for a session-exec item.
    #[serde(default)]
    pub container: Option<serde_json::Value>,
    /// The Codex sandbox mode of the original launch (#10831), so H5 resumes
    /// under the same one instead of re-deriving it.
    #[serde(default)]
    pub sandbox: Option<String>,
    #[serde(default)]
    pub resume_count: u32,
    #[serde(default)]
    pub resume_of: Option<String>,
    /// The sweep id the claim's lease record is published under, when it is
    /// not the item's own id (#10832): a run that was itself resumed keeps
    /// renewing the record its first dispatch wrote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_sweep_id: Option<String>,
}

/// The safe point an item reached before it was stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafePointRecord {
    pub reached_at: String,
    #[serde(default)]
    pub parked_tool: Option<String>,
    #[serde(default)]
    pub parked_summary: Option<String>,
}

/// The worktree an item was working in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRecord {
    pub path: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub head: Option<String>,
    #[serde(default)]
    pub dirty: Option<bool>,
}

/// One in-flight agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestItem {
    pub id: String,
    pub kind: ItemKind,
    pub repo: String,
    pub disposition: Disposition,
    pub status: ItemStatus,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub issue: Option<u32>,
    #[serde(default)]
    pub pr: Option<u32>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub pid_started_at: Option<String>,
    #[serde(default)]
    pub pgid: Option<u32>,
    #[serde(default)]
    pub scope_unit: Option<String>,
    #[serde(default)]
    pub agent_started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub run_started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub resume_handle: Option<ResumeHandle>,
    #[serde(default)]
    pub safe_point: Option<SafePointRecord>,
    #[serde(default)]
    pub checkpoint_phase: Option<String>,
    #[serde(default)]
    pub worktree: Option<WorktreeRecord>,
    #[serde(default)]
    pub claim: Option<serde_json::Value>,
    #[serde(default)]
    pub lease_comment_id: Option<u64>,
    #[serde(default)]
    pub lease_refreshed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub log_path: Option<String>,
    #[serde(default)]
    pub overflow: bool,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub timeout_remaining_secs: Option<u64>,
    #[serde(default)]
    pub holds_issue_creation_mutex: bool,
    #[serde(default)]
    pub stopped_at: Option<DateTime<Utc>>,
}

impl ManifestItem {
    /// The disposition a reader must act on, with the reason when it is a
    /// requeue. An unknown enum value anywhere on the item forces `requeue`
    /// with `unknown-<field>-<value>` (§6): the item is never silently dropped
    /// and never resumed on a guess.
    #[must_use]
    pub fn effective_disposition(&self) -> (Disposition, Option<String>) {
        let unknown = |field: &str, v: &str| format!("unknown-{field}-{v}");
        if let ItemKind::Unknown(v) = &self.kind {
            return (Disposition::Requeue, Some(unknown("kind", v)));
        }
        if let Disposition::Unknown(v) = &self.disposition {
            return (Disposition::Requeue, Some(unknown("disposition", v)));
        }
        if let ItemStatus::Unknown(v) = &self.status {
            return (Disposition::Requeue, Some(unknown("status", v)));
        }
        if let Some(Runtime::Unknown(v)) = self.resume_handle.as_ref().map(|h| &h.runtime) {
            return (Disposition::Requeue, Some(unknown("runtime", v)));
        }
        (self.disposition.clone(), self.reason.clone())
    }
}

/// One audit-trail entry. Both processes append.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEvent {
    pub at: String,
    #[serde(default)]
    pub by_version: Option<String>,
    #[serde(default)]
    pub item: Option<String>,
    pub event: String,
    #[serde(default)]
    pub detail: Option<String>,
}

/// The whole manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PauseManifest {
    pub schema_version: u32,
    pub manifest_id: String,
    pub phase: Phase,
    pub written_by: WrittenBy,
    pub roll: Roll,
    #[serde(default)]
    pub items: Vec<ManifestItem>,
    #[serde(default)]
    pub events: Vec<ManifestEvent>,
}

impl PauseManifest {
    /// Age of the pause at `now`, measured from `roll.pause_started_at`
    /// (which precedes every lease refresh, so the check is conservative).
    #[must_use]
    pub fn age_secs(&self, now: DateTime<Utc>) -> i64 {
        (now - self.roll.pause_started_at).num_seconds()
    }
}

/// The typed result of [`load`] (mirrors #10713). Everything except `Loaded`
/// means "fall back to plain restart recovery"; none of them panics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadOutcome {
    Loaded(PauseManifest),
    Missing,
    /// Unreadable or not a v1 manifest; the string says why.
    Corrupt(String),
    /// Written by a newer schema than this binary knows. Resume nothing.
    UnknownVersion(u32),
    /// Older than `roll.max_age_secs`: its `resume` items must be requeued
    /// with `manifest-stale`, never resumed.
    Stale(PauseManifest),
}

/// Load the manifest at `path` and classify it at `now`.
#[must_use]
pub fn load(path: &Path, now: DateTime<Utc>) -> LoadOutcome {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return LoadOutcome::Missing,
        Err(e) => return LoadOutcome::Corrupt(format!("read {}: {e}", path.display())),
    };
    parse(&raw, now)
}

/// [`load`] for text already in memory.
#[must_use]
pub fn parse(raw: &str, now: DateTime<Utc>) -> LoadOutcome {
    let value: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => return LoadOutcome::Corrupt(format!("not JSON: {e}")),
    };
    // Read the version before the full shape, so a newer schema that changed
    // the shape reports UnknownVersion rather than Corrupt.
    let version = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64);
    match version {
        None => return LoadOutcome::Corrupt("no integer schema_version".to_string()),
        Some(v) if v > u64::from(SCHEMA_VERSION) => {
            return LoadOutcome::UnknownVersion(u32::try_from(v).unwrap_or(u32::MAX));
        }
        Some(_) => {}
    }
    let manifest: PauseManifest = match serde_json::from_value(value) {
        Ok(m) => m,
        Err(e) => return LoadOutcome::Corrupt(format!("not a v1 manifest: {e}")),
    };
    let max_age = i64::try_from(manifest.roll.max_age_secs).unwrap_or(i64::MAX);
    if manifest.age_secs(now) > max_age {
        LoadOutcome::Stale(manifest)
    } else {
        LoadOutcome::Loaded(manifest)
    }
}

/// Write the manifest atomically (temp file, `fsync`, `rename`).
///
/// # Errors
/// Any serialization or I/O failure. The caller must treat a failed save
/// before any agent is signalled as "abort the pause" (§7 H4 failure edges).
pub fn save(path: &Path, manifest: &PauseManifest) -> std::io::Result<()> {
    let body = serde_json::to_vec_pretty(manifest).map_err(std::io::Error::other)?;
    crate::roll_pause::write_atomic(path, &body)
}

#[cfg(test)]
#[path = "pause_manifest_tests.rs"]
mod tests;
