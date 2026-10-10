//! `eta.stage_outcome`: registration, exit classification, the wire shape and
//! the outcome-fact attribute allowlist.

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

fn record() -> StageOutcomeRecord {
    let at = Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap();
    StageOutcomeRecord {
        repo: "rjwalters/loom".to_string(),
        issue: 7,
        pr_number: Some(8),
        stage: FleetStage::ReviewWait,
        entered_at: Some(at - chrono::Duration::seconds(900)),
        entered_at_source: Some(EnteredAtSource::Forge),
        left_at: at,
        dwell_sec: Some(900),
        exit: StageExit::Pass,
        next_stage: Some(FleetStage::MergeWait),
        event: FLEET_STATE_EVENT.to_string(),
        observed_at: at + chrono::Duration::seconds(120),
        resolution_sec: Some(0),
        forge_transition_at: Some(at),
        loom: provenance(),
    }
}

#[test]
fn eta_stage_outcome_is_registered_as_an_otlp_log_kind() {
    let meta = TELEMETRY_KINDS
        .iter()
        .find(|m| m.kind == "eta.stage_outcome")
        .expect("eta.stage_outcome has a registry row");
    assert_eq!(meta.variant, "StageOutcome");
    assert_eq!(meta.otlp, TelemetryKindOtlp::Logs);
    assert!(!meta.native_ingest, "OTLP-only");
    assert_eq!(meta.schema_version, NEW_KIND_SCHEMA_VERSION);
}

#[test]
fn every_stage_move_gets_its_exit() {
    use FleetStage::{Doctor, MergeHold, MergeWait, ReadyWait, ReviewWait, SweepBuilder};
    let cases = [
        (ReviewWait, MergeWait, StageExit::Pass),
        (ReviewWait, Doctor, StageExit::Rework),
        (MergeWait, Doctor, StageExit::Rework),
        (MergeWait, MergeHold, StageExit::Hold),
        (MergeHold, MergeWait, StageExit::Released),
        (Doctor, ReviewWait, StageExit::Advance),
        (ReadyWait, FleetStage::SweepCurator, StageExit::Advance),
        (SweepBuilder, ReviewWait, StageExit::Advance),
    ];
    for (from, to, exit) in cases {
        assert_eq!(StageExit::between(from, to), exit, "{from:?} -> {to:?}");
        assert_eq!(serde_json::to_value(exit).unwrap(), exit.as_str(), "wire name");
    }
}

/// The body keeps every field name it had under the ETA owner, minus the
/// three estimate-derived fields loom-ui agreed to drop (#11098), plus the
/// additive `entered_at_source` (#11367).
#[test]
fn the_body_keeps_its_field_names_without_the_dropped_estimate_fields() {
    let json = serde_json::to_value(record()).unwrap();
    let keys: Vec<&str> = json
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let mut expected = vec![
        "repo",
        "issue",
        "pr_number",
        "stage",
        "entered_at",
        "entered_at_source",
        "left_at",
        "dwell_sec",
        "exit",
        "next_stage",
        "event",
        "observed_at",
        "resolution_sec",
        "forge_transition_at",
        "loom",
    ];
    expected.sort_unstable();
    let mut keys = keys;
    keys.sort_unstable();
    assert_eq!(keys, expected);
    for dropped in ["open_estimates", "estimate_ids", "repo_id"] {
        assert!(json.get(dropped).is_none(), "{dropped} was dropped");
    }
    assert_eq!(json["stage"], "review_wait");
    assert_eq!(json["next_stage"], "merge_wait");
}

#[test]
fn an_old_body_with_the_dropped_fields_still_parses() {
    let old = serde_json::json!({
        "repo": "rjwalters/loom", "repo_id": 1, "issue": 7, "stage": "review_wait",
        "left_at": "2026-10-08T12:00:00Z", "exit": "pass", "event": "label.transition",
        "observed_at": "2026-10-08T12:00:00Z", "open_estimates": 3,
        "estimate_ids": ["a"], "loom": serde_json::to_value(provenance()).unwrap()
    });
    let parsed: StageOutcomeRecord = serde_json::from_value(old).unwrap();
    assert_eq!(parsed.forge_transition_at, None);
    assert_eq!(parsed.entered_at_source, None, "a body from before #11367");
}

#[test]
fn entered_at_source_has_its_wire_names() {
    for (source, name) in [
        (EnteredAtSource::Forge, "forge"),
        (EnteredAtSource::Checkpoint, "checkpoint"),
        (EnteredAtSource::Unknown, "unknown"),
    ] {
        assert_eq!(source.as_str(), name);
        assert_eq!(serde_json::to_value(source).unwrap(), name);
        let back: EnteredAtSource = serde_json::from_value(name.into()).unwrap();
        assert_eq!(back, source);
    }
    assert_eq!(serde_json::to_value(record()).unwrap()["entered_at_source"], "forge");
}

#[test]
fn the_record_round_trips_and_requires_provenance() {
    let record = record();
    assert!(record.has_provenance());
    let envelope = TelemetryEnvelope::new("host-a", TelemetryRecord::StageOutcome(record.clone()));
    let json = serde_json::to_value(&envelope).unwrap();
    assert_eq!(json["record"]["kind"], "eta.stage_outcome");
    let back: TelemetryEnvelope = serde_json::from_value(json).unwrap();
    assert_eq!(back, envelope);
    let unproven = StageOutcomeRecord {
        loom: Provenance {
            revision: "not-a-sha".to_string(),
            ..provenance()
        },
        ..record
    };
    assert!(!unproven.has_provenance());
}

#[test]
fn collector_keeps_every_outcome_fact_log_attribute() {
    const CONFIG: &str = include_str!("../../../../defaults/observability/collector/config.yaml");
    let log_keep = CONFIG
        .lines()
        .find(|l| {
            l.contains("keep_keys(attributes, [")
                && l.contains("loom.ci.chunk_index")
                && l.contains("loom.eta.pr.state")
        })
        .expect("the transform/privacy log keep_keys line");
    for key in OUTCOME_FACT_LOG_ATTRIBUTE_KEYS.iter().chain(&[
        "loom.kind",
        "loom.record_id",
        "loom.repo",
        "loom.issue",
        "loom.pr_number",
    ]) {
        assert!(log_keep.contains(&format!("\"{key}\"")), "collector drops {key}");
    }
}
