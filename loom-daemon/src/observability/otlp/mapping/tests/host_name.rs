//! Issue #10977: the OTLP resource carries `host.name` on every signal.

use super::*;
use crate::telemetry::trace::{SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext};

/// The resource of one signal's first entry, as `(key, string value)` pairs in
/// emission order.
fn resource_pairs(resource: Option<&Resource>) -> Vec<(String, String)> {
    resource
        .unwrap()
        .attributes
        .iter()
        .map(|kv| match kv.value.as_ref().and_then(|v| v.value.as_ref()) {
            Some(any_value::Value::StringValue(s)) => (kv.key.clone(), s.clone()),
            other => panic!("resource attribute {} is not a string: {other:?}", kv.key),
        })
        .collect()
}

/// Issue #10977: `host.name` is on the resource of all three signals and is
/// the envelope's `host_id` (the daemon's configured host identifier), and
/// adding it left `host.id` / `service.instance.id` / `service.name` alone.
/// The exact key list is asserted so a new resource key is a deliberate edit:
/// the bundled collector's resource allowlist has to admit it too.
#[test]
fn host_name_is_the_host_id_on_logs_metrics_and_traces() {
    let now = Utc::now();
    let span = SpanRecord {
        context: TraceContext::root(true),
        parent_span_id: None,
        name: SpanName::Sweep,
        started_at: now,
        ended_at: now,
        status: SpanStatus::Ok,
        attributes: TraceAttributes::new(),
        events: vec![],
        links: vec![],
    };
    let mut health = host_health_envelope();
    health.host_id = "host-a".to_string();
    let batch = vec![
        sweep_started_envelope(),
        health,
        envelope("host-a", TelemetryRecord::Span(span)),
    ];

    let logs = build_logs_request(&batch).unwrap();
    let metrics = build_metrics_request(&batch).unwrap();
    let traces = super::super::super::traces::build_traces_request(&batch).unwrap();
    let per_signal = [
        ("logs", resource_pairs(logs.resource_logs[0].resource.as_ref())),
        ("metrics", resource_pairs(metrics.resource_metrics[0].resource.as_ref())),
        ("traces", resource_pairs(traces.resource_spans[0].resource.as_ref())),
    ];
    for (signal, pairs) in per_signal {
        let keys: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "service.name",
                "service.instance.id",
                "host.id",
                "host.name",
                "service.version"
            ],
            "{signal}: resource key set"
        );
        let get = |key: &str| {
            pairs
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("host.name"), Some("host-a"), "{signal}: host.name");
        assert_eq!(get("host.id"), Some("host-a"), "{signal}: host.id");
        assert_eq!(get("service.instance.id"), Some("host-a"), "{signal}: service.instance.id");
        assert_eq!(get("service.name"), Some("loom-daemon"), "{signal}: service.name");
    }
}
