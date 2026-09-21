//! Completed trace records, grouped by the same host resource as logs/metrics.
use super::mapping::{kv_string, nanos, resource_for_host};
use crate::telemetry::trace::SpanStatus;
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::InstrumentationScope;
use opentelemetry_proto::tonic::trace::v1::{
    span::{Event, Link, SpanKind},
    ResourceSpans, ScopeSpans, Span, Status,
};
use std::collections::BTreeMap;

pub(super) fn build_traces_request(
    envelopes: &[TelemetryEnvelope],
) -> Option<ExportTraceServiceRequest> {
    let mut by_host: BTreeMap<&str, Vec<Span>> = BTreeMap::new();
    for envelope in envelopes {
        let TelemetryRecord::Span(record) = &envelope.record else {
            continue;
        };
        if !record.context.sampled() {
            continue;
        }
        if record.validate().is_err() {
            log::warn!("observability: invalid completed span omitted");
            continue;
        }
        let record = record.clone().bounded();
        let span = Span {
            trace_id: record.context.trace_id.bytes(),
            span_id: record.context.span_id.bytes(),
            parent_span_id: record
                .parent_span_id
                .as_ref()
                .map(|v| v.bytes())
                .unwrap_or_default(),
            flags: u32::from(record.context.flags),
            name: record.name.as_str().to_string(),
            kind: SpanKind::Internal as i32,
            start_time_unix_nano: nanos(record.started_at),
            end_time_unix_nano: nanos(record.ended_at),
            attributes: record
                .attributes
                .iter()
                .map(|(k, v)| kv_string(k, v))
                .collect(),
            events: record
                .events
                .iter()
                .map(|event| Event {
                    time_unix_nano: nanos(event.at),
                    name: event.name.clone(),
                    attributes: event
                        .attributes
                        .iter()
                        .map(|(k, v)| kv_string(k, v))
                        .collect(),
                    ..Default::default()
                })
                .collect(),
            links: record
                .links
                .iter()
                .map(|link| Link {
                    trace_id: link.context.trace_id.bytes(),
                    span_id: link.context.span_id.bytes(),
                    flags: u32::from(link.context.flags),
                    ..Default::default()
                })
                .collect(),
            status: Some(Status {
                code: match record.status {
                    SpanStatus::Unset => 0,
                    SpanStatus::Ok => 1,
                    SpanStatus::Error => 2,
                },
                message: String::new(),
            }),
            ..Default::default()
        };
        by_host.entry(&envelope.host_id).or_default().push(span);
    }
    if by_host.is_empty() {
        return None;
    }
    Some(ExportTraceServiceRequest {
        resource_spans: by_host
            .into_iter()
            .map(|(host_id, spans)| ResourceSpans {
                resource: Some(resource_for_host(host_id, None)),
                scope_spans: vec![ScopeSpans {
                    scope: Some(InstrumentationScope {
                        name: "loom.observability".to_string(),
                        version: env!("CARGO_PKG_VERSION").to_string(),
                        ..Default::default()
                    }),
                    spans,
                    ..Default::default()
                }],
                ..Default::default()
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests;
