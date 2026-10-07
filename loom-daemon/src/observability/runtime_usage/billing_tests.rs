//! `llm.billing` on sweep-scope usage spans (Issue #10749).

use std::path::Path;

use chrono::{Duration, Utc};

use super::*;
use crate::observability::llm_billing::LlmBilling;
use crate::telemetry::trace::SpanStatus;

fn row() -> ModelUsageTotals {
    ModelUsageTotals {
        model: "glm-5.3-flash".into(),
        input: 10,
        output: 20,
        ..ModelUsageTotals::default()
    }
}

/// A traced execution whose `loom.runtime.run` span carries `billing`.
fn traced(root: &Path, execution: &str, billing: &LlmBilling) {
    let store = TraceStore::new(root);
    let saved = store.load_or_create(root, execution).unwrap();
    let journal = Journal::for_context(&store.path(root, execution));
    let t0 = Utc::now() - Duration::minutes(30);
    let mut attributes = TraceAttributes::new();
    billing.stamp(&mut attributes);
    let run = journal
        .start(
            saved.context.child(),
            Some(&saved.context),
            SpanName::RuntimeRun,
            t0,
            attributes,
        )
        .unwrap();
    journal
        .finish(&run, t0 + Duration::minutes(20), SpanStatus::Ok, TraceAttributes::new())
        .unwrap();
}

#[test]
fn each_billing_class_reaches_the_execution_usage_span() {
    let cases = [
        (
            "subscription",
            LlmBilling::for_runtime("claude", None, false),
            Some("oauth-pool"),
        ),
        (
            "subscription",
            LlmBilling::for_runtime("codex", None, false),
            Some("chatgpt-seat"),
        ),
        (
            "subscription",
            LlmBilling::native(Some("zai-flash"), Some("subscription"), true, "pool"),
            Some("api-key"),
        ),
        (
            "api",
            LlmBilling::native(Some("quick-cerebras"), None, true, "pool"),
            Some("api-key"),
        ),
        ("local", LlmBilling::native(Some("lm"), Some("local"), false, "none"), None),
    ];
    for (i, (billing, class, kind)) in cases.iter().enumerate() {
        let tmp = tempfile::tempdir().unwrap();
        let execution = format!("sweep-{i}");
        traced(tmp.path(), &execution, class);
        let window = (Utc::now() - Duration::hours(1), Utc::now());
        let spans =
            journal_usage(tmp.path(), &execution, window, Some(&[row()]), Some("pi")).unwrap();
        let [span] = spans.as_slice() else {
            panic!("one span: {spans:?}");
        };
        assert_eq!(span.attributes["llm.billing"], *billing);
        assert_eq!(
            span.attributes
                .get("llm.credential.kind")
                .map(String::as_str),
            *kind
        );
        assert_eq!(span.attributes.get("llm.provider.profile"), class.profile.as_ref(),);
        let wire = serde_json::to_string(&span.attributes).unwrap();
        assert!(!wire.contains("/home") && !wire.contains("sk-"), "{wire}");
    }
}

#[test]
fn a_run_without_billing_states_unknown_on_its_usage_span() {
    let tmp = tempfile::tempdir().unwrap();
    let store = TraceStore::new(tmp.path());
    let saved = store.load_or_create(tmp.path(), "sweep-x").unwrap();
    let journal = Journal::for_context(&store.path(tmp.path(), "sweep-x"));
    let t0 = Utc::now() - Duration::minutes(5);
    let run = journal
        .start(
            saved.context.child(),
            Some(&saved.context),
            SpanName::RuntimeRun,
            t0,
            TraceAttributes::new(),
        )
        .unwrap();
    journal
        .finish(&run, t0 + Duration::minutes(1), SpanStatus::Ok, TraceAttributes::new())
        .unwrap();
    let window = (Utc::now() - Duration::hours(1), Utc::now());
    let spans = journal_usage(tmp.path(), "sweep-x", window, Some(&[row()]), None).unwrap();
    assert_eq!(spans[0].attributes["llm.billing"], "unknown");
    assert!(!spans[0].attributes.contains_key("llm.credential.kind"));
}
