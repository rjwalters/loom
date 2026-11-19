//! Tests for `loom.runtime.usage_missing` (Issue #14566).

use chrono::Utc;

use super::*;
use crate::telemetry::trace::TraceContext;

const ALL: [UsageMissing; 7] = [
    UsageMissing::NoStepLog,
    UsageMissing::NoSessionId,
    UsageMissing::StoreUnreadable,
    UsageMissing::SessionNotFound,
    UsageMissing::NoUsageTokens,
    UsageMissing::NoAttributableUsage,
    UsageMissing::NoWindow,
];

#[test]
fn every_reason_has_its_own_wire_token() {
    let tokens: std::collections::BTreeSet<&str> = ALL.iter().map(|r| r.as_str()).collect();
    assert_eq!(tokens.len(), ALL.len(), "a reason shares a token: {tokens:?}");
    assert!(tokens.contains("no_session_id") && tokens.contains("session_not_found"));
    assert!(tokens.contains("no_usage_tokens") && tokens.contains("no_step_log"));
}

#[test]
fn a_sweeps_unattributable_reasons_map_and_every_other_reason_is_not_a_gap() {
    use crate::sweep_usage::{REASON_NO_STORE, REASON_NO_TRANSCRIPT, REASON_NO_WINDOW};
    assert_eq!(UsageMissing::from_sweep_reason(REASON_NO_WINDOW), Some(UsageMissing::NoWindow));
    assert_eq!(
        UsageMissing::from_sweep_reason(REASON_NO_STORE),
        Some(UsageMissing::StoreUnreadable)
    );
    assert_eq!(
        UsageMissing::from_sweep_reason(REASON_NO_TRANSCRIPT),
        Some(UsageMissing::NoAttributableUsage)
    );
    // A sweep that never spawned has nothing to measure; a suspect one has rows.
    for other in ["preflight-token-pool", "implausible_input_rate", ""] {
        assert_eq!(UsageMissing::from_sweep_reason(other), None, "{other}");
    }
}

#[test]
fn the_span_carries_the_reason_and_no_counters_and_survives_export_policy() {
    let parent = TraceContext::root(true);
    let at = Utc::now();
    let mut common = TraceAttributes::new();
    common.insert("loom.runtime".into(), "opencode".into());
    common.insert("loom.sweep_id".into(), "step-builder-1-2".into());
    let span =
        span(&parent, (at, at), UsageMissing::SessionNotFound, UsageScope::Execution, &common);

    assert_eq!(span.name, SpanName::RuntimeUsageMissing);
    assert_eq!(span.parent_span_id.as_ref(), Some(&parent.span_id));
    assert_eq!(span.attributes["loom.usage.missing_reason"], "session_not_found");
    assert_eq!(span.attributes["loom.runtime"], "opencode");
    assert!(
        span.attributes
            .keys()
            .all(|k| !k.starts_with("loom.tokens.") && !k.starts_with("gen_ai.")),
        "an unreadable run never carries counters: {:?}",
        span.attributes
    );
    // `loom.runtime` and `loom.sweep_id` ride in from the caller, as on a usage span.
    for key in ["loom.usage.missing_reason", "loom.usage.scope"] {
        assert!(crate::telemetry::ops::OPS_SPAN_ATTRIBUTE_KEYS.contains(&key), "{key}");
    }
    assert_eq!(span.clone().bounded(), span, "survives export policy unchanged");
    let again =
        super::span(&parent, (at, at), UsageMissing::NoStepLog, UsageScope::Execution, &common);
    assert_eq!(again.context.span_id, span.context.span_id, "one span per (parent, scope)");
}
