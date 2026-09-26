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
    // ---- Telemetry mega PR A (Issues #8908, #8931) ----------------------
    // `loom.runtime.usage` (#8908): one execution's exact token breakdown.
    "loom.tokens.input",
    "loom.tokens.output",
    "loom.tokens.cache_read",
    "loom.tokens.cache_write",
    "loom.tokens.total",
    // `loom.pool.hold` (#8931): one pool dispatch hold, armed to cleared.
    "loom.pool.hold.post_mortem",
    "loom.pool.hold.accounts",
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
    /// Age of the oldest waiting ready-queue row, labelled `state` (#8856).
    #[serde(rename = "loom.queue.oldest_wait")]
    QueueOldestWait,
    /// Waiting ready-queue rows older than the starvation threshold,
    /// labelled `state` (#8856).
    #[serde(rename = "loom.queue.starved")]
    QueueStarved,
    /// Starved rows per queue disposition, labelled `reason` (#8856).
    #[serde(rename = "loom.queue.starved.by_reason")]
    QueueStarvedByReason,
    /// Summed queue dwell of the issues dispatched in one tick (#8856).
    #[serde(rename = "loom.queue.dispatch_wait")]
    QueueDispatchWait,
    /// Issues contributing to `loom.queue.dispatch_wait` (#8856).
    #[serde(rename = "loom.queue.dispatch_wait.samples")]
    QueueDispatchWaitSamples,
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
    // Ready-queue depth (Issue #8852, phase 2) — one contiguous block.
    /// Ready issues on the last work-finder tick, labelled `state` and
    /// `reason` (the queue disposition). Never labelled by issue or repo.
    #[serde(rename = "loom.queue.issues")]
    QueueIssues,
    /// Repos whose ready-issue listing failed on the last tick: a non-zero
    /// value means `loom.queue.issues` is missing their backlog.
    #[serde(rename = "loom.queue.listing_failed_repos")]
    QueueListingFailedRepos,
    // ---- Quota burn and pool state (Issue #8857) ------------------------
    /// Uncached input tokens consumed since the previous sample.
    #[serde(rename = "loom.llm.tokens.input")]
    LlmTokensInput,
    /// Output tokens produced since the previous sample.
    #[serde(rename = "loom.llm.tokens.output")]
    LlmTokensOutput,
    /// Cache-read input tokens since the previous sample.
    #[serde(rename = "loom.llm.tokens.cache_read")]
    LlmTokensCacheRead,
    /// Cache-write input tokens since the previous sample.
    #[serde(rename = "loom.llm.tokens.cache_write")]
    LlmTokensCacheWrite,
    /// Model API responses (distinct message ids) since the previous sample.
    #[serde(rename = "loom.llm.requests")]
    LlmRequests,
    /// Accounts in a provider's pool, labelled `state` = `usable`/`exhausted`.
    #[serde(rename = "loom.pool.accounts")]
    PoolAccounts,
    /// 1 when a provider's pool has exhausted accounts and none usable.
    #[serde(rename = "loom.pool.exhausted")]
    PoolExhausted,
    /// Accounts that became exhausted since the previous sample.
    #[serde(rename = "loom.pool.exhaustions")]
    PoolExhaustions,
    /// Seconds since the previous sample the pool read as exhausted.
    #[serde(rename = "loom.pool.exhausted_seconds")]
    PoolExhaustedSeconds,
    // ---- Reason-classified account marks (Issue #8931) -------------------
    /// Pool accounts marked out of selection since the previous point,
    /// labelled `provider` and `reason` (a closed set — see
    /// `observability::ops::pool_marks::MarkReason`). Never an account name.
    #[serde(rename = "loom.pool.account_marks")]
    PoolAccountMarks,
}

impl MetricName {
    /// The OTLP metric name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DispatchDecisions => "loom.dispatch.decisions",
            Self::DispatchCandidates => "loom.dispatch.candidates",
            Self::DispatchMaxConcurrent => "loom.dispatch.max_concurrent",
            Self::QueueOldestWait => "loom.queue.oldest_wait",
            Self::QueueStarved => "loom.queue.starved",
            Self::QueueStarvedByReason => "loom.queue.starved.by_reason",
            Self::QueueDispatchWait => "loom.queue.dispatch_wait",
            Self::QueueDispatchWaitSamples => "loom.queue.dispatch_wait.samples",
            Self::HostMemoryAvailableBytes => "loom.host.memory.available_bytes",
            Self::HostMemoryTotalBytes => "loom.host.memory.total_bytes",
            Self::HostSwapUsedBytes => "loom.host.swap.used_bytes",
            Self::HostSwapTotalBytes => "loom.host.swap.total_bytes",
            Self::HostWorktreeVolumeFreeBytes => "loom.host.worktree_volume.free_bytes",
            Self::HostWorktreeVolumeTotalBytes => "loom.host.worktree_volume.total_bytes",
            Self::QueueIssues => "loom.queue.issues",
            Self::QueueListingFailedRepos => "loom.queue.listing_failed_repos",
            Self::LlmTokensInput => "loom.llm.tokens.input",
            Self::LlmTokensOutput => "loom.llm.tokens.output",
            Self::LlmTokensCacheRead => "loom.llm.tokens.cache_read",
            Self::LlmTokensCacheWrite => "loom.llm.tokens.cache_write",
            Self::LlmRequests => "loom.llm.requests",
            Self::PoolAccounts => "loom.pool.accounts",
            Self::PoolExhausted => "loom.pool.exhausted",
            Self::PoolExhaustions => "loom.pool.exhaustions",
            Self::PoolExhaustedSeconds => "loom.pool.exhausted_seconds",
            Self::PoolAccountMarks => "loom.pool.account_marks",
        }
    }

    /// Gauge or delta counter.
    #[must_use]
    pub fn kind(self) -> MetricKind {
        match self {
            Self::DispatchDecisions
            | Self::LlmTokensInput
            | Self::LlmTokensOutput
            | Self::LlmTokensCacheRead
            | Self::LlmTokensCacheWrite
            | Self::LlmRequests
            | Self::PoolExhaustions
            | Self::PoolExhaustedSeconds
            | Self::PoolAccountMarks
            | Self::QueueDispatchWait
            | Self::QueueDispatchWaitSamples => MetricKind::DeltaCounter,
            _ => MetricKind::Gauge,
        }
    }

    /// UCUM unit string.
    #[must_use]
    pub fn unit(self) -> &'static str {
        match self {
            Self::DispatchDecisions
            | Self::DispatchCandidates
            | Self::QueueStarved
            | Self::QueueStarvedByReason
            | Self::QueueDispatchWaitSamples => "{issue}",
            Self::DispatchMaxConcurrent => "{sweep}",
            Self::QueueIssues => "{issue}",
            Self::QueueListingFailedRepos => "{repository}",
            Self::LlmTokensInput
            | Self::LlmTokensOutput
            | Self::LlmTokensCacheRead
            | Self::LlmTokensCacheWrite => "{token}",
            Self::LlmRequests => "{request}",
            Self::PoolAccounts | Self::PoolExhaustions | Self::PoolAccountMarks => "{account}",
            Self::PoolExhausted => "1",
            Self::PoolExhaustedSeconds => "s",
            Self::QueueOldestWait | Self::QueueDispatchWait => "s",
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
            Self::QueueOldestWait => "Age of the oldest waiting ready-queue issue, by state.",
            Self::QueueStarved => "Waiting ready-queue issues past the starvation threshold.",
            Self::QueueStarvedByReason => "Starved ready-queue issues by queue disposition.",
            Self::QueueDispatchWait => "Summed queue dwell of the issues dispatched per tick.",
            Self::QueueDispatchWaitSamples => "Issues counted in loom.queue.dispatch_wait.",
            Self::HostMemoryAvailableBytes => "Memory available without swapping.",
            Self::HostMemoryTotalBytes => "Physical memory installed.",
            Self::HostSwapUsedBytes => "Swap space in use.",
            Self::HostSwapTotalBytes => "Swap space configured.",
            Self::HostWorktreeVolumeFreeBytes => "Free space on the worktree-root volume.",
            Self::HostWorktreeVolumeTotalBytes => "Capacity of the worktree-root volume.",
            Self::QueueIssues => "Ready issues on the last work-finder tick, by state and reason.",
            Self::QueueListingFailedRepos => "Repos whose ready-issue listing failed last tick.",
            Self::LlmTokensInput => "Uncached input tokens consumed, by provider and model.",
            Self::LlmTokensOutput => "Output tokens produced, by provider and model.",
            Self::LlmTokensCacheRead => "Cache-read input tokens, by provider and model.",
            Self::LlmTokensCacheWrite => "Cache-write input tokens, by provider and model.",
            Self::LlmRequests => "Model API responses, by provider and model.",
            Self::PoolAccounts => "Enabled pool accounts by provider and state.",
            Self::PoolExhausted => "1 when no account in the provider's pool is usable.",
            Self::PoolExhaustions => "Accounts that became exhausted since the last sample.",
            Self::PoolExhaustedSeconds => "Seconds the provider's pool read as exhausted.",
            Self::PoolAccountMarks => {
                "Pool accounts marked out of selection, by provider and reason."
            }
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

    /// Add one label (builder style). Value policy is applied at emit and
    /// again at export; an unallowlisted key is a programming error, caught in
    /// debug builds here and dropped by [`bounded_labels`] in release.
    #[must_use]
    pub fn label(mut self, key: &str, value: impl Into<String>) -> Self {
        debug_assert!(
            OPS_METRIC_LABEL_KEYS.contains(&key),
            "metric label key {key:?} is not in OPS_METRIC_LABEL_KEYS"
        );
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
    /// Start of the interval the delta counters in this batch cover (the OTLP
    /// `start_time_unix_nano`). `None` falls back to `captured_at`; gauges
    /// ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_start: Option<DateTime<Utc>>,
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
