//! `roster.change` (#12322): the roster pass changed this host's workspace
//! registry, or could not because the repo is not cloned.
//!
//! # Why this kind exists
//!
//! `loom.dispatch.disposition` spans exist only for repos with a queue item, so
//! a repo that is registered but idle emitted nothing: "absent from SigNoz"
//! could not be told apart from "not registered on this host". The roster pass
//! (`fleet_sync::roster_clone::summarize`) is the one place that knows, so it
//! emits one record per change it applied and per desired repo it could not
//! register, whatever the queue holds.
//!
//! `skip:not-cloned` repeats every timer pass while the repo stays uncloned: a
//! repeating row is the proof the repo is still unregistered. The volume is one
//! row per missing repo per pass.
//!
//! **OTLP only.** The scalars ride as `loom.roster.*` attributes
//! ([`ROSTER_CHANGE_LOG_ATTRIBUTE_KEYS`], which the collector's log `keep_keys`
//! must list; contract-tested). The body is the record's JSON, which carries no
//! absolute path. **Provenance is required.**

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::telemetry::provenance::Provenance;
use crate::telemetry::RepoVisibility;

/// Every log attribute key `roster.change` exports.
pub const ROSTER_CHANGE_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.roster.action",
    "loom.roster.name",
    "loom.roster.from_weight",
    "loom.roster.to_weight",
    "loom.roster.maintain_only",
    "loom.roster.host",
    "loom.roster.version",
    "loom.roster.revision",
    "loom.roster.tree_state",
    "loom.roster.provenance_complete",
];

/// What the roster pass did (or could not do) to one repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RosterAction {
    /// Registered a desired workspace.
    #[serde(rename = "register")]
    Register,
    /// Changed a registered workspace's draw weight.
    #[serde(rename = "set-weight")]
    SetWeight,
    /// Made a registered workspace maintain-only, or normal again.
    #[serde(rename = "set-maintain-only")]
    SetMaintainOnly,
    /// Deregistered a workspace the store says must not be managed.
    #[serde(rename = "remove")]
    Remove,
    /// A desired repo that is not cloned under `root`, so not registered.
    #[serde(rename = "skip:not-cloned")]
    SkipNotCloned,
}

impl RosterAction {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Register => "register",
            Self::SetWeight => "set-weight",
            Self::SetMaintainOnly => "set-maintain-only",
            Self::Remove => "remove",
            Self::SkipNotCloned => "skip:not-cloned",
        }
    }
}

/// The `roster.change` record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RosterChangeRecord {
    /// `owner/repo` from the repo's remote, when it parses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Private until resolved: a missing tag never decodes to public.
    #[serde(default)]
    pub visibility: RepoVisibility,
    /// The roster record's name (the repo's identity when no slug parses).
    pub name: String,
    /// What happened.
    pub action: RosterAction,
    /// The weight before the change (`set-weight` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_weight: Option<u32>,
    /// The weight after the change (`register`, `set-weight`, and the weight a
    /// `skip:not-cloned` repo would be registered at).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_weight: Option<u32>,
    /// Maintain-only after the change (`register`, `set-maintain-only`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintain_only: Option<bool>,
    /// The host whose registry this is.
    pub host: String,
    /// When the pass made the change.
    pub at: DateTime<Utc>,
    /// The observing build.
    pub loom: Provenance,
}

impl RosterChangeRecord {
    /// Whether the record carries valid provenance.
    #[must_use]
    pub fn has_provenance(&self) -> bool {
        self.loom.is_valid()
    }
}
