//! OTLP mapping for `merge.authorization.evaluated` (#12666): one INFO log
//! record per merge-group authorization evaluation, stamped at the evaluation.
//! The body is the record's JSON; the scalars ride as
//! `loom.merge_authorization.*` attributes, with `loom.pr_number` for the
//! entry the group was built for.

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
    let TelemetryRecord::MergeAuthorization(r) = record else {
        return None;
    };
    if !r.loom.is_valid() {
        return None;
    }
    let mut attributes = vec![
        kv_string("loom.merge_authorization.reason", &r.reason),
        kv_string("loom.merge_authorization.version", &r.loom.version),
        kv_string("loom.merge_authorization.revision", &r.loom.revision),
        kv_bool("loom.merge_authorization.provenance_complete", r.loom.complete),
    ];
    if let Some(pr) = r.pr {
        attributes.push(kv_int("loom.pr_number", i64::from(pr)));
    }
    if let Some(write) = &r.status_write {
        attributes.push(kv_string("loom.merge_authorization.status_write", write));
    }
    if let Some(confirmed) = r.status_confirmed {
        attributes.push(kv_bool("loom.merge_authorization.status_confirmed", confirmed));
    }
    for (key, value) in [
        ("loom.merge_authorization.members", r.members),
        ("loom.merge_authorization.other_total", r.other_total),
        ("loom.merge_authorization.other_success", r.other_success),
        ("loom.merge_authorization.other_pending", r.other_pending),
        ("loom.merge_authorization.other_failure", r.other_failure),
    ] {
        if let Some(n) = value {
            attributes.push(kv_int(key, i64::from(n)));
        }
    }
    Some(LogParts {
        event_name: "merge.authorization.evaluated",
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
    use crate::telemetry::kinds::merge_authorization::{
        MergeAuthorizationRecord, MERGE_AUTHORIZATION_LOG_ATTRIBUTE_KEYS,
    };
    use crate::telemetry::provenance::Provenance;
    use crate::telemetry::TelemetryEnvelope;

    fn record() -> MergeAuthorizationRecord {
        MergeAuthorizationRecord {
            commit: "9".repeat(40),
            reason: "pending-other-checks".into(),
            pr: Some(42),
            members: Some(2),
            status_write: Some("posted".into()),
            status_confirmed: Some(true),
            other_total: Some(3),
            other_success: Some(1),
            other_pending: Some(1),
            other_failure: Some(1),
            at: Utc.with_ymd_and_hms(2026, 10, 17, 12, 0, 0).unwrap(),
            loom: Provenance {
                version: "0.19.1".into(),
                revision: "9d8e226ce0123456789abcdef0123456789abcde".into(),
                tree_state: "clean".into(),
                complete: true,
            },
        }
    }

    fn keys_of(r: MergeAuthorizationRecord) -> Vec<String> {
        let envelope = TelemetryEnvelope::new("host-1", TelemetryRecord::MergeAuthorization(r));
        let log = log_record_for(&envelope).unwrap();
        assert_eq!(log.event_name, "merge.authorization.evaluated");
        log.attributes.iter().map(|kv| kv.key.clone()).collect()
    }

    #[test]
    fn every_attribute_is_declared_and_emitted() {
        let keys = keys_of(record());
        for key in MERGE_AUTHORIZATION_LOG_ATTRIBUTE_KEYS {
            assert!(keys.iter().any(|k| k == key), "{key} is emitted");
        }
        for key in &keys {
            assert!(
                MERGE_AUTHORIZATION_LOG_ATTRIBUTE_KEYS.contains(&key.as_str())
                    || ["loom.record_id", "loom.kind", "loom.pr_number"].contains(&key.as_str()),
                "{key} is not allowlisted"
            );
        }
    }

    #[test]
    fn unread_fields_are_omitted_not_defaulted() {
        let mut r = record();
        r.pr = None;
        r.members = None;
        r.status_write = None;
        r.status_confirmed = None;
        r.other_total = None;
        r.other_success = None;
        r.other_pending = None;
        r.other_failure = None;
        let keys = keys_of(r);
        for absent in [
            "loom.pr_number",
            "loom.merge_authorization.members",
            "loom.merge_authorization.status_write",
            "loom.merge_authorization.status_confirmed",
            "loom.merge_authorization.other_total",
            "loom.merge_authorization.other_success",
            "loom.merge_authorization.other_pending",
            "loom.merge_authorization.other_failure",
        ] {
            assert!(!keys.iter().any(|k| k == absent), "{absent}");
        }
    }

    #[test]
    fn the_collector_keeps_every_merge_authorization_attribute() {
        const CONFIG: &str =
            include_str!("../../../../../defaults/observability/collector/config.yaml");
        let keep = CONFIG
            .lines()
            .find(|l| l.contains("keep_keys(attributes, [") && l.contains("loom.ci.chunk_index"))
            .unwrap();
        for key in MERGE_AUTHORIZATION_LOG_ATTRIBUTE_KEYS {
            assert!(keep.contains(&format!("\"{key}\"")), "collector drops {key}");
        }
    }
}
