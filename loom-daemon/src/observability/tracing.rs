//! Owned process-boundary trace propagation. Export remains independently opt-in.
use super::lifecycle::StepKeys;
use crate::telemetry::trace::store::{
    TraceStore, CONTEXT_FILE_ENV, TRACEPARENT_ENV, W3C_TRACEPARENT_ENV,
};
use std::path::Path;
use std::process::Command;

#[must_use]
pub fn enabled(root: &Path) -> bool {
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
/// Every execution is its own root trace; the `keys` are plain span attributes.
///
/// The context is exported under **both** [`TRACEPARENT_ENV`] (Loom's own
/// namespaced variable) and [`W3C_TRACEPARENT_ENV`] (the standard `TRACEPARENT`,
/// #9215), always with the same value and always together — a third-party
/// harness inside the spawned process (Claude Code's `-p`/Agent-SDK sessions
/// read `TRACEPARENT`; interactive sessions deliberately ignore it) then
/// parents its own spans inside this execution's trace instead of emitting
/// orphan roots. Both are `env_remove`d on the disabled and error paths, so a
/// stale ambient value can never be mistaken for this execution's context.
pub fn prepare_child(command: &mut Command, root: &Path, execution: &str, keys: &StepKeys) {
    command
        .env_remove(TRACEPARENT_ENV)
        .env_remove(W3C_TRACEPARENT_ENV)
        .env_remove(CONTEXT_FILE_ENV);
    if !enabled(root) {
        return;
    }
    let store = TraceStore::new(root);
    match store.load_or_create(root, execution) {
        Ok(saved) => {
            command.env(TRACEPARENT_ENV, saved.context.traceparent());
            command.env(W3C_TRACEPARENT_ENV, saved.context.traceparent());
            command.env(CONTEXT_FILE_ENV, store.path(root, execution));
            super::lifecycle::prepare_execution(command, root, execution, keys);
        }
        Err(error) => {
            log::warn!("observability: trace context unavailable; child is untraced: {error}")
        }
    }
}

/// The checkout's repo as GitHub spells it (#10637), so `loom.repo` agrees
/// across hosts whose origins differ only in case and matches
/// `loom.dispatch.*`. `None` when the `origin` is not a GitHub repo or its
/// identity cannot be resolved (warned once per repo by
/// [`crate::telemetry::repo_identity`]).
#[must_use]
pub fn resolve_repo(root: &Path) -> Option<String> {
    let slug = crate::release_resolve::host::repo_slug(root)?;
    Some(crate::telemetry::repo_identity::resolve(&slug)?.full_name)
}

#[cfg(test)]
mod tests;
