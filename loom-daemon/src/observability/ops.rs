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
//! multi-workspace tick, #8856). [`pool_marks`] (#8931) emits a
//! reason-classified counter at each seam that marks a pool account, and one
//! span per pool dispatch hold. [`turnaround`] (#8929) covers slot turnaround
//! and idle slots per host, [`stage_dwell`] forge label-stage dwell, and
//! [`disposition`] (#9222) one span per ready-queue row's disposition, on
//! transition or periodic refresh. [`ratelimit`] (#10022) exports GitHub
//! rate-limit breaker trips, quota gauges (per bucket since W1) and breaker
//! skips, [`forge_calls`] (W1) the facade's `loom.forge.calls` counter, and
//! [`redate_chain`] (#10163) #8508 re-date pressure (re-dated PRs, re-dates
//! per PR, time to land), [`eta_health`] (#10391) the per-host ETA
//! pipeline health gauges, [`reader_withdrawal`] (W4-A) one
//! `forge.reader.withdrawn` span per reader withdrawal, and [`reader_spill`]
//! (W4-B) one `forge.reader.spill` span per read-pool spill-latch
//! transition. A new emitter adds a `MetricName`/`SpanName`
//! variant and calls the same two functions.
//!
//! Tests observe what a seam emitted through the global functions with
//! [`capture::capture`], a per-thread recorder (test builds only).

#[cfg(test)]
pub mod capture;
pub mod dispatch;
pub mod disposition;
pub mod dwell;
pub mod eta_health;
pub mod forge_calls;
pub mod host;
pub mod liveness;
pub mod lockout;
pub mod pool_marks;
pub mod queue;
pub mod quota;
pub mod ratelimit;
pub mod read_shed;
pub mod reader_spill;
pub mod reader_withdrawal;
pub mod redate_chain;
pub mod stage_dwell;
pub mod turnaround;

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
        self.emit_metrics_over(points, interval_start, Utc::now());
    }

    /// [`Self::emit_metrics_since`] with the points stamped at `end` rather
    /// than now, so a sampler whose window ends before the sample can make
    /// consecutive intervals abut exactly (#8941 item 2).
    pub fn emit_metrics_over(
        &self,
        points: Vec<MetricPoint>,
        interval_start: Option<chrono::DateTime<Utc>>,
        end: chrono::DateTime<Utc>,
    ) {
        let mut record = MetricPointsRecord {
            captured_at: end,
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

    /// The host id this sink stamps on every envelope.
    #[must_use]
    pub fn host_id(&self) -> &str {
        &self.host_id
    }

    /// Enqueue one OTLP-only log record (Issue #10414: `auto_update.tick`),
    /// for a loop that has no sink of its own.
    pub fn emit_record(&self, record: TelemetryRecord) {
        self.queue
            .offer(TelemetryEnvelope::new(self.host_id.clone(), record));
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
    #[cfg(test)]
    let Some(points) = capture::metrics(points) else {
        return;
    };
    if let Some(sink) = global_ops_sink() {
        sink.emit_metrics(points);
    }
}

/// Emit one OTLP-only log record through the global sink; a no-op when none
/// is registered (Issue #10414).
pub fn emit_record(record: TelemetryRecord) {
    #[cfg(test)]
    let Some(record) = capture::record(record) else {
        return;
    };
    if let Some(sink) = global_ops_sink() {
        sink.emit_record(record);
    }
}

/// Whether [`emit_span`] would deliver a span from this thread: a global sink
/// is registered, or (test builds) a [`capture::capture`] is active. Lets a
/// seam avoid handing a child a parent span that will never be exported.
#[must_use]
pub fn spans_exported() -> bool {
    #[cfg(test)]
    if capture::active() {
        return true;
    }
    global_ops_sink().is_some()
}

/// Emit a completed span through the global sink; a no-op when none is
/// registered.
pub fn emit_span(span: SpanRecord) {
    #[cfg(test)]
    let Some(span) = capture::span(span) else {
        return;
    };
    if let Some(sink) = global_ops_sink() {
        sink.emit_span(span);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
