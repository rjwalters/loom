//! `metric.points` — the generic operational-metrics record kind (Issue #8860).
//!
//! Before this kind existed, a daemon loop that wanted a number in SigNoz had
//! to add a record kind of its own, an OTLP mapping arm, and native-backend
//! handling. `metric.points` is the one shared carrier: a batch of named
//! points, each a gauge or a delta counter, with a small allowlisted label
//! set. It is OTLP-only (the native HTTPS `/ingest` backend never receives
//! it — see `observability::tracing::native_envelopes`), so adding a metric
//! name here never needs a dashboard-backend change.
//!
//! # Fixed vocabulary
//!
//! [`MetricName`] is a closed enum, exactly like
//! [`SpanName`](super::trace::SpanName): free text can never become a metric
//! name, and each name fixes its own [`MetricKind`], unit and description, so
//! two emitters cannot disagree about whether `loom.dispatch.decisions` is a
//! gauge or a counter. Adding a metric means adding a variant (and, when it
//! needs a new label key, extending [`OPS_METRIC_LABEL_KEYS`] together with
//! the gateway collector's `keep_keys` — contract-tested).
//!
//! # Bounded labels
//!
//! [`bounded_labels`] keeps only [`OPS_METRIC_LABEL_KEYS`], drops values over
//! [`MAX_LABEL_VALUE_BYTES`] or carrying control characters, and caps the
//! count at [`MAX_LABELS_PER_POINT`]. The exporter applies it again at export
//! so a restored on-disk queue cannot bypass the policy.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Label keys a `metric.points` data point may carry. Low-cardinality by
/// construction — never an issue number, sha, sweep id or path. The gateway
/// collector's datapoint `keep_keys` must include every key (contract-tested).
pub const OPS_METRIC_LABEL_KEYS: &[&str] = &["reason", "provider", "account", "model", "state"];

/// Span attribute keys the ops span names (`loom.dispatch.tick`) may carry, in
/// addition to the lifecycle allowlist in `trace::span::bounded_attributes`.
/// The gateway collector's span `keep_keys` must include every key
/// (contract-tested).
pub const OPS_SPAN_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.dispatch.result",
    "loom.dispatch.seen",
    "loom.dispatch.dispatched",
    "loom.dispatch.errors",
    "loom.dispatch.max_concurrent",
];

/// Longest label value kept, in bytes.
pub const MAX_LABEL_VALUE_BYTES: usize = 128;
/// Most labels kept on one point.
pub const MAX_LABELS_PER_POINT: usize = 8;
/// Most points kept in one record.
pub const MAX_POINTS_PER_RECORD: usize = 256;

/// How a metric aggregates. Fixed per [`MetricName`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    /// A point-in-time reading (OTLP `Gauge`).
    Gauge,
    /// A count of events since the previous point from the same emitter (OTLP
    /// monotonic `Sum`, `Delta` temporality).
    DeltaCounter,
}

/// Every metric name `metric.points` can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MetricName {
    /// Work-finder candidate outcomes for one tick, labelled `reason`.
    #[serde(rename = "loom.dispatch.decisions")]
    DispatchDecisions,
    /// Ready candidates the work finder saw in one tick.
    #[serde(rename = "loom.dispatch.candidates")]
    DispatchCandidates,
    /// The dynamic concurrency cap the tick ran under.
    #[serde(rename = "loom.dispatch.max_concurrent")]
    DispatchMaxConcurrent,
    /// Memory available for new allocations without swapping.
    #[serde(rename = "loom.host.memory.available_bytes")]
    HostMemoryAvailableBytes,
    /// Physical memory installed.
    #[serde(rename = "loom.host.memory.total_bytes")]
    HostMemoryTotalBytes,
    /// Swap in use.
    #[serde(rename = "loom.host.swap.used_bytes")]
    HostSwapUsedBytes,
    /// Swap configured.
    #[serde(rename = "loom.host.swap.total_bytes")]
    HostSwapTotalBytes,
    /// Free space on the worktree-root volume.
    #[serde(rename = "loom.host.worktree_volume.free_bytes")]
    HostWorktreeVolumeFreeBytes,
    /// Capacity of the worktree-root volume.
    #[serde(rename = "loom.host.worktree_volume.total_bytes")]
    HostWorktreeVolumeTotalBytes,
}

impl MetricName {
    /// The OTLP metric name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DispatchDecisions => "loom.dispatch.decisions",
            Self::DispatchCandidates => "loom.dispatch.candidates",
            Self::DispatchMaxConcurrent => "loom.dispatch.max_concurrent",
            Self::HostMemoryAvailableBytes => "loom.host.memory.available_bytes",
            Self::HostMemoryTotalBytes => "loom.host.memory.total_bytes",
            Self::HostSwapUsedBytes => "loom.host.swap.used_bytes",
            Self::HostSwapTotalBytes => "loom.host.swap.total_bytes",
            Self::HostWorktreeVolumeFreeBytes => "loom.host.worktree_volume.free_bytes",
            Self::HostWorktreeVolumeTotalBytes => "loom.host.worktree_volume.total_bytes",
        }
    }

    /// Gauge or delta counter.
    #[must_use]
    pub fn kind(self) -> MetricKind {
        match self {
            Self::DispatchDecisions => MetricKind::DeltaCounter,
            _ => MetricKind::Gauge,
        }
    }

    /// UCUM unit string.
    #[must_use]
    pub fn unit(self) -> &'static str {
        match self {
            Self::DispatchDecisions | Self::DispatchCandidates => "{issue}",
            Self::DispatchMaxConcurrent => "{sweep}",
            _ => "By",
        }
    }

    /// One-line description.
    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            Self::DispatchDecisions => "Work-finder candidate outcomes per tick, by reason.",
            Self::DispatchCandidates => "Ready candidates seen by one work-finder tick.",
            Self::DispatchMaxConcurrent => "Dynamic concurrency cap for the work-finder tick.",
            Self::HostMemoryAvailableBytes => "Memory available without swapping.",
            Self::HostMemoryTotalBytes => "Physical memory installed.",
            Self::HostSwapUsedBytes => "Swap space in use.",
            Self::HostSwapTotalBytes => "Swap space configured.",
            Self::HostWorktreeVolumeFreeBytes => "Free space on the worktree-root volume.",
            Self::HostWorktreeVolumeTotalBytes => "Capacity of the worktree-root volume.",
        }
    }
}

/// One data point's value.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MetricValue {
    Int(i64),
    Double(f64),
}

/// One data point.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricPoint {
    pub name: MetricName,
    pub value: MetricValue,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

impl MetricPoint {
    /// An unlabelled integer point.
    #[must_use]
    pub fn int(name: MetricName, value: i64) -> Self {
        MetricPoint {
            name,
            value: MetricValue::Int(value),
            labels: BTreeMap::new(),
        }
    }

    /// Add one label (builder style). Policy is applied at export, not here.
    #[must_use]
    pub fn label(mut self, key: &str, value: impl Into<String>) -> Self {
        self.labels.insert(key.to_string(), value.into());
        self
    }
}

/// `metric.points` — a batch of points sampled at one instant. Host-level: it
/// references no repository, so it carries no visibility tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricPointsRecord {
    /// When the points were sampled.
    pub captured_at: DateTime<Utc>,
    pub points: Vec<MetricPoint>,
}

/// Keep only allowlisted, short, control-free labels, at most
/// [`MAX_LABELS_PER_POINT`] of them.
#[must_use]
pub fn bounded_labels(labels: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    labels
        .iter()
        .filter(|(key, value)| {
            OPS_METRIC_LABEL_KEYS.contains(&key.as_str())
                && value.len() <= MAX_LABEL_VALUE_BYTES
                && !value.chars().any(char::is_control)
        })
        .take(MAX_LABELS_PER_POINT)
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

impl MetricPointsRecord {
    /// Points with policy applied: at most [`MAX_POINTS_PER_RECORD`], labels
    /// bounded, non-finite doubles dropped.
    #[must_use]
    pub fn bounded_points(&self) -> Vec<MetricPoint> {
        self.points
            .iter()
            .filter(|point| match point.value {
                MetricValue::Int(_) => true,
                MetricValue::Double(value) => value.is_finite(),
            })
            .take(MAX_POINTS_PER_RECORD)
            .map(|point| MetricPoint {
                name: point.name,
                value: point.value,
                labels: bounded_labels(&point.labels),
            })
            .collect()
    }
}
