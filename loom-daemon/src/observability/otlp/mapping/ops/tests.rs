//! `metric.points` → OTLP mapping tests (Issue #8860).

use chrono::{TimeZone, Utc};
use opentelemetry_proto::tonic::metrics::v1::{metric, number_data_point, AggregationTemporality};

use super::super::{build_logs_request, build_metrics_request};
use crate::telemetry::ops::{MetricName, MetricPoint, MetricPointsRecord};
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
fn queue_gauges_map_to_otlp_gauges_and_queue_snapshot_maps_to_nothing() {
    // Issue #8852 phase 2: SigNoz gets the queue as gauges ...
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
    // ... and the per-issue `queue.snapshot` record is native-HTTPS only.
    let snapshot = vec![TelemetryEnvelope::new(
        "host-a",
        TelemetryRecord::QueueSnapshot(crate::telemetry::QueueSnapshotRecord {
            tick_at: Utc.timestamp_opt(1_790_000_000, 0).unwrap(),
            max_concurrent: 1,
            seen: 0,
            counts: crate::telemetry::queue_snapshot::QueueStateCounts::default(),
            listing_failed: Vec::new(),
            listing_failed_unresolved: 0,
            rows: Vec::new(),
            unresolved_rows: 0,
            rows_truncated: 0,
        }),
    )];
    assert!(build_logs_request(&snapshot).is_none());
    assert!(build_metrics_request(&snapshot).is_none());
}
