//! Typed OTLP attributes for the daemon lifecycle `daemon.event`s (Issue
//! #10023): `daemon.start`, `daemon.shutdown`, `daemon.heartbeat`.
//!
//! A generic `daemon.event` carries its payload as one `loom.payload` string,
//! which the attribute bound (`metadata::bounded`, 256 bytes) omits when too
//! long — and a heartbeat's per-exporter list is. So the fields a SigNoz
//! query filters or groups on are lifted into their own scalar attributes
//! (each admitted by the gateway's log `keep_keys`), and the full payload
//! rides the log **body**, which is not attribute-bounded.

use opentelemetry_proto::tonic::common::v1::KeyValue;

use crate::observability::daemon_lifecycle::{TOPIC_HEARTBEAT, TOPIC_SHUTDOWN, TOPIC_START};

/// Whether `topic` is one of the three lifecycle topics.
pub(super) fn is_lifecycle(topic: &str) -> bool {
    matches!(topic, TOPIC_START | TOPIC_SHUTDOWN | TOPIC_HEARTBEAT)
}

/// `(payload field, attribute key)` for string-valued fields.
const STRING_FIELDS: &[(&str, &str)] = &[
    ("version", "loom.daemon.version"),
    ("build_commit", "loom.daemon.revision"),
    ("build_tree_state", "loom.daemon.tree_state"),
    ("supervisor", "loom.daemon.supervisor"),
    ("reason", "loom.daemon.exit_reason"),
    ("last_success_at", "loom.export.last_success_at"),
    ("last_failure_at", "loom.export.last_failure_at"),
];

/// `(payload field, attribute key)` for integer-valued fields.
const INT_FIELDS: &[(&str, &str)] = &[
    ("exit_code", "loom.daemon.exit_code"),
    ("uptime_sec", "loom.daemon.uptime_sec"),
    ("queued", "loom.export.queued"),
    ("dropped", "loom.export.dropped"),
];

/// Every attribute key this module can emit — the gateway `keep_keys`
/// contract test iterates it.
#[cfg(test)]
pub(super) fn attribute_keys() -> impl Iterator<Item = &'static str> {
    STRING_FIELDS
        .iter()
        .chain(INT_FIELDS.iter())
        .map(|(_, key)| *key)
}

/// The typed attributes present in `payload`. An absent or `null` field
/// (e.g. `last_success_at` before the first successful export) emits nothing,
/// never a fabricated zero or empty string.
pub(super) fn attributes(payload: &serde_json::Value) -> Vec<KeyValue> {
    let mut out = Vec::new();
    for (field, key) in STRING_FIELDS {
        if let Some(value) = payload.get(field).and_then(serde_json::Value::as_str) {
            out.push(super::kv_string(key, value));
        }
    }
    for (field, key) in INT_FIELDS {
        if let Some(value) = payload.get(field).and_then(serde_json::Value::as_i64) {
            out.push(super::kv_int(key, value));
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::time::Duration;

    use opentelemetry_proto::tonic::common::v1::any_value;

    use crate::observability::daemon_lifecycle::{heartbeat_record, shutdown_record, start_record};
    use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};

    fn resource_host_id(resource: &opentelemetry_proto::tonic::resource::v1::Resource) -> String {
        let value = resource
            .attributes
            .iter()
            .find(|kv| kv.key == "host.id")
            .and_then(|kv| kv.value.as_ref())
            .and_then(|v| v.value.clone());
        match value {
            Some(any_value::Value::StringValue(s)) => s,
            other => panic!("host.id missing or not a string: {other:?}"),
        }
    }

    fn attr(
        log: &opentelemetry_proto::tonic::logs::v1::LogRecord,
        key: &str,
    ) -> Option<any_value::Value> {
        log.attributes
            .iter()
            .find(|kv| kv.key == key)
            .and_then(|kv| kv.value.as_ref())
            .and_then(|v| v.value.clone())
    }

    fn string(value: &str) -> Option<any_value::Value> {
        Some(any_value::Value::StringValue(value.to_string()))
    }

    /// AC: start / shutdown / heartbeat are visible through the OTLP mapping
    /// as `daemon.event` logs on this host's Resource, with their queryable
    /// fields as typed attributes and the full payload in the body.
    #[test]
    fn lifecycle_records_map_to_typed_logs_on_the_host_resource() {
        let host = "loom-host-0123456789ab";
        let batch = vec![
            TelemetryEnvelope::new(host, start_record("systemd")),
            TelemetryEnvelope::new(host, heartbeat_record(&[], Duration::from_secs(120))),
            TelemetryEnvelope::new(host, shutdown_record(143, Duration::from_secs(300))),
        ];
        let request = super::super::build_logs_request(&batch).expect("lifecycle logs");
        assert_eq!(request.resource_logs.len(), 1, "one host ⇒ one Resource");
        let resource_logs = &request.resource_logs[0];
        assert_eq!(resource_host_id(resource_logs.resource.as_ref().unwrap()), host);
        let logs = &resource_logs.scope_logs[0].log_records;
        assert_eq!(logs.len(), 3);
        for log in logs {
            assert_eq!(log.event_name, "daemon.event");
            assert_eq!(attr(log, "loom.kind"), string("daemon.event"));
        }

        let start = &logs[0];
        assert_eq!(attr(start, "loom.topic"), string("daemon.start"));
        assert_eq!(attr(start, "loom.daemon.version"), string(env!("CARGO_PKG_VERSION")));
        assert_eq!(
            attr(start, "loom.daemon.revision"),
            string(crate::self_update::BUILT_COMMIT_FULL)
        );
        assert_eq!(attr(start, "loom.daemon.supervisor"), string("systemd"));
        let Some(any_value::Value::StringValue(body)) =
            start.body.as_ref().and_then(|b| b.value.clone())
        else {
            panic!("body must be the payload JSON");
        };
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["supervisor"], "systemd");

        let heartbeat = &logs[1];
        assert_eq!(attr(heartbeat, "loom.topic"), string("daemon.heartbeat"));
        assert_eq!(attr(heartbeat, "loom.export.queued"), Some(any_value::Value::IntValue(0)));
        assert_eq!(attr(heartbeat, "loom.export.dropped"), Some(any_value::Value::IntValue(0)));
        assert_eq!(
            attr(heartbeat, "loom.daemon.uptime_sec"),
            Some(any_value::Value::IntValue(120))
        );
        assert!(
            attr(heartbeat, "loom.export.last_success_at").is_none(),
            "never exported ⇒ absent, not a fabricated timestamp"
        );

        let shutdown = &logs[2];
        assert_eq!(attr(shutdown, "loom.topic"), string("daemon.shutdown"));
        assert_eq!(attr(shutdown, "loom.daemon.exit_code"), Some(any_value::Value::IntValue(143)));
        assert_eq!(attr(shutdown, "loom.daemon.exit_reason"), string("stop"));
    }

    /// Every typed attribute key is admitted by the gateway's log allowlist.
    #[test]
    fn every_typed_lifecycle_attribute_survives_the_gateway() {
        const CONFIG: &str =
            include_str!("../../../../../defaults/observability/collector/config.yaml");
        let log_keep = CONFIG
            .lines()
            .find(|l| l.contains("keep_keys(attributes, [") && l.contains("loom.topic"))
            .unwrap();
        for key in super::attribute_keys() {
            assert!(log_keep.contains(&format!("\"{key}\"")), "collector drops {key}");
        }
    }

    /// A non-lifecycle `daemon.event` keeps its pre-#10023 shape: no typed
    /// lifecycle attributes, body = event name.
    #[test]
    fn other_daemon_events_are_unchanged() {
        let record = TelemetryRecord::DaemonEvent(crate::telemetry::DaemonEventRecord {
            topic: "daemon.drain.started".into(),
            payload: serde_json::json!({"version": "x", "queued": 3}),
        });
        let request =
            super::super::build_logs_request(&[TelemetryEnvelope::new("h", record)]).unwrap();
        let log = &request.resource_logs[0].scope_logs[0].log_records[0];
        assert!(attr(log, "loom.daemon.version").is_none());
        assert!(attr(log, "loom.export.queued").is_none());
        assert_eq!(log.body.as_ref().and_then(|b| b.value.clone()), string("daemon.event"));
    }

    /// AC: logs and metrics Resources report the same `host.id` for one
    /// host — both are built from the envelope's `host_id` (which spawn_task
    /// stamps from a single `host_identity()` call).
    #[test]
    fn logs_and_metrics_share_the_host_resource() {
        let host = "loom-host-shared";
        let health: crate::telemetry::HostHealthRecord =
            serde_json::from_value(serde_json::json!({
                "captured_at": chrono::Utc::now(),
                "daemon_version": "0.19.0",
                "build_commit": "deadbeef",
                "uptime_sec": 10,
                "logical_cpus": 8,
                "active_sweep_ids": [],
                "dispatch_halted": false,
                "managed_repos": [],
            }))
            .unwrap();
        let batch = vec![
            TelemetryEnvelope::new(host, start_record("none")),
            TelemetryEnvelope::new(host, TelemetryRecord::HostHealth(health)),
        ];
        let logs = super::super::build_logs_request(&batch).unwrap();
        let metrics = super::super::build_metrics_request(&batch).unwrap();
        assert_eq!(resource_host_id(logs.resource_logs[0].resource.as_ref().unwrap()), host);
        assert_eq!(resource_host_id(metrics.resource_metrics[0].resource.as_ref().unwrap()), host);
    }
}
