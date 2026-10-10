//! `pr.resolved`: registration and wire shape.

use super::*;
use crate::telemetry::{
    TelemetryEnvelope, TelemetryKindOtlp, TelemetryRecord, NEW_KIND_SCHEMA_VERSION, TELEMETRY_KINDS,
};
use chrono::TimeZone;

fn provenance() -> Provenance {
    Provenance {
        version: "0.19.800".to_string(),
        revision: "9d8e226ce0123456789abcdef0123456789abcde".to_string(),
        tree_state: "clean".to_string(),
        complete: true,
    }
}

fn record() -> PrResolvedRecord {
    let at = Utc.with_ymd_and_hms(2026, 10, 6, 12, 0, 0).unwrap();
    PrResolvedRecord {
        repo: "rjwalters/loom".to_string(),
        pr_number: 101,
        issue: Some(100),
        state: PrResolution::Merged,
        resolved_at: at,
        observed_at: at + chrono::Duration::seconds(300),
        resolution_sec: 0,
        closed_at: Some(at),
        loom: provenance(),
    }
}

#[test]
fn pr_resolved_is_registered_as_an_otlp_log_kind() {
    let meta = TELEMETRY_KINDS
        .iter()
        .find(|m| m.kind == "pr.resolved")
        .expect("pr.resolved has a registry row");
    assert_eq!(meta.variant, "PrResolved");
    assert_eq!(meta.otlp, TelemetryKindOtlp::Logs);
    assert!(!meta.native_ingest, "OTLP-only");
    assert_eq!(meta.schema_version, NEW_KIND_SCHEMA_VERSION);
}

#[test]
fn the_body_keeps_its_field_names_and_adds_closed_at() {
    let json = serde_json::to_value(record()).unwrap();
    let mut keys: Vec<&str> = json
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "closed_at",
            "issue",
            "loom",
            "observed_at",
            "pr_number",
            "repo",
            "resolution_sec",
            "resolved_at",
            "state"
        ]
    );
    let mut old = json;
    old.as_object_mut().unwrap().remove("closed_at");
    let parsed: PrResolvedRecord = serde_json::from_value(old).unwrap();
    assert_eq!(parsed.closed_at, None, "an older build's body still parses");
}

#[test]
fn the_record_round_trips_and_requires_provenance() {
    let record = record();
    assert!(record.has_provenance());
    let envelope = TelemetryEnvelope::new("host-a", TelemetryRecord::PrResolved(record.clone()));
    let json = serde_json::to_value(&envelope).unwrap();
    assert_eq!(json["record"]["kind"], "pr.resolved");
    assert_eq!(json["record"]["state"], "merged");
    let back: TelemetryEnvelope = serde_json::from_value(json).unwrap();
    assert_eq!(back, envelope);

    let unproven = PrResolvedRecord {
        loom: Provenance {
            revision: "not-a-sha".to_string(),
            ..provenance()
        },
        ..record
    };
    assert!(!unproven.has_provenance());
}
