//! OTLP attributes for a `daemon.event` record (#8760): `loom.topic`, the
//! payload whole as compact JSON in `loom.payload`, and (#12230) each payload
//! scalar named in [`TYPED_PAYLOAD_KEYS`] again as a typed attribute.
//!
//! The payload's shape varies per topic, so it travels as one string; a
//! duration inside that string can be read in SigNoz only by parsing JSON per
//! row. The typed copies are opt-in per key, because the collector keeps
//! only the attribute keys it lists: a topic that wants one adds its keys
//! here and to the collector's allowlist (the first-tick record's are
//! contract-tested against it). The names match the `loom.payload.<field>`
//! names the D1 sink already flattens to, so one query reads both stores.

use opentelemetry_proto::tonic::common::v1::{any_value, AnyValue, KeyValue};

use super::{kv, kv_string};
use crate::telemetry::DaemonEventRecord;

/// Every `loom.payload.<field>` carried as a typed attribute.
const TYPED_PAYLOAD_KEYS: &[&[&str]] =
    &[crate::work_finder::startup_gate::FIRST_TICK_LOG_ATTRIBUTE_KEYS];

pub(super) fn attributes(r: &DaemonEventRecord) -> Vec<KeyValue> {
    let mut attributes = vec![
        kv_string("loom.topic", r.topic.clone()),
        kv_string("loom.payload", serde_json::to_string(&r.payload).unwrap_or_default()),
    ];
    let Some(fields) = r.payload.as_object() else {
        return attributes;
    };
    for (field, value) in fields {
        let key = format!("loom.payload.{field}");
        if !TYPED_PAYLOAD_KEYS
            .iter()
            .any(|keys| keys.contains(&key.as_str()))
        {
            continue;
        }
        let typed = match value {
            serde_json::Value::Bool(b) => any_value::Value::BoolValue(*b),
            serde_json::Value::Number(n) => match n.as_i64() {
                Some(i) => any_value::Value::IntValue(i),
                None => match n.as_f64() {
                    Some(f) => any_value::Value::DoubleValue(f),
                    None => continue,
                },
            },
            _ => continue,
        };
        attributes.push(kv(&key, AnyValue { value: Some(typed) }));
    }
    attributes
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use opentelemetry_proto::tonic::common::v1::any_value::Value;

    use super::super::build_logs_request;
    use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};
    use crate::work_finder::startup_gate::{FIRST_TICK_LOG_ATTRIBUTE_KEYS, FIRST_TICK_TOPIC};

    type Pairs = Vec<(String, Value)>;

    /// The resource's and the log record's attributes.
    fn log_for(topic: &str, payload: serde_json::Value) -> (Pairs, Pairs) {
        let record = TelemetryRecord::DaemonEvent(crate::telemetry::DaemonEventRecord {
            topic: topic.to_string(),
            payload,
        });
        let logs = build_logs_request(&[TelemetryEnvelope::new("worker-7", record)]).unwrap();
        let resource_logs = &logs.resource_logs[0];
        let pairs = |kvs: &[opentelemetry_proto::tonic::common::v1::KeyValue]| {
            kvs.iter()
                .filter_map(|kv| {
                    let value = kv.value.as_ref()?.value.clone()?;
                    Some((kv.key.clone(), value))
                })
                .collect::<Vec<_>>()
        };
        let resource = pairs(&resource_logs.resource.as_ref().unwrap().attributes);
        let log = &resource_logs.scope_logs[0].log_records[0];
        assert_eq!(log.event_name, "daemon.event");
        (resource, pairs(&log.attributes))
    }

    fn get<'a>(pairs: &'a [(String, Value)], key: &str) -> Option<&'a Value> {
        pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// #12230: the first-tick record is queryable per host by its duration —
    /// `host.name` on the resource, the seconds as a number.
    #[test]
    fn the_first_tick_record_carries_its_host_and_typed_durations() {
        let (resource, attributes) = log_for(
            FIRST_TICK_TOPIC,
            serde_json::json!({
                "secs_since_start": 94.2,
                "gate_wait_secs": 90.0,
                "jitter_delay_secs": 3,
                "startup_reconcile_cap_fired": true,
            }),
        );
        assert_eq!(get(&resource, "host.name"), Some(&Value::StringValue("worker-7".into())));
        assert_eq!(
            get(&attributes, "loom.topic"),
            Some(&Value::StringValue(FIRST_TICK_TOPIC.into()))
        );
        assert_eq!(
            get(&attributes, "loom.payload.secs_since_start"),
            Some(&Value::DoubleValue(94.2))
        );
        assert_eq!(
            get(&attributes, "loom.payload.gate_wait_secs"),
            Some(&Value::DoubleValue(90.0))
        );
        assert_eq!(get(&attributes, "loom.payload.jitter_delay_secs"), Some(&Value::IntValue(3)));
        assert_eq!(
            get(&attributes, "loom.payload.startup_reconcile_cap_fired"),
            Some(&Value::BoolValue(true))
        );
        for key in FIRST_TICK_LOG_ATTRIBUTE_KEYS {
            assert!(get(&attributes, key).is_some(), "{key} missing");
        }
        assert!(get(&attributes, "loom.payload").is_some(), "the whole payload stays");
    }

    /// Another topic's payload is not flattened: only listed keys are typed.
    #[test]
    fn other_payload_fields_stay_inside_loom_payload() {
        let (_, attributes) = log_for(
            "daemon.drain.started",
            serde_json::json!({"in_flight": 2, "timeout_secs": 300}),
        );
        let keys: Vec<&str> = attributes.iter().map(|(k, _)| k.as_str()).collect();
        assert!(
            !keys.iter().any(|k| k.starts_with("loom.payload.")),
            "unexpected typed payload attribute in {keys:?}"
        );
    }

    #[test]
    fn the_collector_keeps_every_first_tick_attribute() {
        const CONFIG: &str =
            include_str!("../../../../../defaults/observability/collector/config.yaml");
        let keep = CONFIG
            .lines()
            .find(|l| l.contains("keep_keys(attributes, [") && l.contains("loom.ci.chunk_index"))
            .expect("the transform/privacy log keep_keys line");
        for key in FIRST_TICK_LOG_ATTRIBUTE_KEYS {
            assert!(keep.contains(&format!("\"{key}\"")), "collector drops {key}");
        }
    }
}
