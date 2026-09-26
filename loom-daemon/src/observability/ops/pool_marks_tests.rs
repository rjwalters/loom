//! Tests for reason-classified account marks and pool-hold spans (#8931).
//! Seam-level tests (a real mark written through each daemon seam) live next
//! to each seam; these pin the vocabulary and the helpers they share.

use chrono::{Duration, Utc};

use super::*;
use crate::api_keys_pool::ingest::LaunchFeedback;
use crate::observability::ops::capture::capture;
use crate::telemetry::ops::{MetricValue, OPS_METRIC_LABEL_KEYS, OPS_SPAN_ATTRIBUTE_KEYS};

#[test]
fn reasons_are_a_closed_set_of_distinct_snake_case_labels() {
    let labels: Vec<&str> = MarkReason::ALL.iter().map(|r| r.as_str()).collect();
    assert_eq!(
        labels,
        [
            "rate_limited",
            "exhausted",
            "session_limit",
            "model_credits",
            "credential",
            "transient"
        ]
    );
}

#[test]
fn codex_classifications_map_to_a_reason_only_when_health_records_a_hold() {
    use TerminalClassification as T;
    assert_eq!(MarkReason::from_codex(T::TokenExhausted), Some(MarkReason::Exhausted));
    assert_eq!(MarkReason::from_codex(T::ModelCreditsExhausted), Some(MarkReason::ModelCredits));
    assert_eq!(MarkReason::from_codex(T::SessionLimit), Some(MarkReason::SessionLimit));
    assert_eq!(MarkReason::from_codex(T::TokenExpired), Some(MarkReason::Credential));
    assert_eq!(MarkReason::from_codex(T::Recoverable), Some(MarkReason::Transient));
    for none in [
        T::Success,
        T::Timeout,
        T::Fatal,
        T::CwdDeleted,
        T::ModelRefusal,
    ] {
        assert_eq!(MarkReason::from_codex(none), None, "{none:?}");
    }
}

#[test]
fn api_key_and_claude_classifications_map_to_reasons() {
    assert_eq!(MarkReason::from_api_key(Classification::RateLimited), MarkReason::RateLimited);
    assert_eq!(MarkReason::from_api_key(Classification::Exhausted), MarkReason::Exhausted);
    assert_eq!(
        MarkReason::from_api_key(Classification::CredentialFailure),
        MarkReason::Credential
    );
    for (signature, reason) in [
        ("rate-limited", MarkReason::RateLimited),
        ("rate-limit-abort", MarkReason::RateLimited),
        ("model-credits-exhausted", MarkReason::ModelCredits),
        ("model-limit", MarkReason::ModelCredits),
    ] {
        assert_eq!(MarkReason::from_claude_signature(signature), Some(reason));
    }
    assert_eq!(MarkReason::from_claude_signature("hit your limit (free text)"), None);
}

#[test]
fn the_provider_label_never_carries_free_text() {
    assert_eq!(provider_label("zai"), "zai");
    assert_eq!(provider_label("kimi-for_coding2"), "kimi-for_coding2");
    for hostile in [
        "",
        "Alice",
        "zai/alice",
        "zai alice",
        "sk-ant-oat01-SECRET",
        &"x".repeat(33),
    ] {
        assert_eq!(provider_label(hostile), "other", "{hostile:?}");
    }
}

#[test]
fn a_mark_is_one_delta_point_with_only_provider_and_reason() {
    let point = mark_point("codex", MarkReason::SessionLimit);
    assert_eq!(point.name, MetricName::PoolAccountMarks);
    assert_eq!(point.name.as_str(), "loom.pool.account_marks");
    assert_eq!(point.name.kind(), crate::telemetry::ops::MetricKind::DeltaCounter);
    assert_eq!(point.value, MetricValue::Int(1));
    let keys: Vec<&str> = point.labels.keys().map(String::as_str).collect();
    assert_eq!(keys, ["provider", "reason"]);
    assert!(keys.iter().all(|k| OPS_METRIC_LABEL_KEYS.contains(k)));
    assert_eq!(point.labels["reason"], "session_limit");
}

fn feedback(classification: Classification, marked: bool) -> LaunchFeedback {
    LaunchFeedback {
        provider: "zai".into(),
        account: "alice-SECRET-ACCOUNT".into(),
        model_class: None,
        classification,
        mark: marked.then(|| crate::api_keys_pool::bad_marks::BadMark {
            name: "alice-SECRET-ACCOUNT".into(),
            reason: "planted log text: 429 for alice".into(),
            marked_at: 1,
            resets_at: Some(61),
            model_class: None,
        }),
        detail: "api-keys pool: bad-marked zai/alice-SECRET-ACCOUNT".into(),
    }
}

#[test]
fn an_api_key_mark_emits_one_point_and_no_account_or_log_text() {
    let ((), captured) = capture(|| record_api_key(&feedback(Classification::RateLimited, true)));
    assert_eq!(captured.metrics.len(), 1);
    let point = &captured.metrics[0];
    assert_eq!(point.labels["provider"], "zai");
    assert_eq!(point.labels["reason"], "rate_limited");
    let wire = serde_json::to_string(point).unwrap();
    for leaked in ["alice", "SECRET", "429", "planted"] {
        assert!(!wire.contains(leaked), "{leaked} leaked into {wire}");
    }
}

#[test]
fn an_api_key_credential_failure_writes_no_mark_and_emits_nothing() {
    let ((), captured) =
        capture(|| record_api_key(&feedback(Classification::CredentialFailure, false)));
    assert!(captured.metrics.is_empty());
}

#[test]
fn a_codex_outcome_with_no_hold_emits_nothing() {
    let ((), captured) = capture(|| record_codex(TerminalClassification::Success));
    assert!(captured.metrics.is_empty());
    let ((), captured) = capture(|| record_codex(TerminalClassification::TokenExhausted));
    assert_eq!(captured.metrics[0].labels["reason"], "exhausted");
    assert_eq!(captured.metrics[0].labels["provider"], "codex");
}

#[test]
fn a_hold_span_covers_arm_to_clear_with_allowlisted_attributes() {
    let since = Utc::now() - Duration::seconds(600);
    let cleared = since + Duration::seconds(540);
    let span = hold_span(since, cleared, true, 4);
    assert_eq!(span.name.as_str(), "loom.pool.hold");
    assert_eq!((span.started_at, span.ended_at), (since, cleared));
    assert!(span.parent_span_id.is_none() && span.context.sampled());
    assert!(span.validate().is_ok());
    assert_eq!(span.attributes["loom.pool.hold.post_mortem"], "true");
    assert_eq!(span.attributes["loom.pool.hold.accounts"], "4");
    assert!(span.attributes.keys().all(|k| {
        OPS_SPAN_ATTRIBUTE_KEYS.contains(&k.as_str())
            || crate::telemetry::trace::provenance::KEYS.contains(&k.as_str())
    }));
    assert_eq!(span.attributes["loom.daemon.version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(span.clone().bounded().attributes, span.attributes, "survives export policy");
}
