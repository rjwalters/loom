//! `merge.queue.regime` (#12675, slice of #12653): the merge-queue settings
//! that shape `merge_wait` / `merge_hold` duration, emitted on change only.
//!
//! # Why this kind exists
//!
//! `check_response_timeout_minutes`, `max_entries_to_build` and
//! `grouping_strategy` live in the active `merge_queue` rule's `parameters`.
//! The daemon tick already reads that rule (`preflight::github_preflight`, via
//! `tick_groups`), so this record is emitted from that read's result. **No
//! forge call is added.**
//!
//! # Known limits, encoded as unknowns
//!
//! - A key absent from `parameters` is `null`, never a defaulted value.
//! - `ambiguous = true` means more than one `merge_queue` rule with differing
//!   parameters applies to the branch; the three fields are then `null`
//!   rather than a guess.
//! - Dedupe state is in-process: the first observation after a daemon start
//!   emits once more.
//!
//! **OTLP only.** **Provenance is required.**

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::telemetry::provenance::Provenance;

/// Every log attribute key this kind exports besides `loom.kind` and
/// `loom.record_id` (already kept). The collector's `transform/privacy` log
/// `keep_keys` must list each.
pub const MERGE_QUEUE_REGIME_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.merge_queue.check_response_timeout_minutes",
    "loom.merge_queue.max_entries_to_build",
    "loom.merge_queue.grouping_strategy",
    "loom.merge_queue.regime_ambiguous",
    "loom.merge_queue.version",
    "loom.merge_queue.revision",
    "loom.merge_queue.provenance_complete",
];

/// The regime tuple read from the `merge_queue` rule's `parameters`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueRegime {
    /// `check_response_timeout_minutes`, `None` when the key is absent.
    #[serde(default)]
    pub check_response_timeout_minutes: Option<u64>,
    /// `max_entries_to_build`, `None` when the key is absent.
    #[serde(default)]
    pub max_entries_to_build: Option<u64>,
    /// `grouping_strategy`, `None` when the key is absent.
    #[serde(default)]
    pub grouping_strategy: Option<String>,
    /// More than one differing `merge_queue` rule applies: the fields above
    /// are `None`, not a guess.
    #[serde(default)]
    pub ambiguous: bool,
}

/// The `merge.queue.regime` record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeQueueRegimeRecord {
    /// The regime as read.
    #[serde(flatten)]
    pub regime: QueueRegime,
    /// When the read returned.
    pub observed_at: DateTime<Utc>,
    /// The emitting build.
    pub loom: Provenance,
}
