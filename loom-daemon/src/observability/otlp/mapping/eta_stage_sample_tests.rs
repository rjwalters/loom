//! OTLP mapping for `eta.stage_sample` (#10756), and the exporter-to-reader
//! contract: a stage-journal label row, mapped by [`log_record_for`] and read
//! back in the shape `TIMELINE_SQL` returns, is admitted as a daemon label set
//! with the row's `observed_at`.

use super::super::log_record_for;
use crate::eta::fleet_signoz_timeline_rows::{parse_row, ParsedRow, RowBody, Source, Target};
use crate::eta::journal::JournalEntry;
use crate::eta::{Provenance, Stage};
use crate::telemetry::kinds::eta::ETA_LOG_ATTRIBUTE_KEYS;
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};
use chrono::{DateTime, Duration, TimeZone, Utc};
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::logs::v1::LogRecord;
use serde_json::{json, Map};

const REPO: &str = "rjwalters/loom";

fn provenance() -> Provenance {
    Provenance {
        version: "0.19.800".to_string(),
        revision: "9d8e226ce0123456789abcdef0123456789abcde".to_string(),
        tree_state: "clean".to_string(),
        complete: true,
    }
}

fn polled() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap()
}

/// A `label.first_seen` row as the tracker writes it on first sight of a PR
/// in review, dated from the label timeline.
fn first_seen() -> JournalEntry {
    let mut row = JournalEntry::new("label.first_seen", REPO, polled(), &provenance());
    row.issue = Some(10756);
    row.pr_number = Some(11020);
    row.next_stage = Some(Stage::ReviewWait);
    row.forge_at = Some(polled() - Duration::hours(3));
    row.raw = json!({"labels": ["loom:review-requested"], "updated_at": null});
    row
}

fn attr(log: &LogRecord, key: &str) -> Option<Value> {
    log.attributes
        .iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| kv.value.as_ref())
        .and_then(|v| v.value.clone())
}

/// The `JSONEachRow` line `TIMELINE_SQL` returns for `log`, exported by host
/// `service = loom` with no d1sync scope (the daemon).
fn timeline_line(log: &LogRecord) -> String {
    let (mut strings, mut numbers, mut bools) = (Map::new(), Map::new(), Map::new());
    for kv in &log.attributes {
        match kv.value.as_ref().and_then(|v| v.value.clone()) {
            Some(Value::StringValue(s)) => drop(strings.insert(kv.key.clone(), json!(s))),
            Some(Value::IntValue(n)) => drop(numbers.insert(kv.key.clone(), json!(n))),
            Some(Value::BoolValue(b)) => drop(bools.insert(kv.key.clone(), json!(b))),
            _ => {}
        }
    }
    let Some(Value::StringValue(body)) = log.body.as_ref().and_then(|b| b.value.clone()) else {
        panic!("string body");
    };
    let event: serde_json::Value = serde_json::from_str(&body).unwrap();
    json!({
        "record_id": strings["loom.record_id"],
        "identity": strings["loom.record_id"],
        "service": "loom",
        "scope": "",
        "kind": strings["loom.kind"],
        "repo": strings["loom.repo"],
        "journal_event": event["event"],
        "attrs": serde_json::Value::Object(strings).to_string(),
        "nums": serde_json::Value::Object(numbers).to_string(),
        "bools": serde_json::Value::Object(bools).to_string(),
        "body": body,
        "event_time_ns": log.time_unix_nano.to_string(),
        "knowable_time_ns": log.observed_time_unix_nano.to_string(),
    })
    .to_string()
}

#[test]
fn a_stage_sample_is_stamped_at_its_forge_instant_and_observed_at_the_export() {
    let row = first_seen();
    let envelope = TelemetryEnvelope::new("host-b", TelemetryRecord::EtaStageSample(row.clone()));
    let log = log_record_for(&envelope).unwrap();
    assert_eq!(log.event_name, "eta.stage_sample");
    assert_eq!(log.time_unix_nano, super::nanos(row.forge_at.unwrap()), "forge time");
    assert_eq!(log.observed_time_unix_nano, super::nanos(envelope.emitted_at), "knowable-at");
    for kv in &log.attributes {
        assert!(
            ETA_LOG_ATTRIBUTE_KEYS.contains(&kv.key.as_str())
                || [
                    "loom.kind",
                    "loom.repo",
                    "loom.record_id",
                    "loom.pr_number",
                    "loom.issue"
                ]
                .contains(&kv.key.as_str()),
            "{} is not allowlisted",
            kv.key
        );
    }
    for key in ETA_LOG_ATTRIBUTE_KEYS
        .iter()
        .filter(|k| k.starts_with("loom.eta.stage_sample."))
    {
        assert!(attr(&log, key).is_some(), "{key} is emitted");
    }
    assert_eq!(attr(&log, "loom.kind"), Some(Value::StringValue("eta.stage_sample".into())));
    assert!(
        attr(&log, "loom.eta.authority").is_none(),
        "every host exports, not the authority"
    );
    let Some(Value::StringValue(body)) = log.body.as_ref().and_then(|b| b.value.clone()) else {
        panic!("string body");
    };
    let parsed: JournalEntry = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed, row, "the body is the journal row, verbatim");
}

#[test]
fn a_row_with_no_forge_instant_is_stamped_at_the_poll() {
    let mut row = first_seen();
    row.forge_at = None;
    row.stage = Some(Stage::ReviewWait);
    row.sweep_id = Some("sweep-1".to_string());
    let log =
        log_record_for(&TelemetryEnvelope::new("h", TelemetryRecord::EtaStageSample(row))).unwrap();
    assert_eq!(log.time_unix_nano, super::nanos(polled()));
    assert!(attr(&log, "loom.eta.stage_sample.forge_at").is_none(), "absent, never zero");
    assert_eq!(attr(&log, "loom.eta.stage"), Some(Value::StringValue("review_wait".into())));
    assert_eq!(attr(&log, "loom.sweep_id"), Some(Value::StringValue("sweep-1".into())));
}

/// The contract #10756 asked for: what the daemon exports is what the
/// timeline reader admits, as a label set at the row's own `observed_at`.
#[test]
fn the_timeline_reader_admits_an_exported_label_row_as_a_daemon_label_set() {
    for event in ["label.first_seen", "label.transition"] {
        let mut row = first_seen();
        row.event = event.to_string();
        let envelope = TelemetryEnvelope::new("host-b", TelemetryRecord::EtaStageSample(row));
        let log = log_record_for(&envelope).unwrap();
        let (_, parsed) = parse_row(&timeline_line(&log), REPO).unwrap();
        let ParsedRow::Admitted(admitted) = parsed else {
            panic!("{event} not admitted: {parsed:?}");
        };
        assert_eq!(admitted.source, Source::Daemon);
        assert_eq!(admitted.observed_at, Some(polled()), "dated by its own observation");
        let RowBody::LabelSet { item, labels } = admitted.body else {
            panic!("{event} is not a label set: {:?}", admitted.body);
        };
        assert_eq!((item.target, item.number), (Target::Pr, 11020));
        assert_eq!(labels.into_iter().collect::<Vec<_>>(), ["loom:review-requested"]);
    }
}

#[test]
fn an_exported_non_label_row_says_nothing_to_the_timeline() {
    let mut row = first_seen();
    row.event = "sweep.phase".to_string();
    let log =
        log_record_for(&TelemetryEnvelope::new("h", TelemetryRecord::EtaStageSample(row))).unwrap();
    let (_, parsed) = parse_row(&timeline_line(&log), REPO).unwrap();
    assert_eq!(parsed, ParsedRow::Ignored);
}
