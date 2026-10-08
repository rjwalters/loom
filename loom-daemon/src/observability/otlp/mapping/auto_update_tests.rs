use chrono::{TimeZone, Utc};
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

use super::super::log_record_for;
use crate::eta::Provenance;
use crate::telemetry::kinds::auto_update_tick::{
    AutoUpdateTickRecord, DrainSnapshot, TickDecisionKind, AUTO_UPDATE_LOG_ATTRIBUTE_KEYS,
};
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};

fn record() -> AutoUpdateTickRecord {
    AutoUpdateTickRecord {
        tick_id: "0123456789abcdef0123456789abcdef".to_string(),
        started_at: Utc.with_ymd_and_hms(2026, 10, 5, 1, 30, 0).unwrap(),
        decision: TickDecisionKind::Fetch,
        reason: "artifact 0.19.731 > installed 0.19.701 → fetching".to_string(),
        outcome: Some("success".to_string()),
        roll_armed: true,
        installed_version: Some("0.19.701".to_string()),
        target_version: Some("0.19.731".to_string()),
        target_published_at: Some("2026-10-05T11:18:03Z".to_string()),
        commits_behind: Some(30),
        hours_behind: Some(12),
        in_flight: Some(2),
        drain: DrainSnapshot {
            armed: true,
            pending: true,
            refusals: 1,
            target: Some("artifact:0.19.731:feedface".to_string()),
        },
        floor_stall: None,
        consecutive_failures: 0,
        duration_ms: 4200,
        loom: Provenance {
            version: "0.19.701".to_string(),
            revision: "9d8e226ce0123456789abcdef0123456789abcde".to_string(),
            tree_state: "clean".to_string(),
            complete: true,
        },
    }
}

fn attr(log: &opentelemetry_proto::tonic::logs::v1::LogRecord, key: &str) -> Option<Value> {
    log.attributes
        .iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| kv.value.as_ref())
        .and_then(|v| v.value.clone())
}

#[test]
fn a_full_tick_emits_every_key_and_only_allowlisted_ones() {
    let tick = record();
    let envelope = TelemetryEnvelope::new("host", TelemetryRecord::AutoUpdateTick(tick.clone()));
    let log = log_record_for(&envelope).unwrap();
    assert_eq!(log.event_name, "auto_update.tick");
    assert_eq!(log.time_unix_nano, super::nanos(tick.started_at));
    assert_eq!(log.severity_number, SeverityNumber::Info as i32);
    for kv in &log.attributes {
        assert!(
            AUTO_UPDATE_LOG_ATTRIBUTE_KEYS.contains(&kv.key.as_str())
                || ["loom.record_id", "loom.kind"].contains(&kv.key.as_str()),
            "{} is not allowlisted",
            kv.key
        );
    }
    for key in AUTO_UPDATE_LOG_ATTRIBUTE_KEYS {
        assert!(attr(&log, key).is_some(), "{key} is emitted");
    }
    assert_eq!(
        attr(&log, "loom.auto_update.decision"),
        Some(Value::StringValue("fetch".into()))
    );
    assert_eq!(
        attr(&log, "loom.auto_update.target_version"),
        Some(Value::StringValue("0.19.731".into()))
    );
    assert_eq!(
        attr(&log, "loom.auto_update.revision"),
        Some(Value::StringValue("9d8e226ce0123456789abcdef0123456789abcde".into()))
    );
    let Some(Value::StringValue(body)) = log.body.as_ref().and_then(|b| b.value.clone()) else {
        panic!("string body");
    };
    let parsed: AutoUpdateTickRecord = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed, tick);
}

#[test]
fn optional_fields_are_omitted_not_fabricated() {
    let mut tick = record();
    tick.decision = TickDecisionKind::Skip;
    tick.outcome = None;
    tick.target_version = None;
    tick.target_published_at = None;
    tick.in_flight = None;
    tick.drain = DrainSnapshot::default();
    let envelope = TelemetryEnvelope::new("host", TelemetryRecord::AutoUpdateTick(tick));
    let log = log_record_for(&envelope).unwrap();
    for key in [
        "loom.auto_update.outcome",
        "loom.auto_update.target_version",
        "loom.auto_update.in_flight",
        "loom.auto_update.drain_target",
    ] {
        assert_eq!(attr(&log, key), None, "{key}");
    }
    assert_eq!(attr(&log, "loom.auto_update.drain_armed"), Some(Value::BoolValue(false)));
}

#[test]
fn severity_tracks_whether_the_host_is_converging() {
    let cases = [
        (TickDecisionKind::Panic, None, SeverityNumber::Error),
        (TickDecisionKind::RollStall, None, SeverityNumber::Warn),
        (TickDecisionKind::StaleRepo, None, SeverityNumber::Warn),
        (TickDecisionKind::Fetch, Some("retryable"), SeverityNumber::Warn),
        (TickDecisionKind::Fetch, Some("success"), SeverityNumber::Info),
        (TickDecisionKind::Defer, None, SeverityNumber::Info),
    ];
    for (decision, outcome, expected) in cases {
        let mut tick = record();
        tick.decision = decision;
        tick.outcome = outcome.map(str::to_string);
        let envelope = TelemetryEnvelope::new("host", TelemetryRecord::AutoUpdateTick(tick));
        let log = log_record_for(&envelope).unwrap();
        assert_eq!(log.severity_number, expected as i32, "{decision:?} {outcome:?}");
    }
}

#[test]
fn an_unsatisfiable_floor_is_an_error_whatever_the_tick_decided() {
    // #10712: the floor alert rides on any decision, including a healthy fetch.
    for decision in [
        TickDecisionKind::Skip,
        TickDecisionKind::Defer,
        TickDecisionKind::Fetch,
    ] {
        let mut tick = record();
        tick.decision = decision;
        tick.floor_stall = Some("FLEET FLOOR UNSATISFIABLE: ...".to_string());
        let envelope = TelemetryEnvelope::new("host", TelemetryRecord::AutoUpdateTick(tick));
        let log = log_record_for(&envelope).unwrap();
        assert_eq!(log.severity_number, SeverityNumber::Error as i32, "{decision:?}");
        let body = log.body.and_then(|b| b.value);
        assert!(matches!(body, Some(Value::StringValue(b)) if b.contains("floor_stall")));
    }
    // And absent, the field is not serialized at all (no-floor records are unchanged).
    let body = serde_json::to_string(&record()).unwrap();
    assert!(!body.contains("floor_stall"), "{body}");
}

#[test]
fn collector_keeps_every_auto_update_log_attribute() {
    const CONFIG: &str =
        include_str!("../../../../../defaults/observability/collector/config.yaml");
    let log_keep = CONFIG
        .lines()
        .find(|l| {
            l.contains("keep_keys(attributes, [")
                && l.contains("loom.ci.chunk_index")
                && l.contains("loom.eta.estimate_id")
        })
        .expect("the transform/privacy log keep_keys line");
    for key in AUTO_UPDATE_LOG_ATTRIBUTE_KEYS {
        assert!(log_keep.contains(&format!("\"{key}\"")), "collector drops {key}");
    }
}
