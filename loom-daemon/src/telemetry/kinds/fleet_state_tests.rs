//! `fleet.state` registration and wire contract (Issue #10196, slice 2).

use super::*;
use crate::telemetry::{
    TelemetryEnvelope, TelemetryKindOtlp, TelemetryRecord, NEW_KIND_SCHEMA_VERSION, TELEMETRY_KINDS,
};
use chrono::TimeZone;

fn at(h: u32, m: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 4, h, m, 0).unwrap()
}

fn held_row() -> FleetStateRow {
    FleetStateRow {
        issue: 10196,
        stage: Stage::SweepBuilder,
        entered_at: at(12, 0),
        entered_at_lower_bound: false,
        pr: Some(10267),
        host: Some("robb-studio".to_string()),
        slot: Some(FleetSlot::Regular),
    }
}

fn listed_row() -> FleetStateRow {
    FleetStateRow {
        issue: 10193,
        stage: Stage::ReviewWait,
        entered_at: at(11, 0),
        entered_at_lower_bound: true,
        pr: Some(10250),
        host: None,
        slot: None,
    }
}

fn record() -> FleetStateRecord {
    FleetStateRecord {
        schema: FLEET_STATE_SCHEMA.to_string(),
        as_of: at(12, 5),
        anchor: true,
        anchor_as_of: at(12, 5),
        prev_as_of: None,
        census_at: Some(at(12, 4)),
        slots: Some(FleetSlots {
            max_concurrent: 4,
            occupancy: Some(1),
        }),
        repos: vec![FleetStateRepo {
            repo: "rjwalters/loom".to_string(),
            visibility: RepoVisibility::Public,
            census: Some(FleetPrCensus {
                open: 2,
                by_stage: [("review_wait".to_string(), 2)].into_iter().collect(),
            }),
            rows: vec![listed_row(), held_row()],
            removed: Vec::new(),
        }],
        rows_truncated: 0,
    }
}

/// OTLP-only log kind on the shared post-#8921 gate. It is not native: it is a
/// replay record, not a dashboard state key.
#[test]
fn fleet_state_is_registered_as_an_otlp_only_log_kind() {
    let meta = TELEMETRY_KINDS
        .iter()
        .find(|m| m.kind == "fleet.state")
        .expect("fleet.state registered");
    assert_eq!(meta.variant, "FleetState");
    assert_eq!(meta.schema_version, NEW_KIND_SCHEMA_VERSION);
    assert_eq!(meta.otlp, TelemetryKindOtlp::Logs);
    assert!(!meta.native_ingest);

    let record = TelemetryRecord::FleetState(record());
    assert_eq!(record.kind(), "fleet.state");
    assert_eq!(record.otlp_class(), TelemetryKindOtlp::Logs);
    assert!(!record.accepted_by_native_ingest());
    assert_eq!(TelemetryEnvelope::new("host", record).schema_version, NEW_KIND_SCHEMA_VERSION);
}

#[test]
fn fleet_state_round_trips_through_the_envelope() {
    let envelope = TelemetryEnvelope::new("host", TelemetryRecord::FleetState(record()));
    let json = serde_json::to_string(&envelope).unwrap();
    let back: TelemetryEnvelope = serde_json::from_str(&json).unwrap();
    assert_eq!(back, envelope);
}

fn keys(value: &serde_json::Value) -> Vec<String> {
    let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    keys
}

/// The AC's five per-item facts: stage, entered-at, PR, host and slot. Pinned,
/// because adding a field widens what a host publishes about a private repo.
#[test]
fn a_held_row_carries_stage_entered_at_pr_host_and_slot() {
    let wire = serde_json::to_value(held_row()).unwrap();
    assert_eq!(keys(&wire), vec!["entered_at", "host", "issue", "pr", "slot", "stage"]);
    assert_eq!(wire["stage"], "sweep.builder");
    assert_eq!(wire["slot"], "regular");
    assert_eq!(wire["host"], "robb-studio");
}

/// A row seen only through a review listing has no known host. Its host and
/// slot are absent, not guessed, and a lower-bound entry says so.
#[test]
fn a_listed_row_omits_host_and_slot_and_flags_a_lower_bound_entry() {
    let wire = serde_json::to_value(listed_row()).unwrap();
    assert_eq!(
        keys(&wire),
        vec![
            "entered_at",
            "entered_at_lower_bound",
            "issue",
            "pr",
            "stage"
        ]
    );
    assert_eq!(wire["entered_at_lower_bound"], true);
}

/// Slim rows: a held row stays well under the size [`MAX_ROWS`] is sized for.
#[test]
fn rows_are_slim() {
    let held = serde_json::to_string(&held_row()).unwrap().len();
    let listed = serde_json::to_string(&listed_row()).unwrap().len();
    assert!(held <= 140, "held row is {held} bytes");
    assert!(listed <= 130, "listed row is {listed} bytes");
    assert!(held * MAX_ROWS < 72 * 1024, "a full anchor stays under ~70 KB");
}

/// An absent census decodes as unknown, never as zero, and a missing
/// visibility decodes as private.
#[test]
fn absent_census_is_unknown_and_missing_visibility_is_private() {
    let repo: FleetStateRepo = serde_json::from_value(serde_json::json!({
        "repo": "acme/secret"
    }))
    .unwrap();
    assert_eq!(repo.census, None);
    assert_eq!(repo.visibility, RepoVisibility::Private);
    assert!(repo.rows.is_empty() && repo.removed.is_empty());
}

#[test]
fn counts_sum_over_repos() {
    let mut r = record();
    r.repos[0].removed = vec![1, 2, 3];
    assert_eq!(r.row_count(), 2);
    assert_eq!(r.removed_count(), 3);
}

#[test]
fn collector_keeps_every_fleet_state_log_attribute() {
    const CONFIG: &str = include_str!("../../../../defaults/observability/collector/config.yaml");
    let log_keep = CONFIG
        .lines()
        .find(|l| {
            l.contains("keep_keys(attributes, [")
                && l.contains("loom.ci.chunk_index")
                && l.contains("loom.eta.estimate_id")
        })
        .expect("the transform/privacy log keep_keys line");
    for key in FLEET_STATE_LOG_ATTRIBUTE_KEYS
        .iter()
        .chain(&["loom.kind", "loom.record_id"])
    {
        assert!(log_keep.contains(&format!("\"{key}\"")), "collector drops {key}");
    }
}
