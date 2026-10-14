//! `host.export` (#11124, R2 of #10196): this host's own view of its telemetry
//! export, once per `host.health` interval.
//!
//! Export coverage used to reach SigNoz only as gauges. This record carries the
//! facts a fast-refit window needs to see its own holes: which exporters are
//! active, how deep each exporter's queue is, a cumulative `dropped_total` per
//! exporter, and when each last flushed successfully. It also counts the
//! `eta.stage_outcome` exits emitted without a `loom.fact_id` (#11401).
//!
//! **Facts only.** Every host emits its own view; nothing is elected, nothing
//! is capped, and no rate or estimate is computed here. `dropped_total` is
//! the durable loss-ledger sum (Issue #11527): it counts every loss reason —
//! queue overflow, transport-level permanent rejection, partial rejection —
//! and survives a daemon restart. A decrease is therefore no longer an
//! expected counter reset; only a removed or reset ledger file produces one.
//!
//! **OTLP only.** The scalars ride as `loom.host_export.*` attributes
//! ([`HOST_EXPORT_LOG_ATTRIBUTE_KEYS`], which the collector's log `keep_keys`
//! must list; contract-tested). The body is the record's JSON, which carries
//! the per-exporter entries.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::ObservabilityExportStatus;

/// Every log attribute key `host.export` exports.
pub const HOST_EXPORT_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.host_export.exporters",
    "loom.host_export.queue_depth",
    "loom.host_export.dropped_total",
    "loom.host_export.last_flush_ok_at",
    "loom.host_export.fact_id_missing_total",
    // #11530: `opaque_host_id(host)`, the `host-XXXXXXXX` form `loom:lease`
    // comments publish, derived in the OTLP mapping (no wire field).
    "host.lease_id",
];

/// One exporter's queue and flush facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExporterExport {
    /// The exporter name (`https`, `otlp`).
    pub name: String,
    /// Envelopes currently queued, waiting for a flush.
    pub queue_depth: u64,
    /// Records lost over this exporter's lifetime, summed over every reason
    /// in `dropped_by_reason`. **Durable** (Issue #11527): backed by the
    /// on-disk loss ledger, so it survives a daemon restart instead of
    /// resetting to 0. Counts are envelopes, except `receiver_rejected`
    /// which the OTLP receiver reports in signal units.
    pub dropped_total: u64,
    /// The by-reason breakdown behind `dropped_total` (Issue #11527):
    /// `queue_overflow`, `capacity_shrink`, `status_<code>`,
    /// `non_json_response`, `response_too_large`, `invalid_response`,
    /// `receiver_rejected`, `trace_unmappable`. Absent (rather than empty)
    /// when nothing has been lost, and on records from before #11527.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dropped_by_reason: BTreeMap<String, u64>,
    /// When a batch was last acked by the backend. Absent when none has been
    /// acked this process: unknown, never a fabricated time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_flush_ok_at: Option<DateTime<Utc>>,
}

/// The `host.export` record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostExportRecord {
    /// When the view was sampled.
    pub captured_at: DateTime<Utc>,
    /// The host that sampled it.
    pub host: String,
    /// One entry per active exporter, sorted by name.
    pub exporters: Vec<ExporterExport>,
    /// `eta.stage_outcome` exits this process emitted without a
    /// `loom.fact_id` (#11401): each carried `loom.fact_id_missing_reason`.
    /// Cumulative per process lifetime, like `dropped_total`. Absent on
    /// records from before #11401.
    #[serde(default)]
    pub fact_id_missing_total: u64,
}

/// One queue's depth and lifetime loss facts, as the collector reads them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QueueStats {
    /// Envelopes currently queued.
    pub depth: u64,
    /// Lifetime losses over every reason (durable loss-ledger sum).
    pub dropped_total: u64,
    /// The by-reason breakdown behind `dropped_total` (Issue #11527).
    pub dropped_by_reason: BTreeMap<String, u64>,
}

impl HostExportRecord {
    /// Build this host's view from the per-exporter statuses and queue stats.
    /// An exporter that never started (misconfigured or disabled) is not
    /// active and is omitted, as in `host.health`'s `exporters`.
    #[must_use]
    pub fn build(
        host: &str,
        now: DateTime<Utc>,
        statuses: &BTreeMap<String, ObservabilityExportStatus>,
        queues: &BTreeMap<String, QueueStats>,
    ) -> Self {
        let (active, _) = crate::telemetry::export_coverage(statuses, now);
        let exporters = active
            .into_iter()
            .map(|name| {
                let stats = queues.get(&name).cloned().unwrap_or_default();
                let last_flush_ok_at = statuses.get(&name).and_then(|s| s.last_success_at);
                ExporterExport {
                    name,
                    queue_depth: stats.depth,
                    dropped_total: stats.dropped_total,
                    dropped_by_reason: stats.dropped_by_reason,
                    last_flush_ok_at,
                }
            })
            .collect();
        Self {
            captured_at: now,
            host: host.to_string(),
            exporters,
            fact_id_missing_total: super::stage_outcome::fact_id_missing_total(),
        }
    }

    /// Sum of queue depths across exporters.
    #[must_use]
    pub fn queue_depth_sum(&self) -> u64 {
        self.exporters.iter().map(|e| e.queue_depth).sum()
    }

    /// Sum of `dropped_total` across exporters.
    #[must_use]
    pub fn dropped_total_sum(&self) -> u64 {
        self.exporters.iter().map(|e| e.dropped_total).sum()
    }

    /// The most recent successful flush across exporters.
    #[must_use]
    pub fn last_flush_ok_at(&self) -> Option<DateTime<Utc>> {
        self.exporters
            .iter()
            .filter_map(|e| e.last_flush_ok_at)
            .max()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "host_export_tests.rs"]
mod tests;
