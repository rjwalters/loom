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
            repo: "test/fixture".into(),
            visibility: crate::telemetry::RepoVisibility::Private,
            issue: 18,
            sweep_id: "fixture".into(),
            started_at: Utc::now(),
            model: None,
            effort: None,
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
