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
