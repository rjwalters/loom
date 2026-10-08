#![allow(clippy::unwrap_used)]

use super::*;
use crate::observability::ops::capture::capture;
use crate::telemetry::ops::MetricKind;

fn point<'a>(points: &'a [MetricPoint], name: MetricName, kind: &str) -> &'a MetricPoint {
    points
        .iter()
        .find(|p| p.name == name && p.labels.get("kind").map(String::as_str) == Some(kind))
        .unwrap_or_else(|| panic!("no {} point for kind {kind}: {points:?}", name.as_str()))
}

fn secs(p: &MetricPoint) -> f64 {
    match p.value {
        MetricValue::Double(v) => v,
        MetricValue::Int(v) => panic!("expected a seconds (double) point, got int {v}"),
    }
}

#[test]
fn metric_names_kinds_and_units() {
    assert_eq!(MetricName::DaemonIpcLatencyMax.as_str(), "loom.daemon.ipc.latency_max");
    assert_eq!(MetricName::DaemonIpcLatency.as_str(), "loom.daemon.ipc.latency");
    assert_eq!(MetricName::DaemonIpcRequests.as_str(), "loom.daemon.ipc.requests");
    assert_eq!(MetricName::DaemonIpcLatencyMax.kind(), MetricKind::Gauge);
    assert_eq!(MetricName::DaemonIpcLatency.kind(), MetricKind::DeltaCounter);
    assert_eq!(MetricName::DaemonIpcRequests.kind(), MetricKind::DeltaCounter);
    assert_eq!(MetricName::DaemonIpcLatencyMax.unit(), "s");
    assert_eq!(MetricName::DaemonIpcLatency.unit(), "s");
    assert_eq!(MetricName::DaemonIpcRequests.unit(), "{request}");
}

#[test]
fn recorded_requests_drain_into_max_sum_and_count_per_kind() {
    let (points, _) = capture(|| {
        record("ListWorkspaces", Duration::from_millis(100));
        record("ListWorkspaces", Duration::from_millis(300));
        record("DaemonStatus", Duration::from_secs(12));
        drain_points()
    });
    assert_eq!(points.len(), 6, "three points per kind: {points:?}");

    let max = point(&points, MetricName::DaemonIpcLatencyMax, "ListWorkspaces");
    assert!((secs(max) - 0.3).abs() < 1e-9, "{max:?}");
    let sum = point(&points, MetricName::DaemonIpcLatency, "ListWorkspaces");
    assert!((secs(sum) - 0.4).abs() < 1e-9, "{sum:?}");
    let count = point(&points, MetricName::DaemonIpcRequests, "ListWorkspaces");
    assert_eq!(count.value, MetricValue::Int(2));

    let status_max = point(&points, MetricName::DaemonIpcLatencyMax, "DaemonStatus");
    assert!((secs(status_max) - 12.0).abs() < 1e-9);

    assert!(drain_points().is_empty(), "delta: a second drain is empty");
}

#[test]
fn the_request_timer_records_its_kind_when_dropped() {
    let (points, _) = capture(|| {
        let mut timer = RequestTimer::start();
        timer.set_kind(r#"{"type":"QuarantineList","payload":{"workspace_root":null}}"#);
        drop(timer);
        // A frame that never parsed keeps the `invalid` label.
        drop(RequestTimer::start());
        // A disarmed (event-subscription) timer records nothing.
        let mut subscription = RequestTimer::start();
        subscription.set_kind(r#"{"type":"SubscribeEvents","payload":{"topics":[]}}"#);
        subscription.disarm();
        drop(subscription);
        drain_points()
    });
    let count = point(&points, MetricName::DaemonIpcRequests, "QuarantineList");
    assert_eq!(count.value, MetricValue::Int(1));
    let invalid = point(&points, MetricName::DaemonIpcRequests, INVALID_KIND);
    assert_eq!(invalid.value, MetricValue::Int(1));
    assert!(
        !points
            .iter()
            .any(|p| p.labels.get("kind").map(String::as_str) == Some("SubscribeEvents")),
        "{points:?}"
    );
}

#[test]
fn nothing_accumulates_when_ops_signals_are_not_exported() {
    // No capture and (in a unit test) no registered sink.
    record("Ping", Duration::from_millis(5));
    let (points, _) = capture(drain_points);
    assert!(points.is_empty(), "{points:?}");
}

#[test]
fn every_label_survives_the_ops_label_policy() {
    let (points, _) = capture(|| {
        record("ListWorkspaces", Duration::from_millis(1));
        drain_points()
    });
    let record = crate::telemetry::MetricPointsRecord {
        captured_at: chrono::Utc::now(),
        interval_start: None,
        points: points.clone(),
    };
    assert_eq!(record.bounded_points(), points, "the `kind` label is allowlisted");
}

#[test]
fn slow_by_design_kinds_do_not_trigger_the_slow_request_warn() {
    let slow = SLOW_REQUEST_WARN + Duration::from_secs(25);
    for kind in [
        "DaemonStatus",
        "DaemonStatusSections",
        "CancelSweep",
        "DispatchSweep",
    ] {
        assert!(!warns_when_slow(kind, slow), "{kind} is slow by design");
    }
    assert!(warns_when_slow("ListWorkspaces", SLOW_REQUEST_WARN));
    assert!(warns_when_slow(INVALID_KIND, slow));
    assert!(!warns_when_slow("ListWorkspaces", SLOW_REQUEST_WARN - Duration::from_millis(1)));
}
