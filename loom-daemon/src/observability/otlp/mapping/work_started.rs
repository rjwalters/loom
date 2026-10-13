//! OTLP mapping for `stage.work_started` (#11641): one INFO log record per
//! agent start, stamped at `started_at` and observed at `observed_at`. The
//! body is the record's JSON; the join keys ride as attributes.

use super::session_output::LogParts;
use super::{kv_int, kv_string, nanos};
use crate::telemetry::trace::instant;
use crate::telemetry::TelemetryRecord;
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

pub(super) fn log_parts(record: &TelemetryRecord) -> Option<LogParts> {
    let TelemetryRecord::WorkStarted(r) = record else {
        return None;
    };
    if !r.loom.is_valid() {
        return None;
    }
    let mut attributes = vec![
        kv_string("loom.repo", &r.repo),
        kv_string("loom.role", &r.role),
        kv_string("loom.sweep_id", &r.sweep_id),
        kv_string("loom.work_started.stage", r.stage.as_str()),
        kv_string("loom.work_started.started_at", instant(r.started_at)),
        kv_string("loom.work_started.source", &r.source),
        kv_int("loom.work_started.stage_attempt", i64::from(r.stage_attempt)),
    ];
    if let Some(issue) = r.issue {
        attributes.push(kv_int("loom.issue", i64::from(issue)));
    }
    if let Some(pr) = r.pr_number {
        attributes.push(kv_int("loom.pr_number", i64::from(pr)));
    }
    if let Some(attempt) = r.attempt_index {
        attributes.push(kv_int("loom.attempt_index", i64::from(attempt)));
    }
    if let Some(origin) = &r.dispatch_origin {
        attributes.push(kv_string("loom.dispatch_origin", origin));
    }
    Some(LogParts {
        event_name: "stage.work_started",
        severity: SeverityNumber::Info,
        source_at: nanos(r.started_at),
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
    use crate::telemetry::kinds::fleet_state::FleetStage;
    use crate::telemetry::kinds::work_started::{
        WorkStartedRecord, WORK_STARTED_LOG_ATTRIBUTE_KEYS,
    };
    use crate::telemetry::provenance::Provenance;
    use crate::telemetry::TelemetryEnvelope;

    fn record() -> WorkStartedRecord {
        WorkStartedRecord {
            repo: "rjwalters/loom".into(),
            issue: Some(11641),
            pr_number: Some(11700),
            stage: FleetStage::Doctor,
            role: "doctor".into(),
            sweep_id: "sweep-issue-11641-1".into(),
            attempt_index: Some(2),
            stage_attempt: 1,
            dispatch_origin: Some("work_finder".into()),
            started_at: Utc.with_ymd_and_hms(2026, 10, 12, 15, 0, 0).unwrap(),
            source: "sweep_checkpoint".into(),
            observed_at: Utc.with_ymd_and_hms(2026, 10, 12, 15, 0, 30).unwrap(),
            loom: Provenance {
                version: "0.19.1".into(),
                revision: "9d8e226ce0123456789abcdef0123456789abcde".into(),
                tree_state: "clean".into(),
                complete: true,
            },
        }
    }

    #[test]
    fn stamped_at_the_start_with_every_join_key() {
        let r = record();
        let envelope = TelemetryEnvelope::new("host-1", TelemetryRecord::WorkStarted(r.clone()));
        let log = log_record_for(&envelope).unwrap();
        assert_eq!(log.event_name, "stage.work_started");
        assert_eq!(log.time_unix_nano, nanos(r.started_at));
        assert_eq!(log.observed_time_unix_nano, nanos(r.observed_at));
        let keys: Vec<&str> = log.attributes.iter().map(|kv| kv.key.as_str()).collect();
        for key in WORK_STARTED_LOG_ATTRIBUTE_KEYS.iter().chain(&[
            "loom.kind",
            "loom.record_id",
            "loom.fact_id",
            "loom.repo",
            "loom.issue",
            "loom.pr_number",
            "loom.role",
            "loom.sweep_id",
            "loom.attempt_index",
            "loom.dispatch_origin",
        ]) {
            assert!(keys.contains(key), "{key} is emitted");
        }
        let body: serde_json::Value =
            serde_json::from_str(match &log.body.unwrap().value.unwrap() {
                opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(s) => s,
                other => panic!("{other:?}"),
            })
            .unwrap();
        assert_eq!(body["stage"], "doctor");
        assert_eq!(body["dispatch_origin"], "work_finder");
    }

    #[test]
    fn fact_id_is_stable_and_distinguishes_starts() {
        let a = record();
        let mut b = record();
        b.observed_at = Utc::now();
        let id = |r: &WorkStartedRecord| {
            super::super::fact_id::of(&TelemetryRecord::WorkStarted(r.clone())).unwrap()
        };
        assert_eq!(id(&a), id(&b), "re-observing the same start is the same fact");
        b.stage_attempt = 2;
        assert_ne!(id(&a), id(&b), "a second start of the stage is a new fact");
    }

    #[test]
    fn invalid_provenance_is_not_exported() {
        let mut r = record();
        r.loom.version = String::new();
        assert!(log_parts(&TelemetryRecord::WorkStarted(r)).is_none());
    }
}
