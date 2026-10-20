//! Role-runner tick token usage as `loom.runtime.usage` spans (Issue #9303).
//!
//! A tick's root `loom.role_attempt` is finished before its tokens are known,
//! so after the tick's transcript scan (the same pass that fills
//! `role_tick.outcome`'s `tokens_by_model`):
//!
//! - [`journal_execution`] journals one usage span per model with
//!   `loom.usage.scope=execution` under the tick's own root, in the tick's
//!   trace journal — every tick with known usage.

use std::path::Path;

use chrono::{DateTime, Utc};

use crate::observability::lifecycle::RoleTrace;
use crate::observability::llm_billing::LlmBilling;
use crate::observability::runtime_usage::cost::Pricing;
use crate::observability::runtime_usage::spans::{append_new, model_usage_spans, UsageScope};
use crate::script_helpers::sweep_experiment::ModelUsageTotals;
use crate::telemetry::trace::journal::Journal;
use crate::telemetry::trace::store::TraceStore;
use crate::telemetry::trace::{SpanRecord, TraceAttributes};

/// The `loom.runtime` a tick's usage spans carry (2am#4393): the resolved
/// runtime when one is known, else `claude` when the tokens came from Claude
/// Code's own transcripts. Every non-Claude source is selected BY a named
/// runtime, so a transcript-sourced tick with no name is a Claude tick; before
/// this its usage spans left `loom.runtime` empty and read as model-only.
#[must_use]
pub fn span_runtime(
    runtime: Option<&str>,
    source: crate::usage_source::UsageSource,
) -> Option<&str> {
    runtime.filter(|r| !r.trim().is_empty()).or_else(|| {
        (source == crate::usage_source::UsageSource::ClaudeTranscripts).then_some("claude")
    })
}

/// Journal the tick's execution-scoped usage. Best-effort: logs, never fails.
pub fn journal_execution(
    root: &Path,
    trace: &RoleTrace,
    role: &str,
    ended_at: DateTime<Utc>,
    runtime: Option<&str>,
    billing: Option<&LlmBilling>,
    rows: &[ModelUsageTotals],
) {
    let spans = execution_usage(trace, role, ended_at, runtime, billing, rows, &Pricing::active());
    let store = TraceStore::new(root);
    let journal = Journal::for_context(&store.path(root, &trace.execution));
    if let Err(error) = append_new(&journal, spans) {
        log::warn!("role_tick_telemetry: {role} tick usage not journalled (#9303): {error}");
    }
}

/// The tick's execution-scoped usage spans, children of its root. Pure.
#[must_use]
pub fn execution_usage(
    trace: &RoleTrace,
    role: &str,
    ended_at: DateTime<Utc>,
    runtime: Option<&str>,
    billing: Option<&LlmBilling>,
    rows: &[ModelUsageTotals],
    pricing: &Pricing<'_>,
) -> Vec<SpanRecord> {
    let mut common = TraceAttributes::new();
    common.insert("loom.role".into(), role.to_string());
    common.insert("loom.sweep_id".into(), trace.execution.clone());
    if let Some(runtime) = runtime {
        common.insert("loom.runtime".into(), runtime.to_string());
    }
    if let Some(billing) = billing {
        billing.stamp(&mut common);
    }
    model_usage_spans(
        &trace.context,
        (trace.started_at, ended_at),
        rows,
        UsageScope::Execution,
        &common,
        pricing,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "usage_tests.rs"]
mod tests;
