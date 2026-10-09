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
//! Keys are checked too: an attribute whose *name* is secret-shaped is
//! dropped whole, since rewriting a key would merge unrelated attributes.
//! A number under a credential-named key (`password = 123456789`) is scrubbed
//! as text and replaced by the marker when it matches. A `schema_url` that
//! the scrubber would change is cleared. A trace, span or parent id of the
//! wrong length is cleared, so an id field cannot carry arbitrary bytes past
//! the scrubber.
//!
//! # Shape
//!
//! Before binding, empty containers are removed (a resource with no scope
//! that holds a record, a scope with no record) and containers whose sender
//! resource and schema are identical are merged. Every container that is left
//! gets a full daemon-built `Resource`, so this is what keeps a request of
//! many tiny containers from inflating into many full resources; the
//! receiver additionally refuses a request whose bound size is out of
//! proportion to its wire size.
//!
//! This is a secret scrubber, not a content filter: what a CLI chooses to
//! export is decided by that CLI's own switches, which the launch environment
//! leaves off (`agent_relay::RELAY_CLEARED_ENV`).

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{any_value, AnyValue, InstrumentationScope, KeyValue};
use opentelemetry_proto::tonic::logs::v1::ResourceLogs;
use opentelemetry_proto::tonic::metrics::v1::{metric, Exemplar, ResourceMetrics};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::ResourceSpans;
use prost::Message;

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
pub(in crate::observability::otlp) enum Encoding {
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
    /// The protobuf encoding of the request.
    pub(in crate::observability::otlp) fn encode_to_vec(&self) -> Vec<u8> {
        match self {
            Payload::Logs(request) => request.encode_to_vec(),
            Payload::Metrics(request) => request.encode_to_vec(),
            Payload::Traces(request) => request.encode_to_vec(),
        }
    }

    /// The length of [`Self::encode_to_vec`], without encoding.
    pub(in crate::observability::otlp) fn encoded_len(&self) -> usize {
        match self {
            Payload::Logs(request) => request.encoded_len(),
            Payload::Metrics(request) => request.encoded_len(),
            Payload::Traces(request) => request.encoded_len(),
        }
    }

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
pub(in crate::observability::otlp) fn decode(
    signal: Signal,
    encoding: Encoding,
    body: &[u8],
) -> Result<Payload, ()> {
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
/// Compared without regard to ASCII case: a consumer that folds case would
/// otherwise read `Service.Name` as the bound name.
pub(super) fn daemon_owned(key: &str) -> bool {
    let key = key.trim().to_ascii_lowercase();
    matches!(key.as_str(), "service.name" | "service.namespace" | "service.instance.id")
        || key.starts_with("loom.")
        || key.starts_with("host.")
}

/// Whether an attribute *name* is itself secret-shaped.
fn secret_key(key: &str) -> bool {
    !key.is_empty() && redact::scrub(key) != key
}

/// Clear a `schema_url` the scrubber would change: a rewritten URL is not a
/// schema identifier any more, and the original must not leave.
fn clean_schema_url(url: &mut String) {
    if !url.is_empty() && redact::scrub(url) != *url {
        url.clear();
    }
}

/// Clear an id that is not `length` bytes (or empty).
fn clean_id(id: &mut Vec<u8>, length: usize) {
    if !id.is_empty() && id.len() != length {
        id.clear();
    }
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

/// A `Resource*` container: what pruning and merging need from it.
trait Container: Sized {
    fn resource(&mut self) -> &mut Option<Resource>;
    fn schema_url(&mut self) -> &mut String;
    /// Remove empty scopes; `true` when nothing is left.
    fn prune(&mut self) -> bool;
    /// Move `other`'s scopes into this container.
    fn absorb(&mut self, other: Self);
}

impl Container for ResourceLogs {
    fn resource(&mut self) -> &mut Option<Resource> {
        &mut self.resource
    }
    fn schema_url(&mut self) -> &mut String {
        &mut self.schema_url
    }
    fn prune(&mut self) -> bool {
        self.scope_logs
            .retain(|scope| !scope.log_records.is_empty());
        self.scope_logs.is_empty()
    }
    fn absorb(&mut self, other: Self) {
        self.scope_logs.extend(other.scope_logs);
    }
}

impl Container for ResourceSpans {
    fn resource(&mut self) -> &mut Option<Resource> {
        &mut self.resource
    }
    fn schema_url(&mut self) -> &mut String {
        &mut self.schema_url
    }
    fn prune(&mut self) -> bool {
        self.scope_spans.retain(|scope| !scope.spans.is_empty());
        self.scope_spans.is_empty()
    }
    fn absorb(&mut self, other: Self) {
        self.scope_spans.extend(other.scope_spans);
    }
}

impl Container for ResourceMetrics {
    fn resource(&mut self) -> &mut Option<Resource> {
        &mut self.resource
    }
    fn schema_url(&mut self) -> &mut String {
        &mut self.schema_url
    }
    fn prune(&mut self) -> bool {
        for scope in &mut self.scope_metrics {
            scope
                .metrics
                .retain(|metric| points(metric.data.as_ref()) > 0);
        }
        self.scope_metrics.retain(|scope| !scope.metrics.is_empty());
        self.scope_metrics.is_empty()
    }
    fn absorb(&mut self, other: Self) {
        self.scope_metrics.extend(other.scope_metrics);
    }
}

fn points(data: Option<&metric::Data>) -> usize {
    match data {
        Some(metric::Data::Gauge(gauge)) => gauge.data_points.len(),
        Some(metric::Data::Sum(sum)) => sum.data_points.len(),
        Some(metric::Data::Histogram(histogram)) => histogram.data_points.len(),
        Some(metric::Data::ExponentialHistogram(histogram)) => histogram.data_points.len(),
        Some(metric::Data::Summary(summary)) => summary.data_points.len(),
        None => 0,
    }
}

/// Drop empty containers, merge those whose (scrubbed) sender resource and
/// schema are identical, and bind `bound` over each that remains.
fn prune_merge_bind<C: Container>(containers: &mut Vec<C>, bound: &BoundIdentity) {
    let mut kept: Vec<C> = Vec::new();
    let mut index: std::collections::HashMap<Vec<u8>, usize> = std::collections::HashMap::new();
    for mut container in std::mem::take(containers) {
        if container.prune() {
            continue;
        }
        let mut sender = container.resource().take().unwrap_or_default();
        scrub_attributes(&mut sender.attributes);
        // Entity references describe the sender's own idea of the resource;
        // the bound resource replaces it, so they are not kept.
        sender.entity_refs.clear();
        clean_schema_url(container.schema_url());
        let mut key = sender.encode_to_vec();
        key.extend_from_slice(b"\0schema\0");
        key.extend_from_slice(container.schema_url().as_bytes());
        match index.get(&key) {
            Some(&at) => {
                if let Some(existing) = kept.get_mut(at) {
                    existing.absorb(container);
                }
            }
            None => {
                *container.resource() = Some(sender);
                index.insert(key, kept.len());
                kept.push(container);
            }
        }
    }
    for container in &mut kept {
        let sender = container.resource().take();
        *container.resource() = Some(bound_resource(sender, bound));
    }
    *containers = kept;
}

/// Bind `bound` over `payload` and scrub it in place. Returns the number of
/// items it carries, in the signal's own unit (log records, metric data
/// points, spans).
pub(super) fn bind_and_scrub(payload: &mut Payload, bound: &BoundIdentity) -> u64 {
    let mut items = 0u64;
    match payload {
        Payload::Logs(request) => {
            prune_merge_bind(&mut request.resource_logs, bound);
            for resource_logs in &mut request.resource_logs {
                for scope_logs in &mut resource_logs.scope_logs {
                    clean_schema_url(&mut scope_logs.schema_url);
                    scrub_scope(scope_logs.scope.as_mut());
                    items += scope_logs.log_records.len() as u64;
                    for record in &mut scope_logs.log_records {
                        scrub_text(&mut record.severity_text);
                        scrub_text(&mut record.event_name);
                        clean_id(&mut record.trace_id, 16);
                        clean_id(&mut record.span_id, 8);
                        if let Some(body) = record.body.as_mut() {
                            scrub_value(None, body, 0);
                        }
                        scrub_attributes(&mut record.attributes);
                    }
                }
            }
        }
        Payload::Metrics(request) => {
            prune_merge_bind(&mut request.resource_metrics, bound);
            for resource_metrics in &mut request.resource_metrics {
                for scope_metrics in &mut resource_metrics.scope_metrics {
                    clean_schema_url(&mut scope_metrics.schema_url);
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
            prune_merge_bind(&mut request.resource_spans, bound);
            for resource_spans in &mut request.resource_spans {
                for scope_spans in &mut resource_spans.scope_spans {
                    clean_schema_url(&mut scope_spans.schema_url);
                    scrub_scope(scope_spans.scope.as_mut());
                    items += scope_spans.spans.len() as u64;
                    for span in &mut scope_spans.spans {
                        clean_id(&mut span.trace_id, 16);
                        clean_id(&mut span.span_id, 8);
                        clean_id(&mut span.parent_span_id, 8);
                        scrub_text(&mut span.name);
                        scrub_text(&mut span.trace_state);
                        scrub_attributes(&mut span.attributes);
                        for event in &mut span.events {
                            scrub_text(&mut event.name);
                            scrub_attributes(&mut event.attributes);
                        }
                        for link in &mut span.links {
                            clean_id(&mut link.trace_id, 16);
                            clean_id(&mut link.span_id, 8);
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
            clean_id(&mut exemplar.trace_id, 16);
            clean_id(&mut exemplar.span_id, 8);
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

/// Drop daemon-owned and secret-shaped keys, then scrub every remaining
/// value under its (unchanged) key.
fn scrub_attributes(attributes: &mut Vec<KeyValue>) {
    attributes.retain(|attribute| !daemon_owned(&attribute.key) && !secret_key(&attribute.key));
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
            list.values
                .retain(|entry| !daemon_owned(&entry.key) && !secret_key(&entry.key));
            for entry in &mut list.values {
                entry.key_strindex = 0;
                if let Some(nested) = entry.value.as_mut() {
                    scrub_value(Some(&entry.key), nested, depth + 1);
                }
            }
        }
        // A string-table index with no table to resolve it against.
        Some(any_value::Value::StringValueStrindex(_)) => value.value = None,
        // A number is a secret when its key says so: `password = 123456789`.
        Some(any_value::Value::IntValue(number)) => {
            let text = number.to_string();
            let scrubbed = scrub_keyed(key, &text);
            if scrubbed != text {
                value.value = Some(any_value::Value::StringValue(scrubbed));
            }
        }
        Some(any_value::Value::DoubleValue(number)) => {
            let text = number.to_string();
            let scrubbed = scrub_keyed(key, &text);
            if scrubbed != text {
                value.value = Some(any_value::Value::StringValue(scrubbed));
            }
        }
        Some(any_value::Value::BoolValue(_)) | None => {}
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
