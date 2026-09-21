//! Per-signal transport accounting, separate from durable-envelope acknowledgments.
use super::exporter::ExportError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Counters are cumulative per daemon process. `retry_scheduled` counts attempted
/// items left pending after transient failures, not unique items or confirmed loss.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SignalCounts {
    pub accepted: u64,
    pub rejected: u64,
    /// Unknown acceptance (invalid replies) and permanent HTTP failures.
    pub dropped: u64,
    pub retry_scheduled: u64,
    pub warnings: u64,
}
impl SignalCounts {
    pub fn accumulate(&mut self, other: &Self) {
        self.accepted = self.accepted.saturating_add(other.accepted);
        self.rejected = self.rejected.saturating_add(other.rejected);
        self.dropped = self.dropped.saturating_add(other.dropped);
        self.retry_scheduled = self.retry_scheduled.saturating_add(other.retry_scheduled);
        self.warnings = self.warnings.saturating_add(other.warnings);
    }
}

/// Only an acknowledged prefix can be removed from the durable FIFO. A partial
/// OTLP response acknowledges its entire request; its rejected items are never
/// retried. `exported` counts fully accepted envelopes only, conservatively zero
/// for a partially rejected group whose individual rejected identities are unknown.
#[derive(Debug, Default)]
pub struct BatchOutcome {
    pub acknowledged: usize,
    pub exported: usize,
    pub signals: BTreeMap<String, SignalCounts>,
    pub error: Option<ExportError>,
}
impl BatchOutcome {
    pub fn accepted(envelopes: usize) -> Self {
        Self {
            acknowledged: envelopes,
            exported: envelopes,
            ..Self::default()
        }
    }
}
