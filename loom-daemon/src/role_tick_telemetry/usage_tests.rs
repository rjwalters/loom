//! Role-tick usage spans (Issue #9303): execution scope under the tick root,
//! attempt scope only under a single stitched story span.

use std::collections::HashMap;

use chrono::TimeZone as _;

use super::*;
use crate::role_tick_telemetry::story::{plan, Resolved, TickFacts};
use crate::telemetry::repo_identity::RepoIdentity;
use crate::telemetry::trace::{SpanName, TraceContext};

fn rows() -> Vec<ModelUsageTotals> {
    let row = |model: &str, n: i64| ModelUsageTotals {
        model: model.into(),
        speed: "standard".into(),
        service_tier: "standard".into(),
        input: n,
        cache_read: n * 10,
        cache_write_5m: n,
        cache_write_1h: n,
        output: n * 2,
    };
    vec![row("claude-opus-5", 100), row("claude-haiku-4-5", 7)]
}

fn facts() -> TickFacts {
    let started_at = Utc.timestamp_opt(1_800_000_000, 0).unwrap();
    let execution = crate::observability::lifecycle::role_execution_id("judge", started_at);
    TickFacts {
        trace: RoleTrace {
            context: TraceContext::derived("execution", &["rjwalters/usage", &execution]),
            execution,
            started_at,
        },
        role: "judge".into(),
        ended_at: started_at + chrono::Duration::seconds(60),
        result: "success".into(),
        runtime: Some("claude".into()),
        model: None,
        tokens_by_model: Some(rows()),
    }
}

fn story_spans(numbers: &[u32]) -> Vec<SpanRecord> {
    let identity = RepoIdentity {
        id: 1_073_994_527,
        full_name: "rjwalters/usage".into(),
    };
    let resolved: HashMap<u32, Resolved> = numbers.iter().map(|n| (*n, Resolved::Issue)).collect();
    plan(&facts(), &identity, &identity.full_name, numbers, &resolved)
}

fn sum(spans: &[SpanRecord]) -> i64 {
    spans
        .iter()
        .map(|s| s.attributes["loom.tokens.total"].parse::<i64>().unwrap())
        .sum()
}

#[test]
fn a_single_stitched_target_gets_attempt_usage_under_its_story_span() {
    let stories = story_spans(&[42]);
    assert_eq!(stories.len(), 1);
    let facts = facts();
    let usage = attempt_usage(&stories, facts.tokens_by_model.as_deref(), &Pricing::with(None));
    assert_eq!(usage.len(), 2, "one per model");
    for span in &usage {
        assert_eq!(span.name, SpanName::RuntimeUsage);
        assert_eq!(span.parent_span_id.as_ref(), Some(&stories[0].context.span_id));
        assert_eq!(span.context.trace_id, stories[0].context.trace_id, "the story's trace");
        assert_eq!(span.attributes["loom.usage.scope"], "attempt");
        assert_eq!(span.attributes["loom.issue"], "42");
        assert_eq!(span.attributes["loom.role"], "judge");
        assert_eq!(span.attributes["loom.sweep_id"], facts.trace.execution);
    }
    let execution = execution_usage(
        &facts.trace,
        "judge",
        facts.ended_at,
        Some("claude"),
        &rows(),
        &Pricing::with(None),
    );
    assert_eq!(sum(&usage), sum(&execution), "the one attempt is the whole tick");
}

#[test]
fn several_targets_or_unknown_usage_get_no_attempt_usage() {
    let facts = facts();
    let two = story_spans(&[42, 43]);
    assert_eq!(two.len(), 2);
    assert!(attempt_usage(&two, facts.tokens_by_model.as_deref(), &Pricing::with(None)).is_empty());
    assert!(attempt_usage(&story_spans(&[42]), None, &Pricing::with(None)).is_empty());
    assert!(attempt_usage(&[], Some(&rows()), &Pricing::with(None)).is_empty());
}

#[test]
fn execution_usage_is_journalled_under_the_tick_root_in_the_tick_journal() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let facts = facts();
    journal_execution(root, &facts.trace, "judge", facts.ended_at, Some("claude"), &rows());
    // A re-emit (same deterministic ids) is skipped.
    journal_execution(root, &facts.trace, "judge", facts.ended_at, Some("claude"), &rows());
    let store = TraceStore::new(root);
    let spans = Journal::for_context(&store.path(root, &facts.trace.execution))
        .completed()
        .unwrap();
    assert_eq!(spans.len(), 2);
    for span in &spans {
        assert_eq!(span.parent_span_id.as_ref(), Some(&facts.trace.context.span_id));
        assert_eq!(span.attributes["loom.usage.scope"], "execution");
        assert_eq!(span.attributes["loom.runtime"], "claude");
    }
}
