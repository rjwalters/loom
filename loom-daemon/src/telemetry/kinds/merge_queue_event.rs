//! `merge_queue.event` (#12653, F3): one merge-queue record exported to OTLP.
//!
//! # Why this kind exists
//!
//! The `merge_queue.{enqueued,merged,removed}` records
//! ([`crate::forge_merge_queue::events`]) lived only in a local JSONL, so
//! SigNoz could not see queue dwell, the classified removal reason or the
//! enqueue-to-event latency that predict `merge_wait` and `merge_hold`.
//!
//! The JSONL and its `seen/` dedupe stay authoritative: the sink emits this
//! record only when it appended a line, so a repeat, a second reconcile pass
//! or a restart never emits twice. No new forge read is involved.
//!
//! **OTLP only.** The scalars ride as `loom.merge_queue.*` attributes
//! ([`MERGE_QUEUE_EVENT_LOG_ATTRIBUTE_KEYS`], which the collector's log
//! `keep_keys` must list; contract-tested). The body is the record's JSON.
//! **Provenance is required.**

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::telemetry::provenance::Provenance;

/// Every log attribute key this kind exports besides `loom.kind`,
/// `loom.record_id` and `loom.pr_number` (already kept). The collector's
/// `transform/privacy` log `keep_keys` must list each.
pub const MERGE_QUEUE_EVENT_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.merge_queue.event",
    "loom.merge_queue.reason",
    "loom.merge_queue.nonce",
    "loom.merge_queue.enqueue_to_event_secs",
    "loom.merge_queue.merged_after_revocation",
    "loom.merge_queue.version",
    "loom.merge_queue.revision",
    "loom.merge_queue.provenance_complete",
];

/// The `merge_queue.event` record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeQueueEventRecord {
    /// `enqueued`, `merged` or `removed`.
    pub event: String,
    /// The pull request.
    pub pr: u32,
    /// Grant generation (the dedupe nonce).
    pub nonce: u64,
    /// The PR head at the event, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// Removal: the classified reason token, or `revoked-<why>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// When the grant was written (≈ enqueue time).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enqueued_at: Option<DateTime<Utc>>,
    /// When the event happened.
    pub at: DateTime<Utc>,
    /// `at - enqueued_at`, when both parsed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enqueue_to_event_secs: Option<i64>,
    /// Merged although the newest grant record was a revocation.
    #[serde(default)]
    pub merged_after_revocation: bool,
    /// The emitting build.
    pub loom: Provenance,
}
