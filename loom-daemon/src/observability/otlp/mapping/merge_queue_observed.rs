//! OTLP mapping for `merge.queue.observed` (#12667): one INFO log record per
//! queue read, stamped at `observed_at`. The body is the record's JSON; the
//! scalars ride as `loom.merge_queue.*` attributes.

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

fn kv_int(key: &str, value: i64) -> KeyValue {
    kv(
        key,
        AnyValue {
            value: Some(any_value::Value::IntValue(value)),
        },
    )
}

pub(super) fn log_parts(record: &TelemetryRecord) -> Option<LogParts> {
    let TelemetryRecord::MergeQueueObserved(r) = record else {
        return None;
    };
    if !r.loom.is_valid() {
        return None;
    }
    let mut attributes = vec![
        kv_string("loom.merge_queue.queue_state", &r.state),
        kv_bool("loom.merge_queue.queue_complete", r.complete),
        kv_string("loom.merge_queue.version", &r.loom.version),
        kv_string("loom.merge_queue.revision", &r.loom.revision),
        kv_bool("loom.merge_queue.provenance_complete", r.loom.complete),
    ];
    // An unobserved queue has no depth: omitted, never 0.
    if r.state == "observed" {
        attributes.push(kv_int(
            "loom.merge_queue.queue_depth",
            i64::try_from(r.entries.len()).unwrap_or(i64::MAX),
        ));
    }
    Some(LogParts {
        event_name: "merge.queue.observed",
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
    use crate::telemetry::kinds::merge_queue_observed::{
        MergeQueueObservedRecord, QueueEntryObservation, MERGE_QUEUE_OBSERVED_LOG_ATTRIBUTE_KEYS,
    };
    use crate::telemetry::provenance::Provenance;
    use crate::telemetry::TelemetryEnvelope;

    fn record(state: &str) -> MergeQueueObservedRecord {
        let observed = state == "observed";
        MergeQueueObservedRecord {
            state: state.into(),
            complete: observed,
            order: if observed { vec![5, 6] } else { vec![] },
            entries: if observed {
                vec![
                    QueueEntryObservation {
                        position: 1,
                        pr: 5,
                        head: "a".into(),
                        has_group_commit: true,
                    },
                    QueueEntryObservation {
                        position: 2,
                        pr: 6,
                        head: "b".into(),
                        has_group_commit: false,
                    },
                ]
            } else {
                vec![]
            },
            observed_at: Utc.with_ymd_and_hms(2026, 10, 17, 12, 0, 0).unwrap(),
            loom: Provenance {
                version: "0.19.1".into(),
                revision: "9d8e226ce0123456789abcdef0123456789abcde".into(),
                tree_state: "clean".into(),
                complete: true,
            },
        }
    }

    fn keys_of(r: MergeQueueObservedRecord) -> Vec<String> {
        let envelope = TelemetryEnvelope::new("host-1", TelemetryRecord::MergeQueueObserved(r));
        let log = log_record_for(&envelope).unwrap();
        assert_eq!(log.event_name, "merge.queue.observed");
        log.attributes.iter().map(|kv| kv.key.clone()).collect()
    }

    #[test]
    fn an_observed_queue_declares_every_attribute() {
        let keys = keys_of(record("observed"));
        for key in MERGE_QUEUE_OBSERVED_LOG_ATTRIBUTE_KEYS {
            assert!(keys.iter().any(|k| k == key), "{key} is emitted");
        }
        for key in &keys {
            assert!(
                MERGE_QUEUE_OBSERVED_LOG_ATTRIBUTE_KEYS.contains(&key.as_str())
                    || ["loom.record_id", "loom.kind"].contains(&key.as_str()),
                "{key} is not allowlisted"
            );
        }
    }

    #[test]
    fn an_unknown_queue_has_no_depth_not_zero() {
        let keys = keys_of(record("unknown"));
        assert!(!keys.iter().any(|k| k == "loom.merge_queue.queue_depth"));
        assert!(keys.iter().any(|k| k == "loom.merge_queue.queue_state"));
    }

    #[test]
    fn the_collector_keeps_every_queue_observed_attribute() {
        const CONFIG: &str =
            include_str!("../../../../../defaults/observability/collector/config.yaml");
        let keep = CONFIG
            .lines()
            .find(|l| l.contains("keep_keys(attributes, [") && l.contains("loom.ci.chunk_index"))
            .unwrap();
        for key in MERGE_QUEUE_OBSERVED_LOG_ATTRIBUTE_KEYS {
            assert!(keep.contains(&format!("\"{key}\"")), "collector drops {key}");
        }
    }
}
