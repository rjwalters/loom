//! Issue #10977: the OTLP resource carries `host.name` on every signal.
//! Issue #12874: and the host's static shape (CPU count, RAM, arch, type).

use opentelemetry_proto::tonic::resource::v1::Resource;

use super::*;
use crate::telemetry::trace::{SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext};

/// The resource of one signal's first entry, as `(key, rendered value)` pairs
/// in emission order. Only the host-shape counts are ints; everything else is
/// a string.
fn resource_pairs(resource: Option<&Resource>) -> Vec<(String, String)> {
    resource
        .unwrap()
        .attributes
        .iter()
        .map(|kv| match kv.value.as_ref().and_then(|v| v.value.as_ref()) {
            Some(any_value::Value::StringValue(s)) => (kv.key.clone(), s.clone()),
            Some(any_value::Value::IntValue(i))
                if kv.key == "host.cpu.count" || kv.key == "loom.host.mem_total_bytes" =>
            {
                (kv.key.clone(), i.to_string())
            }
            other => panic!("resource attribute {} has an unexpected type: {other:?}", kv.key),
        })
        .collect()
}

/// The resource pairs of all three signals for one mixed batch.
fn per_signal_pairs() -> [(&'static str, Vec<(String, String)>); 3] {
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
    [
        ("logs", resource_pairs(logs.resource_logs[0].resource.as_ref())),
        ("metrics", resource_pairs(metrics.resource_metrics[0].resource.as_ref())),
        ("traces", resource_pairs(traces.resource_spans[0].resource.as_ref())),
    ]
}

/// The resource keys this host emits with no cloud shape set: the seven
/// identity/provenance keys, then the host shape (#12874) — RAM only when
/// measured, `host.type` only on a host with a model (macOS `hw.model`).
fn expected_keys_without_cloud() -> Vec<&'static str> {
    let shape = crate::host_shape::static_shape();
    let mut keys = vec![
        "service.name",
        "service.instance.id",
        "host.id",
        "host.name",
        "service.version",
        "loom.daemon.revision",
        "loom.fleet.config_sha",
        "host.cpu.count",
    ];
    if shape.mem_total_bytes.is_some() {
        keys.push("loom.host.mem_total_bytes");
    }
    keys.push("host.arch");
    if shape.model.is_some() {
        keys.push("host.type");
    }
    keys
}

/// Issue #10977: `host.name` is on the resource of all three signals and is
/// the envelope's `host_id` (the daemon's configured host identifier), and
/// adding it left `host.id` / `service.instance.id` / `service.name` alone.
/// The exact key list is asserted so a new resource key is a deliberate edit:
/// the bundled collector's resource allowlist has to admit it too.
#[test]
fn host_name_is_the_host_id_on_logs_metrics_and_traces() {
    crate::host_shape::set_cloud_for_test(None);
    let per_signal = per_signal_pairs();
    for (signal, pairs) in per_signal {
        let keys: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, expected_keys_without_cloud(), "{signal}: resource key set");
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

/// Issue #12874: the static host shape rides every signal, and `host.arch`
/// uses the OTel vocabulary.
#[test]
fn host_shape_is_on_logs_metrics_and_traces() {
    crate::host_shape::set_cloud_for_test(None);
    let shape = crate::host_shape::static_shape();
    for (signal, pairs) in per_signal_pairs() {
        let get = |key: &str| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        assert_eq!(get("host.cpu.count"), Some(shape.cpu_count.to_string()), "{signal}");
        assert_eq!(
            get("loom.host.mem_total_bytes"),
            shape.mem_total_bytes.map(|b| b.to_string()),
            "{signal}"
        );
        assert_ne!(get("loom.host.mem_total_bytes").as_deref(), Some("0"), "{signal}");
        let arch = get("host.arch").unwrap();
        assert!(!["x86_64", "aarch64"].contains(&arch.as_str()), "{signal}: {arch}");
        assert_eq!(get("host.type"), shape.model.clone(), "{signal}: host.type");
        assert_eq!(get("cloud.provider"), None, "{signal}: no cloud.* without IMDS");
        assert_eq!(get("cloud.region"), None, "{signal}: no cloud.* without IMDS");
    }
}

/// Issue #12874: once the IMDS probe has filled the cloud cell, every signal
/// carries `host.type` (the instance type, winning over any `hw.model`) and
/// `cloud.provider` / `cloud.region` — each exactly once.
#[test]
fn cloud_shape_is_on_logs_metrics_and_traces_once_probed() {
    crate::host_shape::set_cloud_for_test(Some(crate::host_shape::CloudShape {
        provider: "aws",
        instance_type: "c7i.2xlarge".to_string(),
        region: "us-east-1".to_string(),
    }));
    let per_signal = per_signal_pairs();
    crate::host_shape::set_cloud_for_test(None);
    for (signal, pairs) in per_signal {
        let all = |key: &str| {
            pairs
                .iter()
                .filter(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(all("host.type"), ["c7i.2xlarge"], "{signal}");
        assert_eq!(all("cloud.provider"), ["aws"], "{signal}");
        assert_eq!(all("cloud.region"), ["us-east-1"], "{signal}");
    }
}

/// Issue #12874: every resource key `resource_for_host` can emit is admitted
/// by all three of the bundled gateway collector's `context: resource`
/// `keep_keys` lists (logs, traces, metrics) — a new key that the collector
/// would silently drop fails here instead of in SigNoz.
#[test]
fn gateway_collector_keeps_every_resource_key() {
    const CONFIG: &str =
        include_str!("../../../../../../defaults/observability/collector/config.yaml");
    let mut current = String::new();
    let mut lists: Vec<std::collections::BTreeSet<String>> = Vec::new();
    for line in CONFIG.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("- context:") {
            current = rest.trim().to_string();
        }
        if current == "resource" && trimmed.contains("keep_keys(") {
            lists.push(
                trimmed
                    .split('"')
                    .skip(1)
                    .step_by(2)
                    .map(str::to_string)
                    .collect(),
            );
        }
    }
    assert_eq!(lists.len(), 3, "expected the logs, traces and metrics resource lists");
    let keys = [
        "service.name",
        "service.version",
        "service.instance.id",
        "host.id",
        "host.name",
        "loom.daemon.revision",
        "loom.fleet.config_sha",
    ]
    .into_iter()
    .chain(crate::host_shape::RESOURCE_KEYS);
    for key in keys {
        for (index, keep) in lists.iter().enumerate() {
            assert!(keep.contains(key), "resource keep_keys list #{index} lacks {key}");
        }
    }
}
