//! OTLP mapping for `scope.memory` (#12241): one log record per ended agent
//! scope, stamped and observed when the daemon saw it gone. WARN when the
//! scope had OOM kills, INFO otherwise. The body is the record's JSON; the
//! scalars ride as `loom.scope.*` attributes.

use super::session_output::LogParts;
use super::{kv, kv_int, kv_string, nanos};
use crate::telemetry::TelemetryRecord;
use opentelemetry_proto::tonic::common::v1::{any_value, AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

fn kv_bool(key: &str, value: bool) -> KeyValue {
    kv(
        key,
        AnyValue {
            value: Some(any_value::Value::BoolValue(value)),
        },
    )
}

fn int(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

pub(super) fn log_parts(record: &TelemetryRecord) -> Option<LogParts> {
    let TelemetryRecord::ScopeMemory(r) = record else {
        return None;
    };
    if !r.loom.is_valid() {
        return None;
    }
    let mut attributes = vec![
        kv_string("loom.role", &r.role),
        kv_string("loom.scope.unit", &r.scope),
        kv_string("loom.scope.workspace", &r.workspace),
        kv_string("loom.scope.limit_source", r.limit_source.as_str()),
        kv_int("loom.scope.peak_bytes", int(r.peak_bytes)),
        kv_int("loom.scope.events_max", int(r.events_max)),
        kv_int("loom.scope.oom_kills", int(r.oom_kills)),
        kv_bool("loom.scope.clipped", r.clipped),
    ];
    if let Some(repo) = &r.repo {
        attributes.push(kv_string("loom.repo", repo));
    }
    if let Some(v) = &r.repo_visibility {
        attributes.push(kv_string("loom.repo.visibility", v));
    }
    if let Some(issue) = r.issue {
        attributes.push(kv_int("loom.issue", i64::from(issue)));
    }
    if let Some(limit) = r.limit_bytes {
        attributes.push(kv_int("loom.scope.limit_bytes", int(limit)));
    }
    Some(LogParts {
        event_name: "scope.memory",
        severity: if r.oom_kills > 0 {
            SeverityNumber::Warn
        } else {
            SeverityNumber::Info
        },
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
    use crate::telemetry::kinds::scope_memory::{
        LimitSource, ScopeMemoryRecord, SCOPE_MEMORY_LOG_ATTRIBUTE_KEYS,
    };
    use crate::telemetry::provenance::Provenance;
    use crate::telemetry::TelemetryEnvelope;

    fn record() -> ScopeMemoryRecord {
        ScopeMemoryRecord {
            scope: "loom-agent-step-builder-42-1.scope".into(),
            workspace: "loom".into(),
            repo: Some("rjwalters/loom".into()),
            repo_visibility: Some("public".into()),
            role: "builder".into(),
            issue: Some(42),
            limit_bytes: Some(8 * 1024 * 1024 * 1024),
            limit_source: LimitSource::History,
            peak_bytes: 8 * 1024 * 1024 * 1024,
            events_max: 120,
            oom_kills: 4,
            clipped: true,
            observed_at: Utc.with_ymd_and_hms(2026, 10, 15, 12, 0, 0).unwrap(),
            loom: Provenance {
                version: "0.19.1".into(),
                revision: "9d8e226ce0123456789abcdef0123456789abcde".into(),
                tree_state: "clean".into(),
                complete: true,
            },
        }
    }

    fn log(r: ScopeMemoryRecord) -> opentelemetry_proto::tonic::logs::v1::LogRecord {
        let envelope = TelemetryEnvelope::new("host-1", TelemetryRecord::ScopeMemory(r));
        log_record_for(&envelope).unwrap()
    }

    fn keys(r: ScopeMemoryRecord) -> Vec<String> {
        let log = log(r);
        assert_eq!(log.event_name, "scope.memory");
        log.attributes.iter().map(|kv| kv.key.clone()).collect()
    }

    #[test]
    fn carries_every_scope_memory_attribute() {
        let keys = keys(record());
        for key in SCOPE_MEMORY_LOG_ATTRIBUTE_KEYS.iter().chain(&[
            "loom.kind",
            "loom.record_id",
            "loom.repo",
            "loom.repo.visibility",
            "loom.role",
            "loom.issue",
        ]) {
            assert!(keys.iter().any(|k| k == key), "{key} is emitted");
        }
    }

    #[test]
    fn no_limit_and_no_slug_omit_their_attributes() {
        let mut r = record();
        r.limit_bytes = None;
        r.limit_source = LimitSource::None;
        r.repo = None;
        r.repo_visibility = None;
        r.issue = None;
        let keys = keys(r);
        for absent in ["loom.scope.limit_bytes", "loom.repo", "loom.issue"] {
            assert!(!keys.iter().any(|k| k == absent), "{absent} omitted");
        }
    }

    #[test]
    fn clipped_is_a_bool_attribute() {
        let log = log(record());
        let clipped = log
            .attributes
            .iter()
            .find(|kv| kv.key == "loom.scope.clipped")
            .and_then(|kv| kv.value.as_ref()?.value.clone());
        assert_eq!(clipped, Some(any_value::Value::BoolValue(true)));
    }

    #[test]
    fn oom_kills_raise_the_severity() {
        assert_eq!(log(record()).severity_number, SeverityNumber::Warn as i32);
        let mut r = record();
        r.oom_kills = 0;
        assert_eq!(log(r).severity_number, SeverityNumber::Info as i32);
    }

    #[test]
    fn invalid_provenance_is_not_exported() {
        let mut r = record();
        r.loom.version = String::new();
        assert!(log_parts(&TelemetryRecord::ScopeMemory(r)).is_none());
    }
}
