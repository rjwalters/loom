//! `eta.stage_sample` (#10756): registration, wire shape and times.

use super::*;
use crate::eta::{Provenance, Stage};
use crate::telemetry::{
    TelemetryEnvelope, TelemetryKindOtlp, TelemetryRecord, NEW_KIND_SCHEMA_VERSION, TELEMETRY_KINDS,
};
use chrono::{Duration, TimeZone};

fn provenance() -> Provenance {
    Provenance {
        version: "0.19.800".to_string(),
        revision: "9d8e226ce0123456789abcdef0123456789abcde".to_string(),
        tree_state: "clean".to_string(),
        complete: true,
    }
}

fn t(sec: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap() + Duration::seconds(sec)
}

fn first_seen() -> EtaStageSampleRecord {
    let mut row =
        EtaStageSampleRecord::new("label.first_seen", "rjwalters/loom", t(0), &provenance());
    row.issue = Some(10756);
    row.pr_number = Some(11020);
    row.next_stage = Some(Stage::ReviewWait);
    row.raw = serde_json::json!({"labels": ["loom:review-requested"]});
    row
}

#[test]
fn eta_stage_sample_is_registered_as_an_otlp_log_kind() {
    let meta = TELEMETRY_KINDS
        .iter()
        .find(|m| m.kind == "eta.stage_sample")
        .expect("eta.stage_sample has a registry row");
    assert_eq!(meta.variant, "EtaStageSample");
    assert_eq!(meta.otlp, TelemetryKindOtlp::Logs);
    assert!(!meta.native_ingest, "OTLP-only, like the eta.* log kinds");
    assert_eq!(meta.schema_version, NEW_KIND_SCHEMA_VERSION);
}

#[test]
fn the_row_round_trips_verbatim_and_requires_provenance() {
    let mut row = first_seen();
    row.forge_at = Some(t(-3600));
    assert!(row.has_provenance());
    let envelope = TelemetryEnvelope::new("host-a", TelemetryRecord::EtaStageSample(row.clone()));
    let json = serde_json::to_value(&envelope).unwrap();
    assert_eq!(json["record"]["kind"], "eta.stage_sample");
    assert_eq!(json["record"]["event"], "label.first_seen");
    assert_eq!(json["record"]["schema"], "eta-stage-sample/v1");
    assert_eq!(json["record"]["raw"]["labels"][0], "loom:review-requested");
    let back: TelemetryEnvelope = serde_json::from_value(json).unwrap();
    assert_eq!(back, envelope);

    row.loom.revision = "not-a-sha".to_string();
    assert!(!row.has_provenance());
}

#[test]
fn the_record_time_is_the_forge_instant_when_known_and_never_after_the_poll() {
    let mut row = first_seen();
    assert_eq!(row.record_time(), t(0), "no forge instant: the poll");
    assert!(
        !serde_json::to_string(&row).unwrap().contains("forge_at"),
        "absent, never null: older readers see the row unchanged"
    );
    row.forge_at = Some(t(-3600));
    assert_eq!(row.record_time(), t(-3600), "the label application");
    row.forge_at = Some(t(60));
    assert_eq!(row.record_time(), t(0), "never after the observation");
}
