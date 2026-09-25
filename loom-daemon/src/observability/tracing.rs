//! Owned process-boundary trace propagation. Export remains independently opt-in.
use crate::telemetry::trace::store::{TraceStore, CONTEXT_FILE_ENV, TRACEPARENT_ENV};
use std::path::Path;
use std::process::Command;

#[must_use]
pub fn enabled(root: &Path) -> bool {
    if !cfg!(feature = "otlp") {
        return false;
    }
    let config = super::read_config(root);
    // Tracing rides the OTLP sink (#4858/#8756): enabled when at least one
    // CONFIGURED exporter is otlp and resolves a valid endpoint of its own.
    super::resolve_enabled(&config)
        && super::resolve_exporters(&config).into_iter().any(|entry| {
            entry.kind == super::ExporterKind::Otlp
                && entry
                    .endpoint
                    .as_deref()
                    .or(super::resolve_endpoint(&config).as_deref())
                    .is_some_and(valid_endpoint)
        })
}

fn valid_endpoint(endpoint: &str) -> bool {
    super::endpoint_policy::valid_otlp_endpoint(endpoint)
        && super::endpoint_policy::reserved_placeholder_host(endpoint).is_none()
}

/// Persist before spawn. A corrupt/busy/full store disables this execution's
/// tracing with a diagnostic; it cannot fail the actual issue dispatch.
pub fn prepare_child(command: &mut Command, root: &Path, execution: &str) {
    command
        .env_remove(TRACEPARENT_ENV)
        .env_remove(CONTEXT_FILE_ENV);
    if !enabled(root) {
        return;
    }
    let store = TraceStore::new(root);
    match store.load_or_create(root, execution) {
        Ok(saved) => {
            command.env(TRACEPARENT_ENV, saved.context.traceparent());
            command.env(CONTEXT_FILE_ENV, store.path(root, execution));
            super::lifecycle::prepare_execution(command, root, execution);
        }
        Err(error) => {
            log::warn!("observability: trace context unavailable; child is untraced: {error}")
        }
    }
}

/// The OTLP signal an OTLP-only record kind belongs to, or `None` for a kind
/// the native HTTPS backend also accepts. Spans and `metric.points` (#8860)
/// never reach native ingest.
pub(super) fn otlp_only_signal(record: &crate::telemetry::TelemetryRecord) -> Option<&'static str> {
    match record {
        crate::telemetry::TelemetryRecord::Span(_) => Some("spans"),
        crate::telemetry::TelemetryRecord::MetricPoints(_) => Some("metrics"),
        _ => None,
    }
}

/// Existing native ingest accepts lifecycle records, never OTLP-only payloads
/// ([`otlp_only_signal`]). Also strips the additive context fields for older
/// native backend versions.
pub(super) fn native_envelopes(
    envelopes: &[crate::telemetry::TelemetryEnvelope],
) -> Vec<crate::telemetry::TelemetryEnvelope> {
    envelopes
        .iter()
        .filter(|e| otlp_only_signal(&e.record).is_none())
        .cloned()
        .map(|mut envelope| {
            envelope.trace_context = None;
            envelope
        })
        .collect()
}

#[cfg(test)]
mod tests;
