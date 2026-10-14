//! The decoded-size estimate (#10964): large for inflating input, close to
//! the wire size for ordinary telemetry, and never a panic.
use super::*;
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value;
use opentelemetry_proto::tonic::logs::v1::ScopeLogs;
use opentelemetry_proto::tonic::trace::v1::ScopeSpans;
use prost::Message;

fn string_kv(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(value.to_string())),
        }),
        ..Default::default()
    }
}

/// A log request shaped like an agent CLI's: a few resource attributes and
/// records with a dozen short attributes each.
fn realistic_logs(records: usize) -> ExportLogsServiceRequest {
    let attributes: Vec<KeyValue> = (0..12)
        .map(|i| string_kv(&format!("event.attribute_{i}"), "fixture-value-0123456789"))
        .collect();
    ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(opentelemetry_proto::tonic::resource::v1::Resource {
                attributes: vec![
                    string_kv("service.version", "1.2.3"),
                    string_kv("os.type", "fixture"),
                ],
                ..Default::default()
            }),
            scope_logs: vec![ScopeLogs {
                log_records: (0..records)
                    .map(|_| LogRecord {
                        time_unix_nano: 1_700_000_000_000_000_000,
                        event_name: "fixture.api_request".to_string(),
                        body: Some(AnyValue {
                            value: Some(any_value::Value::StringValue("fixture body".to_string())),
                        }),
                        attributes: attributes.clone(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

#[test]
fn ordinary_telemetry_estimates_close_to_its_wire_size_in_both_encodings() {
    let request = realistic_logs(200);
    let protobuf_body = request.encode_to_vec();
    let estimate = protobuf(Signal::Logs, &protobuf_body).unwrap();
    let ratio = estimate as f64 / protobuf_body.len() as f64;
    assert!(
        ratio < 4.0,
        "protobuf estimate {estimate} for {} wire bytes",
        protobuf_body.len()
    );
    assert!(estimate > protobuf_body.len(), "struct overhead is real: {estimate}");

    let json_body = serde_json::to_vec(&request).unwrap();
    let estimate = json(&json_body);
    let ratio = estimate as f64 / json_body.len() as f64;
    assert!(ratio < 4.0, "JSON estimate {estimate} for {} wire bytes", json_body.len());
}

#[test]
fn empty_elements_are_charged_their_full_struct_size() {
    // 1000 empty spans in one scope: two wire bytes each.
    let request = ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            scope_spans: vec![ScopeSpans {
                spans: vec![Span::default(); 1000],
                ..Default::default()
            }],
            ..Default::default()
        }],
    };
    let body = request.encode_to_vec();
    let estimate = protobuf(Signal::Traces, &body).unwrap();
    assert!(estimate >= 1000 * size_of::<Span>(), "{estimate}");
    assert!(estimate / body.len() > 100, "{estimate} for {} bytes", body.len());

    let json_body = serde_json::to_vec(&request).unwrap();
    assert!(json(&json_body) >= 1000 * size_of::<Span>());
}

#[test]
fn malformed_protobuf_is_an_error_and_never_a_panic() {
    for body in [
        vec![0x0a],             // a length that never arrives
        vec![0x0a, 0x05, 0x00], // a length past the end
        vec![0x00, 0x00],       // field number zero
        vec![0x0b, 0x00],       // a group (wire type 3)
        vec![0xff; 12],         // an over-long varint
        vec![0x09, 0x00, 0x00], // a truncated fixed64
    ] {
        assert!(protobuf(Signal::Logs, &body).is_err(), "{body:?}");
    }
    assert_eq!(protobuf(Signal::Metrics, &[]).unwrap(), 0);
    // Nesting past the decoder's own limit: arrays of arrays of AnyValue.
    let mut value = AnyValue::default();
    for _ in 0..(MAX_DEPTH + 5) {
        value = AnyValue {
            value: Some(any_value::Value::ArrayValue(
                opentelemetry_proto::tonic::common::v1::ArrayValue {
                    values: vec![value],
                },
            )),
        };
    }
    let record = LogRecord {
        body: Some(value),
        ..Default::default()
    };
    let deep = ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            scope_logs: vec![ScopeLogs {
                log_records: vec![record],
                ..Default::default()
            }],
            ..Default::default()
        }],
    };
    assert!(protobuf(Signal::Logs, &deep.encode_to_vec()).is_err());
}

#[test]
fn json_charges_unknown_arrays_the_maximum_and_refuses_runaway_nesting() {
    let unknown = br#"{"resourceLogs":[{"futureField":[{},{},{}]}]}"#;
    assert!(json(unknown) >= 3 * largest_struct());
    let deep = "[".repeat(10_000);
    assert_eq!(json(deep.as_bytes()), usize::MAX);
    // Strings with escapes, braces inside strings, and garbage: no panic, and
    // a brace inside a string is not an object.
    let tricky = br#"{"attributes":[{"key":"a\"{[{","value":{"stringValue":"}]}\\"}}]}"#;
    let charged = json(tricky);
    assert!(charged < tricky.len() + 2 * largest_struct(), "{charged}");
    for garbage in [&b"}}]]"[..], b"\"", b"\\", b"{\"", &[0xff, 0xfe]] {
        let _ = json(garbage);
    }
}
