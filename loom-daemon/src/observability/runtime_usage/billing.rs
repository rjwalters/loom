//! Which launch billed a unit of usage (Issue #10749).
//!
//! The class lives on each launch's `loom.runtime.run` span. A usage span may
//! copy it only when the association is established, never by guessing:
//!
//! - **execution scope**: the per-model totals cover every launch of the
//!   execution and cannot be split between them, so they carry the class all
//!   of those launches agree on, else `unknown` ([`execution`]);
//! - **attempt scope**: the runs in the attempt's own span ancestry (a run
//!   under the attempt, or the run the attempt itself sits under), else the
//!   runs that belong to no attempt and whose interval encloses the attempt's
//!   ([`attempt`]). Those candidates must agree; none, or a disagreement, is
//!   `unknown`. A run under a *different* attempt is never borrowed.

use crate::observability::llm_billing::LlmBilling;
use crate::telemetry::trace::{SpanId, SpanName, SpanRecord};

/// The class of usage aggregated over all of `runs`.
pub(super) fn execution(runs: &[SpanRecord]) -> LlmBilling {
    LlmBilling::agreed(
        runs.iter()
            .map(|run| LlmBilling::of_launch(&run.attributes)),
    )
}

/// The class of `attempt`'s usage. `spans` is every span of the trace the
/// journal holds (for ancestry), `runs` its `loom.runtime.run` spans.
pub(super) fn attempt(
    spans: &[SpanRecord],
    runs: &[SpanRecord],
    attempt: &SpanRecord,
) -> LlmBilling {
    let id = &attempt.context.span_id;
    let related: Vec<&SpanRecord> = runs
        .iter()
        .filter(|run| {
            descends(spans, run, |s| &s.context.span_id == id)
                || descends(spans, attempt, |s| s.context.span_id == run.context.span_id)
        })
        .collect();
    let candidates = if related.is_empty() {
        runs.iter()
            .filter(|run| {
                run.started_at <= attempt.started_at
                    && attempt.ended_at <= run.ended_at
                    && !descends(spans, run, |s| s.name == SpanName::RoleAttempt)
            })
            .collect()
    } else {
        related
    };
    LlmBilling::agreed(
        candidates
            .into_iter()
            .map(|run| LlmBilling::of_launch(&run.attributes)),
    )
}

/// Whether some proper ancestor of `span` (as far as `spans` records the
/// chain) satisfies `matches`.
fn descends(
    spans: &[SpanRecord],
    span: &SpanRecord,
    matches: impl Fn(&SpanRecord) -> bool,
) -> bool {
    let mut parent: Option<&SpanId> = span.parent_span_id.as_ref();
    // Bounded by the span count, so a malformed cyclic journal cannot loop.
    for _ in 0..=spans.len() {
        let Some(id) = parent else {
            return false;
        };
        let Some(ancestor) = spans.iter().find(|s| &s.context.span_id == id) else {
            return false;
        };
        if matches(ancestor) {
            return true;
        }
        parent = ancestor.parent_span_id.as_ref();
    }
    false
}
