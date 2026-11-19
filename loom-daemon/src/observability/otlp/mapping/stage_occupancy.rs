//! OTLP mapping for `eta.stage_occupancy` (#12866).
//!
//! One log record per `(repo, stage)` and per global `stage` per collector
//! sample. The **body** is the record's JSON, so ClickHouse can `JSONExtract`
//! `stage`, `count`, `oldest_age_sec` and `age_buckets`. Only `loom.repo` and
//! `loom.repo.visibility` ride as attributes
//! ([`STAGE_OCCUPANCY_LOG_ATTRIBUTE_KEYS`]), both already on the gateway's log
//! `keep_keys`, so no collector change is needed. A global row has no
//! `loom.repo`. `log_record_for` adds `loom.kind` centrally. The record time
//! is the sample instant (`sampled_at`).

use opentelemetry_proto::tonic::common::v1::KeyValue;
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

use super::{kv_string, nanos};
use crate::telemetry::TelemetryRecord;

/// `(event_name, severity, record time, attributes, body)` for an
/// `eta.stage_occupancy` record; `None` for every other kind.
pub(super) fn log_parts(
    record: &TelemetryRecord,
) -> Option<(&'static str, SeverityNumber, u64, Vec<KeyValue>, String)> {
    let TelemetryRecord::StageOccupancy(r) = record else {
        return None;
    };
    let mut attributes = vec![kv_string("loom.repo.visibility", r.visibility.as_str())];
    if let Some(repo) = &r.repo {
        attributes.push(kv_string("loom.repo", repo.clone()));
    }
    let body = serde_json::to_string(r).unwrap_or_default();
    Some((
        "eta.stage_occupancy",
        SeverityNumber::Info,
        nanos(r.sampled_at),
        attributes,
        body,
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::log_record_for;
    use crate::telemetry::kinds::stage_occupancy::{
        OccupancyScope, StageOccupancyRecord, STAGE_OCCUPANCY_LOG_ATTRIBUTE_KEYS,
        STAGE_OCCUPANCY_SCHEMA_VERSION, STAGE_TRIAGE_WAIT,
    };
    use crate::telemetry::{RepoVisibility, TelemetryEnvelope, TelemetryRecord};
    use chrono::Utc;

    fn record(repo: Option<&str>) -> StageOccupancyRecord {
        StageOccupancyRecord {
            schema_version: STAGE_OCCUPANCY_SCHEMA_VERSION,
            host: "host-a".into(),
            scope: if repo.is_some() {
                OccupancyScope::Repo
            } else {
                OccupancyScope::Global
            },
            repo: repo.map(str::to_string),
            stage: STAGE_TRIAGE_WAIT.into(),
            count: 3,
            oldest_age_sec: Some(90_000),
            age_buckets: vec![1, 0, 0, 2, 0, 0, 0],
            repos_counted: repo.is_none().then_some(2),
            repos_unknown: repo.is_none().then_some(0),
            sampled_at: Utc::now(),
            visibility: RepoVisibility::Private,
        }
    }

    #[test]
    fn maps_to_a_log_whose_body_is_the_record_json() {
        let envelope = TelemetryEnvelope::new(
            "host-a",
            TelemetryRecord::StageOccupancy(record(Some("rjwalters/loom"))),
        );
        let log = log_record_for(&envelope).expect("eta.stage_occupancy exports as a log");
        assert_eq!(log.event_name, "eta.stage_occupancy");
        let body = format!("{:?}", log.body);
        assert!(body.contains("oldest_age_sec"), "{body}");
        assert!(body.contains("triage_wait"), "{body}");
        let attr = |key: &str| {
            log.attributes
                .iter()
                .find(|a| a.key == key)
                .map(|a| format!("{:?}", a.value))
        };
        assert!(attr("loom.repo").unwrap().contains("rjwalters/loom"));
        assert!(attr("loom.repo.visibility").unwrap().contains("private"));
        assert!(attr("loom.kind").unwrap().contains("eta.stage_occupancy"));
    }

    #[test]
    fn a_global_row_has_no_repo_attribute() {
        let envelope =
            TelemetryEnvelope::new("host-a", TelemetryRecord::StageOccupancy(record(None)));
        let log = log_record_for(&envelope).unwrap();
        assert!(log.attributes.iter().all(|a| a.key != "loom.repo"));
    }

    #[test]
    fn exported_attribute_keys_are_the_declared_allowlist() {
        // Anti-leak: nothing but the declared keys (plus the centrally
        // added `loom.kind` / `loom.record_id`) leaves as an attribute, and the collector keeps them.
        const CONFIG: &str =
            include_str!("../../../../../defaults/observability/collector/config.yaml");
        for repo in [Some("rjwalters/loom"), None] {
            let envelope =
                TelemetryEnvelope::new("host-a", TelemetryRecord::StageOccupancy(record(repo)));
            let log = log_record_for(&envelope).unwrap();
            for attribute in &log.attributes {
                assert!(
                    ["loom.kind", "loom.record_id"].contains(&attribute.key.as_str())
                        || STAGE_OCCUPANCY_LOG_ATTRIBUTE_KEYS.contains(&attribute.key.as_str()),
                    "undeclared attribute {}",
                    attribute.key
                );
            }
        }
        for key in STAGE_OCCUPANCY_LOG_ATTRIBUTE_KEYS {
            assert!(CONFIG.contains(&format!("\"{key}\"")), "collector drops {key}");
        }
    }

    #[test]
    fn body_keys_carry_no_forge_text() {
        let value = serde_json::to_value(record(Some("rjwalters/loom"))).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "age_buckets",
                "count",
                "host",
                "oldest_age_sec",
                "repo",
                "sampled_at",
                "schema_version",
                "scope",
                "stage",
                "visibility"
            ]
        );
    }
}
