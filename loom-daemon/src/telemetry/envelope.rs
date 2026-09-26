use super::TelemetryRecord;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The versioned wrapper every telemetry record is emitted inside. Carries the
/// [`schema_version`](Self::schema_version) a mixed-version fleet's backend gates
/// on, plus host-identifying context shared by every record kind, and the tagged
/// [`record`](Self::record) payload itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelemetryEnvelope {
    /// Wire-schema version — [`CURRENT_SCHEMA_VERSION`](super::CURRENT_SCHEMA_VERSION) for a freshly constructed
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
    /// Wrap `record` in an envelope stamped with the record kind's own gate
    /// version (usually [`CURRENT_SCHEMA_VERSION`](super::CURRENT_SCHEMA_VERSION)) and the current time.
    /// `host_id` identifies the emitting host.
    ///
    /// **The per-kind gate is declared with the kind, not here** (#8921). This
    /// used to be a hand-maintained `TelemetryRecord::Kind(_) => N` ladder in
    /// which every new kind claimed "the next number", so two concurrent PRs
    /// adding a kind conflicted on that one line by construction (#8915 vs
    /// #8909, 2026-09-25). The ladder now lives as a `gate:` column in
    /// `telemetry/kinds.rs`, read through
    /// [`TelemetryRecord::schema_version`] — every historical value
    /// (`sweep.identity` 4, `trace.span` 3, …, `queue.snapshot` 11) is
    /// unchanged, and the eight kinds that never pinned one still report
    /// [`CURRENT_SCHEMA_VERSION`](super::CURRENT_SCHEMA_VERSION), including after a future bump of it.
    #[must_use]
    pub fn new(host_id: impl Into<String>, record: TelemetryRecord) -> Self {
        TelemetryEnvelope {
            schema_version: record.schema_version(),
            emitted_at: Utc::now(),
            host_id: host_id.into(),
            record,
            trace_context: None,
        }
    }
}
