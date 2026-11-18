//! `metric.points` → OTLP mapping tests (Issue #8860).

use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use opentelemetry_proto::tonic::common::v1::{any_value, KeyValue};
use opentelemetry_proto::tonic::metrics::v1::NumberDataPoint;
use opentelemetry_proto::tonic::metrics::v1::{metric, number_data_point, AggregationTemporality};
use uuid::Uuid;

use super::super::build_metrics_request;
use crate::fleet_sync::applied::{force_for_tests, reset_for_tests, AppliedSha};
use crate::observability::ops::{
    busy_slots::{step_points, StepStarts},
    OpsSink,
};
use crate::observability::queue::DurableQueue;
use crate::telemetry::ops::{
    MetricName, MetricPoint, MetricPointsRecord, STEP_OCCUPANCY_PROCESS_ID,
};
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};

fn points_envelope(host_id: &str, points: Vec<MetricPoint>) -> TelemetryEnvelope {
    TelemetryEnvelope::new(
        host_id,
        TelemetryRecord::MetricPoints(MetricPointsRecord {
            captured_at: Utc.timestamp_opt(1_790_000_000, 0).unwrap(),
            interval_start: Some(Utc.timestamp_opt(1_789_999_990, 0).unwrap()),
            points,
        }),
    )
}

#[test]
fn gauges_and_delta_counters_map_to_their_fixed_otlp_kinds() {
    let batch = vec![points_envelope(
        "host-a",
        vec![
            MetricPoint::int(MetricName::DispatchDecisions, 2).label("reason", "backoff"),
            MetricPoint::int(MetricName::DispatchDecisions, 1).label("reason", "dispatched"),
            MetricPoint::int(MetricName::HostMemoryAvailableBytes, 4096),
        ],
    )];
    let request = build_metrics_request(&batch).expect("metric.points yields a request");
    assert_eq!(request.resource_metrics.len(), 1);
    let metrics = &request.resource_metrics[0].scope_metrics[0].metrics;
    let decisions = metrics
        .iter()
        .find(|m| m.name == "loom.dispatch.decisions")
        .unwrap();
    let Some(metric::Data::Sum(sum)) = &decisions.data else {
        panic!("decisions must be a Sum: {:?}", decisions.data);
    };
    assert!(sum.is_monotonic);
    assert_eq!(sum.aggregation_temporality, AggregationTemporality::Delta as i32);
    assert_eq!(sum.data_points.len(), 2);
    assert_eq!(sum.data_points[0].start_time_unix_nano, 1_789_999_990 * 1_000_000_000);
    assert_eq!(sum.data_points[0].attributes[0].key, "reason");
    let memory = metrics
        .iter()
        .find(|m| m.name == "loom.host.memory.available_bytes")
        .unwrap();
    assert_eq!(memory.unit, "By");
    let Some(metric::Data::Gauge(gauge)) = &memory.data else {
        panic!("memory must be a Gauge");
    };
    assert_eq!(gauge.data_points[0].value, Some(number_data_point::Value::AsInt(4096)));
    assert_eq!(gauge.data_points[0].start_time_unix_nano, 0, "gauges carry no interval");
}

#[test]
fn non_allowlisted_labels_are_dropped_at_export() {
    // Inserted directly, as a record restored from an older on-disk queue
    // could carry it: `MetricPoint::label` debug-asserts the allowlist (#8857).
    let mut point = MetricPoint::int(MetricName::DispatchDecisions, 1).label("reason", "error");
    point.labels.insert("issue".into(), "8860".into());
    let batch = vec![points_envelope("host-a", vec![point])];
    let request = build_metrics_request(&batch).unwrap();
    let point = match &request.resource_metrics[0].scope_metrics[0].metrics[0].data {
        Some(metric::Data::Sum(sum)) => sum.data_points[0].clone(),
        other => panic!("unexpected {other:?}"),
    };
    let keys: Vec<&str> = point.attributes.iter().map(|kv| kv.key.as_str()).collect();
    assert_eq!(keys, vec!["reason"]);
}

#[test]
fn ops_points_group_per_host_and_empty_batches_produce_no_request() {
    let batch = vec![
        points_envelope("host-a", vec![MetricPoint::int(MetricName::DispatchCandidates, 1)]),
        points_envelope("host-b", vec![MetricPoint::int(MetricName::DispatchCandidates, 2)]),
    ];
    let request = build_metrics_request(&batch).unwrap();
    assert_eq!(request.resource_metrics.len(), 2);
    assert!(build_metrics_request(&[points_envelope("host-a", Vec::new())]).is_none());
}

#[test]
fn queue_gauges_map_to_otlp_gauges() {
    // Issue #8852 phase 2: SigNoz gets the queue as gauges.
    let batch = vec![points_envelope(
        "host-a",
        vec![MetricPoint::int(MetricName::QueueIssues, 3)
            .label("state", "ready")
            .label("reason", "deferred_capacity")],
    )];
    let request = build_metrics_request(&batch).unwrap();
    let metric = &request.resource_metrics[0].scope_metrics[0].metrics[0];
    assert_eq!(metric.name, "loom.queue.issues");
    assert!(matches!(metric.data, Some(metric::Data::Gauge(_))));
}

fn string_attr<'a>(attrs: &'a [KeyValue], key: &str) -> Option<&'a str> {
    let attr = attrs.iter().find(|attr| attr.key == key)?;
    match attr.value.as_ref()?.value.as_ref()? {
        any_value::Value::StringValue(value) => Some(value),
        _ => None,
    }
}

fn busy_slots(metrics: &[opentelemetry_proto::tonic::metrics::v1::Metric]) -> &[NumberDataPoint] {
    let metric = metrics
        .iter()
        .find(|metric| metric.name == "loom.dispatch.busy_slots")
        .unwrap();
    let Some(metric::Data::Gauge(gauge)) = &metric.data else {
        panic!("busy slots must be a gauge");
    };
    &gauge.data_points
}

fn fixture_health(version: &str, captured_at: DateTime<Utc>) -> TelemetryEnvelope {
    let record = serde_json::from_value(serde_json::json!({
        "captured_at": captured_at,
        "daemon_version": version,
        "uptime_sec": 0,
        "logical_cpus": 2,
    }))
    .unwrap();
    TelemetryEnvelope::new("host-a", TelemetryRecord::HostHealth(record))
}

#[test]
fn queued_step_occupancy_preserves_the_capture_process_after_restart_and_retry() {
    const PROCESS_A: &str = "aaaaaaaa-0000-4000-8000-000000000001";
    const PROCESS_B: &str = "bbbbbbbb-0000-4000-8000-000000000002";
    let at_a = Utc.timestamp_opt(1_790_000_000, 123_456_789).unwrap();
    let at_b = Utc.timestamp_opt(1_790_000_010, 987_654_321).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ops.jsonl");
    let source_point = |process, starts: StepStarts| {
        step_points(&starts, Uuid::parse_str(process).unwrap())
            .into_iter()
            .find(|point| point.labels["role"] == "judge")
            .unwrap()
    };
    let legacy = points_envelope(
        "host-a",
        vec![MetricPoint::int(MetricName::DispatchBusySlots, 3)
            .label("role", "judge")
            .label("pool", "step")],
    );

    let original = {
        let queue = Arc::new(DurableQueue::open(path.clone(), 16));
        let sink = OpsSink::new(queue.clone(), "host-a");
        sink.emit_metrics_over(vec![source_point(PROCESS_A, StepStarts::new())], None, at_a);
        queue.push_durable(legacy.clone()).unwrap();
        let serialized = std::fs::read_to_string(&path).unwrap();
        assert!(serialized.contains(PROCESS_A));
        let batch = queue.peek_batch(16);
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[1], legacy);

        force_for_tests(AppliedSha::Sha("a".repeat(40)));
        let request =
            build_metrics_request(&[batch[0].clone(), fixture_health("0.1.0", at_a)]).unwrap();
        let host = &request.resource_metrics[0];
        let attrs = &host.resource.as_ref().unwrap().attributes;
        assert_eq!(string_attr(attrs, "service.version"), Some("0.1.0"));
        assert_eq!(
            string_attr(attrs, "loom.fleet.config_sha"),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        let point = &busy_slots(&host.scope_metrics[0].metrics)[0];
        assert_eq!(string_attr(&point.attributes, STEP_OCCUPANCY_PROCESS_ID), Some(PROCESS_A));
        point.clone()
    }; // Close A's queue without acknowledging its observations.

    let queue = Arc::new(DurableQueue::open(path, 16));
    assert_eq!(queue.peek_batch(16)[1], legacy, "legacy JSONL reopens unchanged");
    let sink = OpsSink::new(queue.clone(), "host-a");
    sink.emit_metrics_over(
        vec![source_point(
            PROCESS_B,
            StepStarts::from([("judge", vec![at_b; 2])]),
        )],
        None,
        at_b,
    );
    let mut batch = queue.peek_batch(16);
    assert_eq!(batch.len(), 3);
    batch.push(fixture_health("0.2.0", at_b));
    force_for_tests(AppliedSha::Sha("b".repeat(40)));
    let mut replay = build_metrics_request(&batch).unwrap();
    let retry_batch = queue.peek_batch(16);
    assert_eq!(
        &batch[..3],
        retry_batch.as_slice(),
        "unacknowledged points are retried unchanged"
    );
    let retry =
        build_metrics_request(&[retry_batch[0].clone(), fixture_health("0.2.0", at_b)]).unwrap();
    reset_for_tests();

    let host = &mut replay.resource_metrics[0];
    let resource = host.resource.as_mut().unwrap();
    assert_eq!(string_attr(&resource.attributes, "service.instance.id"), Some("host-a"));
    assert_eq!(string_attr(&resource.attributes, "service.version"), Some("0.2.0"));
    assert_eq!(
        string_attr(&resource.attributes, "loom.fleet.config_sha"),
        Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
    );
    assert!(string_attr(&resource.attributes, STEP_OCCUPANCY_PROCESS_ID).is_none());
    let points = busy_slots(&host.scope_metrics[0].metrics);
    assert_eq!(points.len(), 3);
    assert_eq!(points[0], original, "A's entire data point survives B's resource metadata");
    assert_eq!(busy_slots(&retry.resource_metrics[0].scope_metrics[0].metrics)[0], original);
    for (point, time, value, process) in [
        (&points[0], 1_790_000_000_123_456_789, 0, Some(PROCESS_A)),
        (&points[1], 1_790_000_000_000_000_000, 3, None),
        (&points[2], 1_790_000_010_987_654_321, 2, Some(PROCESS_B)),
    ] {
        assert_eq!(point.time_unix_nano, time);
        assert_eq!(point.start_time_unix_nano, 0);
        assert_eq!(point.value, Some(number_data_point::Value::AsInt(value)));
        assert_eq!(string_attr(&point.attributes, "role"), Some("judge"));
        assert_eq!(string_attr(&point.attributes, "pool"), Some("step"));
        assert_eq!(string_attr(&point.attributes, STEP_OCCUPANCY_PROCESS_ID), process);
    }

    // Retain a reproducible OTLP JSON fixture when the test runs with --nocapture.
    // Remove unrelated health metrics and host-specific export resource metadata.
    resource.attributes.retain(|attr| {
        matches!(
            attr.key.as_str(),
            "host.id"
                | "service.name"
                | "service.instance.id"
                | "service.version"
                | "loom.fleet.config_sha"
        )
    });
    host.scope_metrics[0]
        .metrics
        .retain(|metric| metric.name == "loom.dispatch.busy_slots");
    println!("step-occupancy-replay-fixture: {}", serde_json::to_string(&replay).unwrap());
}

#[test]
fn restored_step_occupancy_points_reapply_process_identity_policy() {
    const CANONICAL: &str = "aaaaaaaa-0000-4000-8000-000000000001";
    let step = MetricPoint::int(MetricName::DispatchBusySlots, 5)
        .label("role", "judge")
        .label("pool", "step");
    let mut full = step.clone();
    for key in [
        "account",
        "age",
        "agent",
        "artifact",
        "basis",
        "caller",
        "container",
        "cred_owner",
        "hold",
        "host",
    ] {
        full = full.label(key, "fixture");
    }
    let mut candidates = [
        step.clone(),
        step,
        MetricPoint::int(MetricName::DispatchBusySlots, 7).label("role", "judge"),
        full,
    ];
    for (point, token) in candidates.iter_mut().zip([
        CANONICAL,
        "AAAAAAAA-0000-4000-8000-000000000001",
        CANONICAL,
        CANONICAL,
    ]) {
        point
            .labels
            .insert(STEP_OCCUPANCY_PROCESS_ID.to_owned(), token.to_owned());
    }
    let request =
        build_metrics_request(&[points_envelope("host-a", Vec::from(candidates))]).unwrap();
    let points = busy_slots(&request.resource_metrics[0].scope_metrics[0].metrics);
    assert_eq!(points.len(), 4);
    assert_eq!(string_attr(&points[0].attributes, STEP_OCCUPANCY_PROCESS_ID), Some(CANONICAL));
    for point in &points[1..] {
        assert!(string_attr(&point.attributes, STEP_OCCUPANCY_PROCESS_ID).is_none());
    }
    assert_eq!(points[3].attributes.len(), 12);
    assert_eq!(string_attr(&points[3].attributes, "pool"), Some("step"));
    assert_eq!(string_attr(&points[3].attributes, "role"), Some("judge"));
}
