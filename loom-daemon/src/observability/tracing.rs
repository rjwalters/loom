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
/// An `issue` execution joins that issue's D32 v1 story trace (#9037, #9068)
/// when the checkout's GitHub `origin` resolves to a `repo_id`; otherwise it
/// is its own root. There is no name-derived fallback.
pub fn prepare_child(command: &mut Command, root: &Path, execution: &str, issue: Option<u32>) {
    command
        .env_remove(TRACEPARENT_ENV)
        .env_remove(CONTEXT_FILE_ENV);
    if !enabled(root) {
        return;
    }
    let story = issue.and_then(|issue| resolve_story(root, issue));
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

/// The D32 v1 story of `issue` in the checkout at `root`, or `None` when the
/// `origin` is not a GitHub repo or its `repo_id` cannot be resolved (warned
/// once per repo by [`crate::telemetry::repo_identity`]).
#[must_use]
pub fn resolve_story(root: &Path, issue: u32) -> Option<StoryRef> {
    let slug = crate::release_resolve::host::repo_slug(root)?;
    let identity = crate::telemetry::repo_identity::resolve(&slug)?;
    let context = match crate::telemetry::trace::story_context(identity.id, issue) {
        Ok(context) => context,
        Err(error) => {
            log::warn!("observability: no story trace for {slug}#{issue}: {error}");
            return None;
        }
    };
    Some(StoryRef {
        // Lowercased so `loom.repo` agrees across hosts whose origins differ
        // only in case; `story` carries GitHub's own spelling.
        repo: slug.to_ascii_lowercase(),
        story: format!("{}#{issue}", identity.full_name),
        issue,
        context,
    })
}

/// The issue an execution's story trace belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoryRef {
    pub repo: String,
    pub issue: u32,
    /// `owner/repo#n` at resolution time (`loom.story`, `Loom-Story:`).
    pub story: String,
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
