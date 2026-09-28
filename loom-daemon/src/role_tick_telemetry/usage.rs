//! Role-runner tick token usage as `loom.runtime.usage` spans (Issue #9303).
//!
//! A tick's root `loom.role_attempt` is finished before its tokens are known,
//! so after the tick's transcript scan (the same pass that fills
//! `role_tick.outcome`'s `tokens_by_model`):
//!
//! - [`journal_execution`] journals one usage span per model with
//!   `loom.usage.scope=execution` under the tick's own root, in the tick's
//!   trace journal — every tick with known usage;
//! - [`attempt_usage`] adds `scope=attempt` children under the tick's story
//!   span **only when the tick stitched exactly one target**. With several
//!   targets (e.g. a Champion pass merging five PRs) there is no honest way to
//!   split the tokens between them, so the usage stays in the tick trace only.

use std::path::Path;

use chrono::{DateTime, Utc};

use crate::observability::lifecycle::RoleTrace;
use crate::observability::runtime_usage::cost::Pricing;
use crate::observability::runtime_usage::spans::{append_new, model_usage_spans, UsageScope};
use crate::script_helpers::sweep_experiment::ModelUsageTotals;
use crate::telemetry::trace::journal::Journal;
use crate::telemetry::trace::store::TraceStore;
use crate::telemetry::trace::{SpanRecord, TraceAttributes};

/// Journal the tick's execution-scoped usage. Best-effort: logs, never fails.
pub fn journal_execution(
    root: &Path,
    trace: &RoleTrace,
    role: &str,
    ended_at: DateTime<Utc>,
    runtime: Option<&str>,
    rows: &[ModelUsageTotals],
) {
    let spans = execution_usage(trace, role, ended_at, runtime, rows, &Pricing::active());
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
    rows: &[ModelUsageTotals],
    pricing: &Pricing<'_>,
) -> Vec<SpanRecord> {
    let mut common = TraceAttributes::new();
    common.insert("loom.role".into(), role.to_string());
    common.insert("loom.sweep_id".into(), trace.execution.clone());
    if let Some(runtime) = runtime {
        common.insert("loom.runtime".into(), runtime.to_string());
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

/// Attempt-scoped usage under the tick's one story span, or nothing when the
/// tick stitched zero or several targets, or its usage is unknown. Pure.
#[must_use]
pub fn attempt_usage(
    story_spans: &[SpanRecord],
    rows: Option<&[ModelUsageTotals]>,
    pricing: &Pricing<'_>,
) -> Vec<SpanRecord> {
    let ([story], Some(rows)) = (story_spans, rows) else {
        return Vec::new();
    };
    let common: TraceAttributes = [
        "loom.role",
        "loom.issue",
        "loom.pr_number",
        "loom.sweep_id",
        "loom.runtime",
    ]
    .into_iter()
    .filter_map(|key| {
        story
            .attributes
            .get(key)
            .map(|value| (key.to_string(), value.clone()))
    })
    .collect();
    model_usage_spans(
        &story.context,
        (story.started_at, story.ended_at),
        rows,
        UsageScope::Attempt,
        &common,
        pricing,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "usage_tests.rs"]
mod tests;
