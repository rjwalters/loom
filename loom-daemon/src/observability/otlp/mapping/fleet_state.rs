//! OTLP mapping for `fleet.state` (#10196).
//!
//! Each record becomes one log record. The **body** is the record's JSON, the
//! rows and census, so ClickHouse can `JSONExtract` it and no attribute
//! policy bounds it. A few scalars ride as `loom.fleet.*` attributes so a
//! replay query can find anchors, and tell a complete chunked anchor from a
//! partial one, without parsing bodies. The record time is the record's
//! `as_of`, shared by every chunk.

use opentelemetry_proto::tonic::common::v1::{any_value, AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

use super::{kv, kv_int, kv_string, nanos};
use crate::telemetry::TelemetryRecord;

fn count(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// `(event_name, severity, record time, attributes, body)` for a
/// `fleet.state` record; `None` for every other kind.
pub(super) fn log_parts(
    record: &TelemetryRecord,
) -> Option<(&'static str, SeverityNumber, u64, Vec<KeyValue>, String)> {
    let TelemetryRecord::FleetState(r) = record else {
        return None;
    };
    let attributes = vec![
        kv_string("loom.kind", record.kind()),
        kv_string("loom.fleet.schema", r.schema.clone()),
        kv(
            "loom.fleet.anchor",
            AnyValue {
                value: Some(any_value::Value::BoolValue(r.anchor)),
            },
        ),
        kv_string("loom.fleet.anchor_as_of", crate::telemetry::trace::instant(r.anchor_as_of)),
        kv_int("loom.fleet.repos", count(r.repos.len())),
        kv_int("loom.fleet.rows", count(r.row_count())),
        kv_int("loom.fleet.removed", count(r.removed_count())),
        kv_int("loom.fleet.chunk_index", i64::from(r.chunk_index)),
        kv_int("loom.fleet.chunk_count", i64::from(r.chunk_count)),
    ];
    let body = serde_json::to_string(r).unwrap_or_default();
    Some(("fleet.state", SeverityNumber::Info, nanos(r.as_of), attributes, body))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::log_record_for;
    use crate::telemetry::kinds::fleet_state::{
        FleetPrCensus, FleetSlot, FleetStage, FleetStateRecord, FleetStateRepo, FleetStateRow,
        PlannerStamps, FLEET_STATE_LOG_ATTRIBUTE_KEYS, FLEET_STATE_SCHEMA,
    };
    use crate::telemetry::{RepoVisibility, TelemetryEnvelope, TelemetryRecord};
    use chrono::{TimeZone, Utc};
    use opentelemetry_proto::tonic::common::v1::any_value::Value;

    fn record(anchor: bool) -> FleetStateRecord {
        let as_of = Utc.with_ymd_and_hms(2026, 10, 4, 12, 5, 0).unwrap();
        FleetStateRecord {
            schema: FLEET_STATE_SCHEMA.to_string(),
            as_of,
            anchor,
            anchor_as_of: Utc.with_ymd_and_hms(2026, 10, 4, 12, 0, 0).unwrap(),
            prev_as_of: (!anchor).then_some(as_of),
            chunk_index: 1,
            chunk_count: 2,
            stamps: PlannerStamps {
                planner_version: "0.19.958".to_string(),
                planner_config_hash: "0123456789ab".to_string(),
                fleet_config_hash: None,
            },
            census_at: Some(as_of),
            slots: None,
            repos: vec![FleetStateRepo {
                repo: "rjwalters/loom".to_string(),
                visibility: RepoVisibility::Public,
                ready_complete: true,
                census: Some(FleetPrCensus {
                    open: 1,
                    by_stage: Default::default(),
                }),
                rows: vec![FleetStateRow {
                    host: Some("robb-studio".to_string()),
                    slot: Some(FleetSlot::Regular),
                    ..FleetStateRow::new(10196, FleetStage::SweepBuilder, as_of)
                }],
                removed: vec![10193],
            }],
        }
    }

    fn attr(log: &opentelemetry_proto::tonic::logs::v1::LogRecord, key: &str) -> Option<Value> {
        log.attributes
            .iter()
            .find(|kv| kv.key == key)
            .and_then(|kv| kv.value.as_ref())
            .and_then(|v| v.value.clone())
    }

    #[test]
    fn a_fleet_state_log_carries_the_record_id_body_and_anchor_scalars() {
        let record = record(false);
        let envelope =
            TelemetryEnvelope::new("robb-studio", TelemetryRecord::FleetState(record.clone()));
        let log = log_record_for(&envelope).expect("fleet.state is a log kind");
        assert_eq!(log.event_name, "fleet.state");
        assert_eq!(log.time_unix_nano, super::nanos(record.as_of));
        match attr(&log, "loom.record_id") {
            Some(Value::StringValue(id)) => assert_eq!(id.len(), 16),
            other => panic!("loom.record_id missing: {other:?}"),
        }
        assert_eq!(attr(&log, "loom.kind"), Some(Value::StringValue("fleet.state".into())));
        assert_eq!(attr(&log, "loom.fleet.anchor"), Some(Value::BoolValue(false)));
        assert_eq!(attr(&log, "loom.fleet.rows"), Some(Value::IntValue(1)));
        assert_eq!(attr(&log, "loom.fleet.removed"), Some(Value::IntValue(1)));
        assert_eq!(attr(&log, "loom.fleet.chunk_index"), Some(Value::IntValue(1)));
        assert_eq!(attr(&log, "loom.fleet.chunk_count"), Some(Value::IntValue(2)));
        assert_eq!(attr(&log, "loom.fleet.rows_truncated"), None);
        let body = match log.body.and_then(|b| b.value) {
            Some(Value::StringValue(body)) => body,
            other => panic!("body must be the record JSON, got {other:?}"),
        };
        let back: FleetStateRecord = serde_json::from_str(&body).unwrap();
        assert_eq!(back, record);
    }

    #[test]
    fn the_record_id_is_stable_on_retry_and_differs_between_anchor_and_delta() {
        let id = |r: FleetStateRecord| {
            let mut envelope =
                TelemetryEnvelope::new("robb-studio", TelemetryRecord::FleetState(r));
            envelope.emitted_at = Utc.with_ymd_and_hms(2026, 10, 4, 12, 5, 1).unwrap();
            match attr(&log_record_for(&envelope).unwrap(), "loom.record_id") {
                Some(Value::StringValue(id)) => id,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(id(record(true)), id(record(true)));
        assert_ne!(id(record(true)), id(record(false)));
        // Two chunks of one anchor are two records.
        let mut first = record(true);
        first.chunk_index = 0;
        assert_ne!(id(first), id(record(true)));
    }

    #[test]
    fn every_fleet_state_attribute_is_allowlisted() {
        for anchor in [true, false] {
            let envelope =
                TelemetryEnvelope::new("robb-studio", TelemetryRecord::FleetState(record(anchor)));
            let log = log_record_for(&envelope).unwrap();
            for kv in &log.attributes {
                assert!(
                    FLEET_STATE_LOG_ATTRIBUTE_KEYS.contains(&kv.key.as_str())
                        || ["loom.kind", "loom.record_id"].contains(&kv.key.as_str()),
                    "{} is not allowlisted",
                    kv.key
                );
            }
            for key in FLEET_STATE_LOG_ATTRIBUTE_KEYS {
                assert!(attr(&log, key).is_some(), "{key} is emitted");
            }
        }
    }
}
