//! `host.export` (#11124, R2 of #10196): this host's own view of its telemetry
//! export, once per `host.health` interval.
//!
//! Export coverage used to reach SigNoz only as gauges. This record carries the
//! facts a fast-refit window needs to see its own holes: which exporters are
//! active, how deep each exporter's queue is, a cumulative `dropped_total` per
//! exporter, and when each last flushed successfully.
//!
//! **Facts only.** Every host emits its own view; nothing is elected, nothing
//! is capped, and no rate or estimate is computed here. `dropped_total` is
//! cumulative **per process lifetime**: it restarts at 0 when the daemon
//! restarts, so a reader must treat a decrease as a counter reset.
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
];

/// One exporter's queue and flush facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExporterExport {
    /// The exporter name (`https`, `otlp`).
    pub name: String,
    /// Envelopes currently queued, waiting for a flush.
    pub queue_depth: u64,
    /// Envelopes dropped (oldest first) for arriving at a full queue, summed
    /// over this process's lifetime. Resets to 0 on a daemon restart.
    pub dropped_total: u64,
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
}

/// One queue's depth and lifetime drop count, as the collector reads them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct QueueStats {
    /// Envelopes currently queued.
    pub depth: u64,
    /// Lifetime drops.
    pub dropped_total: u64,
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
                let stats = queues.get(&name).copied().unwrap_or_default();
                let last_flush_ok_at = statuses.get(&name).and_then(|s| s.last_success_at);
                ExporterExport {
                    name,
                    queue_depth: stats.depth,
                    dropped_total: stats.dropped_total,
                    last_flush_ok_at,
                }
            })
            .collect();
        Self {
            captured_at: now,
            host: host.to_string(),
            exporters,
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
