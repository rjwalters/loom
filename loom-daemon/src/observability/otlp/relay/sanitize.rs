//! Decode a relayed OTLP request, bind the daemon's identity over it, and
//! scrub it (Issue #10964). Everything a request carries passes through here
//! before it can be queued, so the forward queue only ever holds records that
//! are already safe to export.
//!
//! # Identity
//!
//! [`bound_resource`] builds each `Resource` from what the daemon knew when it
//! launched the session. Every key in a daemon-owned namespace
//! ([`daemon_owned`]) is removed from what the sender supplied — on the
//! resource, on every scope, and on every record, data point, span, span
//! event and link — so a sender can neither claim another session's identity
//! nor shadow the bound one with a record-level attribute of the same name.
//!
//! # Redaction
//!
//! Every string the sender controls is passed through
//! [`redact::scrub`](crate::telemetry::kinds::session_output::redact::scrub) —
//! the `session.output` scrubber, not a second one. An attribute value is
//! scrubbed together with its key (`key=value`), because the credential
//! classes key on the *name* next to a value and a bare attribute value has
//! none. A `bytes` value cannot be scrubbed and is replaced by its length.
//!
//! This is a secret scrubber, not a content filter: what a CLI chooses to
//! export is decided by that CLI's own switches, which the launch environment
//! leaves off (`agent_relay::RELAY_CLEARED_ENV`).

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{any_value, AnyValue, InstrumentationScope, KeyValue};
use opentelemetry_proto::tonic::metrics::v1::{metric, Exemplar};
use opentelemetry_proto::tonic::resource::v1::Resource;

use super::super::mapping::{kv_string, resource_for_host};
use super::super::transport::Signal;
use crate::observability::agent_relay::BoundIdentity;
use crate::telemetry::kinds::session_output::redact;

/// The redaction policy stamped on every relayed resource, so a consumer can
/// tell which rows were scrubbed under which class set.
pub(super) const REDACTION_ATTRIBUTE: &str = "loom.relay.redaction";

/// Nesting depth past which an `AnyValue` is replaced rather than walked.
/// Both decoders already bound depth (prost at 100, `serde_json` at 128);
/// this keeps the walk's own stack use independent of either.
const MAX_VALUE_DEPTH: usize = 64;

/// The wire encodings the receiver accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Encoding {
    /// `application/json` — OTLP/HTTP JSON.
    Json,
    /// `application/x-protobuf` — OTLP/HTTP binary protobuf.
    Protobuf,
}

impl Encoding {
    /// The encoding a `Content-Type` value names, ignoring parameters.
    pub(super) fn from_content_type(value: &str) -> Option<Self> {
        let media = value.split(';').next()?.trim();
        if media.eq_ignore_ascii_case("application/json") {
            Some(Encoding::Json)
        } else if media.eq_ignore_ascii_case("application/x-protobuf") {
            Some(Encoding::Protobuf)
        } else {
            None
        }
    }

    pub(super) fn content_type(self) -> &'static str {
        match self {
            Encoding::Json => "application/json",
            Encoding::Protobuf => "application/x-protobuf",
        }
    }
}

/// One decoded export request.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::observability::otlp) enum Payload {
    Logs(ExportLogsServiceRequest),
    Metrics(ExportMetricsServiceRequest),
    Traces(ExportTraceServiceRequest),
}

impl Payload {
    pub(in crate::observability::otlp) fn signal(&self) -> Signal {
        match self {
            Payload::Logs(_) => Signal::Logs,
            Payload::Metrics(_) => Signal::Metrics,
            Payload::Traces(_) => Signal::Traces,
        }
    }
}

/// Decode `body` as a `signal` export request. A malformed body is an `Err`,
/// never a panic: both decoders are total over arbitrary bytes.
pub(super) fn decode(signal: Signal, encoding: Encoding, body: &[u8]) -> Result<Payload, ()> {
    fn parse<T>(encoding: Encoding, body: &[u8]) -> Result<T, ()>
    where
        T: prost::Message + Default + serde::de::DeserializeOwned,
    {
        match encoding {
            Encoding::Json => serde_json::from_slice(body).map_err(|_| ()),
            Encoding::Protobuf => T::decode(body).map_err(|_| ()),
        }
    }
    Ok(match signal {
        Signal::Logs => Payload::Logs(parse(encoding, body)?),
        Signal::Metrics => Payload::Metrics(parse(encoding, body)?),
        Signal::Traces => Payload::Traces(parse(encoding, body)?),
    })
}

/// Whether `key` belongs to the daemon: a name whose value only the daemon
/// may set on relayed telemetry.
pub(super) fn daemon_owned(key: &str) -> bool {
    matches!(key, "service.name" | "service.namespace" | "service.instance.id")
        || key.starts_with("loom.")
        || key.starts_with("host.")
}

/// The `Resource` every record of `bound`'s session is filed under.
///
/// Host attributes come from [`resource_for_host`] — the same function the
/// daemon's own telemetry uses, so the two describe a host identically — with
/// `service.name` replaced by the harness and the daemon's build recorded as
/// `loom.daemon.version` instead of `service.version` (which stays whatever
/// the harness reported about itself).
pub(super) fn bound_resource(sender: Option<Resource>, bound: &BoundIdentity) -> Resource {
    let identity = &bound.identity;
    let mut attributes: Vec<KeyValue> = Vec::new();
    for mut attribute in resource_for_host(&bound.host_id, None).attributes {
        match attribute.key.as_str() {
            "service.name" => {
                attributes.push(kv_string("service.name", identity.harness.service_name()));
            }
            "service.version" => {
                attribute.key = "loom.daemon.version".to_string();
                attributes.push(attribute);
            }
            _ => attributes.push(attribute),
        }
    }
    attributes.push(kv_string("loom.runtime", identity.harness.runtime()));
    attributes.push(kv_string("loom.session.kind", identity.kind.as_str()));
    attributes.push(kv_string("loom.session.launch", "daemon"));
    attributes.push(kv_string(REDACTION_ATTRIBUTE, redact::POLICY));
    if let Some(repo) = &bound.repo {
        attributes.push(kv_string("loom.repo", repo.clone()));
    }
    if let Some(issue) = identity.issue {
        attributes.push(KeyValue {
            key: "loom.issue".to_string(),
            value: Some(AnyValue {
                value: Some(any_value::Value::IntValue(i64::from(issue))),
            }),
            ..Default::default()
        });
    }
    if let Some(role) = &identity.role {
        attributes.push(kv_string("loom.role", role.clone()));
    }
    if let Some(sweep_id) = &identity.sweep_id {
        attributes.push(kv_string("loom.sweep_id", sweep_id.clone()));
    }
    let mut dropped = 0;
    if let Some(sender) = sender {
        dropped = sender.dropped_attributes_count;
        let mut rest = sender.attributes;
        scrub_attributes(&mut rest);
        attributes.extend(rest);
    }
    Resource {
        attributes,
        dropped_attributes_count: dropped,
        ..Default::default()
    }
}

/// Bind `bound` over `payload` and scrub it in place. Returns the number of
/// items it carries, in the signal's own unit (log records, metric data
/// points, spans).
pub(super) fn bind_and_scrub(payload: &mut Payload, bound: &BoundIdentity) -> u64 {
    let mut items = 0u64;
    match payload {
        Payload::Logs(request) => {
            for resource_logs in &mut request.resource_logs {
                resource_logs.resource = Some(bound_resource(resource_logs.resource.take(), bound));
                for scope_logs in &mut resource_logs.scope_logs {
                    scrub_scope(scope_logs.scope.as_mut());
                    items += scope_logs.log_records.len() as u64;
                    for record in &mut scope_logs.log_records {
                        scrub_text(&mut record.severity_text);
                        scrub_text(&mut record.event_name);
                        if let Some(body) = record.body.as_mut() {
                            scrub_value(None, body, 0);
                        }
                        scrub_attributes(&mut record.attributes);
                    }
                }
            }
        }
        Payload::Metrics(request) => {
            for resource_metrics in &mut request.resource_metrics {
                resource_metrics.resource =
                    Some(bound_resource(resource_metrics.resource.take(), bound));
                for scope_metrics in &mut resource_metrics.scope_metrics {
                    scrub_scope(scope_metrics.scope.as_mut());
                    for metric in &mut scope_metrics.metrics {
                        scrub_text(&mut metric.name);
                        scrub_text(&mut metric.description);
                        scrub_text(&mut metric.unit);
                        scrub_attributes(&mut metric.metadata);
                        items += scrub_metric_data(metric.data.as_mut());
                    }
                }
            }
        }
        Payload::Traces(request) => {
            for resource_spans in &mut request.resource_spans {
                resource_spans.resource =
                    Some(bound_resource(resource_spans.resource.take(), bound));
                for scope_spans in &mut resource_spans.scope_spans {
                    scrub_scope(scope_spans.scope.as_mut());
                    items += scope_spans.spans.len() as u64;
                    for span in &mut scope_spans.spans {
                        scrub_text(&mut span.name);
                        scrub_text(&mut span.trace_state);
                        scrub_attributes(&mut span.attributes);
                        for event in &mut span.events {
                            scrub_text(&mut event.name);
                            scrub_attributes(&mut event.attributes);
                        }
                        for link in &mut span.links {
                            scrub_text(&mut link.trace_state);
                            scrub_attributes(&mut link.attributes);
                        }
                        if let Some(status) = span.status.as_mut() {
                            scrub_text(&mut status.message);
                        }
                    }
                }
            }
        }
    }
    items
}

fn scrub_metric_data(data: Option<&mut metric::Data>) -> u64 {
    fn exemplars(exemplars: &mut [Exemplar]) {
        for exemplar in exemplars {
            scrub_attributes(&mut exemplar.filtered_attributes);
        }
    }
    let Some(data) = data else { return 0 };
    match data {
        metric::Data::Gauge(gauge) => {
            for point in &mut gauge.data_points {
                scrub_attributes(&mut point.attributes);
                exemplars(&mut point.exemplars);
            }
            gauge.data_points.len() as u64
        }
        metric::Data::Sum(sum) => {
            for point in &mut sum.data_points {
                scrub_attributes(&mut point.attributes);
                exemplars(&mut point.exemplars);
            }
            sum.data_points.len() as u64
        }
        metric::Data::Histogram(histogram) => {
            for point in &mut histogram.data_points {
                scrub_attributes(&mut point.attributes);
                exemplars(&mut point.exemplars);
            }
            histogram.data_points.len() as u64
        }
        metric::Data::ExponentialHistogram(histogram) => {
            for point in &mut histogram.data_points {
                scrub_attributes(&mut point.attributes);
                exemplars(&mut point.exemplars);
            }
            histogram.data_points.len() as u64
        }
        metric::Data::Summary(summary) => {
            for point in &mut summary.data_points {
                scrub_attributes(&mut point.attributes);
            }
            summary.data_points.len() as u64
        }
    }
}

fn scrub_scope(scope: Option<&mut InstrumentationScope>) {
    if let Some(scope) = scope {
        scrub_text(&mut scope.name);
        scrub_text(&mut scope.version);
        scrub_attributes(&mut scope.attributes);
    }
}

/// Scrub free text in place. Empty strings — the common case for an unset
/// proto field — skip the regex pass entirely.
fn scrub_text(text: &mut String) {
    if !text.is_empty() {
        *text = redact::scrub(text);
    }
}

/// Drop daemon-owned keys, then scrub every remaining value.
fn scrub_attributes(attributes: &mut Vec<KeyValue>) {
    attributes.retain(|attribute| !daemon_owned(&attribute.key));
    for attribute in attributes {
        // Dictionary-indexed keys belong to the profiles signal, which the
        // receiver does not accept; an index with no table is meaningless.
        attribute.key_strindex = 0;
        if let Some(value) = attribute.value.as_mut() {
            scrub_value(Some(&attribute.key), value, 0);
        }
    }
}

/// Scrub one value. `key` is the attribute name it sits under, when it has
/// one, so the credential classes that match on `name: value` can see it.
fn scrub_value(key: Option<&str>, value: &mut AnyValue, depth: usize) {
    if depth >= MAX_VALUE_DEPTH {
        value.value = Some(any_value::Value::StringValue("[REDACTED:too-deep]".to_string()));
        return;
    }
    match value.value.as_mut() {
        Some(any_value::Value::StringValue(text)) => {
            *text = scrub_keyed(key, text);
        }
        Some(any_value::Value::BytesValue(bytes)) => {
            let marker = format!("[REDACTED:bytes len={}]", bytes.len());
            value.value = Some(any_value::Value::StringValue(marker));
        }
        Some(any_value::Value::ArrayValue(array)) => {
            for element in &mut array.values {
                scrub_value(key, element, depth + 1);
            }
        }
        Some(any_value::Value::KvlistValue(list)) => {
            // A nested map can shadow a daemon-owned name just as well as a
            // top-level attribute can, so it gets the same treatment.
            list.values.retain(|entry| !daemon_owned(&entry.key));
            for entry in &mut list.values {
                entry.key_strindex = 0;
                if let Some(nested) = entry.value.as_mut() {
                    scrub_value(Some(&entry.key), nested, depth + 1);
                }
            }
        }
        // A string-table index with no table to resolve it against.
        Some(any_value::Value::StringValueStrindex(_)) => value.value = None,
        Some(
            any_value::Value::BoolValue(_)
            | any_value::Value::IntValue(_)
            | any_value::Value::DoubleValue(_),
        )
        | None => {}
    }
}

/// [`redact::scrub`] over `text` as the value of `key`.
///
/// `password = hunter2hunter2` reaches OTLP as the attribute `password` with
/// the bare value `hunter2hunter2`, which no class matches on its own. The
/// pair is therefore scrubbed as `key=value` and the value read back: when a
/// class consumed the key too, the whole result (a marker) is the value.
fn scrub_keyed(key: Option<&str>, text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let Some(key) = key.filter(|key| !key.is_empty()) else {
        return redact::scrub(text);
    };
    let scrubbed = redact::scrub(&format!("{key}={text}"));
    match scrubbed
        .strip_prefix(key)
        .and_then(|rest| rest.strip_prefix('='))
    {
        Some(value) => value.to_string(),
        None => scrubbed,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "sanitize_tests.rs"]
mod tests;
