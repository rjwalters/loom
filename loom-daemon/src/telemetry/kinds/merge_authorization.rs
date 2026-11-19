//! `merge.authorization.evaluated` (#12666): one merge-group authorization
//! evaluation, exported to OTLP.
//!
//! # Why this kind exists
//!
//! `run_group_check` posts the `loom/merge-authorization` commit status for a
//! merge-group commit, but the *why* of each evaluation (still waiting on
//! other required checks, denied, an unconfirmed status write, a revoked
//! authorization) was visible only in the status description. This record
//! makes it queryable next to `merge_queue.event`.
//!
//! One record per evaluation, on every return path. The reason is a closed
//! token set ([`REASONS`]); the facts are read once by the evaluation itself,
//! so no forge call is added. A field the evaluation did not read is omitted,
//! never defaulted. There is no policy-version constant, so the build
//! provenance is the version stamp. **OTLP only; provenance is required.**
//! Both clocks are the evaluation time.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::telemetry::provenance::Provenance;

/// The closed set of `reason` tokens.
pub const REASONS: &[&str] = &[
    "pending-other-checks",
    "pending-checks-unreadable",
    "denied",
    "denied-membership-unknown",
    "status-post-failed",
    "stale-authorization",
    "groups-unreadable",
    "authorized",
];

/// Every log attribute key this kind exports besides `loom.kind`,
/// `loom.record_id` and `loom.pr_number` (already kept). The collector's
/// `transform/privacy` log `keep_keys` must list each.
pub const MERGE_AUTHORIZATION_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.merge_authorization.reason",
    "loom.merge_authorization.members",
    "loom.merge_authorization.status_write",
    "loom.merge_authorization.status_confirmed",
    "loom.merge_authorization.other_total",
    "loom.merge_authorization.other_success",
    "loom.merge_authorization.other_pending",
    "loom.merge_authorization.other_failure",
    "loom.merge_authorization.version",
    "loom.merge_authorization.revision",
    "loom.merge_authorization.provenance_complete",
];

/// The `merge.authorization.evaluated` record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeAuthorizationRecord {
    /// The merge-group commit evaluated.
    pub commit: String,
    /// One of [`REASONS`].
    pub reason: String,
    /// The entry the group was built for (its last member), when the group
    /// was found with members.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<u32>,
    /// Members the group carries, when the group was found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub members: Option<u32>,
    /// `posted`, `unchanged` (the identical status was already on the
    /// commit) or `failed`; absent when nothing was concluded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_write: Option<String>,
    /// Whether the status is confirmed on the commit (`posted` or
    /// `unchanged`); absent when nothing was concluded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_confirmed: Option<bool>,
    /// Other required contexts read, when that read succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other_total: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other_success: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other_pending: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other_failure: Option<u32>,
    /// When the evaluation finished.
    pub at: DateTime<Utc>,
    /// The emitting build.
    pub loom: Provenance,
}
