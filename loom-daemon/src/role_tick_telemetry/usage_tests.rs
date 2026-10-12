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
            failure: None,
        },
        role: "judge".into(),
        ended_at: started_at + chrono::Duration::seconds(60),
        result: "success".into(),
        runtime: Some("claude".into()),
        model: None,
        tokens_by_model: Some(rows()),
        llm_billing: None,
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
        None,
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
    journal_execution(root, &facts.trace, "judge", facts.ended_at, Some("claude"), None, &rows());
    // A re-emit (same deterministic ids) is skipped.
    journal_execution(root, &facts.trace, "judge", facts.ended_at, Some("claude"), None, &rows());
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

#[test]
fn a_metered_backstop_tick_is_api_billed_on_execution_and_attempt_usage() {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("role-judge.log");
    std::fs::write(
        &log,
        "# LOOM_RUNTIME_PREFERENCE order=claude,codex tier=2 tap=claude:cerebras source=preference backstop=1/4\n",
    )
    .unwrap();
    let billing = crate::role_tick_telemetry::tick_llm_billing(
        None,
        None,
        Some("claude:quick-cerebras"),
        &log,
    );
    assert_eq!(billing.billing, "api");
    assert_eq!(billing.credential_kind.as_deref(), Some("api-key"));
    assert_eq!(billing.profile.as_deref(), Some("quick-cerebras"));

    let facts = facts();
    let execution = execution_usage(
        &facts.trace,
        "judge",
        facts.ended_at,
        Some("claude"),
        Some(&billing),
        &rows(),
        &Pricing::with(None),
    );
    assert!(!execution.is_empty());
    let mut stories = story_spans(&[42]);
    for story in &mut stories {
        billing.stamp(&mut story.attributes);
    }
    let attempt = attempt_usage(&stories, Some(&rows()), &Pricing::with(None));
    for span in execution.iter().chain(&attempt) {
        assert_eq!(span.attributes["llm.billing"], "api");
        assert_eq!(span.attributes["llm.credential.kind"], "api-key");
        assert_eq!(span.attributes["llm.provider.profile"], "quick-cerebras");
    }
}

#[test]
fn a_plain_claude_tick_is_subscription_and_a_launch_record_wins() {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("missing.log");
    let claude = crate::role_tick_telemetry::tick_llm_billing(None, None, None, &log);
    assert_eq!(claude.billing, "subscription");
    assert_eq!(claude.credential_kind.as_deref(), Some("oauth-pool"));
    let codex = crate::role_tick_telemetry::tick_llm_billing(None, Some("codex"), None, &log);
    assert_eq!(codex.credential_kind.as_deref(), Some("chatgpt-seat"));
    let record = crate::launch_record::RuntimeAttribution {
        runtime: "pi".into(),
        provider: None,
        model: None,
        profile: Some("zai-flash".into()),
        llm_billing: Some("subscription".into()),
        llm_credential_kind: Some("api-key".into()),
    };
    let zai = crate::role_tick_telemetry::tick_llm_billing(Some(&record), None, None, &log);
    assert_eq!(zai.billing, "subscription");
    assert_eq!(zai.profile.as_deref(), Some("zai-flash"));
}
