//! `session.analysis` emission sink (Issue #8760, G3 part 2 of epic #8714).
//!
//! Mirrors [`super::session_summary`]'s process-global registration pattern
//! exactly, for the same reason: the transcript-ingest pass runs on its own
//! background thread, started before the event bus and the observability
//! task exist, so it cannot be handed the collector's `DurableQueue` at
//! construction. [`spawn_task`](super::spawn_task) registers this sink once
//! the shared queue exists; the ingest pass resolves
//! [`global_session_analysis_sink`] at run time.
//!
//! When observability is disabled (the FLAGS-OFF default) nothing is ever
//! registered and [`global_session_analysis_sink`] returns `None` — the
//! ingest pass then skips `session.analysis` emission entirely, at zero
//! cost, exactly like `session.summary`.

use std::sync::Arc;

use crate::telemetry::{SessionAnalysisRecord, TelemetryEnvelope, TelemetryRecord};

/// The exporter-facing half of `session.analysis` emission: wraps one record
/// in a [`TelemetryEnvelope`] stamped with the same host id every other
/// exported record carries, and offers it onto the shared fan-out sink for
/// the configured exporter(s) to drain.
#[derive(Clone)]
pub struct SessionAnalysisSink {
    queue: Arc<dyn super::queue::QueueSink>,
    host_id: String,
}

impl SessionAnalysisSink {
    /// Wrap `queue`/`host_id` (both owned by `super::spawn_task`) in a sink.
    #[must_use]
    pub fn new(queue: Arc<dyn super::queue::QueueSink>, host_id: impl Into<String>) -> Self {
        SessionAnalysisSink {
            queue,
            host_id: host_id.into(),
        }
    }

    /// The host id this sink stamps on every envelope.
    #[must_use]
    pub fn host_id(&self) -> &str {
        &self.host_id
    }

    /// Enqueue one `session.analysis` record. Best-effort like every queue
    /// producer: a persistence failure is logged by the queue and never
    /// propagates into the ingestion pass.
    pub fn push(&self, record: SessionAnalysisRecord) {
        self.queue.offer(TelemetryEnvelope::new(
            self.host_id.clone(),
            TelemetryRecord::SessionAnalysis(record),
        ));
    }
}

/// `IngestOptions` is `Debug` and now holds this sink; `DurableQueue` is not
/// `Debug`, so this names the identity fields and elides the queue.
impl std::fmt::Debug for SessionAnalysisSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionAnalysisSink")
            .field("host_id", &self.host_id)
            .finish_non_exhaustive()
    }
}

static GLOBAL_SESSION_ANALYSIS_SINK: std::sync::OnceLock<SessionAnalysisSink> =
    std::sync::OnceLock::new();

/// Register the process-global sink. Called exactly once, from
/// `super::spawn_task`, after the shared queue exists; a second call is a
/// no-op (the first registration wins), matching
/// [`super::session_summary::register_global_session_summary_sink`]'s
/// contract.
pub fn register_global_session_analysis_sink(sink: SessionAnalysisSink) {
    let _ = GLOBAL_SESSION_ANALYSIS_SINK.set(sink);
}

/// The sink the transcript-ingest pass should emit through, when
/// observability is enabled and configured. `None` means "emit nothing" —
/// records still land in `activity.db` exactly as they did before.
#[must_use]
pub fn global_session_analysis_sink() -> Option<&'static SessionAnalysisSink> {
    GLOBAL_SESSION_ANALYSIS_SINK.get()
}
