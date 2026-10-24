//! `merge.lane_wait` (#13029 row 1): a Champion merge step was held back
//! because another PR's merge is in flight in the same repo.
//!
//! # Why this kind exists
//!
//! The dispatch tick counts the steps the merge lane deferred
//! (`loom.dispatch.merge_deferred`, `excluded_items{reason=merge_in_flight}`),
//! which cannot give a wait **duration**: nothing recorded when a PR's wait
//! began, who blocked it, or when it ended. This kind is that per-PR pair.
//!
//! # Semantics
//!
//! - `enter`: the first tick that drops the PR's merge step as merge-in-flight
//!   behind a given blocker. A change of blocker while the wait continues is a
//!   new `enter` (the wait on the old blocker is not closed: `wait_started_at`
//!   carries over, so duration is one query on the latest pair).
//! - `leave`: the first observation of the PR no longer being dropped.
//!   `left_as` says whether it was `admitted` (back in the pool or running) or
//!   `gone` (no longer a candidate: merged, closed, or labels changed).
//!   `wait_secs` is `observed_at - wait_started_at`.
//! - The tracker is in memory: after a daemon restart the first observation
//!   starts a fresh wait with `wait_started_at_lower_bound: true`.
//!
//! **OTLP only.** No free-text bodies. **Provenance is required.**

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::telemetry::provenance::Provenance;

/// Every log attribute key this kind exports besides `loom.kind`,
/// `loom.record_id`, `loom.repo` and `loom.pr_number` (already kept). The
/// collector's `transform/privacy` log `keep_keys` must list each.
pub const MERGE_LANE_WAIT_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.merge_lane.phase",
    "loom.merge_lane.step",
    "loom.merge_lane.blocker_pr",
    "loom.merge_lane.blocker_source",
    "loom.merge_lane.wait_started_at",
    "loom.merge_lane.wait_started_at_lower_bound",
    "loom.merge_lane.left_as",
    "loom.merge_lane.wait_secs",
    "loom.merge_lane.version",
    "loom.merge_lane.revision",
    "loom.merge_lane.provenance_complete",
];

/// `phase` values.
pub mod phase {
    pub const ENTER: &str = "enter";
    pub const LEAVE: &str = "leave";
}

/// `left_as` values.
pub mod left_as {
    /// Back in the drawable pool (or running).
    pub const ADMITTED: &str = "admitted";
    /// No longer a candidate in the repo.
    pub const GONE: &str = "gone";
}

/// The `merge.lane_wait` record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeLaneWaitRecord {
    /// `owner/repo`, lower-cased.
    pub repo: String,
    /// The PR whose merge step was held back.
    pub pr: u32,
    /// `enter` or `leave`.
    pub phase: String,
    /// The held step (`champion_merge` / `champion_hold_reevaluate`).
    pub step: String,
    /// The PR whose merge is in flight.
    pub blocker_pr: u32,
    /// Whose merge: `local` (this host) or `peer`.
    pub blocker_source: String,
    /// When the wait began (the first tick that dropped the step).
    pub wait_started_at: DateTime<Utc>,
    /// The wait began no later than `wait_started_at` (daemon restarted).
    #[serde(default)]
    pub wait_started_at_lower_bound: bool,
    /// `leave` only: `admitted` or `gone`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left_as: Option<String>,
    /// `leave` only: `observed_at - wait_started_at`, whole seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_secs: Option<u64>,
    /// When the tick observed it.
    pub observed_at: DateTime<Utc>,
    /// The emitting build.
    pub loom: Provenance,
}
