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

mod billing;
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

    /// The `(tokens_in, tokens_out)` split `sweep.outcome` reports (Issue
    /// #9443): every input-side counter summed — uncached input, cache reads and
    /// cache writes — against `output` alone.
    ///
    /// Mirrors [`crate::transcript_tokens::sum_sweep_tokens_split`] exactly, so
    /// a per-phase split derived from per-model rows reconciles against the
    /// sweep total that function produces. Negative counters (impossible from a
    /// real reader) clamp to `0` rather than wrapping.
    #[must_use]
    pub fn split(&self) -> (u64, u64) {
        let input = self
            .input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write);
        (u64::try_from(input).unwrap_or(0), u64::try_from(self.output).unwrap_or(0))
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

/// One phase attempt's usage, exactly as `sweep.outcome`'s `phase_durations`
/// carries it (Issue #9443) — the input to [`record_phase_usage`].
#[derive(Debug, Clone, Copy)]
pub struct PhaseUsage<'a> {
    /// The lifecycle phase / role name (`"curator"`, `"builder"`, `"judge"`, …).
    pub role: &'a str,
    /// 1-based attempt index within the sweep for this role.
    pub attempt: u32,
    /// The attempt's sampled window.
    pub window: (DateTime<Utc>, DateTime<Utc>),
    /// The attempt's per-model usage. Empty journals nothing.
    pub rows: &'a [ModelUsageTotals],
}

/// Journal each phase attempt's per-model usage as `scope=attempt`
/// `loom.runtime.usage` spans under the execution's own matching
/// `loom.role_attempt` span (Issue #9443), so the OTLP path carries the same
/// per-phase numbers `sweep.outcome`'s `phase_durations` do and the two cannot
/// disagree about what a phase cost.
///
/// Matching is on the attempt span's own `loom.role` plus, when it carries one,
/// `loom.attempt`; otherwise the role's spans are taken in start order and the
/// Nth matches attempt N. A phase with **no** attempt span in the journal is
/// skipped rather than given a fabricated parent — `sweep.outcome` still reports
/// its usage, and an invented span would claim a role attempt the trace never
/// observed.
///
/// Span ids derive from `(parent, scope, model)`, so a phase whose usage the
/// in-session `usage-record` path already journalled keeps that span: the
/// first writer wins and the counters are never double-counted. Call before
/// `lifecycle::finish_execution`, which lets the journal retire.
pub fn record_phase_usage(
    root: &Path,
    execution: &str,
    phases: &[PhaseUsage<'_>],
    runtime: Option<&str>,
) {
    if !super::tracing::enabled(root) || phases.is_empty() {
        return;
    }
    if let Err(error) = journal_phase_usage(root, execution, phases, runtime) {
        log::warn!("observability: per-phase usage spans not journalled: {error}");
    }
}

/// [`record_phase_usage`] without the enablement check. Returns the journalled
/// spans.
pub fn journal_phase_usage(
    root: &Path,
    execution: &str,
    phases: &[PhaseUsage<'_>],
    runtime: Option<&str>,
) -> anyhow::Result<Vec<SpanRecord>> {
    let store = TraceStore::new(root);
    let path = store.path(root, execution);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let saved = TraceStore::load(&path)?;
    let journal = Journal::for_context(&path);
    let completed: Vec<SpanRecord> = journal
        .completed()?
        .into_iter()
        .filter(|span| span.context.trace_id == saved.context.trace_id)
        .collect();
    let of_name = |name: SpanName| -> Vec<SpanRecord> {
        completed
            .iter()
            .filter(|span| span.name == name)
            .cloned()
            .collect()
    };
    let mut attempts = of_name(SpanName::RoleAttempt);
    attempts.sort_by_key(|span| span.started_at);
    // #10749: the billing class lives on each launch's `loom.runtime.run` span.
    let runs = of_name(SpanName::RuntimeRun);
    // Every span of the trace (open ones too), for attempt/run ancestry.
    let mut trace = completed.clone();
    trace.extend(
        journal
            .active()?
            .into_iter()
            .map(|active| active.record)
            .filter(|span| span.context.trace_id == saved.context.trace_id),
    );
    let pricing = cost::Pricing::active();
    let mut spans = Vec::new();
    for phase in phases {
        if phase.rows.is_empty() {
            continue;
        }
        let Some(attempt) = match_attempt(&attempts, phase.role, phase.attempt) else {
            continue;
        };
        let parent = &attempt.context;
        let mut common = TraceAttributes::new();
        billing::attempt(&trace, &runs, attempt).stamp(&mut common);
        common.insert("loom.sweep_id".into(), execution.to_string());
        common.insert("loom.role".into(), phase.role.to_string());
        common.insert("loom.phase".into(), phase.role.to_string());
        common.insert("loom.attempt".into(), phase.attempt.to_string());
        if let Some(runtime) = runtime.filter(|r| !r.is_empty()) {
            common.insert("loom.runtime".into(), runtime.to_string());
        }
        spans.extend(spans::model_usage_spans(
            parent,
            phase.window,
            phase.rows,
            spans::UsageScope::Attempt,
            &common,
            &pricing,
        ));
    }
    spans::append_new(&journal, spans)
}

/// The `loom.role_attempt` span `attempt` of `role` refers to: the one whose own
/// `loom.attempt` says so, else the `attempt`-th of that role's spans in start
/// order (`attempts` must already be sorted).
fn match_attempt<'a>(
    attempts: &'a [SpanRecord],
    role: &str,
    attempt: u32,
) -> Option<&'a SpanRecord> {
    let of_role: Vec<&SpanRecord> = attempts
        .iter()
        .filter(|span| span.attributes.get("loom.role").is_some_and(|r| r == role))
        .collect();
    let labelled = of_role
        .iter()
        .find(|span| {
            span.attributes
                .get("loom.attempt")
                .is_some_and(|a| a == &attempt.to_string())
        })
        .copied();
    labelled.or_else(|| {
        let index = usize::try_from(attempt).ok()?.checked_sub(1)?;
        of_role.get(index).copied()
    })
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
    let runs: Vec<SpanRecord> = journal
        .completed()?
        .into_iter()
        .filter(|span| {
            span.name == SpanName::RuntimeRun && span.context.trace_id == saved.context.trace_id
        })
        .collect();
    let run = runs.iter().max_by_key(|span| span.ended_at);
    let (parent, started_at, ended_at) = match run {
        Some(run) => (&run.context, run.started_at, run.ended_at),
        None => (&saved.context, window.0, window.1),
    };
    let mut common = TraceAttributes::new();
    common.insert("loom.sweep_id".into(), execution.to_string());
    if let Some(runtime) = runtime.filter(|r| !r.is_empty()) {
        common.insert("loom.runtime".into(), runtime.to_string());
    }
    // #10749: the per-model totals span every launch of the execution, so
    // they carry only a class all launches share (mixed: `unknown`).
    billing::execution(&runs).stamp(&mut common);
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

// `llm.billing` on sweep-scope usage spans (Issue #10749).
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
#[path = "runtime_usage/billing_tests.rs"]
mod billing_tests;

// Billing attribution never borrows an unrelated launch's class (#10749).
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
#[path = "runtime_usage/attribution_tests.rs"]
mod attribution_tests;

// Per-phase usage on the execution's `loom.role_attempt` spans (Issue #9443).
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
#[path = "runtime_usage/phase_tests.rs"]
mod phase_tests;
