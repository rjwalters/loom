//! Shared emission path for operational signals from any daemon loop (Issue
//! #8860).
//!
//! Every exported signal used to need its own record kind, collector wiring
//! and OTLP mapping. This module is the one path a daemon loop uses instead:
//!
//! - [`emit_metrics`] enqueues a [`MetricPointsRecord`] (`metric.points`) of
//!   points named from the fixed [`crate::telemetry::ops::MetricName`]
//!   vocabulary.
//! - [`emit_span`] enqueues a completed [`SpanRecord`] (a fixed
//!   [`crate::telemetry::trace::SpanName`]).
//!
//! Both land in the OTLP exporters' durable queues through the process-global
//! [`OpsSink`] that [`super::spawn_task`] registers — the same pattern as
//! [`super::session_summary`]. With observability off, or with no `otlp`
//! exporter configured, nothing is registered and both calls return
//! immediately without allocating a record (the FLAGS-OFF posture).
//!
//! Emitters: [`dispatch`] (one span plus decision counters per work-finder
//! tick), [`host`] (memory, swap and worktree-volume gauges on the
//! `host.health` cadence), [`queue`] (ready-queue depth per tick, #8852
//! phase 2), [`quota`] (#8857: token burn and pool exhaustion on the
//! `host.health` cadence) and [`dwell`] (ready-queue wait and starvation per
//! multi-workspace tick, #8856). A new emitter adds a `MetricName`/`SpanName`
//! variant and calls the same two functions.

pub mod dispatch;
pub mod dwell;
pub mod host;
pub mod queue;
pub mod quota;

use std::sync::{Arc, OnceLock};

use chrono::Utc;

use super::queue::QueueSink;
use crate::telemetry::ops::MetricPoint;
use crate::telemetry::trace::SpanRecord;
use crate::telemetry::{MetricPointsRecord, TelemetryEnvelope, TelemetryRecord};

/// Wraps ops signals in envelopes stamped with this daemon's host id and
/// offers them to the OTLP exporters' queues.
#[derive(Clone)]
pub struct OpsSink {
    queue: Arc<dyn QueueSink>,
    host_id: String,
}

impl OpsSink {
    /// A sink over `queue` (the OTLP exporters' fan-out) for `host_id`.
    #[must_use]
    pub fn new(queue: Arc<dyn QueueSink>, host_id: impl Into<String>) -> Self {
        OpsSink {
            queue,
            host_id: host_id.into(),
        }
    }

    /// Enqueue one `metric.points` record. An empty batch enqueues nothing.
    pub fn emit_metrics(&self, points: Vec<MetricPoint>) {
        self.emit_metrics_since(points, None);
    }

    /// [`Self::emit_metrics`] with the interval the batch's delta counters
    /// cover starting at `interval_start`.
    ///
    /// Policy ([`MetricPointsRecord::bounded_points`]) is applied here, before
    /// the record reaches the on-disk queue, as well as again at export: a
    /// non-finite double would otherwise serialise as `null` and poison the
    /// whole queued batch on reload (#8857). A batch that bounds to nothing
    /// enqueues nothing.
    pub fn emit_metrics_since(
        &self,
        points: Vec<MetricPoint>,
        interval_start: Option<chrono::DateTime<Utc>>,
    ) {
        let mut record = MetricPointsRecord {
            captured_at: Utc::now(),
            interval_start,
            points,
        };
        record.points = record.bounded_points();
        if record.points.is_empty() {
            return;
        }
        self.queue.offer(TelemetryEnvelope::new(
            self.host_id.clone(),
            TelemetryRecord::MetricPoints(record),
        ));
    }

    /// Enqueue one completed span, carrying its own context so logs can join
    /// it. Unsampled or invalid spans are not enqueued.
    pub fn emit_span(&self, span: SpanRecord) {
        if !span.context.sampled() || span.validate().is_err() {
            return;
        }
        let mut envelope =
            TelemetryEnvelope::new(self.host_id.clone(), TelemetryRecord::Span(span.clone()));
        envelope.trace_context = Some(span.context);
        self.queue.offer(envelope);
    }
}

impl std::fmt::Debug for OpsSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpsSink")
            .field("host_id", &self.host_id)
            .finish_non_exhaustive()
    }
}

static GLOBAL_OPS_SINK: OnceLock<OpsSink> = OnceLock::new();

/// The sink to register for a daemon whose running OTLP exporters' queues are
/// `otlp_queues`: `None` when there are none, so an HTTPS-only (or disabled)
/// daemon never registers one and every `emit_*` stays a no-op.
#[must_use]
pub fn sink_for_otlp_queues(
    otlp_queues: Vec<Arc<super::queue::DurableQueue>>,
    host_id: &str,
) -> Option<OpsSink> {
    if otlp_queues.is_empty() {
        return None;
    }
    Some(OpsSink::new(Arc::new(super::queue::FanoutQueue::new(otlp_queues)), host_id))
}

/// Register the process-global sink. Called once from [`super::spawn_task`]
/// when at least one OTLP exporter started; later calls are no-ops.
pub fn register_global_ops_sink(sink: OpsSink) {
    let _ = GLOBAL_OPS_SINK.set(sink);
}

/// The registered sink, or `None` when ops signals are not exported.
#[must_use]
pub fn global_ops_sink() -> Option<&'static OpsSink> {
    GLOBAL_OPS_SINK.get()
}

/// Emit `points` through the global sink; a no-op when none is registered.
pub fn emit_metrics(points: Vec<MetricPoint>) {
    if let Some(sink) = global_ops_sink() {
        sink.emit_metrics(points);
    }
}

/// Emit a completed span through the global sink; a no-op when none is
/// registered.
pub fn emit_span(span: SpanRecord) {
    if let Some(sink) = global_ops_sink() {
        sink.emit_span(span);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
