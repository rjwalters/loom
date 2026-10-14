//! OTLP mapping for `backlog.level` (#11819): one INFO log record per Curator
//! dispatch that carried a backlog level, stamped and observed at the instant
//! the daemon computed it. The body is the record's JSON; the scalars ride as
//! `loom.backlog.*` attributes.

use super::session_output::LogParts;
use super::{kv_int, kv_string, nanos};
use crate::telemetry::TelemetryRecord;
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

pub(super) fn log_parts(record: &TelemetryRecord) -> Option<LogParts> {
    let TelemetryRecord::BacklogLevel(r) = record else {
        return None;
    };
    if !r.loom.is_valid() {
        return None;
    }
    let mut attributes = vec![
        kv_string("loom.repo", &r.repo),
        kv_string("loom.role", "curator"),
        kv_string("loom.sweep_id", &r.dispatch_id),
        kv_string("loom.backlog.level", r.level.as_str()),
        kv_int("loom.backlog.target", i64::try_from(r.target).unwrap_or(i64::MAX)),
        kv_string("loom.backlog.count_failed", if r.open.is_some() { "false" } else { "true" }),
    ];
    if let Some(open) = r.open {
        attributes.push(kv_int("loom.backlog.open", i64::try_from(open).unwrap_or(i64::MAX)));
    }
    Some(LogParts {
        event_name: "backlog.level",
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
    use crate::telemetry::kinds::backlog_level::{
        BacklogLevel, BacklogLevelRecord, BACKLOG_LEVEL_LOG_ATTRIBUTE_KEYS,
    };
    use crate::telemetry::provenance::Provenance;
    use crate::telemetry::TelemetryEnvelope;

    fn record() -> BacklogLevelRecord {
        BacklogLevelRecord {
            repo: "rjwalters/loom".into(),
            open: Some(412),
            target: 140,
            level: BacklogLevel::Strict,
            dispatch_id: "role-curator-2026-10-13T12:00:00Z".into(),
            count_error: None,
            observed_at: Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap(),
            loom: Provenance {
                version: "0.19.1".into(),
                revision: "9d8e226ce0123456789abcdef0123456789abcde".into(),
                tree_state: "clean".into(),
                complete: true,
            },
        }
    }

    fn keys(r: BacklogLevelRecord) -> Vec<String> {
        let envelope = TelemetryEnvelope::new("host-1", TelemetryRecord::BacklogLevel(r));
        let log = log_record_for(&envelope).unwrap();
        assert_eq!(log.event_name, "backlog.level");
        log.attributes.iter().map(|kv| kv.key.clone()).collect()
    }

    #[test]
    fn carries_every_backlog_attribute() {
        let keys = keys(record());
        for key in BACKLOG_LEVEL_LOG_ATTRIBUTE_KEYS.iter().chain(&[
            "loom.kind",
            "loom.record_id",
            "loom.repo",
            "loom.role",
            "loom.sweep_id",
        ]) {
            assert!(keys.iter().any(|k| k == key), "{key} is emitted");
        }
    }

    #[test]
    fn a_failed_count_omits_open() {
        let mut r = record();
        r.open = None;
        r.level = BacklogLevel::Normal;
        r.count_error = Some("search failed".into());
        assert!(!keys(r).iter().any(|k| k == "loom.backlog.open"));
    }

    #[test]
    fn invalid_provenance_is_not_exported() {
        let mut r = record();
        r.loom.version = String::new();
        assert!(log_parts(&TelemetryRecord::BacklogLevel(r)).is_none());
    }
}
