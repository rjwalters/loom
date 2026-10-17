//! OTLP mapping for `merge_queue.event` (#12653, F3): one INFO log record per
//! merge-queue record the sink appended, stamped at the event. The body is the
//! record's JSON; the scalars ride as `loom.merge_queue.*` attributes, with
//! `loom.pr_number` for the pull request.

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
    let TelemetryRecord::MergeQueueEvent(r) = record else {
        return None;
    };
    if !r.loom.is_valid() {
        return None;
    }
    let mut attributes = vec![
        kv_string("loom.merge_queue.event", &r.event),
        kv_int("loom.pr_number", i64::from(r.pr)),
        kv_int("loom.merge_queue.nonce", i64::try_from(r.nonce).unwrap_or(i64::MAX)),
        kv_bool("loom.merge_queue.merged_after_revocation", r.merged_after_revocation),
        kv_string("loom.merge_queue.version", &r.loom.version),
        kv_string("loom.merge_queue.revision", &r.loom.revision),
        kv_bool("loom.merge_queue.provenance_complete", r.loom.complete),
    ];
    if let Some(reason) = &r.reason {
        attributes.push(kv_string("loom.merge_queue.reason", reason));
    }
    if let Some(secs) = r.enqueue_to_event_secs {
        attributes.push(kv_int("loom.merge_queue.enqueue_to_event_secs", secs));
    }
    Some(LogParts {
        event_name: "merge_queue.event",
        severity: SeverityNumber::Info,
        source_at: nanos(r.at),
        observed_at: nanos(r.at),
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
    use crate::telemetry::kinds::merge_queue_event::{
        MergeQueueEventRecord, MERGE_QUEUE_EVENT_LOG_ATTRIBUTE_KEYS,
    };
    use crate::telemetry::provenance::Provenance;
    use crate::telemetry::TelemetryEnvelope;

    fn record() -> MergeQueueEventRecord {
        MergeQueueEventRecord {
            event: "removed".into(),
            pr: 42,
            nonce: 3,
            head: Some("abc".into()),
            reason: Some("checks-timed-out".into()),
            enqueued_at: Some(Utc.with_ymd_and_hms(2026, 10, 17, 11, 0, 0).unwrap()),
            at: Utc.with_ymd_and_hms(2026, 10, 17, 12, 0, 0).unwrap(),
            enqueue_to_event_secs: Some(3600),
            merged_after_revocation: false,
            loom: Provenance {
                version: "0.19.1".into(),
                revision: "9d8e226ce0123456789abcdef0123456789abcde".into(),
                tree_state: "clean".into(),
                complete: true,
            },
        }
    }

    #[test]
    fn every_attribute_is_declared_and_stamped_at_the_event() {
        let r = record();
        let envelope =
            TelemetryEnvelope::new("host-1", TelemetryRecord::MergeQueueEvent(r.clone()));
        let log = log_record_for(&envelope).unwrap();
        assert_eq!(log.event_name, "merge_queue.event");
        assert_eq!(log.time_unix_nano, nanos(r.at));
        let keys: Vec<&str> = log.attributes.iter().map(|kv| kv.key.as_str()).collect();
        for key in MERGE_QUEUE_EVENT_LOG_ATTRIBUTE_KEYS {
            assert!(keys.contains(key), "{key} is emitted");
        }
        for key in keys {
            assert!(
                MERGE_QUEUE_EVENT_LOG_ATTRIBUTE_KEYS.contains(&key)
                    || ["loom.record_id", "loom.kind", "loom.pr_number"].contains(&key),
                "{key} is not allowlisted"
            );
        }
    }

    #[test]
    fn a_missing_latency_and_reason_are_omitted_not_guessed() {
        let mut r = record();
        r.reason = None;
        r.enqueue_to_event_secs = None;
        let envelope = TelemetryEnvelope::new("host-1", TelemetryRecord::MergeQueueEvent(r));
        let log = log_record_for(&envelope).unwrap();
        for key in [
            "loom.merge_queue.reason",
            "loom.merge_queue.enqueue_to_event_secs",
        ] {
            assert!(!log.attributes.iter().any(|kv| kv.key == key), "{key}");
        }
    }

    #[test]
    fn the_collector_keeps_every_merge_queue_attribute() {
        const CONFIG: &str =
            include_str!("../../../../../defaults/observability/collector/config.yaml");
        let keep = CONFIG
            .lines()
            .find(|l| l.contains("keep_keys(attributes, [") && l.contains("loom.ci.chunk_index"))
            .unwrap();
        for key in MERGE_QUEUE_EVENT_LOG_ATTRIBUTE_KEYS {
            assert!(keep.contains(&format!("\"{key}\"")), "collector drops {key}");
        }
    }
}
