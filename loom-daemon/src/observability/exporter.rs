//! The [`Exporter`] trait the sender's drain loop pushes batches through
//! (Epic #4702, Phase 1 — issue #4705). The OTLP exporter (`otlp::OtlpExporter`)
//! is its only implementation since the native JSON-over-HTTPS `/ingest`
//! exporter was removed (#12813).
//!
//! [`Exporter`] is deliberately narrow — "emit a batch, optionally flush" —
//! so [`super::sender`]'s drain loop is generic over `E: Exporter` and never
//! sees a concrete transport.
//!
//! Native `async fn` in a trait (stable since Rust 1.75) is used instead of
//! the `async-trait` crate — nothing here needs a `dyn Exporter` trait
//! object; [`super::sender::spawn_task`] is generic over a single concrete
//! `E: Exporter`, so no vtable/boxing is required.

use crate::telemetry::TelemetryEnvelope;

pub use super::outcome::{BatchOutcome, SignalCounts};

/// A sink [`super::sender`]'s drain loop can push batches of
/// [`TelemetryEnvelope`]s to. Implementations must never panic on a
/// transport failure. The outcome identifies acknowledged prefixes; retryable
/// failures leave the remaining envelopes queued.
pub trait Exporter: Send + Sync {
    /// Push `envelopes` to the sink. `envelopes` is never empty (the caller
    /// only invokes this with a non-empty batch).
    fn emit_batch(
        &self,
        envelopes: &[TelemetryEnvelope],
    ) -> impl std::future::Future<Output = Result<(), ExportError>> + Send;

    /// Detailed acknowledgment for transports with multiple signal requests.
    /// The prefix is removed durably before retrying any remaining envelopes.
    fn emit_batch_outcome(
        &self,
        envelopes: &[TelemetryEnvelope],
    ) -> impl std::future::Future<Output = BatchOutcome> + Send {
        async {
            match self.emit_batch(envelopes).await {
                Ok(()) => BatchOutcome::accepted(envelopes.len()),
                Err(error) => BatchOutcome {
                    error: Some(error),
                    ..BatchOutcome::default()
                },
            }
        }
    }

    /// Best-effort flush of any transport-level buffering. The default no-op
    /// is correct for any sink whose [`Exporter::emit_batch`] call is already a
    /// complete HTTP request/response round trip, i.e. with no internal buffer.
    fn flush(&self) -> impl std::future::Future<Output = Result<(), ExportError>> + Send {
        async { Ok(()) }
    }
}

/// An export failure. [`std::fmt::Display`] **never** includes the ingest
/// key — exporters send it only as an `Authorization: Bearer` header
/// value, which `reqwest::Error`'s `Display` does not print (it surfaces the
/// request URL and failure kind, not headers), and this type's own variants
/// carry no credential material either. This is the AC's "ingest key …
/// absent from logs and error messages" guarantee.
#[derive(Debug)]
pub enum ExportError {
    /// The HTTP request itself failed (connect refused, DNS, TLS, timeout).
    Transport(String),
    /// The sink responded, but with a non-2xx status.
    Rejected { status: u16, body_snippet: String },
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportError::Transport(message) => write!(f, "transport error: {message}"),
            ExportError::Rejected {
                status,
                body_snippet,
            } => {
                write!(f, "sink rejected batch: HTTP {status} — {body_snippet}")
            }
        }
    }
}

impl std::error::Error for ExportError {}
