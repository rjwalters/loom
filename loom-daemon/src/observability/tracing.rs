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
///
/// An `issue` execution joins that issue's story trace (#9037) when the
/// checkout's GitHub `origin` names the repo; otherwise it is its own root.
pub fn prepare_child(command: &mut Command, root: &Path, execution: &str, issue: Option<u32>) {
    command
        .env_remove(TRACEPARENT_ENV)
        .env_remove(CONTEXT_FILE_ENV);
    if !enabled(root) {
        return;
    }
    let story = issue.and_then(|issue| {
        crate::release_resolve::host::repo_slug(root).map(|repo| StoryRef {
            context: crate::telemetry::trace::story_context(&repo, issue),
            repo,
            issue,
        })
    });
    let store = TraceStore::new(root);
    match store.load_or_create_story(root, execution, story.as_ref().map(|s| &s.context)) {
        Ok(saved) => {
            command.env(TRACEPARENT_ENV, saved.context.traceparent());
            command.env(CONTEXT_FILE_ENV, store.path(root, execution));
            super::lifecycle::prepare_execution(command, root, execution, story.as_ref());
        }
        Err(error) => {
            log::warn!("observability: trace context unavailable; child is untraced: {error}")
        }
    }
}

/// The issue an execution's story trace belongs to.
pub struct StoryRef {
    pub repo: String,
    pub issue: u32,
    pub context: crate::telemetry::trace::TraceContext,
}

/// The OTLP signal an OTLP-only record kind belongs to, or `None` for a kind
/// the native HTTPS backend also accepts. Spans and `metric.points` (#8860)
/// never reach native ingest.
///
/// Both halves are read off the kind's own registry row in
/// `telemetry/kinds.rs` (#8921) — `native: false` makes a kind OTLP-only, and
/// its `otlp:` class names the signal — so adding a record kind never edits
/// this function.
pub(super) fn otlp_only_signal(record: &crate::telemetry::TelemetryRecord) -> Option<&'static str> {
    if record.accepted_by_native_ingest() {
        return None;
    }
    record.otlp_class().signal()
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
