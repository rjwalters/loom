//! Identity binding and redaction over relayed OTLP (#10964). Every secret
//! here is a synthetic, shape-only fixture; every identity a made-up one.
use super::*;
use crate::observability::agent_relay::{Harness, SessionIdentity, SessionKind};
use prost::Message;
use serde_json::json;

pub(in crate::observability::otlp::relay) fn bound() -> BoundIdentity {
    BoundIdentity {
        identity: SessionIdentity {
            harness: Harness::ClaudeCode,
            kind: SessionKind::Sweep,
            role: None,
            issue: Some(4242),
            sweep_id: Some("sweep-fixture-1".to_string()),
            workspace_root: std::path::PathBuf::from("/nonexistent/fixture-workspace"),
        },
        host_id: "host-fixture".to_string(),
        repo: Some("example-owner/example-repo".to_string()),
    }
}

/// Every `(key, rendered value)` on a resource.
fn resource_pairs(resource: &Resource) -> Vec<(String, String)> {
    resource
        .attributes
        .iter()
        .map(|kv| (kv.key.clone(), render(kv.value.as_ref())))
        .collect()
}

fn render(value: Option<&AnyValue>) -> String {
    match value.and_then(|v| v.value.as_ref()) {
        Some(any_value::Value::StringValue(s)) => s.clone(),
        Some(any_value::Value::IntValue(i)) => i.to_string(),
        Some(other) => format!("{other:?}"),
        None => String::new(),
    }
}

fn one<'a>(pairs: &'a [(String, String)], key: &str) -> &'a str {
    let matches: Vec<&str> = pairs
        .iter()
        .filter(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .collect();
    assert_eq!(matches.len(), 1, "{key} must appear exactly once, got {matches:?}");
    matches[0]
}

/// A sender that claims to be the daemon, on another host, working another
/// repository's issue — at every level a claim can be made.
fn lying_logs() -> serde_json::Value {
    let lies = json!([
        {"key": "service.name", "value": {"stringValue": "loom-daemon"}},
        {"key": "service.namespace", "value": {"stringValue": "forged-namespace"}},
        {"key": "service.instance.id", "value": {"stringValue": "forged-host"}},
        {"key": "host.id", "value": {"stringValue": "forged-host"}},
        {"key": "host.name", "value": {"stringValue": "forged-hostname"}},
        {"key": "loom.repo", "value": {"stringValue": "forged-owner/forged-repo"}},
        {"key": "loom.issue", "value": {"intValue": "1"}},
        {"key": "loom.role", "value": {"stringValue": "champion"}},
        {"key": "loom.sweep_id", "value": {"stringValue": "forged-sweep"}},
        {"key": "loom.session.launch", "value": {"stringValue": "forged-launch"}},
        {"key": "loom.relay.redaction", "value": {"stringValue": "none"}},
        {"key": "service.version", "value": {"stringValue": "9.9.9-fixture"}},
        {"key": "os.type", "value": {"stringValue": "fixture-os"}}
    ]);
    json!({"resourceLogs": [{
        "resource": {"attributes": lies},
        "scopeLogs": [{
            "scope": {"name": "fixture.scope", "attributes": lies},
            "logRecords": [{
                "timeUnixNano": "1700000000000000000",
                "severityNumber": 9,
                "eventName": "fixture.event",
                "body": {"kvlistValue": {"values": lies}},
                "attributes": lies
            }]
        }]
    }]})
}

fn decode_json(signal: Signal, value: &serde_json::Value) -> Payload {
    decode(signal, Encoding::Json, value.to_string().as_bytes()).unwrap()
}

#[test]
fn identity_comes_from_the_daemon_whatever_the_sender_claimed() {
    let mut payload = decode_json(Signal::Logs, &lying_logs());
    assert_eq!(bind_and_scrub(&mut payload, &bound()), 1);
    let Payload::Logs(request) = &payload else {
        panic!("logs in, logs out")
    };
    let resource = request.resource_logs[0].resource.as_ref().unwrap();
    let pairs = resource_pairs(resource);
    assert_eq!(one(&pairs, "service.name"), "claude-code");
    assert_eq!(one(&pairs, "service.instance.id"), "host-fixture");
    assert_eq!(one(&pairs, "host.id"), "host-fixture");
    // Every host attribute the daemon's own telemetry carries, the relay
    // carries too — with the daemon's value, not the sender's `host.name`.
    assert_eq!(one(&pairs, "host.name"), "host-fixture");
    assert_eq!(one(&pairs, "loom.repo"), "example-owner/example-repo");
    assert_eq!(one(&pairs, "loom.issue"), "4242");
    assert_eq!(one(&pairs, "loom.sweep_id"), "sweep-fixture-1");
    assert_eq!(one(&pairs, "loom.runtime"), "claude");
    assert_eq!(one(&pairs, "loom.session.kind"), "sweep");
    assert_eq!(one(&pairs, "loom.session.launch"), "daemon");
    assert_eq!(one(&pairs, REDACTION_ATTRIBUTE), redact::POLICY);
    assert_eq!(one(&pairs, "loom.daemon.version"), env!("CARGO_PKG_VERSION"));
    // A sweep has no single role, and the sender does not get to supply one.
    assert!(!pairs
        .iter()
        .any(|(k, _)| k == "loom.role" || k == "service.namespace"));
    // What the harness says about itself, outside the owned names, survives.
    assert_eq!(one(&pairs, "service.version"), "9.9.9-fixture");
    assert_eq!(one(&pairs, "os.type"), "fixture-os");
    // And not one forged value survives anywhere in the request — resource,
    // scope, record attributes, or a map nested in the body.
    let exported = serde_json::to_string(request).unwrap();
    assert!(!exported.contains("forged"), "{exported}");
    assert!(!exported.contains("champion"), "{exported}");
}

#[test]
fn a_role_tick_is_bound_to_its_role_and_execution() {
    let mut identity = bound();
    identity.identity.kind = SessionKind::Role;
    identity.identity.role = Some("judge".to_string());
    identity.identity.issue = None;
    identity.identity.sweep_id = Some("role-judge-fixture".to_string());
    identity.repo = None;
    let pairs = resource_pairs(&bound_resource(None, &identity));
    assert_eq!(one(&pairs, "loom.role"), "judge");
    assert_eq!(one(&pairs, "loom.session.kind"), "role");
    assert_eq!(one(&pairs, "loom.sweep_id"), "role-judge-fixture");
    // Unknown is absent — never a path, a basename or a zero.
    assert!(!pairs
        .iter()
        .any(|(k, _)| k == "loom.repo" || k == "loom.issue"));
    assert!(!pairs.iter().any(|(_, v)| v.contains("fixture-workspace")));
}

#[test]
fn identity_is_bound_on_metrics_and_traces_too() {
    let lie = json!([{"key": "service.name", "value": {"stringValue": "forged-service"}},
                     {"key": "loom.repo", "value": {"stringValue": "forged-owner/forged-repo"}}]);
    let metrics = json!({"resourceMetrics": [{
        "resource": {"attributes": lie},
        "scopeMetrics": [{"metrics": [
            {"name": "fixture.counter", "sum": {"aggregationTemporality": 2, "isMonotonic": true,
                "dataPoints": [
                    {"asInt": "3", "timeUnixNano": "1700000000000000000", "attributes": lie},
                    {"asDouble": 1.5, "timeUnixNano": "1700000000000000000"}]}},
            {"name": "fixture.gauge", "gauge": {"dataPoints": [{"asDouble": 2.0}]}},
            {"name": "fixture.histogram", "histogram": {"aggregationTemporality": 2,
                "dataPoints": [{"count": "1", "sum": 4.0, "bucketCounts": ["1"],
                    "explicitBounds": [], "attributes": lie}]}}
        ]}]
    }]});
    let mut payload = decode_json(Signal::Metrics, &metrics);
    assert_eq!(bind_and_scrub(&mut payload, &bound()), 4, "data points, across metric shapes");
    let Payload::Metrics(request) = &payload else {
        panic!()
    };
    let pairs = resource_pairs(request.resource_metrics[0].resource.as_ref().unwrap());
    assert_eq!(one(&pairs, "service.name"), "claude-code");
    assert!(!serde_json::to_string(request).unwrap().contains("forged"));

    let traces = json!({"resourceSpans": [{
        "resource": {"attributes": lie},
        "scopeSpans": [{"spans": [{
            "traceId": "0af7651916cd43dd8448eb211c80319c",
            "spanId": "b7ad6b7169203331",
            "parentSpanId": "00f067aa0ba902b7",
            "name": "fixture.span",
            "startTimeUnixNano": "1700000000000000000",
            "endTimeUnixNano": "1700000001000000000",
            "attributes": lie,
            "events": [{"name": "fixture.event", "attributes": lie}],
            "links": [{"traceId": "0af7651916cd43dd8448eb211c80319c",
                       "spanId": "b7ad6b7169203331", "attributes": lie}]
        }]}]
    }]});
    let mut payload = decode_json(Signal::Traces, &traces);
    assert_eq!(bind_and_scrub(&mut payload, &bound()), 1);
    let Payload::Traces(request) = &payload else {
        panic!()
    };
    let exported = serde_json::to_string(request).unwrap();
    assert!(!exported.contains("forged"), "{exported}");
    // The trace context itself is the sender's and is preserved: it is what
    // parents the session's spans under the launch's own trace.
    let span = &request.resource_spans[0].scope_spans[0].spans[0];
    assert_eq!(hex::encode(&span.trace_id), "0af7651916cd43dd8448eb211c80319c");
    assert_eq!(hex::encode(&span.parent_span_id), "00f067aa0ba902b7");
    assert_eq!(span.name, "fixture.span");
}

/// Synthetic, shape-only secrets: each matches one redaction class.
const GITHUB_TOKEN: &str = "ghp_abcdefghijklmnopqrstuvwxyz0123";
const ANTHROPIC_KEY: &str = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz";
const AWS_KEY_ID: &str = "AKIAIOSFODNN7EXAMPLE";
const BEARER: &str = "Bearer abcdefghijklmnopqrstuvwxyz012345";
const PASSWORD: &str = "hunter2hunter2";
const EMAIL: &str = "someone@example.com";
const SECRETS: &[&str] = &[
    GITHUB_TOKEN,
    ANTHROPIC_KEY,
    AWS_KEY_ID,
    BEARER,
    PASSWORD,
    EMAIL,
];

#[test]
fn secret_shapes_are_scrubbed_from_log_bodies_and_attribute_values() {
    let logs = json!({"resourceLogs": [{
        "resource": {"attributes": [
            {"key": "user.email", "value": {"stringValue": EMAIL}}]},
        "scopeLogs": [{"logRecords": [
            {"body": {"stringValue": format!("pushed with {GITHUB_TOKEN} then called {ANTHROPIC_KEY}")},
             "severityText": format!("WARN {AWS_KEY_ID}"),
             "attributes": [
                {"key": "http.request.header.authorization", "value": {"stringValue": BEARER}},
                {"key": "password", "value": {"stringValue": PASSWORD}},
                {"key": "command", "value": {"stringValue": format!("curl -H 'Authorization: {BEARER}' x")}},
                {"key": "nested", "value": {"arrayValue": {"values": [
                    {"stringValue": GITHUB_TOKEN},
                    {"kvlistValue": {"values": [
                        {"key": "api_key", "value": {"stringValue": PASSWORD}},
                        {"key": "note", "value": {"stringValue": format!("mail {EMAIL}")}}]}}]}}},
                {"key": "input_tokens", "value": {"stringValue": "1234567"}},
                {"key": "model", "value": {"stringValue": "fixture-model-1"}},
                {"key": "duration_ms", "value": {"intValue": "1500"}}
             ]},
            {"body": {"kvlistValue": {"values": [
                {"key": "token", "value": {"stringValue": PASSWORD}},
                {"key": "text", "value": {"stringValue": format!("key id {AWS_KEY_ID}")}}]}}}
        ]}]
    }]});
    let mut payload = decode_json(Signal::Logs, &logs);
    assert_eq!(bind_and_scrub(&mut payload, &bound()), 2);
    let Payload::Logs(request) = &payload else {
        panic!()
    };
    let exported = serde_json::to_string(request).unwrap();
    for secret in SECRETS {
        assert!(!exported.contains(secret), "{secret} survived: {exported}");
    }
    for marker in [
        "[REDACTED:github-token]",
        "[REDACTED:anthropic-key]",
        "[REDACTED:aws-access-key-id]",
        "[REDACTED:authorization]",
        "[REDACTED:credential]",
        "[REDACTED:email]",
    ] {
        assert!(exported.contains(marker), "{marker} missing: {exported}");
    }
    // Ordinary telemetry is left exactly as it was sent.
    let record = &request.resource_logs[0].scope_logs[0].log_records[0];
    let attributes: Vec<(String, String)> = record
        .attributes
        .iter()
        .map(|kv| (kv.key.clone(), render(kv.value.as_ref())))
        .collect();
    assert_eq!(one(&attributes, "input_tokens"), "1234567");
    assert_eq!(one(&attributes, "model"), "fixture-model-1");
    assert_eq!(one(&attributes, "duration_ms"), "1500");
    // A value redacted because of its key keeps its key.
    assert_eq!(one(&attributes, "password"), "[REDACTED:credential]");
}

#[test]
fn secret_shapes_are_scrubbed_from_spans_and_metric_points() {
    let secret = json!([{"key": "tool.input", "value": {"stringValue": format!("export GH={GITHUB_TOKEN}")}}]);
    let traces = json!({"resourceSpans": [{"scopeSpans": [{"spans": [{
        "traceId": "0af7651916cd43dd8448eb211c80319c", "spanId": "b7ad6b7169203331",
        "name": format!("run {ANTHROPIC_KEY}"),
        "attributes": secret,
        "events": [{"name": "fixture", "attributes": secret}],
        "links": [{"traceId": "0af7651916cd43dd8448eb211c80319c",
                   "spanId": "b7ad6b7169203331", "attributes": secret}],
        "status": {"code": 2, "message": format!("failed: password={PASSWORD}")}
    }]}]}]});
    let mut payload = decode_json(Signal::Traces, &traces);
    bind_and_scrub(&mut payload, &bound());
    let Payload::Traces(request) = &payload else {
        panic!()
    };
    let exported = serde_json::to_string(request).unwrap();
    for secret in SECRETS {
        assert!(!exported.contains(secret), "{secret} survived: {exported}");
    }

    let metrics = json!({"resourceMetrics": [{"scopeMetrics": [{"metrics": [{
        "name": "fixture.counter", "description": format!("contact {EMAIL}"),
        "sum": {"dataPoints": [{"asInt": "1", "attributes": secret}]}
    }]}]}]});
    let mut payload = decode_json(Signal::Metrics, &metrics);
    add_exemplar_from_point_attributes(&mut payload);
    assert_eq!(bind_and_scrub(&mut payload, &bound()), 1, "the metric must not be empty");
    let Payload::Metrics(request) = &payload else {
        panic!()
    };
    let exported = serde_json::to_string(request).unwrap();
    for secret in SECRETS {
        assert!(!exported.contains(secret), "{secret} survived: {exported}");
    }
}

#[test]
fn bytes_values_are_replaced_and_deep_nesting_is_cut_off() {
    let mut value = AnyValue {
        value: Some(any_value::Value::BytesValue(GITHUB_TOKEN.as_bytes().to_vec())),
    };
    scrub_value(Some("blob"), &mut value, 0);
    assert_eq!(render(Some(&value)), format!("[REDACTED:bytes len={}]", GITHUB_TOKEN.len()));

    let mut nested = AnyValue {
        value: Some(any_value::Value::StringValue(GITHUB_TOKEN.to_string())),
    };
    for _ in 0..(MAX_VALUE_DEPTH + 8) {
        nested = AnyValue {
            value: Some(any_value::Value::ArrayValue(
                opentelemetry_proto::tonic::common::v1::ArrayValue {
                    values: vec![nested],
                },
            )),
        };
    }
    scrub_value(None, &mut nested, 0);
    assert!(!format!("{nested:?}").contains(GITHUB_TOKEN));
}

#[test]
fn scrubbing_twice_changes_nothing() {
    let once = scrub_keyed(Some("password"), PASSWORD);
    assert_eq!(scrub_keyed(Some("password"), &once), once);
    let prose = scrub_keyed(Some("note"), &format!("token: {PASSWORD} and {GITHUB_TOKEN}"));
    assert_eq!(scrub_keyed(Some("note"), &prose), prose);
}

#[test]
fn protobuf_and_json_decode_to_the_same_request() {
    let from_json = decode_json(Signal::Logs, &lying_logs());
    let Payload::Logs(request) = &from_json else {
        panic!()
    };
    let from_protobuf = decode(Signal::Logs, Encoding::Protobuf, &request.encode_to_vec()).unwrap();
    assert_eq!(from_json, from_protobuf);
}

#[test]
fn a_malformed_body_is_an_error_in_either_encoding_never_a_panic() {
    let garbage: &[&[u8]] = &[
        b"",
        b"{",
        b"not json at all",
        b"[1,2,3]",
        &[0xff; 64],
        &[0x0a, 0xff, 0xff, 0xff, 0xff, 0x0f],
    ];
    for signal in [Signal::Logs, Signal::Metrics, Signal::Traces] {
        for body in garbage {
            // An empty protobuf body is a valid, empty request; everything
            // else here must be refused. Either way: no panic.
            let json = decode(signal, Encoding::Json, body);
            assert!(json.is_err(), "JSON {body:?} decoded");
            let protobuf = decode(signal, Encoding::Protobuf, body);
            if !body.is_empty() {
                assert!(protobuf.is_err(), "protobuf {body:?} decoded");
            }
        }
    }
    // Well-formed JSON of the wrong shape for its own signal. (A key another
    // signal uses is an unknown field, ignored like any other.)
    for (signal, body) in [
        (Signal::Logs, r#"{"resourceLogs": "nope"}"#),
        (
            Signal::Logs,
            r#"{"resourceLogs": [{"scopeLogs": [{"logRecords": [{"timeUnixNano": "minus one"}]}]}]}"#,
        ),
        (Signal::Metrics, r#"{"resourceMetrics": {"scopeMetrics": 1}}"#),
        (
            Signal::Traces,
            r#"{"resourceSpans": [{"scopeSpans": [{"spans": [{"traceId": "not hex"}]}]}]}"#,
        ),
    ] {
        assert!(decode(signal, Encoding::Json, body.as_bytes()).is_err(), "{body}");
    }
    // Deeply nested JSON hits the decoder's own recursion limit.
    let deep = format!(
        r#"{{"resourceLogs":[{{"scopeLogs":[{{"logRecords":[{{"body":{}{}{}}}]}}]}}]}}"#,
        r#"{"arrayValue":{"values":["#.repeat(400),
        r#"{"stringValue":"x"}"#,
        "]}}".repeat(400)
    );
    assert!(decode(Signal::Logs, Encoding::Json, deep.as_bytes()).is_err());
}

#[test]
fn content_types_map_to_encodings() {
    assert_eq!(Encoding::from_content_type("application/json"), Some(Encoding::Json));
    assert_eq!(
        Encoding::from_content_type("Application/JSON; charset=utf-8"),
        Some(Encoding::Json)
    );
    assert_eq!(Encoding::from_content_type("application/x-protobuf"), Some(Encoding::Protobuf));
    assert_eq!(Encoding::from_content_type("application/grpc"), None);
    assert_eq!(Encoding::from_content_type("text/plain"), None);
}

// ---------------------------------------------------------------------------
// Keys, schema URLs, numbers and ids (review round 1)
// ---------------------------------------------------------------------------

/// Synthetic fixtures for the places a value used to slip past the scrubber.
const SECRET_KEY: &str = "ghp_abcdefghijklmnopqrstuvwxyz0123";
const SECRET_SCHEMA: &str = "https://example.invalid/?token=syntheticsecret123";
const SECRET_NUMBER: &str = "123456789";

/// `value` decoded from JSON, and the same request round-tripped through
/// protobuf: every check runs against both encodings.
fn both_encodings(signal: Signal, value: &serde_json::Value) -> Vec<Payload> {
    let from_json = decode_json(signal, value);
    let from_protobuf = decode(signal, Encoding::Protobuf, &from_json.encode_to_vec()).unwrap();
    assert_eq!(from_json, from_protobuf);
    vec![from_json, from_protobuf]
}

/// Assert none of `secrets` survives anywhere in the forwarded request, in
/// either its JSON or its protobuf form.
fn assert_clean(payload: &Payload, secrets: &[&str]) {
    let json = match payload {
        Payload::Logs(r) => serde_json::to_string(r),
        Payload::Metrics(r) => serde_json::to_string(r),
        Payload::Traces(r) => serde_json::to_string(r),
    }
    .unwrap();
    let wire = payload.encode_to_vec();
    for secret in secrets {
        assert!(!json.contains(secret), "{secret} survived in JSON: {json}");
        assert!(
            !wire.windows(secret.len()).any(|w| w == secret.as_bytes()),
            "{secret} survived in protobuf"
        );
    }
}

/// Give every data point an exemplar carrying the point's own attributes.
///
/// Exemplars are added to the decoded struct rather than written in the JSON
/// fixture: `opentelemetry-proto`'s JSON decoder drops a metric's whole
/// `data` when a data point has any exemplar (protobuf is unaffected), so a
/// JSON exemplar fixture would test an empty metric.
fn add_exemplar_from_point_attributes(payload: &mut Payload) {
    let Payload::Metrics(request) = payload else {
        panic!("metrics only")
    };
    for resource in &mut request.resource_metrics {
        for scope in &mut resource.scope_metrics {
            for metric in &mut scope.metrics {
                if let Some(metric::Data::Sum(sum)) = metric.data.as_mut() {
                    for point in &mut sum.data_points {
                        point.exemplars.push(Exemplar {
                            filtered_attributes: point.attributes.clone(),
                            trace_id: SECRET_KEY.as_bytes().to_vec(),
                            ..Default::default()
                        });
                    }
                }
            }
        }
    }
}

/// Attributes carrying each fixture: a secret-shaped key, a credential-named
/// number, a nested map with both, and a forged daemon key in odd case.
fn hostile_attributes() -> serde_json::Value {
    json!([
        {"key": SECRET_KEY, "value": {"stringValue": "ok"}},
        {"key": "password", "value": {"intValue": SECRET_NUMBER}},
        {"key": "api_key", "value": {"doubleValue": 123456789.5}},
        {"key": "Service.Name", "value": {"stringValue": "forged-service"}},
        {"key": "LOOM.repo", "value": {"stringValue": "forged-owner/forged-repo"}},
        {"key": "Host.Name", "value": {"stringValue": "forged-host"}},
        {"key": "nested", "value": {"kvlistValue": {"values": [
            {"key": SECRET_KEY, "value": {"stringValue": "ok"}},
            {"key": "token", "value": {"intValue": SECRET_NUMBER}},
            {"key": "Loom.Sweep_Id", "value": {"stringValue": "forged-sweep"}}]}}},
        {"key": "duration_ms", "value": {"intValue": "1500"}},
        {"key": "cost_usd", "value": {"doubleValue": 0.25}}
    ])
}

const HOSTILE: &[&str] = &[
    SECRET_KEY,
    SECRET_SCHEMA,
    SECRET_NUMBER,
    "123456789.5",
    "forged",
];

#[test]
fn keys_schema_urls_and_numbers_are_scrubbed_on_logs_in_both_encodings() {
    let attributes = hostile_attributes();
    let logs = json!({"resourceLogs": [{
        "resource": {"attributes": attributes},
        "schemaUrl": SECRET_SCHEMA,
        "scopeLogs": [{
            "scope": {"name": "fixture", "attributes": attributes},
            "schemaUrl": SECRET_SCHEMA,
            "logRecords": [{"body": {"kvlistValue": {"values": attributes}}, "attributes": attributes}]
        }]
    }]});
    for mut payload in both_encodings(Signal::Logs, &logs) {
        assert_eq!(bind_and_scrub(&mut payload, &bound()), 1);
        assert_clean(&payload, HOSTILE);
        let Payload::Logs(request) = &payload else {
            panic!()
        };
        let resource = &request.resource_logs[0];
        assert!(resource.schema_url.is_empty());
        assert!(resource.scope_logs[0].schema_url.is_empty());
        let pairs = resource_pairs(resource.resource.as_ref().unwrap());
        assert_eq!(one(&pairs, "service.name"), "claude-code");
        assert_eq!(one(&pairs, "host.name"), "host-fixture");
        // Ordinary numbers keep their type and value; a credential-named one
        // becomes a marker.
        let record = &resource.scope_logs[0].log_records[0];
        let value = |key: &str| {
            record
                .attributes
                .iter()
                .find(|kv| kv.key == key)
                .and_then(|kv| kv.value.clone())
        };
        assert_eq!(
            value("duration_ms").and_then(|v| v.value),
            Some(any_value::Value::IntValue(1500))
        );
        assert_eq!(
            value("cost_usd").and_then(|v| v.value),
            Some(any_value::Value::DoubleValue(0.25))
        );
        assert_eq!(
            value("password").and_then(|v| v.value),
            Some(any_value::Value::StringValue("[REDACTED:credential]".to_string()))
        );
    }
}

#[test]
fn keys_schema_urls_and_numbers_are_scrubbed_on_spans_and_metrics_in_both_encodings() {
    let attributes = hostile_attributes();
    let traces = json!({"resourceSpans": [{
        "resource": {"attributes": attributes},
        "schemaUrl": SECRET_SCHEMA,
        "scopeSpans": [{"schemaUrl": SECRET_SCHEMA, "scope": {"attributes": attributes}, "spans": [{
            "traceId": "0af7651916cd43dd8448eb211c80319c", "spanId": "b7ad6b7169203331",
            "name": "fixture", "attributes": attributes,
            "events": [{"name": "e", "attributes": attributes}],
            "links": [{"traceId": "0af7651916cd43dd8448eb211c80319c",
                       "spanId": "b7ad6b7169203331", "attributes": attributes}]
        }]}]
    }]});
    for mut payload in both_encodings(Signal::Traces, &traces) {
        assert_eq!(bind_and_scrub(&mut payload, &bound()), 1);
        assert_clean(&payload, HOSTILE);
    }
    let metrics = json!({"resourceMetrics": [{
        "resource": {"attributes": attributes},
        "schemaUrl": SECRET_SCHEMA,
        "scopeMetrics": [{"schemaUrl": SECRET_SCHEMA, "metrics": [{
            "name": "fixture", "metadata": attributes,
            "sum": {"dataPoints": [{"asInt": "1", "attributes": attributes}]}
        }]}]
    }]});
    for mut payload in both_encodings(Signal::Metrics, &metrics) {
        add_exemplar_from_point_attributes(&mut payload);
        assert_eq!(bind_and_scrub(&mut payload, &bound()), 1);
        assert_clean(&payload, HOSTILE);
    }
}

#[test]
fn the_daemon_owned_filter_ignores_case() {
    for key in [
        "service.name",
        "Service.Name",
        "SERVICE.INSTANCE.ID",
        "Service.Namespace",
        "LOOM.repo",
        "Loom.Sweep_Id",
        "Host.Name",
        "HOST.ID",
        " host.id",
    ] {
        assert!(daemon_owned(key), "{key}");
    }
    for key in ["service.version", "os.type", "hostname", "loomish", "model"] {
        assert!(!daemon_owned(key), "{key}");
    }
}

#[test]
fn ids_of_the_wrong_length_are_cleared_and_valid_ones_kept() {
    let mut payload = Payload::Traces(ExportTraceServiceRequest {
        resource_spans: vec![opentelemetry_proto::tonic::trace::v1::ResourceSpans {
            scope_spans: vec![opentelemetry_proto::tonic::trace::v1::ScopeSpans {
                spans: vec![opentelemetry_proto::tonic::trace::v1::Span {
                    trace_id: SECRET_KEY.as_bytes().to_vec(),
                    span_id: vec![7; 8],
                    parent_span_id: SECRET_NUMBER.as_bytes().to_vec(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    });
    bind_and_scrub(&mut payload, &bound());
    assert_clean(&payload, &[SECRET_KEY, SECRET_NUMBER]);
    let Payload::Traces(request) = &payload else {
        panic!()
    };
    let span = &request.resource_spans[0].scope_spans[0].spans[0];
    assert!(span.trace_id.is_empty());
    assert!(span.parent_span_id.is_empty());
    assert_eq!(span.span_id, vec![7; 8]);
}

// ---------------------------------------------------------------------------
// Amplification (review round 1)
// ---------------------------------------------------------------------------

#[test]
fn empty_containers_are_dropped_and_identical_resources_merged_before_binding() {
    // 10 000 empty resource containers, 10 000 containers holding only an
    // empty scope, and two record-bearing containers with the same sender
    // resource: one bound resource out.
    let mut resource_logs = vec![json!({}); 10_000];
    resource_logs.extend(vec![json!({"scopeLogs": [{}]}); 10_000]);
    let sender = json!({"attributes": [{"key": "os.type", "value": {"stringValue": "fixture"}}]});
    for name in ["first", "second"] {
        resource_logs.push(
            json!({"resource": sender, "scopeLogs": [{"logRecords": [{"eventName": name}]}]}),
        );
    }
    let logs = json!({ "resourceLogs": resource_logs });
    for mut payload in both_encodings(Signal::Logs, &logs) {
        let wire = payload.encoded_len();
        assert_eq!(bind_and_scrub(&mut payload, &bound()), 2);
        let Payload::Logs(request) = &payload else {
            panic!()
        };
        assert_eq!(request.resource_logs.len(), 1, "merged into one bound resource");
        assert_eq!(request.resource_logs[0].scope_logs.len(), 2);
        assert!(payload.encoded_len() < wire, "{} vs {wire}", payload.encoded_len());
    }
    // Different sender resources stay apart: their attributes differ.
    let distinct = json!({"resourceLogs": [
        {"resource": {"attributes": [{"key": "os.type", "value": {"stringValue": "a"}}]},
         "scopeLogs": [{"logRecords": [{}]}]},
        {"resource": {"attributes": [{"key": "os.type", "value": {"stringValue": "b"}}]},
         "scopeLogs": [{"logRecords": [{}]}]}]});
    let mut payload = decode_json(Signal::Logs, &distinct);
    bind_and_scrub(&mut payload, &bound());
    let Payload::Logs(request) = &payload else {
        panic!()
    };
    assert_eq!(request.resource_logs.len(), 2);
}

#[test]
fn metrics_and_spans_without_items_are_pruned_too() {
    let metrics = json!({"resourceMetrics": [
        {"scopeMetrics": [{"metrics": [{"name": "no-data"}, {"name": "empty", "gauge": {}}]}]},
        {"scopeMetrics": [{"metrics": [{"name": "kept", "gauge": {"dataPoints": [{"asInt": "1"}]}}]}]}]});
    let mut payload = decode_json(Signal::Metrics, &metrics);
    assert_eq!(bind_and_scrub(&mut payload, &bound()), 1);
    let Payload::Metrics(request) = &payload else {
        panic!()
    };
    assert_eq!(request.resource_metrics.len(), 1);
    assert_eq!(request.resource_metrics[0].scope_metrics[0].metrics.len(), 1);

    let traces = json!({"resourceSpans": [{}, {"scopeSpans": [{}, {"spans": []}]}]});
    let mut payload = decode_json(Signal::Traces, &traces);
    assert_eq!(bind_and_scrub(&mut payload, &bound()), 0);
    let Payload::Traces(request) = &payload else {
        panic!()
    };
    assert!(request.resource_spans.is_empty());
}
