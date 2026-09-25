//! Tests for the shared ops-signal path (Issue #8860).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use chrono::{Duration, TimeZone, Utc};

use super::dispatch::{decision_counts, tick_points, tick_result, tick_span};
use super::host::{parse_macos_swapusage, parse_meminfo, HostResources};
use super::OpsSink;
use crate::observability::queue::QueueSink;
use crate::telemetry::ops::{
    bounded_labels, MetricName, MetricPoint, MetricPointsRecord, MetricValue,
    OPS_METRIC_LABEL_KEYS, OPS_SPAN_ATTRIBUTE_KEYS,
};
use crate::telemetry::trace::{SpanName, SpanStatus, TraceContext};
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};
use crate::work_finder::TickReport;

#[derive(Default)]
struct RecordingQueue(Mutex<Vec<TelemetryEnvelope>>);

impl QueueSink for RecordingQueue {
    fn offer(&self, envelope: TelemetryEnvelope) {
        self.0.lock().unwrap().push(envelope);
    }
    fn offer_durable(&self, envelope: TelemetryEnvelope) -> std::io::Result<()> {
        self.offer(envelope);
        Ok(())
    }
}

fn sink() -> (Arc<RecordingQueue>, OpsSink) {
    let queue = Arc::new(RecordingQueue::default());
    (queue.clone(), OpsSink::new(queue, "host-a"))
}

fn at(secs: i64) -> chrono::DateTime<Utc> {
    Utc.timestamp_opt(1_790_000_000 + secs, 0).unwrap()
}

// ---------------------------------------------------------------- sink

#[test]
fn emit_metrics_enqueues_one_metric_points_envelope_for_this_host() {
    let (queue, sink) = sink();
    sink.emit_metrics(vec![MetricPoint::int(MetricName::DispatchCandidates, 3)]);
    let queued = queue.0.lock().unwrap();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].host_id, "host-a");
    assert_eq!(queued[0].schema_version, 10);
    let TelemetryRecord::MetricPoints(record) = &queued[0].record else {
        panic!("expected metric.points, got {:?}", queued[0].record);
    };
    assert_eq!(record.points.len(), 1);
}

#[test]
fn emit_metrics_with_no_points_enqueues_nothing() {
    let (queue, sink) = sink();
    sink.emit_metrics(Vec::new());
    assert!(queue.0.lock().unwrap().is_empty());
}

#[test]
fn emit_span_carries_its_context_and_skips_unsampled_spans() {
    let (queue, sink) = sink();
    let span = tick_span(&TickReport::default(), 2, at(0), at(1));
    sink.emit_span(span.clone());
    let mut unsampled = span.clone();
    unsampled.context = TraceContext::root(false);
    sink.emit_span(unsampled);
    let queued = queue.0.lock().unwrap();
    assert_eq!(queued.len(), 1, "unsampled span must not be enqueued");
    assert_eq!(queued[0].trace_context.as_ref(), Some(&span.context));
    assert!(matches!(queued[0].record, TelemetryRecord::Span(_)));
}

#[test]
fn no_otlp_queue_means_no_sink_is_registered() {
    // spawn_task registers the ops sink only from this decision, so an
    // HTTPS-only or disabled daemon has no sink and every emit is a no-op.
    assert!(super::sink_for_otlp_queues(Vec::new(), "host-a").is_none());
    let dir = tempfile::tempdir().unwrap();
    let queue =
        Arc::new(crate::observability::queue::DurableQueue::open(dir.path().join("q.jsonl"), 16));
    let sink = super::sink_for_otlp_queues(vec![queue.clone()], "host-a").unwrap();
    sink.emit_metrics(vec![MetricPoint::int(MetricName::DispatchCandidates, 1)]);
    assert_eq!(queue.len(), 1);
}

#[test]
fn emit_metrics_since_records_the_interval_start() {
    let (queue, sink) = sink();
    sink.emit_metrics_since(
        vec![MetricPoint::int(MetricName::DispatchCandidates, 1)],
        Some(at(-5)),
    );
    let queued = queue.0.lock().unwrap();
    let TelemetryRecord::MetricPoints(record) = &queued[0].record else {
        panic!("expected metric.points");
    };
    assert_eq!(record.interval_start, Some(at(-5)));
}

// ---------------------------------------------------------------- wire

#[test]
fn metric_points_round_trip_through_json_with_a_kind_tag() {
    let envelope = TelemetryEnvelope::new(
        "host-a",
        TelemetryRecord::MetricPoints(MetricPointsRecord {
            captured_at: at(0),
            interval_start: None,
            points: vec![
                MetricPoint::int(MetricName::DispatchDecisions, 4).label("reason", "backoff"),
                MetricPoint {
                    name: MetricName::HostMemoryAvailableBytes,
                    value: MetricValue::Double(1.5),
                    labels: BTreeMap::new(),
                },
            ],
        }),
    );
    let json = serde_json::to_value(&envelope).unwrap();
    assert_eq!(json["record"]["kind"], "metric.points");
    assert_eq!(json["record"]["points"][0]["name"], "loom.dispatch.decisions");
    assert_eq!(json["record"]["points"][0]["value"], 4);
    let back: TelemetryEnvelope = serde_json::from_value(json).unwrap();
    assert_eq!(back, envelope);
}

#[test]
fn bounded_labels_keep_only_short_allowlisted_values() {
    let labels: BTreeMap<String, String> = [
        ("reason", "backoff".to_string()),
        ("issue", "8860".to_string()),
        ("provider", "x".repeat(129)),
        ("state", "bad\nvalue".to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    let kept = bounded_labels(&labels);
    assert_eq!(kept.keys().collect::<Vec<_>>(), vec!["reason"]);
}

#[test]
fn bounded_points_drop_non_finite_doubles() {
    let record = MetricPointsRecord {
        captured_at: at(0),
        interval_start: None,
        points: vec![
            MetricPoint {
                name: MetricName::HostSwapUsedBytes,
                value: MetricValue::Double(f64::NAN),
                labels: BTreeMap::new(),
            },
            MetricPoint::int(MetricName::HostSwapTotalBytes, 7),
        ],
    };
    let bounded = record.bounded_points();
    assert_eq!(bounded.len(), 1);
    assert_eq!(bounded[0].name, MetricName::HostSwapTotalBytes);
}

#[test]
fn every_metric_name_serializes_to_its_as_str() {
    for name in [
        MetricName::DispatchDecisions,
        MetricName::DispatchCandidates,
        MetricName::DispatchMaxConcurrent,
        MetricName::HostMemoryAvailableBytes,
        MetricName::HostMemoryTotalBytes,
        MetricName::HostSwapUsedBytes,
        MetricName::HostSwapTotalBytes,
        MetricName::HostWorktreeVolumeFreeBytes,
        MetricName::HostWorktreeVolumeTotalBytes,
        MetricName::QueueIssues,
        MetricName::QueueListingFailedRepos,
    ] {
        assert_eq!(serde_json::to_value(name).unwrap(), name.as_str());
    }
}

#[test]
fn native_https_path_drops_metric_points() {
    let envelopes = vec![
        TelemetryEnvelope::new(
            "host-a",
            TelemetryRecord::MetricPoints(MetricPointsRecord {
                captured_at: at(0),
                interval_start: None,
                points: vec![MetricPoint::int(MetricName::DispatchCandidates, 1)],
            }),
        ),
        TelemetryEnvelope::new(
            "host-a",
            TelemetryRecord::Span(tick_span(&TickReport::default(), 1, at(0), at(1))),
        ),
    ];
    assert!(crate::observability::tracing::native_envelopes(&envelopes).is_empty());
}

// ---------------------------------------------------------------- dispatch

#[test]
fn an_empty_backlog_is_no_eligible_work() {
    assert_eq!(tick_result(&TickReport::default()), "no_eligible_work");
}

#[test]
fn tick_result_precedence() {
    let with = |f: fn(&mut TickReport)| {
        let mut report = TickReport {
            seen: 3,
            ..TickReport::default()
        };
        f(&mut report);
        tick_result(&report)
    };
    assert_eq!(
        with(|r| {
            r.dispatched = 1;
            r.halted = true;
        }),
        "dispatched"
    );
    assert_eq!(with(|r| r.halted = true), "halted_main_red");
    assert_eq!(with(|r| r.saturation_held = true), "saturation_held");
    assert_eq!(with(|r| r.errors = 1), "error");
    assert_eq!(with(|r| r.deferred_ramp_cap = 2), "capacity_full");
    assert_eq!(with(|r| r.skipped_backoff = 3), "all_skipped");
}

#[test]
fn tick_points_emit_only_nonzero_reasons_plus_gauges() {
    let report = TickReport {
        seen: 5,
        dispatched: 2,
        skipped_peer_claim: 1,
        deferred_capacity: 2,
        ..TickReport::default()
    };
    let points = tick_points(&report, 4);
    let reasons: BTreeMap<String, MetricValue> = points
        .iter()
        .filter(|p| p.name == MetricName::DispatchDecisions)
        .map(|p| (p.labels["reason"].clone(), p.value))
        .collect();
    assert_eq!(
        reasons,
        [
            ("capacity".to_string(), MetricValue::Int(2)),
            ("dispatched".to_string(), MetricValue::Int(2)),
            ("peer_claim".to_string(), MetricValue::Int(1)),
        ]
        .into_iter()
        .collect()
    );
    assert!(points.contains(&MetricPoint::int(MetricName::DispatchCandidates, 5)));
    assert!(points.contains(&MetricPoint::int(MetricName::DispatchMaxConcurrent, 4)));
}

#[test]
fn every_reason_label_is_distinct() {
    let reasons: std::collections::BTreeSet<_> = decision_counts(&TickReport::default())
        .iter()
        .map(|(r, _)| *r)
        .collect();
    assert_eq!(reasons.len(), decision_counts(&TickReport::default()).len());
}

#[test]
fn tick_span_is_a_bounded_root_with_the_result_attribute() {
    let report = TickReport {
        errors: 1,
        seen: 2,
        ..TickReport::default()
    };
    let span = tick_span(&report, 3, at(10), at(5));
    assert_eq!(span.name, SpanName::DispatchTick);
    assert!(span.parent_span_id.is_none());
    assert!(span.context.sampled());
    assert_eq!(span.ended_at, span.started_at, "clock skew never ends before start");
    assert!(span.validate().is_ok());
    assert_eq!(span.status, SpanStatus::Error);
    assert_eq!(span.attributes["loom.dispatch.result"], "error");
    assert_eq!(span.attributes["loom.dispatch.max_concurrent"], "3");
    // Every attribute survives the export-time allowlist.
    assert_eq!(span.clone().bounded().attributes, span.attributes);
    let later = tick_span(&report, 3, at(0), at(0) + Duration::seconds(2));
    assert_ne!(later.context.trace_id, span.context.trace_id, "one trace per tick");
}

// ---------------------------------------------------------------- host

#[test]
fn meminfo_yields_available_total_and_swap_used_in_bytes() {
    let meminfo = "MemTotal:       16384000 kB\n\
                   MemFree:         1000000 kB\n\
                   MemAvailable:    8192000 kB\n\
                   SwapTotal:       2048000 kB\n\
                   SwapFree:        1536000 kB\n";
    let parsed = parse_meminfo(meminfo);
    assert_eq!(parsed.memory_total, Some(16_384_000 * 1024));
    assert_eq!(parsed.memory_available, Some(8_192_000 * 1024));
    assert_eq!(parsed.swap_total, Some(2_048_000 * 1024));
    assert_eq!(parsed.swap_used, Some(512_000 * 1024));
}

#[test]
fn meminfo_missing_fields_stay_unknown() {
    let parsed = parse_meminfo("MemTotal: 100 kB\n");
    assert_eq!(parsed.memory_total, Some(102_400));
    assert_eq!(parsed.memory_available, None);
    assert_eq!(parsed.swap_used, None);
}

#[test]
fn macos_swapusage_parses_total_and_used() {
    let out = "total = 2048.00M  used = 1024.50M  free = 1023.50M  (encrypted)";
    assert_eq!(parse_macos_swapusage(out), Some((2048 * 1024 * 1024, 1_074_266_112)));
    assert_eq!(
        parse_macos_swapusage("total = 1.00G  used = 0.00M  free = 1.00G"),
        Some((1024 * 1024 * 1024, 0))
    );
    assert_eq!(parse_macos_swapusage("garbage"), None);
}

#[test]
fn host_points_skip_unmeasured_fields() {
    let resources = HostResources {
        memory_available: Some(10),
        worktree_volume_total: Some(20),
        ..HostResources::default()
    };
    assert_eq!(
        resources.points(),
        vec![
            MetricPoint::int(MetricName::HostMemoryAvailableBytes, 10),
            MetricPoint::int(MetricName::HostWorktreeVolumeTotalBytes, 20),
        ]
    );
    assert!(HostResources::default().points().is_empty());
}

// ---------------------------------------------------------------- gateway contract

const COLLECTOR_CONFIG: &str =
    include_str!("../../../../defaults/observability/collector/config.yaml");

fn keep_keys(context: &str) -> std::collections::BTreeSet<String> {
    let mut current = String::new();
    let mut keys = std::collections::BTreeSet::new();
    for line in COLLECTOR_CONFIG.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("- context:") {
            current = rest.trim().to_string();
        }
        if current == context && trimmed.contains("keep_keys(") {
            keys.extend(trimmed.split('"').skip(1).step_by(2).map(str::to_string));
        }
    }
    keys
}

#[test]
fn gateway_collector_keeps_every_ops_label_and_span_attribute() {
    let datapoint = keep_keys("datapoint");
    for key in OPS_METRIC_LABEL_KEYS {
        assert!(datapoint.contains(*key), "collector datapoint keep_keys lacks {key}");
    }
    let span = keep_keys("span");
    for key in OPS_SPAN_ATTRIBUTE_KEYS {
        assert!(span.contains(*key), "collector span keep_keys lacks {key}");
    }
}

// Ready-queue depth gauges (Issue #8852, phase 2).

fn queue_summary(
    dispositions: &[crate::types::QueueDisposition],
) -> crate::types::WorkFinderTickSummary {
    crate::types::WorkFinderTickSummary {
        queue: dispositions
            .iter()
            .enumerate()
            .map(|(i, d)| crate::types::ReadyQueueRow {
                rank: i + 1,
                repo: "/r".into(),
                issue: u32::try_from(i).unwrap(),
                workspace_priority: 100,
                urgent: false,
                created_at: None,
                tier: None,
                disposition: *d,
                detail: None,
                state: d.state().into(),
                reason: d.reason().into(),
            })
            .collect(),
        ..Default::default()
    }
}

#[test]
fn queue_points_emit_every_disposition_with_zeros_and_no_issue_label() {
    use crate::types::QueueDisposition as Qd;
    let mut summary = queue_summary(&[
        Qd::Dispatched,
        Qd::DeferredCapacity,
        Qd::DeferredCapacity,
        Qd::OpenPr,
    ]);
    summary.listing_failed = vec!["/r2".into()];
    let points = super::queue::queue_points(&summary);
    assert_eq!(points.len(), Qd::ALL.len() + 1);
    let value = |reason: &str| {
        points
            .iter()
            .find(|p| {
                p.name == MetricName::QueueIssues
                    && p.labels.get("reason").map(String::as_str) == Some(reason)
            })
            .map(|p| p.value)
            .unwrap()
    };
    assert_eq!(value("deferred_capacity"), MetricValue::Int(2));
    assert_eq!(value("dispatched"), MetricValue::Int(1));
    assert_eq!(value("peer_claim"), MetricValue::Int(0));
    let open_pr = points
        .iter()
        .find(|p| p.labels.get("reason").map(String::as_str) == Some("open_pr"))
        .unwrap();
    assert_eq!(open_pr.labels.get("state").map(String::as_str), Some("blocked"));
    let failed = points
        .iter()
        .find(|p| p.name == MetricName::QueueListingFailedRepos)
        .unwrap();
    assert_eq!(failed.value, MetricValue::Int(1));
    for point in &points {
        // Every label survives the export policy unchanged: only allowlisted,
        // bounded keys (state, reason), never an issue number or repo.
        assert_eq!(bounded_labels(&point.labels), point.labels);
        assert!(point.labels.keys().all(|k| k == "state" || k == "reason"));
    }
}

#[test]
fn an_empty_queue_reads_as_zeros_not_as_missing() {
    let points = super::queue::queue_points(&queue_summary(&[]));
    assert!(!points.is_empty());
    assert!(points.iter().all(|p| p.value == MetricValue::Int(0)));
}
