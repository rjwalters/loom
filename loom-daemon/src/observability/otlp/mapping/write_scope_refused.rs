//! OTLP mapping for `write_scope.refused` (#12168): one WARN log record per
//! daemon-pass write the #9548 gate refused, stamped at the refusal. The body is
//! the record's JSON; the scalars ride as `loom.write_scope.*` attributes, with
//! `loom.repo` when the refused repository is known.

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

pub(super) fn log_parts(record: &TelemetryRecord) -> Option<LogParts> {
    let TelemetryRecord::WriteScopeRefused(r) = record else {
        return None;
    };
    if !r.loom.is_valid() {
        return None;
    }
    let mut attributes = vec![
        kv_string("loom.write_scope.reason", r.reason.as_str()),
        kv_string("loom.write_scope.credential", &r.credential),
        kv_string("loom.write_scope.call_site", &r.call_site),
        kv_string("loom.write_scope.version", &r.loom.version),
        kv_string("loom.write_scope.revision", &r.loom.revision),
        kv_bool("loom.write_scope.provenance_complete", r.loom.complete),
    ];
    if let Some(repo) = &r.repo {
        attributes.push(kv_string("loom.repo", repo));
    }
    Some(LogParts {
        event_name: "write_scope.refused",
        severity: SeverityNumber::Warn,
        source_at: nanos(r.refused_at),
        observed_at: nanos(r.refused_at),
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
    use crate::telemetry::kinds::write_scope_refused::{
        RefusalClass, WriteScopeRefusedRecord, WRITE_SCOPE_REFUSED_LOG_ATTRIBUTE_KEYS,
    };
    use crate::telemetry::provenance::Provenance;
    use crate::telemetry::TelemetryEnvelope;

    fn record(repo: Option<&str>) -> WriteScopeRefusedRecord {
        WriteScopeRefusedRecord {
            repo: repo.map(Into::into),
            credential: "gh-config:acme".into(),
            call_site: "claim reconciliation".into(),
            reason: RefusalClass::NotInInstallation,
            refused_at: Utc.with_ymd_and_hms(2026, 10, 15, 12, 0, 0).unwrap(),
            loom: Provenance {
                version: "0.19.1".into(),
                revision: "9d8e226ce0123456789abcdef0123456789abcde".into(),
                tree_state: "clean".into(),
                complete: true,
            },
        }
    }

    #[test]
    fn every_attribute_is_declared_and_stamped_at_the_refusal() {
        let r = record(Some("acme/widgets"));
        let envelope =
            TelemetryEnvelope::new("host-1", TelemetryRecord::WriteScopeRefused(r.clone()));
        let log = log_record_for(&envelope).unwrap();
        assert_eq!(log.event_name, "write_scope.refused");
        assert_eq!(log.time_unix_nano, nanos(r.refused_at));
        assert_eq!(log.severity_number, SeverityNumber::Warn as i32);
        let keys: Vec<&str> = log.attributes.iter().map(|kv| kv.key.as_str()).collect();
        for key in WRITE_SCOPE_REFUSED_LOG_ATTRIBUTE_KEYS {
            assert!(keys.contains(key), "{key} is emitted");
        }
        for key in keys {
            assert!(
                WRITE_SCOPE_REFUSED_LOG_ATTRIBUTE_KEYS.contains(&key)
                    || ["loom.record_id", "loom.kind", "loom.repo"].contains(&key),
                "{key} is not allowlisted"
            );
        }
    }

    #[test]
    fn an_unknown_repo_omits_the_repo_attribute() {
        let envelope =
            TelemetryEnvelope::new("host-1", TelemetryRecord::WriteScopeRefused(record(None)));
        let log = log_record_for(&envelope).unwrap();
        assert!(!log.attributes.iter().any(|kv| kv.key == "loom.repo"));
    }

    #[test]
    fn the_collector_keeps_every_write_scope_attribute() {
        const CONFIG: &str =
            include_str!("../../../../../defaults/observability/collector/config.yaml");
        let keep = CONFIG
            .lines()
            .find(|l| l.contains("keep_keys(attributes, [") && l.contains("loom.ci.chunk_index"))
            .unwrap();
        for key in WRITE_SCOPE_REFUSED_LOG_ATTRIBUTE_KEYS {
            assert!(keep.contains(&format!("\"{key}\"")), "collector drops {key}");
        }
    }
}
