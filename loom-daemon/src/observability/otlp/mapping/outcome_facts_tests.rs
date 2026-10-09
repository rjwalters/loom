//! OTLP mapping of the outcome facts (#11126): the wire tag and attribute
//! keys are unchanged from the ETA owner, except the agreed removal of
//! `loom.eta.stage_outcome.open_estimates`.

use super::super::log_record_for;
use super::nanos;
use crate::telemetry::kinds::fleet_state::FleetStage;
use crate::telemetry::kinds::pr_resolved::{PrResolution, PrResolvedRecord};
use crate::telemetry::kinds::stage_outcome::{
    StageExit, StageOutcomeRecord, FLEET_STATE_EVENT, OUTCOME_FACT_LOG_ATTRIBUTE_KEYS,
};
use crate::telemetry::provenance::Provenance;
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};
use chrono::{Duration, TimeZone, Utc};
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::logs::v1::LogRecord;
use std::collections::BTreeSet;

const GENERIC: [&str; 5] = [
    "loom.kind",
    "loom.record_id",
    "loom.repo",
    "loom.issue",
    "loom.pr_number",
];

fn attr(log: &LogRecord, key: &str) -> Option<Value> {
    log.attributes
        .iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| kv.value.as_ref())
        .and_then(|v| v.value.clone())
}

fn keys(log: &LogRecord) -> BTreeSet<&str> {
    log.attributes.iter().map(|kv| kv.key.as_str()).collect()
}

fn provenance() -> Provenance {
    Provenance {
        version: "0.19.800".to_string(),
        revision: "9d8e226ce0123456789abcdef0123456789abcde".to_string(),
        tree_state: "clean".to_string(),
        complete: true,
    }
}

fn body(log: &LogRecord) -> String {
    let Some(Value::StringValue(body)) = log.body.as_ref().and_then(|b| b.value.clone()) else {
        panic!("string body");
    };
    body
}

fn assert_allowlisted(log: &LogRecord) {
    for kv in &log.attributes {
        assert!(
            OUTCOME_FACT_LOG_ATTRIBUTE_KEYS.contains(&kv.key.as_str())
                || GENERIC.contains(&kv.key.as_str()),
            "{} is not allowlisted",
            kv.key
        );
    }
}

/// Golden: the attribute keys `pr.resolved` carried under the ETA owner, plus
/// the additive `loom.fact_id`.
#[test]
fn pr_resolved_keeps_its_wire_tag_and_every_attribute_key() {
    let merged_at = Utc.with_ymd_and_hms(2026, 10, 6, 12, 0, 0).unwrap();
    let observed_at = merged_at + Duration::seconds(240);
    let resolved = PrResolvedRecord {
        repo: "rjwalters/loom".to_string(),
        pr_number: 10547,
        issue: Some(10511),
        state: PrResolution::Merged,
        resolved_at: merged_at,
        observed_at,
        resolution_sec: 0,
        closed_at: Some(merged_at),
        loom: provenance(),
    };
    let envelope = TelemetryEnvelope::new("host", TelemetryRecord::PrResolved(resolved.clone()));
    let log = log_record_for(&envelope).unwrap();
    assert_eq!(log.event_name, "pr.resolved");
    assert_eq!(log.time_unix_nano, nanos(merged_at), "event time");
    assert_eq!(log.observed_time_unix_nano, nanos(observed_at), "knowable-at");
    let golden: BTreeSet<&str> = [
        "loom.record_id",
        "loom.kind",
        "loom.fact_id",
        "loom.repo",
        "loom.pr_number",
        "loom.issue",
        "loom.eta.pr.state",
        "loom.eta.pr.resolved_at",
        "loom.eta.pr.observed_at",
        "loom.eta.pr.resolution_sec",
        "loom.eta.version",
        "loom.eta.revision",
        "loom.eta.tree_state",
        "loom.eta.provenance_complete",
    ]
    .into();
    assert_eq!(keys(&log), golden);
    assert_allowlisted(&log);
    assert_eq!(attr(&log, "loom.kind"), Some(Value::StringValue("pr.resolved".into())));
    assert_eq!(attr(&log, "loom.eta.pr.state"), Some(Value::StringValue("merged".into())));
    assert!(attr(&log, "loom.eta.authority").is_none(), "not a stage outcome");
    let parsed: PrResolvedRecord = serde_json::from_str(&body(&log)).unwrap();
    assert_eq!(parsed, resolved);
}

/// Golden: the attribute keys `eta.stage_outcome` carried under the ETA owner,
/// minus `loom.eta.stage_outcome.open_estimates` (dropped with loom-ui's
/// sign-off on #11098), plus the additive `loom.fact_id`.
#[test]
fn stage_outcome_keeps_its_wire_tag_and_every_attribute_key_but_open_estimates() {
    let left_at = Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap();
    let observed_at = left_at + Duration::seconds(300);
    let record = StageOutcomeRecord {
        repo: "rjwalters/loom".to_string(),
        issue: 10929,
        pr_number: Some(10950),
        stage: FleetStage::ReviewWait,
        entered_at: Some(left_at - Duration::seconds(5400)),
        left_at,
        dwell_sec: Some(5400),
        exit: StageExit::Pass,
        next_stage: Some(FleetStage::MergeWait),
        event: FLEET_STATE_EVENT.to_string(),
        observed_at,
        resolution_sec: Some(0),
        forge_transition_at: Some(left_at),
        loom: provenance(),
    };
    let envelope = TelemetryEnvelope::new("host-a", TelemetryRecord::StageOutcome(record.clone()));
    let log = log_record_for(&envelope).unwrap();
    assert_eq!(log.event_name, "eta.stage_outcome");
    assert_eq!(log.time_unix_nano, nanos(left_at), "event time");
    assert_eq!(log.observed_time_unix_nano, nanos(observed_at), "knowable-at");
    let golden: BTreeSet<&str> = [
        "loom.record_id",
        "loom.kind",
        "loom.fact_id",
        "loom.repo",
        "loom.issue",
        "loom.pr_number",
        "loom.eta.stage_outcome.stage",
        "loom.eta.stage_outcome.exit",
        "loom.eta.stage_outcome.left_at",
        "loom.eta.stage_outcome.dwell_sec",
        "loom.eta.stage_outcome.next_stage",
        "loom.eta.stage_outcome.entered_at",
        "loom.eta.version",
        "loom.eta.revision",
        "loom.eta.tree_state",
        "loom.eta.provenance_complete",
        "loom.eta.authority",
    ]
    .into();
    assert_eq!(keys(&log), golden);
    assert_allowlisted(&log);
    assert_eq!(attr(&log, "loom.kind"), Some(Value::StringValue("eta.stage_outcome".into())));
    assert_eq!(
        attr(&log, "loom.eta.authority"),
        Some(Value::StringValue("host-a".to_string())),
        "the emitting host; nothing is elected"
    );
    assert_eq!(
        attr(&log, "loom.eta.stage_outcome.exit"),
        Some(Value::StringValue("pass".into()))
    );
    assert_eq!(attr(&log, "loom.eta.stage_outcome.dwell_sec"), Some(Value::IntValue(5400)));
    let parsed: StageOutcomeRecord = serde_json::from_str(&body(&log)).unwrap();
    assert_eq!(parsed, record);
}

#[test]
fn every_outcome_fact_key_is_emitted_by_some_record() {
    let at = Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap();
    let stage = StageOutcomeRecord {
        repo: "o/r".to_string(),
        issue: 1,
        pr_number: Some(2),
        stage: FleetStage::MergeWait,
        entered_at: Some(at - Duration::seconds(60)),
        left_at: at,
        dwell_sec: Some(60),
        exit: StageExit::Hold,
        next_stage: Some(FleetStage::MergeHold),
        event: FLEET_STATE_EVENT.to_string(),
        observed_at: at,
        resolution_sec: Some(0),
        forge_transition_at: Some(at),
        loom: provenance(),
    };
    let pr = PrResolvedRecord {
        repo: "o/r".to_string(),
        pr_number: 2,
        issue: Some(1),
        state: PrResolution::Closed,
        resolved_at: at,
        observed_at: at,
        resolution_sec: 0,
        closed_at: Some(at),
        loom: provenance(),
    };
    let mut emitted = BTreeSet::new();
    for record in [
        TelemetryRecord::StageOutcome(stage),
        TelemetryRecord::PrResolved(pr),
    ] {
        let log = log_record_for(&TelemetryEnvelope::new("h", record)).unwrap();
        emitted.extend(log.attributes.iter().map(|kv| kv.key.clone()));
    }
    for key in OUTCOME_FACT_LOG_ATTRIBUTE_KEYS {
        assert!(emitted.contains(*key), "{key} is never emitted");
    }
}
