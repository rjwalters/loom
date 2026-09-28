//! Exact per-execution token usage joined to the execution's trace (Issue
//! #8908, phase 3 of #8860).
//!
//! A `loom.runtime.run` span closes when its child exits, before anything
//! has totalled what the child spent, and a completed span cannot be amended.
//! The per-sweep usage readers (`usage_source::sweep_tokens_by_model`:
//! Claude transcripts, OpenCode, Kimi, Codex, Pi) do total it, at the sweep's
//! terminal transition, right beside `lifecycle::finish_execution`. So that
//! is where [`record_execution_usage`] journals one late **`loom.runtime.usage`**
//! span into the execution's own trace journal:
//!
//! - parented to the execution's last completed `loom.runtime.run` span
//!   (a `worker_spawn` launch journals one), else to the execution's root
//!   span, and covering the run span's interval (else the sweep window);
//! - **one span per model** ([`spans`], #9204/#9303), each with
//!   `loom.usage.scope=execution`, `loom.sweep_id`, `loom.model`, that model's
//!   `loom.tokens.{input,output,cache_read,cache_write,total}` (plus the
//!   `cache_write_5m`/`cache_write_1h` split and the `gen_ai.usage.*`
//!   aliases), and a USD estimate from the daemon's rate card ([`cost`]).
//!   `input` is uncached input and `total` is the four counters added
//!   together. Summed over the spans, the counters equal the execution's old
//!   single-span totals;
//! - **only when usage is known**: a reader that found nothing (`None`)
//!   journals no span, so unknown never reads as zero (nor does an empty row
//!   set: there is no model to name), while a model row's measured-zero
//!   counter is exported as `"0"`.
//!
//! The same span shape records one role attempt's usage with
//! `loom.usage.scope=attempt`: `loom-daemon usage-record` for in-session
//! subagents ([`record`]) and single-target role-runner ticks
//! (`role_tick_telemetry::usage`).
//!
//! Only counters are exported: no prompt, tool argument or transcript text.
//! The span drains to the OTLP queue with every other journalled span.
//!
//! [`join`] is the other half: it lets the transcript-ingest pass stamp a
//! `session.summary` log with the same execution's trace context, so the log
//! and the usage span share one trace id in SigNoz.

pub mod cost;
pub mod join;
pub mod record;
pub mod spans;

use std::path::Path;

use chrono::{DateTime, Utc};

use crate::script_helpers::sweep_experiment::ModelUsageTotals;
use crate::telemetry::trace::journal::Journal;
use crate::telemetry::trace::store::TraceStore;
use crate::telemetry::trace::{SpanName, SpanRecord, TraceAttributes};

/// One execution's token totals, in Claude's disjoint vocabulary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
}

impl TokenUsage {
    /// The totals over every per-model row a usage reader returned.
    #[must_use]
    pub fn from_models(rows: &[ModelUsageTotals]) -> Self {
        rows.iter().fold(Self::default(), |sum, row| Self {
            input: sum.input.saturating_add(row.input),
            output: sum.output.saturating_add(row.output),
            cache_read: sum.cache_read.saturating_add(row.cache_read),
            cache_write: sum
                .cache_write
                .saturating_add(row.cache_write_5m)
                .saturating_add(row.cache_write_1h),
        })
    }

    /// Every counter added together.
    #[must_use]
    pub fn total(&self) -> i64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
    }

    /// The `loom.tokens.*` span attributes.
    #[must_use]
    pub fn attributes(&self) -> TraceAttributes {
        [
            ("loom.tokens.input", self.input),
            ("loom.tokens.output", self.output),
            ("loom.tokens.cache_read", self.cache_read),
            ("loom.tokens.cache_write", self.cache_write),
            ("loom.tokens.total", self.total()),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
    }
}

/// Journal `execution`'s per-model usage spans, when tracing is on and usage
/// is known. Call before `lifecycle::finish_execution`, which lets the journal
/// retire.
pub fn record_execution_usage(
    root: &Path,
    execution: &str,
    window: (DateTime<Utc>, DateTime<Utc>),
    tokens_by_model: Option<&[ModelUsageTotals]>,
    runtime: Option<&str>,
) {
    if !super::tracing::enabled(root) {
        return;
    }
    if let Err(error) = journal_usage(root, execution, window, tokens_by_model, runtime) {
        log::warn!("observability: runtime usage span not journalled: {error}");
    }
}

/// The sweep terminal's one call (Issue #8908): journal the per-model usage
/// spans (`scope=execution`) from the per-model totals the outcome record
/// already carries, and close the session join entry [`join::open`] wrote at
/// dispatch. Call before `lifecycle::finish_execution`.
pub fn finish_sweep(
    root: &Path,
    execution: &str,
    started_at: Option<DateTime<Utc>>,
    tokens_by_model: Option<&[ModelUsageTotals]>,
    runtime: Option<&str>,
) {
    let now = Utc::now();
    let window = (started_at.unwrap_or(now), now);
    record_execution_usage(root, execution, window, tokens_by_model, runtime);
    join::close(root, execution, now);
}

/// [`record_execution_usage`] without the enablement check. Returns the
/// journalled spans — none when usage is unknown or the execution has no
/// persisted trace context.
pub fn journal_usage(
    root: &Path,
    execution: &str,
    window: (DateTime<Utc>, DateTime<Utc>),
    tokens_by_model: Option<&[ModelUsageTotals]>,
    runtime: Option<&str>,
) -> anyhow::Result<Vec<SpanRecord>> {
    let Some(rows) = tokens_by_model.filter(|rows| !rows.is_empty()) else {
        return Ok(Vec::new());
    };
    let store = TraceStore::new(root);
    let path = store.path(root, execution);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let saved = TraceStore::load(&path)?;
    let journal = Journal::for_context(&path);
    let run = journal
        .completed()?
        .into_iter()
        .filter(|span| {
            span.name == SpanName::RuntimeRun && span.context.trace_id == saved.context.trace_id
        })
        .max_by_key(|span| span.ended_at);
    let (parent, started_at, ended_at) = match &run {
        Some(run) => (&run.context, run.started_at, run.ended_at),
        None => (&saved.context, window.0, window.1),
    };
    let mut common = TraceAttributes::new();
    common.insert("loom.sweep_id".into(), execution.to_string());
    if let Some(runtime) = runtime.filter(|r| !r.is_empty()) {
        common.insert("loom.runtime".into(), runtime.to_string());
    }
    let spans = spans::model_usage_spans(
        parent,
        (started_at, ended_at),
        rows,
        spans::UsageScope::Execution,
        &common,
        &cost::Pricing::active(),
    );
    spans::append_new(&journal, spans)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "runtime_usage/tests.rs"]
mod tests;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "runtime_usage/model_tests.rs"]
mod model_tests;
