//! `session.summary` emission sink (Issue #8757, G3 of epic #8714).
//!
//! The transcript-ingest pass
//! ([`crate::activity::transcript_ingest`]) runs on its own background
//! thread, started before the event bus and the observability task exist
//! (`daemon_service.rs` boots ingestion at the top of its setup and
//! `observability::spawn_task` much later), so it cannot be handed the
//! collector's [`DurableQueue`] at construction. This module gives that
//! thread a handle to the **same** `Arc<DurableQueue>` the collector,
//! backfill pass and sender already share: [`spawn_task`](super::spawn_task)
//! registers it process-globally once the queue exists, and each ingestion
//! pass resolves [`global_session_summary_sink`] at run time — by which
//! point the daemon's boot sequence has long since completed (the pass's
//! first tick sleeps its full interval before doing any work).
//!
//! Sharing the queue — rather than opening a second `DurableQueue` on the
//! same file — is load-bearing: the queue mirrors an in-memory deque to
//! disk on every mutation, so a second instance would clobber this one's
//! state. The global here is the same pattern
//! [`register_global_export_status`](super::register_global_export_status)
//! already established for cross-subsystem status handles.
//!
//! When observability is disabled (the FLAGS-OFF default) nothing is ever
//! registered and [`global_session_summary_sink`] returns `None` — the
//! ingest pass then skips emission entirely, at zero cost.

use std::sync::Arc;

use crate::telemetry::{SessionSummaryRecord, TelemetryEnvelope, TelemetryRecord};

/// The exporter-facing half of `session.summary` emission: wraps one record
/// in a [`TelemetryEnvelope`] stamped with the same host id every other
/// exported record carries (resolved once in `super::spawn_task`, threaded
/// here so the two can never disagree) and offers it onto the shared
/// fan-out sink for the configured exporter(s) to drain (#8756 — with N
/// exporters the record lands in every per-exporter queue).
#[derive(Clone)]
pub struct SessionSummarySink {
    queue: Arc<dyn super::queue::QueueSink>,
    host_id: String,
}

impl SessionSummarySink {
    /// Wrap `queue`/`host_id` (both owned by `super::spawn_task`) in a sink.
    #[must_use]
    pub fn new(queue: Arc<dyn super::queue::QueueSink>, host_id: impl Into<String>) -> Self {
        SessionSummarySink {
            queue,
            host_id: host_id.into(),
        }
    }

    /// The host id this sink stamps on every envelope.
    #[must_use]
    pub fn host_id(&self) -> &str {
        &self.host_id
    }

    /// Enqueue one `session.summary` record. Best-effort like every queue
    /// producer: a persistence failure is logged by the queue and never
    /// propagates into the ingestion pass.
    pub fn push(&self, record: SessionSummaryRecord) {
        self.queue.offer(TelemetryEnvelope::new(
            self.host_id.clone(),
            TelemetryRecord::SessionSummary(record),
        ));
    }
}

/// `IngestOptions` is `Debug` and now holds this sink; `DurableQueue` is not
/// `Debug`, so this names the identity fields and elides the queue.
impl std::fmt::Debug for SessionSummarySink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionSummarySink")
            .field("host_id", &self.host_id)
            .finish_non_exhaustive()
    }
}

static GLOBAL_SESSION_SUMMARY_SINK: std::sync::OnceLock<SessionSummarySink> =
    std::sync::OnceLock::new();

/// Register the process-global sink. Called exactly once, from
/// `super::spawn_task`, after the shared queue exists; a second call is a
/// no-op (the first registration wins, matching the `set`-once contract of
/// the sibling global status handles).
pub fn register_global_session_summary_sink(sink: SessionSummarySink) {
    let _ = GLOBAL_SESSION_SUMMARY_SINK.set(sink);
}

/// The sink the transcript-ingest pass should emit through, when
/// observability is enabled and configured. `None` means "emit nothing" —
/// records still land in `activity.db` exactly as they did before #8757.
#[must_use]
pub fn global_session_summary_sink() -> Option<&'static SessionSummarySink> {
    GLOBAL_SESSION_SUMMARY_SINK.get()
}
