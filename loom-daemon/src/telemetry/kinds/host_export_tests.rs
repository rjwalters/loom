use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::Utc;

use super::*;
use crate::observability::queue::DurableQueue;
use crate::observability::ExportStatus;
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};

fn envelope() -> TelemetryEnvelope {
    TelemetryEnvelope::new(
        "host-a",
        TelemetryRecord::HostExport(HostExportRecord {
            captured_at: Utc::now(),
            host: "host-a".to_string(),
            exporters: vec![],
            fact_id_missing_total: 0,
        }),
    )
}

fn started(name: &str) -> Arc<ExportStatus> {
    Arc::new(ExportStatus::started("host-a", "http://localhost", name, 30))
}

#[test]
fn forced_overflow_increments_dropped_total_and_appears_in_the_record() {
    let dir = tempfile::tempdir().unwrap();
    let queue = DurableQueue::open(dir.path().join("q.jsonl"), 2);
    for _ in 0..5 {
        queue.push(envelope());
    }
    assert_eq!(queue.len(), 2);
    assert_eq!(queue.dropped_total(), 3);

    let status = started("otlp");
    status.record_success(1);
    let statuses = BTreeMap::from([("otlp".to_string(), status.snapshot())]);
    let stats = BTreeMap::from([(
        "otlp".to_string(),
        QueueStats {
            depth: queue.len() as u64,
            dropped_total: queue.dropped_total(),
            dropped_by_reason: queue.dropped_by_reason(),
        },
    )]);
    let record = HostExportRecord::build("host-a", Utc::now(), &statuses, &stats);
    assert_eq!(record.exporters.len(), 1);
    assert_eq!(record.exporters[0].name, "otlp");
    assert_eq!(record.exporters[0].queue_depth, 2);
    assert_eq!(record.exporters[0].dropped_total, 3);
    assert_eq!(
        record.exporters[0].dropped_by_reason,
        BTreeMap::from([("queue_overflow".to_string(), 3)]),
        "dropped_total = the sum over dropped_by_reason"
    );
    assert!(record.exporters[0].last_flush_ok_at.is_some());

    // A further overflow increments again, cumulatively.
    queue.push(envelope());
    assert_eq!(queue.dropped_total(), 4);
}

#[test]
fn record_round_trips_and_omits_unknown_flush_time() {
    let record = HostExportRecord {
        captured_at: Utc::now(),
        host: "h".to_string(),
        exporters: vec![ExporterExport {
            name: "https".to_string(),
            queue_depth: 1,
            dropped_total: 0,
            dropped_by_reason: BTreeMap::new(),
            last_flush_ok_at: None,
        }],
        fact_id_missing_total: 0,
    };
    let wrapped = TelemetryRecord::HostExport(record.clone());
    let json = serde_json::to_string(&wrapped).unwrap();
    assert!(json.contains(r#""kind":"host.export""#), "{json}");
    assert!(!json.contains("last_flush_ok_at"), "{json}");
    assert!(!json.contains("dropped_by_reason"), "{json}");
    let decoded: TelemetryRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, wrapped);
}

#[test]
fn build_carries_dropped_by_reason_and_the_body_round_trips() {
    let record = HostExportRecord {
        captured_at: Utc::now(),
        host: "h".to_string(),
        exporters: vec![ExporterExport {
            name: "otlp".to_string(),
            queue_depth: 4,
            dropped_total: 6,
            dropped_by_reason: BTreeMap::from([
                ("status_400".to_string(), 2),
                ("queue_overflow".to_string(), 4),
            ]),
            last_flush_ok_at: None,
        }],
        fact_id_missing_total: 0,
    };
    let json = serde_json::to_string(&record).unwrap();
    assert!(
        json.contains(r#""dropped_by_reason":{"queue_overflow":4,"status_400":2}"#),
        "{json}"
    );
    let decoded: HostExportRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, record);
}

#[test]
fn a_body_from_before_the_by_reason_map_still_parses() {
    let old = r#"{"captured_at":"2026-10-08T12:00:00Z","host":"h","exporters":[{"name":"https","queue_depth":1,"dropped_total":2}]}"#;
    let record: HostExportRecord = serde_json::from_str(old).unwrap();
    assert_eq!(record.exporters[0].dropped_total, 2);
    assert!(record.exporters[0].dropped_by_reason.is_empty());
}

#[test]
fn inactive_exporters_are_omitted() {
    let record = HostExportRecord::build("h", Utc::now(), &BTreeMap::new(), &BTreeMap::new());
    assert!(record.exporters.is_empty());
}

/// #11401: `host.export` reports the exits emitted without a fact id.
#[test]
fn build_reports_the_exits_emitted_without_a_fact_id() {
    let before = super::super::stage_outcome::fact_id_missing_total();
    super::super::stage_outcome::note_fact_id_missing();
    let record = HostExportRecord::build("h", Utc::now(), &BTreeMap::new(), &BTreeMap::new());
    // Process-wide and cumulative; parallel tests may add more.
    assert!(record.fact_id_missing_total > before, "{record:?}");
}

#[test]
fn a_body_from_before_the_fact_id_count_still_parses() {
    let old = r#"{"captured_at":"2026-10-08T12:00:00Z","host":"h","exporters":[]}"#;
    let record: HostExportRecord = serde_json::from_str(old).unwrap();
    assert_eq!(record.fact_id_missing_total, 0);
}
