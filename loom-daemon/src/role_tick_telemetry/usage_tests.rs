//! Role-tick usage spans (Issue #9303): execution scope under the tick root.

use chrono::TimeZone as _;

use super::*;
use crate::telemetry::trace::TraceContext;

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

/// The tick-level facts a usage span needs: its trace and end time.
struct Facts {
    trace: RoleTrace,
    ended_at: DateTime<Utc>,
}

fn facts() -> Facts {
    let started_at = Utc.timestamp_opt(1_800_000_000, 0).unwrap();
    let execution = crate::observability::lifecycle::role_execution_id("judge", started_at);
    Facts {
        trace: RoleTrace {
            context: TraceContext::derived("execution", &["rjwalters/usage", &execution]),
            execution,
            started_at,
            failure: None,
        },
        ended_at: started_at + chrono::Duration::seconds(60),
    }
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
fn a_metered_backstop_tick_is_api_billed_on_execution_usage() {
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
    for span in &execution {
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

#[test]
fn a_transcript_sourced_tick_reads_as_claude_and_a_named_runtime_wins() {
    use crate::usage_source::UsageSource;
    assert_eq!(span_runtime(None, UsageSource::ClaudeTranscripts), Some("claude"));
    assert_eq!(span_runtime(Some(""), UsageSource::ClaudeTranscripts), Some("claude"));
    assert_eq!(span_runtime(Some("codex"), UsageSource::CodexSessionRollouts), Some("codex"));
    assert_eq!(span_runtime(Some("opencode"), UsageSource::OpenCodeSessionDb), Some("opencode"));
    assert_eq!(span_runtime(None, UsageSource::PiLaunchStream), None, "never guessed");
}
