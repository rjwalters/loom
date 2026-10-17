//! `hold.transition` (#12665, #12653 F2): a hold label was applied to or
//! removed from a PR.
//!
//! # Why this kind exists
//!
//! `merge_hold` ETA needs the hold **cause** and who released it, which no
//! exported fact carried once `eta.stage_outcome`'s producer was removed
//! (#11301). The fact is emitted at the write that applies or removes the hold
//! (Champion's merge-risk and critical-file holds via `park-record apply`, and
//! the release via `park-record released`), from the role tick's pick journal:
//! no forge read and no polling.
//!
//! # Semantics
//!
//! - One record per (PR, transition): an `add` is emitted only when the label
//!   was not already on the PR, so the every-tick idempotent re-apply emits
//!   nothing.
//! - `confirmed` is `false` for a write that was attempted and failed.
//! - An `add` opens an episode (`episode_id`, `entered_at == event_at`). A
//!   `remove` cannot name the episode without a forge read: it carries the
//!   `entered_at` the caller knows, else `entered_at_lower_bound: true` and no
//!   `entered_at`. A first-seen time is never stamped as the entry.
//!
//! **OTLP only.** No free-text bodies. **Provenance is required.**

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::telemetry::provenance::Provenance;

/// Every log attribute key this kind exports besides `loom.kind`,
/// `loom.record_id`, `loom.repo` and `loom.pr_number` (already kept). The
/// collector's `transform/privacy` log `keep_keys` must list each.
pub const HOLD_TRANSITION_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.hold.transition",
    "loom.hold.reason_code",
    "loom.hold.trigger",
    "loom.hold.confirmed",
    "loom.hold.episode_id",
    "loom.hold.entered_at",
    "loom.hold.entered_at_lower_bound",
    "loom.hold.version",
    "loom.hold.revision",
    "loom.hold.provenance_complete",
];

/// `transition` values.
pub mod transition {
    pub const ADD: &str = "add";
    pub const REMOVE: &str = "remove";
}

/// `reason_code` values.
pub mod reason {
    pub const MERGE_RISK_HOLD: &str = "merge-risk-hold";
    pub const CRITICAL_FILE_HOLD: &str = "critical-file-hold";
    pub const CHAMPION_PARK: &str = "champion-park";
    pub const HOLD_RELEASED: &str = "hold-released";
}

/// The `hold.transition` record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoldTransitionRecord {
    /// `owner/repo`, lower-cased.
    pub repo: String,
    /// The held pull request.
    pub pr: u32,
    /// `add` or `remove`.
    pub transition: String,
    /// Structured cause (a closed vocabulary, never free text).
    pub reason_code: String,
    /// What caused the write (a closed token, e.g. `champion_role`).
    pub trigger: String,
    /// `false`: the write was attempted and failed.
    pub confirmed: bool,
    /// The hold episode, known on `add` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode_id: Option<String>,
    /// When the hold began, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entered_at: Option<DateTime<Utc>>,
    /// The entry time is unknown: the real entry is no later than the first
    /// record that names this episode.
    #[serde(default)]
    pub entered_at_lower_bound: bool,
    /// When the write happened.
    pub event_at: DateTime<Utc>,
    /// When the daemon drained the journal.
    pub observed_at: DateTime<Utc>,
    /// The emitting build.
    pub loom: Provenance,
}
