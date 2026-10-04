//! `host.health` export-coverage fields (Issue #10196).

use super::super::*;
use super::fleet_captain::host_health_with_captain;

#[test]
fn coverage_fields_are_omitted_when_empty_and_round_trip_when_present() {
    let TelemetryRecord::HostHealth(mut health) = host_health_with_captain(None, Vec::new()) else {
        unreachable!()
    };
    let value = serde_json::to_value(&health).unwrap();
    assert!(value.get("exported_kinds").is_none());
    assert!(value.get("exporters").is_none());

    health.exporters = vec!["otlp".to_string()];
    health.exported_kinds = exported_kinds_for(&health.exporters);
    let value = serde_json::to_value(&health).unwrap();
    let back: HostHealthRecord = serde_json::from_value(value).unwrap();
    assert_eq!(back.exporters, vec!["otlp".to_string()]);
    assert!(back.exported_kinds.iter().any(|k| k == "sweep.outcome"));
}

#[test]
fn pre_10196_record_decodes_with_empty_coverage() {
    let baseline = host_health_with_captain(None, Vec::new());
    let mut value = serde_json::to_value(&baseline).unwrap();
    let obj = value.as_object_mut().unwrap();
    obj.remove("exported_kinds");
    obj.remove("exporters");
    let TelemetryRecord::HostHealth(health) = serde_json::from_value(value).unwrap() else {
        panic!("expected host.health");
    };
    assert!(health.exported_kinds.is_empty());
    assert!(health.exporters.is_empty());
}

#[test]
fn exported_kinds_follow_the_registry_routing() {
    let https = exported_kinds_for(&["https".to_string()]);
    let otlp = exported_kinds_for(&["otlp".to_string()]);
    // native-only kinds never appear under otlp-only and vice versa.
    assert!(https.iter().any(|k| k == "queue.snapshot"));
    assert!(!otlp.iter().any(|k| k == "queue.snapshot"));
    assert!(otlp.iter().any(|k| k == "session.output"));
    assert!(!https.iter().any(|k| k == "session.output"));
    assert!(exported_kinds_for(&[]).is_empty());
    assert!(exported_kinds_for(&["bogus".to_string()]).is_empty());
}

/// Judge P1 on #10267: the exact `spawn_task_two_exporters_isolate_the_unbuildable_kind`
/// shape — HTTPS started, OTLP rejected as misconfigured — must advertise only
/// `https` and its kinds. A never-started sink in the coverage lists would let
/// a replay reader read its silence as "nothing happened".
#[test]
fn coverage_excludes_misconfigured_exporters_in_a_mixed_configuration() {
    use crate::observability::ExportStatus;
    let now = chrono::Utc::now();
    let mut statuses = std::collections::BTreeMap::new();
    statuses.insert(
        "https".to_string(),
        ExportStatus::started("host-a", "https://ingest.example.com/v1/telemetry", "https", 30)
            .snapshot(),
    );
    statuses.insert(
        "otlp".to_string(),
        ExportStatus::misconfigured(
            Some("http://127.0.0.1:4318".to_string()),
            "exporter = \"otlp\" requires the `otlp` Cargo feature".to_string(),
        )
        .snapshot(),
    );

    let (exporters, kinds) = export_coverage(&statuses, now);
    assert_eq!(exporters, vec!["https".to_string()]);
    assert_eq!(kinds, exported_kinds_for(&["https".to_string()]));
    assert!(kinds.iter().any(|k| k == "queue.snapshot"));
    // OTLP-only kinds must not be advertised by a sink that never ran.
    assert!(!kinds.iter().any(|k| k == "session.output"));
}

#[test]
fn coverage_is_empty_meaning_unknown_when_nothing_started() {
    use crate::observability::ExportStatus;
    let now = chrono::Utc::now();
    let mut statuses = std::collections::BTreeMap::new();
    statuses.insert(
        "otlp".to_string(),
        ExportStatus::misconfigured(None, "observability.endpoint not configured".to_string())
            .snapshot(),
    );
    statuses.insert("https".to_string(), crate::types::ObservabilityExportStatus::disabled());
    let (exporters, kinds) = export_coverage(&statuses, now);
    assert!(exporters.is_empty());
    assert!(kinds.is_empty());
    assert_eq!(export_coverage(&std::collections::BTreeMap::new(), now), (vec![], vec![]));
}
