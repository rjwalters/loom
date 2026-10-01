//! `session.output` emission sink (Issue #9764).
//!
//! Mirrors [`super::session_summary`]'s process-global registration pattern
//! exactly, for the same reason: the live-output emitter
//! ([`crate::activity::transcript_output`]) runs on its own background
//! thread, started before the event bus and the observability task exist
//! (`daemon_service.rs` boots the activity maintenance threads at the top of
//! its setup and `observability::spawn_task` much later), so it cannot be
//! handed the collector's `DurableQueue` at construction. `spawn_task`
//! registers this sink once the shared queue exists; the emitter resolves
//! [`global_session_output_sink`] at run time, on every tick — so a tick
//! before observability finishes booting simply emits nothing, and a host
//! with `session.output` disabled never registers a sink at all.
//!
//! Sharing the fan-out queue — rather than opening a second `DurableQueue` —
//! is load-bearing; see `session_summary`'s module doc.
//!
//! When observability is disabled (the FLAGS-OFF default) nothing is ever
//! registered and [`global_session_output_sink`] returns `None` — the emitter
//! then skips emission entirely, at zero cost, exactly like `session.summary`.

use std::sync::Arc;

use crate::telemetry::kinds::session_output::SessionOutputRecord;
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};

/// The exporter-facing half of `session.output` emission: wraps one record
/// in a [`TelemetryEnvelope`] stamped with the same host id every other
/// exported record carries, and offers it onto the shared fan-out sink for
/// the configured exporter(s) to drain.
#[derive(Clone)]
pub struct SessionOutputSink {
    queue: Arc<dyn super::queue::QueueSink>,
    host_id: String,
}

impl SessionOutputSink {
    /// Wrap `queue`/`host_id` (both owned by `super::spawn_task`) in a sink.
    #[must_use]
    pub fn new(queue: Arc<dyn super::queue::QueueSink>, host_id: impl Into<String>) -> Self {
        SessionOutputSink {
            queue,
            host_id: host_id.into(),
        }
    }

    /// The host id this sink stamps on every envelope.
    #[must_use]
    pub fn host_id(&self) -> &str {
        &self.host_id
    }

    /// Enqueue one `session.output` record. Best-effort like every queue
    /// producer: a persistence failure is logged by the queue and never
    /// propagates into the emitter's tick.
    pub fn push(&self, record: SessionOutputRecord) {
        self.push_traced(record, None);
    }

    /// [`Self::push`], joined to the trace of the execution that ran the
    /// session when one is known (Issue #8908,
    /// [`super::runtime_usage::join`]): the envelope's `trace_context`
    /// becomes the OTLP log record's trace and span id.
    pub fn push_traced(
        &self,
        record: SessionOutputRecord,
        trace_context: Option<crate::telemetry::trace::TraceContext>,
    ) {
        let mut envelope =
            TelemetryEnvelope::new(self.host_id.clone(), TelemetryRecord::SessionOutput(record));
        envelope.trace_context = trace_context;
        self.queue.offer(envelope);
    }
}

/// `EmitterState` is `Debug` and holds this sink; `DurableQueue` is not
/// `Debug`, so this names the identity field and elides the queue.
impl std::fmt::Debug for SessionOutputSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionOutputSink")
            .field("host_id", &self.host_id)
            .finish_non_exhaustive()
    }
}

static GLOBAL_SESSION_OUTPUT_SINK: std::sync::OnceLock<SessionOutputSink> =
    std::sync::OnceLock::new();

/// Register the process-global sink. Called exactly once, from
/// `super::spawn_task`, after the shared queue exists; a second call is a
/// no-op (the first registration wins), matching
/// [`super::session_summary::register_global_session_summary_sink`]'s
/// contract.
pub fn register_global_session_output_sink(sink: SessionOutputSink) {
    let _ = GLOBAL_SESSION_OUTPUT_SINK.set(sink);
}

/// The sink the live-output emitter should emit through, when observability
/// is enabled and configured. `None` means "emit nothing" — transcripts are
/// still ingested into `activity.db` exactly as before #9764.
#[must_use]
pub fn global_session_output_sink() -> Option<&'static SessionOutputSink> {
    GLOBAL_SESSION_OUTPUT_SINK.get()
}
