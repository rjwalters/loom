//! OTLP mapping for `eta.stage_entry` (#12869).
//!
//! One log record per observed arrival. The **body** is the record's JSON;
//! `loom.repo`, `loom.issue` and `loom.repo.visibility` ride as attributes
//! (all already on the gateway's log `keep_keys`), so no collector change is
//! needed. The record time is `observed_at`.

use opentelemetry_proto::tonic::common::v1::KeyValue;
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

use super::{kv_int, kv_string, nanos};
use crate::telemetry::{RepoVisibility, TelemetryRecord};

/// `(event_name, severity, record time, attributes, body)` for an
/// `eta.stage_entry` record; `None` for every other kind.
pub(super) fn log_parts(
    record: &TelemetryRecord,
) -> Option<(&'static str, SeverityNumber, u64, Vec<KeyValue>, String)> {
    let TelemetryRecord::StageEntry(r) = record else {
        return None;
    };
    let visibility = match r.visibility {
        RepoVisibility::Public => "public",
        RepoVisibility::Private => "private",
    };
    let attributes = vec![
        kv_string("loom.repo", r.repo.clone()),
        kv_string("loom.repo.visibility", visibility),
        kv_int("loom.issue", i64::from(r.issue)),
    ];
    let body = serde_json::to_string(r).unwrap_or_default();
    Some(("eta.stage_entry", SeverityNumber::Info, nanos(r.observed_at), attributes, body))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::log_record_for;
    use crate::telemetry::kinds::stage_entry::{
        EnteredAtSource, StageEntryRecord, STAGE_ENTRY_SCHEMA_VERSION,
    };
    use crate::telemetry::{RepoVisibility, TelemetryEnvelope, TelemetryRecord};
    use chrono::Utc;

    #[test]
    fn maps_to_a_log_whose_body_is_the_record_json() {
        let now = Utc::now();
        let record = StageEntryRecord {
            schema_version: STAGE_ENTRY_SCHEMA_VERSION,
            repo: "rjwalters/loom".into(),
            issue: 42,
            stage: "loom:issue".into(),
            entered_at: now,
            entered_at_source: EnteredAtSource::Observed,
            observed_at: now,
            visibility: RepoVisibility::Private,
        };
        let envelope = TelemetryEnvelope::new("host-a", TelemetryRecord::StageEntry(record));
        let log = log_record_for(&envelope).expect("eta.stage_entry exports as a log");
        assert_eq!(log.event_name, "eta.stage_entry");
        let body = format!("{:?}", log.body);
        assert!(body.contains("loom:issue"), "{body}");
        let attr = |key: &str| {
            log.attributes
                .iter()
                .find(|a| a.key == key)
                .map(|a| format!("{:?}", a.value))
        };
        assert!(attr("loom.repo").unwrap().contains("rjwalters/loom"));
        assert!(attr("loom.issue").unwrap().contains("42"));
        assert!(attr("loom.repo.visibility").unwrap().contains("private"));
        assert!(attr("loom.kind").unwrap().contains("eta.stage_entry"));
    }
}
