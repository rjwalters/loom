#![allow(clippy::unwrap_used)]
use super::*;
use crate::telemetry::trace::{SpanName, SpanRecord, TraceAttributes, TraceContext};
use chrono::Utc;

fn span(context: TraceContext) -> SpanRecord {
    let now = Utc::now();
    SpanRecord {
        context,
        parent_span_id: None,
        name: SpanName::Sweep,
        started_at: now,
        ended_at: now,
        status: SpanStatus::Ok,
        attributes: TraceAttributes::new(),
        events: vec![],
        links: vec![],
    }
}

#[test]
fn trace_wire_has_hex_ids_and_preserves_parent_and_log_relationship() {
    let root = TraceContext::root(true);
    let child = root.child();
    let mut child_span = span(child.clone());
    child_span.name = SpanName::RoleAttempt;
    child_span.parent_span_id = Some(root.span_id.clone());
    let request = build_traces_request(&[
        TelemetryEnvelope::new("host", TelemetryRecord::Span(span(root.clone()))),
        TelemetryEnvelope::new("host", TelemetryRecord::Span(child_span)),
    ])
    .unwrap();
    let json = serde_json::to_value(&request).unwrap();
    let spans = &json["resourceSpans"][0]["scopeSpans"][0]["spans"];
    assert_eq!(spans[0]["traceId"], root.trace_id.as_str());
    assert_eq!(spans[1]["parentSpanId"], root.span_id.as_str());
    assert_eq!(spans[1]["spanId"], child.span_id.as_str());
    let mut log = TelemetryEnvelope::new(
        "host",
        TelemetryRecord::SweepStarted(crate::telemetry::SweepStartedRecord {
            story_points: None,
            repo: "test/fixture".into(),
            visibility: crate::telemetry::RepoVisibility::Private,
            issue: 18,
            sweep_id: "fixture".into(),
            started_at: Utc::now(),
            model: None,
            effort: None,
            runtime: None,
        }),
    );
    log.trace_context = Some(child.clone());
    let logs = super::super::mapping::build_logs_request(&[log]).unwrap();
    let log = &logs.resource_logs[0].scope_logs[0].log_records[0];
    assert_eq!(log.trace_id, root.trace_id.bytes());
    assert_eq!(log.span_id, child.span_id.bytes());
}

#[test]
fn unsampled_and_invalid_spans_are_not_fabricated() {
    let unsampled =
        TelemetryEnvelope::new("host", TelemetryRecord::Span(span(TraceContext::root(false))));
    assert!(build_traces_request(&[unsampled]).is_none());
    let mut invalid = span(TraceContext::root(true));
    invalid.parent_span_id = Some(invalid.context.span_id.clone());
    assert!(build_traces_request(&[TelemetryEnvelope::new(
        "host",
        TelemetryRecord::Span(invalid)
    )])
    .is_none());
}

#[test]
fn unrepresentable_timestamps_are_dropped_instead_of_rewritten_to_epoch() {
    for timestamp in ["1969-12-31T23:59:59Z", "9999-01-01T00:00:00Z"] {
        let mut invalid = span(TraceContext::root(true));
        invalid.started_at = timestamp.parse().unwrap();
        invalid.ended_at = invalid.started_at;
        assert!(invalid.validate().is_err());
        assert!(build_traces_request(&[TelemetryEnvelope::new(
            "host",
            TelemetryRecord::Span(invalid)
        )])
        .is_none());
    }
}

#[test]
fn traces_resource_carries_the_build_service_version() {
    let request = build_traces_request(&[TelemetryEnvelope::new(
        "host",
        TelemetryRecord::Span(span(TraceContext::root(true))),
    )])
    .unwrap();
    let json = serde_json::to_value(&request).unwrap();
    let attributes = json["resourceSpans"][0]["resource"]["attributes"]
        .as_array()
        .unwrap();
    let version = attributes
        .iter()
        .find(|kv| kv["key"] == "service.version")
        .map(|kv| kv["value"]["stringValue"].clone());
    assert_eq!(version, Some(serde_json::json!(env!("CARGO_PKG_VERSION"))));
}

/// #9985: an `invoke github` span reaches the OTLP wire with its fixed name,
/// derived IDs, every `github.*` attribute and `service.name=loom-daemon`.
#[test]
fn invoke_github_spans_export_with_deterministic_ids_and_service_name() {
    use crate::gh_invocation::telemetry::{InvocationSpan, Outcome, SPAN_ATTRIBUTE_KEYS};
    use crate::gh_invocation::ParentContext;
    use crate::gh_invocation::{AccessIntent, GhBinSource, GhInvocation, GhTarget, Operation};
    let parent = TraceContext::derived("sweep", &["acme/widgets", "sweep-issue-9-1"]);
    // #10752: `github.caller` is only stamped inside a caller scope and
    // `github.number` only on a write whose argv names an issue/PR.
    let _scope = crate::gh_invocation::caller_scope::enter("stale_blocked_release");
    let inv = GhInvocation::new(
        Operation::new("api.graphql"),
        AccessIntent::Write,
        GhTarget::repo("acme/widgets").unwrap(),
        std::time::Duration::from_secs(5),
    )
    .args(["api", "repos/acme/widgets/issues/9/labels"])
    .parent(ParentContext::Parent(parent.clone()));
    let at = Utc::now();
    let open = InvocationSpan::open_at(&inv, at, "42.0".into());
    let cred = crate::gh_invocation::accounting::cred_of_with(None, false);
    let billing = crate::gh_invocation::billing::Billing::sent(
        Some(200),
        "",
        Some(1),
        crate::gh_invocation::billing::BillingClass::Ok,
        "graphql",
        &cred,
        "writer",
    )
    .with_repo(Some("acme/widgets"));
    let record = open.record(&inv, GhBinSource::Path, Outcome::Ok, Some(0), at, &billing);
    let again = InvocationSpan::open_at(&inv, at, "42.0".into());
    assert_eq!(open.context, again.context, "IDs recompute from the span's facts");

    let request = build_traces_request(&[TelemetryEnvelope::new(
        "host",
        TelemetryRecord::Span(record),
    )])
    .unwrap();
    let json = serde_json::to_value(&request).unwrap();
    let resource = json["resourceSpans"][0]["resource"]["attributes"]
        .as_array()
        .unwrap();
    assert!(resource
        .iter()
        .any(|kv| kv["key"] == "service.name" && kv["value"]["stringValue"] == "loom-daemon"));
    let span = &json["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
    assert_eq!(span["name"], "invoke github");
    assert_eq!(span["traceId"], parent.trace_id.as_str());
    assert_eq!(span["parentSpanId"], parent.span_id.as_str());
    assert_eq!(span["spanId"], open.context.span_id.as_str());
    let keys: Vec<_> = span["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|kv| kv["key"].as_str().unwrap().to_string())
        .collect();
    for key in SPAN_ATTRIBUTE_KEYS {
        assert!(keys.iter().any(|k| k == key), "exported span lacks {key}");
    }
}

/// #10640: a failed span's one-line reason is the OTLP status message, not
/// an exported attribute; an `Ok`/`Unset` span exports no message even when
/// the attribute is present.
#[test]
fn the_status_message_attribute_becomes_the_error_status_message() {
    let mut failed = span(TraceContext::root(true));
    failed.status = SpanStatus::Error;
    failed
        .attributes
        .insert("loom.failure_class".into(), "exit-1".into());
    failed
        .attributes
        .insert(STATUS_MESSAGE.into(), "role child exited with code 1".into());
    let mut ok = failed.clone();
    ok.context = TraceContext::root(true);
    ok.status = SpanStatus::Ok;
    let request = build_traces_request(&[
        TelemetryEnvelope::new("host", TelemetryRecord::Span(failed)),
        TelemetryEnvelope::new("host", TelemetryRecord::Span(ok)),
    ])
    .unwrap();
    let spans = &request.resource_spans[0].scope_spans[0].spans;
    let status = spans[0].status.as_ref().unwrap();
    assert_eq!(status.code, 2);
    assert_eq!(status.message, "role child exited with code 1");
    assert!(spans[0]
        .attributes
        .iter()
        .any(|kv| kv.key == "loom.failure_class"));
    for span in spans {
        assert!(
            span.attributes.iter().all(|kv| kv.key != STATUS_MESSAGE),
            "the description is not also exported as an attribute"
        );
    }
    assert_eq!(spans[1].status.as_ref().unwrap().message, "", "no description off Error");
}
