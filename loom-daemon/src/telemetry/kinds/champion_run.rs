//! `champion.run` (#13029 row 2): what one Champion merge or hold-reevaluate
//! run decided, and the standing-authorization policy it read.
//!
//! # Why this kind exists
//!
//! `pass.verdict` says what a Champion step concluded in its own reason set,
//! but nothing per run said whether a hold re-read (#12933) merged the PR,
//! kept the hold, or released it, nor which policy digest the hold was drawn
//! under versus the one in force when the run ended.
//!
//! # Semantics
//!
//! One record per `champion_merge`-role step end on a PR (hold re-evaluation
//! and PR triage run as the same role). Built from what the step already
//! wrote (its journal and log), plus the digest the hold recorded when the
//! item was drawn: no forge read.
//!
//! - `outcome`: `merged`, `held` (a hold was applied or stands), `released`
//!   (a hold was removed), `error` (the step failed or a hold write failed),
//!   or `other` (the run ended with none of those: requeued, rejected,
//!   head moved, not eligible, triage). `verdict_reason` carries the
//!   `pass.verdict` reason for every outcome.
//! - `hold_class`: the `hold.transition` `reason_code` of the run's last hold
//!   write on the PR; absent when the run wrote none.
//! - `auth_before`: the digest the standing hold recorded when drawn (absent:
//!   no hold, a legacy hold, or a step this daemon did not spawn).
//!   `auth_after`: the digest of the effective policy at the step's end.
//! - `digest_recorded`: the run wrote a `champion:hold-state` record carrying
//!   an `auth=` digest.
//!
//! **OTLP only.** No free-text bodies. **Provenance is required.**

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::telemetry::provenance::Provenance;

/// Every log attribute key this kind exports besides `loom.kind`,
/// `loom.record_id`, `loom.repo` and `loom.pr_number` (already kept). The
/// collector's `transform/privacy` log `keep_keys` must list each.
pub const CHAMPION_RUN_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.champion.outcome",
    "loom.champion.hold_class",
    "loom.champion.verdict_reason",
    "loom.champion.auth_before",
    "loom.champion.auth_after",
    "loom.champion.digest_recorded",
    "loom.champion.sweep_id",
    "loom.champion.version",
    "loom.champion.revision",
    "loom.champion.provenance_complete",
];

/// `outcome` values.
pub mod outcome {
    pub const MERGED: &str = "merged";
    pub const HELD: &str = "held";
    pub const RELEASED: &str = "released";
    pub const ERROR: &str = "error";
    pub const OTHER: &str = "other";
}

/// The `champion.run` record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChampionRunRecord {
    /// `owner/repo`, lower-cased.
    pub repo: String,
    /// The PR the run was invoked on.
    pub pr: u32,
    /// The run's sweep id, so the record joins to its `loom.fleet_dispatch.call`
    /// span and its session.
    pub sweep_id: String,
    /// See the module docs.
    pub outcome: String,
    /// A `hold.transition` `reason_code`, when the run wrote a hold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_class: Option<String>,
    /// The run's `pass.verdict` reason.
    pub verdict_reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_after: Option<String>,
    /// The run wrote a `champion:hold-state` record with an `auth=` digest.
    #[serde(default)]
    pub digest_recorded: bool,
    /// When the step ended.
    pub event_at: DateTime<Utc>,
    /// The emitting build.
    pub loom: Provenance,
}
