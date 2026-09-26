//! The CI queue segment (#9007 follow-up): a `ci.run` record and its
//! `loom.ci.run` span carry `queued_ms` = `run_started_at − created_at`, so a
//! per-PR breakdown can separate "CI queued" from "CI running". Before this
//! only the effective `started_at` reached telemetry and the queue wait was
//! invisible.

use super::*;
use crate::ci_telemetry::records::RunJson;

fn run(started: Option<&str>) -> RunJson {
    let mut value = serde_json::json!({
        "id": 5, "head_sha": "abc", "event": "push", "status": "completed",
        "created_at": "2026-09-20T10:00:00Z", "updated_at": "2026-09-20T10:05:00Z",
    });
    if let Some(started) = started {
        value["run_started_at"] = serde_json::json!(started);
    }
    serde_json::from_value(value).unwrap()
}

#[test]
fn queued_ms_is_created_to_started_and_absent_without_a_start() {
    assert_eq!(run(Some("2026-09-20T10:01:30Z")).queued_ms(), Some(90_000));
    // Clock skew never produces a negative queue.
    assert_eq!(run(Some("2026-09-20T09:59:00Z")).queued_ms(), Some(0));
    assert_eq!(run(None).queued_ms(), None);
}

#[test]
fn every_fixture_run_carries_its_queue_time_on_record_and_run_span() {
    let dir = TempDir::new().unwrap();
    run_cycle(&ctx(dir.path()), &FixtureApi::new()).unwrap();
    let (mut records, mut run_spans) = (0, 0);
    for env in journal(dir.path()) {
        match env.record {
            TelemetryRecord::CiRun(r) => {
                // Every fixture run starts 10s after it is created.
                assert_eq!(r.queued_ms, Some(10_000), "run {}", r.run_id);
                assert!(r
                    .log_attributes()
                    .iter()
                    .any(|(k, v)| *k == "loom.ci.queued_ms"
                        && *v == crate::telemetry::ci::CiAttr::Int(10_000)));
                records += 1;
            }
            TelemetryRecord::Span(s) if s.parent_span_id.is_none() => {
                assert_eq!(
                    s.attributes.get("loom.ci.queued_ms").map(String::as_str),
                    Some("10000")
                );
                run_spans += 1;
            }
            TelemetryRecord::Span(s) => {
                assert!(!s.attributes.contains_key("loom.ci.queued_ms"), "job spans carry none");
            }
            _ => {}
        }
    }
    assert_eq!((records, run_spans), (6, 6));
}

#[test]
fn a_pre_follow_up_run_record_still_decodes_without_queue_time() {
    let old = serde_json::json!({
        "repo": "o/r", "run_id": 1, "run_attempt": 1, "workflow": "CI", "head_sha": "a",
        "event": "push", "status": "completed", "started_at": "2026-09-20T10:00:00Z",
        "completed_at": "2026-09-20T10:01:00Z", "duration_ms": 60000,
    });
    let record: crate::telemetry::CiRunRecord = serde_json::from_value(old).unwrap();
    assert_eq!(record.queued_ms, None);
    assert!(!record
        .log_attributes()
        .iter()
        .any(|(k, _)| *k == "loom.ci.queued_ms"));
}
