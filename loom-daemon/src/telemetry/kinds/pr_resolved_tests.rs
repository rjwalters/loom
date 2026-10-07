//! `pr.resolved` (#10519): registration, wire shape, and the journal mapping.

use super::*;
use crate::eta::Stage;
use crate::telemetry::{
    TelemetryEnvelope, TelemetryKindOtlp, TelemetryRecord, NEW_KIND_SCHEMA_VERSION, TELEMETRY_KINDS,
};
use chrono::{Duration, TimeZone};

const REPO: &str = "rjwalters/loom";

fn provenance() -> Provenance {
    Provenance {
        version: "0.19.800".to_string(),
        revision: "9d8e226ce0123456789abcdef0123456789abcde".to_string(),
        tree_state: "clean".to_string(),
        complete: true,
    }
}

fn t(sec: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 6, 12, 0, 0).unwrap() + Duration::seconds(sec)
}

fn resolved_row(pr: u32, state: &str, left_at: Option<DateTime<Utc>>) -> JournalEntry {
    let mut row = JournalEntry::new("pr.resolved", REPO, left_at.unwrap_or(t(0)), &provenance());
    row.issue = Some(pr - 1);
    row.pr_number = Some(pr);
    row.left_at = left_at;
    row.raw = serde_json::json!({"pr": pr, "state": state});
    row
}

#[test]
fn pr_resolved_is_registered_as_an_otlp_log_kind() {
    let meta = TELEMETRY_KINDS
        .iter()
        .find(|m| m.kind == "pr.resolved")
        .expect("pr.resolved has a registry row");
    assert_eq!(meta.variant, "PrResolved");
    assert_eq!(meta.otlp, TelemetryKindOtlp::Logs);
    assert!(!meta.native_ingest, "OTLP-only, like the eta.* log kinds");
    assert_eq!(meta.schema_version, NEW_KIND_SCHEMA_VERSION);
}

#[test]
fn a_merge_carries_the_forge_instant_and_a_close_the_observation() {
    let merged_at = t(-900);
    let rows = vec![
        resolved_row(101, "merged", Some(merged_at)),
        resolved_row(202, "closed", None),
    ];
    let records = from_journal(&rows, t(0), 300, &provenance());
    assert_eq!(records.len(), 2);

    let merge = &records[0];
    assert_eq!((merge.pr_number, merge.state), (101, PrResolution::Merged));
    assert_eq!(merge.issue, Some(100));
    assert_eq!(merge.resolved_at, merged_at, "the forge's merged_at");
    assert_eq!(merge.observed_at, t(0), "knowable when the pass saw it");
    assert_eq!(merge.resolution_sec, 0);

    let close = &records[1];
    assert_eq!((close.pr_number, close.state), (202, PrResolution::Closed));
    assert_eq!(close.resolved_at, t(0), "no close instant: the observation");
    assert_eq!(close.resolution_sec, 300, "at most one listing interval late");
}

#[test]
fn a_held_merge_is_one_record_and_other_rows_are_none() {
    let merged_at = t(-60);
    let mut hold = resolved_row(101, "merged", Some(merged_at));
    hold.stage = Some(Stage::MergeHold);
    let mut open = resolved_row(303, "open", None);
    open.raw = serde_json::json!({"pr": 303, "state": "open"});
    let mut label = resolved_row(404, "merged", Some(merged_at));
    label.event = "label.transition".to_string();
    let mut no_pr = resolved_row(505, "merged", Some(merged_at));
    no_pr.pr_number = None;
    no_pr.raw = serde_json::json!({"state": "merged"});
    let rows = vec![
        hold,
        resolved_row(101, "merged", Some(merged_at)),
        open,
        label,
        no_pr,
    ];
    let records = from_journal(&rows, t(0), 300, &provenance());
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].pr_number, 101);
}

#[test]
fn an_event_time_is_never_after_the_observation() {
    let rows = vec![resolved_row(101, "merged", Some(t(60)))];
    let records = from_journal(&rows, t(0), 300, &provenance());
    assert_eq!(records[0].resolved_at, t(0));
}

#[test]
fn the_record_round_trips_and_requires_provenance() {
    let records =
        from_journal(&[resolved_row(101, "merged", Some(t(-5)))], t(0), 300, &provenance());
    let record = records[0].clone();
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
