//! Issue #11197: the OTLP resource carries the fleet provenance of the
//! exporting host — `loom.daemon.revision` and `loom.fleet.config_sha` — on
//! every signal, read **at call time** so a config roll surfaces on the next
//! export batch without a restart.

use super::*;
use crate::fleet_sync::{self, applied::AppliedSha};
use crate::telemetry::trace::{SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext};
use serial_test::serial;

/// The process-wide applied-SHA cell is shared with `fleet_sync`'s own tests:
/// every test here serializes on the same key, and pins `LOOM_SOCKET_PATH` to
/// a fresh temp dir so the empty cell cannot seed from this host's real
/// `fleet-sync-status.json`.
fn with_clean_cell<T>(f: impl FnOnce() -> T) -> T {
    let dir = tempfile::tempdir().expect("dir");
    let prior = std::env::var("LOOM_SOCKET_PATH").ok();
    std::env::set_var("LOOM_SOCKET_PATH", dir.path().join("loom-daemon.sock"));
    fleet_sync::applied::reset_for_tests();
    let out = f();
    fleet_sync::applied::reset_for_tests();
    match prior {
        Some(value) => std::env::set_var("LOOM_SOCKET_PATH", value),
        None => std::env::remove_var("LOOM_SOCKET_PATH"),
    }
    out
}

/// The resource of each signal's first entry, as `(key, string value)` pairs.
fn per_signal_pairs(batch: &[TelemetryEnvelope]) -> [(&'static str, Vec<(String, String)>); 3] {
    let logs = build_logs_request(batch).unwrap();
    let metrics = build_metrics_request(batch).unwrap();
    let traces = super::super::super::traces::build_traces_request(batch).unwrap();
    let pairs = |resource: Option<&Resource>| {
        resource
            .unwrap()
            .attributes
            .iter()
            .map(|kv| match kv.value.as_ref().and_then(|v| v.value.as_ref()) {
                Some(any_value::Value::StringValue(s)) => (kv.key.clone(), s.clone()),
                other => panic!("resource attribute {} is not a string: {other:?}", kv.key),
            })
            .collect()
    };
    [
        ("logs", pairs(logs.resource_logs[0].resource.as_ref())),
        ("metrics", pairs(metrics.resource_metrics[0].resource.as_ref())),
        ("traces", pairs(traces.resource_spans[0].resource.as_ref())),
    ]
}

fn batch() -> Vec<TelemetryEnvelope> {
    let now = Utc::now();
    let span = SpanRecord {
        context: TraceContext::root(true),
        parent_span_id: None,
        name: SpanName::Sweep,
        started_at: now,
        ended_at: now,
        status: SpanStatus::Ok,
        attributes: TraceAttributes::new(),
        events: vec![],
        links: vec![],
    };
    // The host-health envelope is what makes `build_metrics_request` return
    // `Some` — an all-lifecycle batch skips the metrics POST entirely.
    let mut health = host_health_envelope();
    health.host_id = "host-a".to_string();
    vec![
        sweep_started_envelope(),
        health,
        envelope("host-a", TelemetryRecord::Span(span)),
    ]
}

fn get<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// The applied SHA set through the test seam is on the resource of all three
/// signals, and `loom.daemon.revision` is the exporting build's revision —
/// the same value `provenance::stamp` puts on every span.
#[test]
#[serial(loom_socket_path_env)]
fn fleet_provenance_is_on_the_resource_of_every_signal() {
    with_clean_cell(|| {
        fleet_sync::applied::force_for_tests(AppliedSha::Sha(
            "0123456789abcdef0123456789abcdef01234567".to_string(),
        ));
        for (signal, pairs) in per_signal_pairs(&batch()) {
            assert_eq!(
                get(&pairs, "loom.fleet.config_sha"),
                Some("0123456789abcdef0123456789abcdef01234567"),
                "{signal}: applied SHA"
            );
            assert_eq!(
                get(&pairs, "loom.daemon.revision"),
                Some(crate::telemetry::trace::provenance::daemon().revision),
                "{signal}: daemon revision"
            );
        }
    });
}

/// The `config_sha` attribute is always present and never empty: `unknown`
/// on a fleet host with nothing established, `none` without a store — so a
/// backend `resources_string` read cannot confuse "missing" with "not known
/// yet" (the operator's empty-count criterion holds by construction).
#[test]
#[serial(loom_socket_path_env)]
fn config_sha_sentinels_are_unknown_and_none_never_absent_or_empty() {
    with_clean_cell(|| {
        fleet_sync::applied::force_for_tests(AppliedSha::Unknown);
        for (signal, pairs) in per_signal_pairs(&batch()) {
            assert_eq!(get(&pairs, "loom.fleet.config_sha"), Some("unknown"), "{signal}");
        }
        fleet_sync::applied::force_for_tests(AppliedSha::NoStore);
        for (signal, pairs) in per_signal_pairs(&batch()) {
            assert_eq!(get(&pairs, "loom.fleet.config_sha"), Some("none"), "{signal}");
        }
    });
}

/// The resource is rebuilt per export batch and reads the cell at call time:
/// a SHA that changes in process (a timer pass applied a new commit) shows
/// up on the **next** built request with no restart — the guard against a
/// future `OnceLock` memoization of the resource.
#[test]
#[serial(loom_socket_path_env)]
fn the_next_request_carries_a_sha_that_changed_in_process() {
    with_clean_cell(|| {
        fleet_sync::applied::force_for_tests(AppliedSha::Sha(
            "0123456789abcdef0123456789abcdef01234567".to_string(),
        ));
        let first = per_signal_pairs(&batch());
        for (signal, pairs) in first {
            assert_eq!(
                get(&pairs, "loom.fleet.config_sha"),
                Some("0123456789abcdef0123456789abcdef01234567"),
                "{signal}: before the simulated timer pass"
            );
        }
        fleet_sync::applied::force_for_tests(AppliedSha::Sha(
            "fedcba9876543210fedcba9876543210fedcba98".to_string(),
        ));
        for (signal, pairs) in per_signal_pairs(&batch()) {
            assert_eq!(
                get(&pairs, "loom.fleet.config_sha"),
                Some("fedcba9876543210fedcba9876543210fedcba98"),
                "{signal}: after the simulated timer pass, no restart"
            );
        }
    });
}

/// The `daemon.start` record rides the existing `daemon.event` mapping: event
/// name `daemon.event`, `loom.topic = "daemon.start"`, and the payload —
/// carried whole as compact JSON — still names the build, supervisor,
/// applied config SHA and pid.
#[test]
fn daemon_start_maps_to_a_log_with_topic_and_full_payload() {
    let batch = vec![envelope(
        "host-a",
        crate::observability::daemon_lifecycle::start_record("systemd"),
    )];
    let logs = build_logs_request(&batch).unwrap();
    let log = &logs.resource_logs[0].scope_logs[0].log_records[0];
    let attribute = |key: &str| {
        log.attributes
            .iter()
            .find(|kv| kv.key == key)
            .and_then(|kv| kv.value.as_ref())
            .and_then(|v| v.value.as_ref())
            .and_then(|v| match v {
                any_value::Value::StringValue(s) => Some(s.as_str()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{key} missing from the daemon.start log"))
    };
    assert_eq!(attribute("loom.topic"), "daemon.start");
    let payload: serde_json::Value =
        serde_json::from_str(attribute("loom.payload")).expect("payload is compact JSON");
    let build = crate::telemetry::trace::provenance::daemon();
    assert_eq!(payload["version"], build.version);
    assert_eq!(payload["revision"], build.revision);
    assert_eq!(payload["tree_state"], build.tree_state);
    assert_eq!(payload["supervisor"], "systemd");
    let sha = payload["config_sha"]
        .as_str()
        .expect("config_sha is a string");
    assert!(
        sha == "unknown" || sha == "none" || sha.len() == 40,
        "config_sha must be a SHA or an explicit sentinel, got {sha:?}"
    );
    assert_eq!(payload["pid"], serde_json::json!(std::process::id()));
}
