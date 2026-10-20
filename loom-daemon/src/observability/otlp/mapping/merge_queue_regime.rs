//! OTLP mapping for `merge.queue.regime` (#12675): one INFO log record per
//! change of the merge-queue regime, stamped at `observed_at`. The body is the
//! record's JSON; the scalars ride as `loom.merge_queue.*` attributes. An
//! unknown field is omitted, never defaulted.

use opentelemetry_proto::tonic::common::v1::{any_value, AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

use super::session_output::LogParts;
use super::{kv, kv_string, nanos};
use crate::telemetry::TelemetryRecord;

fn kv_bool(key: &str, value: bool) -> KeyValue {
    kv(
        key,
        AnyValue {
            value: Some(any_value::Value::BoolValue(value)),
        },
    )
}

fn kv_int(key: &str, value: u64) -> KeyValue {
    kv(
        key,
        AnyValue {
            value: Some(any_value::Value::IntValue(i64::try_from(value).unwrap_or(i64::MAX))),
        },
    )
}

pub(super) fn log_parts(record: &TelemetryRecord) -> Option<LogParts> {
    let TelemetryRecord::MergeQueueRegime(r) = record else {
        return None;
    };
    if !r.loom.is_valid() {
        return None;
    }
    let mut attributes = vec![
        kv_bool("loom.merge_queue.regime_ambiguous", r.regime.ambiguous),
        kv_string("loom.merge_queue.version", &r.loom.version),
        kv_string("loom.merge_queue.revision", &r.loom.revision),
        kv_bool("loom.merge_queue.provenance_complete", r.loom.complete),
    ];
    if let Some(v) = r.regime.check_response_timeout_minutes {
        attributes.push(kv_int("loom.merge_queue.check_response_timeout_minutes", v));
    }
    if let Some(v) = r.regime.max_entries_to_build {
        attributes.push(kv_int("loom.merge_queue.max_entries_to_build", v));
    }
    if let Some(v) = &r.regime.grouping_strategy {
        attributes.push(kv_string("loom.merge_queue.grouping_strategy", v));
    }
    Some(LogParts {
        event_name: "merge.queue.regime",
        severity: SeverityNumber::Info,
        source_at: nanos(r.observed_at),
        observed_at: nanos(r.observed_at),
        attributes,
        body: serde_json::to_string(r).ok()?,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::super::log_record_for;
    use super::*;
    use crate::telemetry::kinds::merge_queue_regime::{
        MergeQueueRegimeRecord, QueueRegime, MERGE_QUEUE_REGIME_LOG_ATTRIBUTE_KEYS,
    };
    use crate::telemetry::provenance::Provenance;
    use crate::telemetry::TelemetryEnvelope;

    fn record(regime: QueueRegime) -> MergeQueueRegimeRecord {
        MergeQueueRegimeRecord {
            regime,
            observed_at: Utc.with_ymd_and_hms(2026, 10, 18, 12, 0, 0).unwrap(),
            loom: Provenance {
                version: "0.19.1".into(),
                revision: "9d8e226ce0123456789abcdef0123456789abcde".into(),
                tree_state: "clean".into(),
                complete: true,
            },
        }
    }

    fn keys_of(r: MergeQueueRegimeRecord) -> Vec<String> {
        let envelope = TelemetryEnvelope::new("host-1", TelemetryRecord::MergeQueueRegime(r));
        let log = log_record_for(&envelope).unwrap();
        assert_eq!(log.event_name, "merge.queue.regime");
        log.attributes.iter().map(|kv| kv.key.clone()).collect()
    }

    #[test]
    fn a_full_regime_declares_every_attribute() {
        let keys = keys_of(record(QueueRegime {
            check_response_timeout_minutes: Some(60),
            max_entries_to_build: Some(5),
            grouping_strategy: Some("ALLGREEN".into()),
            ambiguous: false,
        }));
        for key in MERGE_QUEUE_REGIME_LOG_ATTRIBUTE_KEYS {
            assert!(keys.iter().any(|k| k == key), "{key} is emitted");
        }
        for key in &keys {
            assert!(
                MERGE_QUEUE_REGIME_LOG_ATTRIBUTE_KEYS.contains(&key.as_str())
                    || ["loom.record_id", "loom.kind"].contains(&key.as_str()),
                "{key} is not allowlisted"
            );
        }
    }

    #[test]
    fn an_unknown_field_is_omitted_not_defaulted() {
        let keys = keys_of(record(QueueRegime {
            ambiguous: true,
            ..QueueRegime::default()
        }));
        assert!(keys
            .iter()
            .any(|k| k == "loom.merge_queue.regime_ambiguous"));
        assert!(!keys
            .iter()
            .any(|k| k == "loom.merge_queue.max_entries_to_build"));
        assert!(!keys
            .iter()
            .any(|k| k == "loom.merge_queue.grouping_strategy"));
        assert!(!keys
            .iter()
            .any(|k| k == "loom.merge_queue.check_response_timeout_minutes"));
    }

    #[test]
    fn the_collector_keeps_every_regime_attribute() {
        const CONFIG: &str =
            include_str!("../../../../../defaults/observability/collector/config.yaml");
        let keep = CONFIG
            .lines()
            .find(|l| l.contains("keep_keys(attributes, [") && l.contains("loom.ci.chunk_index"))
            .unwrap();
        for key in MERGE_QUEUE_REGIME_LOG_ATTRIBUTE_KEYS {
            assert!(keep.contains(&format!("\"{key}\"")), "collector drops {key}");
        }
    }
}
