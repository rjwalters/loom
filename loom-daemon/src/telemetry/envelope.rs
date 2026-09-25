use super::{TelemetryRecord, CURRENT_SCHEMA_VERSION};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The versioned wrapper every telemetry record is emitted inside. Carries the
/// [`schema_version`](Self::schema_version) a mixed-version fleet's backend gates
/// on, plus host-identifying context shared by every record kind, and the tagged
/// [`record`](Self::record) payload itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelemetryEnvelope {
    /// Wire-schema version — [`CURRENT_SCHEMA_VERSION`] for a freshly constructed
    /// envelope. A `#[serde(default)]` is intentionally NOT applied: an envelope
    /// with no `schema_version` on the wire is a bug the backend should see,
    /// not silently coerce to version 0.
    pub schema_version: u32,
    /// When the emitting daemon produced this envelope.
    pub emitted_at: DateTime<Utc>,
    /// Stable identifier for the emitting host (e.g. hostname or a configured
    /// fleet host id). Populated by the exporter (#4705); opaque to the schema.
    pub host_id: String,
    /// The record payload — internally tagged on a `kind` discriminant so it
    /// serializes to a single flat object (see [`TelemetryRecord`]).
    pub record: TelemetryRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_context: Option<super::trace::TraceContext>,
}

impl TelemetryEnvelope {
    /// Wrap `record` in an envelope stamped with [`CURRENT_SCHEMA_VERSION`] and
    /// the current time. `host_id` identifies the emitting host.
    #[must_use]
    pub fn new(host_id: impl Into<String>, record: TelemetryRecord) -> Self {
        TelemetryEnvelope {
            schema_version: match record {
                TelemetryRecord::SweepIdentity(_) => 4,
                TelemetryRecord::Span(_) => 3,
                // Issue #8757: a new record kind, gated like `trace.span`
                // (3) and `sweep.identity` (4) — only session-summary
                // envelopes carry 5, so every existing kind's version is
                // byte-identical to what a pre-#8757 reader expects.
                TelemetryRecord::SessionSummary(_) => 5,
                // Issue #8760: two more new record kinds, gated the same
                // way — only `session.analysis` envelopes carry 6 and only
                // `daemon.event` envelopes carry 7, so every pre-existing
                // kind's version is unchanged for a mixed-version fleet's
                // backend.
                TelemetryRecord::SessionAnalysis(_) => 6,
                TelemetryRecord::DaemonEvent(_) => 7,
                // Issue #8824: the CI record family (`ci.run`, `ci.job`,
                // `ci.duration`) shares one gate — it ships as one unit
                // and only its own envelopes carry 8.
                TelemetryRecord::CiRun(_)
                | TelemetryRecord::CiJob(_)
                | TelemetryRecord::CiDuration(_) => 8,
                // Issue #8825: `ci.job.log` ships as its own unit (phase 2 of
                // the same epic, behind its own `logCaptureEnabled` gate), so
                // it gets its own version — a backend that has not taught
                // itself about free-text log bodies can refuse exactly this
                // kind without also losing the phase-1 CI family at 8.
                TelemetryRecord::CiJobLog(_) => 9,
                _ => CURRENT_SCHEMA_VERSION,
            },
            emitted_at: Utc::now(),
            host_id: host_id.into(),
            record,
            trace_context: None,
        }
    }
}
