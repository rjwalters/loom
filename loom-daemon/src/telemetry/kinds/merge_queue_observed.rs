//! `merge.queue.observed` (#12667, slice of #12653): the merge queue as one
//! existing read saw it.
//!
//! # Why this kind exists
//!
//! Queue position and depth shape `merge_wait` and `merge_hold`, but only the
//! group a Loom-owned transition revokes was ever visible. The
//! `mergeQueue.entries` read behind `GroupForge::groups()` already returns
//! every entry's position, PR and group commit, so this record reports what
//! that read saw. **No forge call is added**: the record is emitted from the
//! read's own result, on transition, never on a timer.
//!
//! # Known limits, encoded as unknowns
//!
//! - `state = "unknown"` means the read failed before any entry was read. It
//!   is never an empty queue; an observed-empty queue (`mergeQueue: null` or
//!   no entries) is `state = "observed"`, `complete = true`, `order = []`.
//! - `complete = false` means a later page failed: `entries` / `order` are a
//!   prefix of the queue, not the queue.
//! - A group head identifies the top PR of that group, not its rank; `order`
//!   is the entries' `position` order as the read reported it.
//! - Gaps between records are not an empty queue: records exist only when the
//!   read ran.
//!
//! **OTLP only.** **Provenance is required.**

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::telemetry::provenance::Provenance;

/// Every log attribute key this kind exports besides `loom.kind` and
/// `loom.record_id` (already kept). The collector's `transform/privacy` log
/// `keep_keys` must list each. The version/revision/provenance keys are shared
/// with `merge_queue.event`.
pub const MERGE_QUEUE_OBSERVED_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.merge_queue.queue_state",
    "loom.merge_queue.queue_complete",
    "loom.merge_queue.queue_depth",
    "loom.merge_queue.version",
    "loom.merge_queue.revision",
    "loom.merge_queue.provenance_complete",
];

/// One queue entry as read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueEntryObservation {
    /// The entry's queue position.
    pub position: u64,
    /// The pull request.
    pub pr: u32,
    /// The PR head the entry carries.
    pub head: String,
    /// Whether a group commit has been built for the entry yet.
    pub has_group_commit: bool,
}

/// The `merge.queue.observed` record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeQueueObservedRecord {
    /// `observed` or `unknown` (the read failed before any entry).
    pub state: String,
    /// Every page was read. `false` makes `entries` a prefix.
    pub complete: bool,
    /// PR numbers in position order.
    #[serde(default)]
    pub order: Vec<u32>,
    /// The entries read, in position order.
    #[serde(default)]
    pub entries: Vec<QueueEntryObservation>,
    /// When the read returned.
    pub observed_at: DateTime<Utc>,
    /// The emitting build.
    pub loom: Provenance,
}
