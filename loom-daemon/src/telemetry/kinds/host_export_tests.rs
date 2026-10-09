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
        },
    )]);
    let record = HostExportRecord::build("host-a", Utc::now(), &statuses, &stats);
    assert_eq!(record.exporters.len(), 1);
    assert_eq!(record.exporters[0].name, "otlp");
    assert_eq!(record.exporters[0].queue_depth, 2);
    assert_eq!(record.exporters[0].dropped_total, 3);
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
            last_flush_ok_at: None,
        }],
    };
    let wrapped = TelemetryRecord::HostExport(record.clone());
    let json = serde_json::to_string(&wrapped).unwrap();
    assert!(json.contains(r#""kind":"host.export""#), "{json}");
    assert!(!json.contains("last_flush_ok_at"), "{json}");
    let decoded: TelemetryRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, wrapped);
}

#[test]
fn inactive_exporters_are_omitted() {
    let record = HostExportRecord::build("h", Utc::now(), &BTreeMap::new(), &BTreeMap::new());
    assert!(record.exporters.is_empty());
}
