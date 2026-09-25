//! Work-finder tick telemetry (Issue #8860): one `loom.dispatch.tick` span per
//! tick, plus the tick's candidate outcomes as `loom.dispatch.decisions` delta
//! counters labelled with an explicit `reason`.
//!
//! Everything here is derived from the [`TickReport`] the tick already
//! produces for `loom-daemon health` (#4761), so the exported reasons cannot
//! disagree with the per-tick summary log line. The mapping is pure;
//! [`record_tick`] is the only side effect, and it returns before building
//! anything when no ops sink is registered.

use chrono::{DateTime, Utc};

use crate::telemetry::ops::{MetricName, MetricPoint};
use crate::telemetry::trace::{SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext};
use crate::work_finder::TickReport;

/// Every `reason` label value, paired with its [`TickReport`] counter. Zero
/// counters are not emitted. `error` counts failed dispatches and failed
/// ready-issue listings.
#[must_use]
pub fn decision_counts(report: &TickReport) -> [(&'static str, usize); 19] {
    [
        ("dispatched", report.dispatched),
        ("labeled", report.skipped_labeled),
        ("in_flight", report.skipped_in_flight),
        ("quarantined", report.skipped_quarantined),
        ("workspace_commands_missing", report.skipped_workspace_commands_missing),
        ("pr_open", report.skipped_pr_open),
        ("peer_claim", report.skipped_peer_claim),
        ("backoff", report.skipped_backoff),
        ("pr_open_backoff", report.skipped_pr_open_backoff),
        ("noop_cooldown", report.skipped_noop_cooldown),
        ("declined", report.skipped_declined),
        ("prless_retry", report.skipped_prless_retry),
        ("recheck_interval", report.skipped_recheck_interval),
        ("host_constraint", report.skipped_host_constraint),
        ("capacity", report.deferred_capacity),
        ("ramp_cap", report.deferred_ramp_cap),
        ("saturation", report.deferred_saturation),
        ("out_of_slice", report.deferred_out_of_slice),
        ("error", report.errors),
    ]
}

/// The tick's single outcome, first match wins:
///
/// | value | when |
/// |---|---|
/// | `dispatched` | at least one sweep started |
/// | `halted_main_red` | a main-health gate held a workspace |
/// | `saturation_held` | the saturation admission brake was engaged |
/// | `error` | a dispatch or listing failed and nothing started |
/// | `no_eligible_work` | no ready candidates at all |
/// | `capacity_full` | candidates deferred by the concurrency or ramp cap |
/// | `all_skipped` | every candidate was skipped for a per-issue reason |
#[must_use]
pub fn tick_result(report: &TickReport) -> &'static str {
    if report.dispatched > 0 {
        "dispatched"
    } else if report.halted {
        "halted_main_red"
    } else if report.saturation_held {
        "saturation_held"
    } else if report.errors > 0 {
        "error"
    } else if report.seen == 0 {
        "no_eligible_work"
    } else if report.deferred_capacity + report.deferred_ramp_cap > 0 {
        "capacity_full"
    } else {
        "all_skipped"
    }
}

fn int(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// The tick's metric points: non-zero decision counters plus the candidate
/// and cap gauges.
#[must_use]
pub fn tick_points(report: &TickReport, max_concurrent: usize) -> Vec<MetricPoint> {
    let mut points: Vec<MetricPoint> = decision_counts(report)
        .into_iter()
        .filter(|(_, count)| *count > 0)
        .map(|(reason, count)| {
            MetricPoint::int(MetricName::DispatchDecisions, int(count)).label("reason", reason)
        })
        .collect();
    points.push(MetricPoint::int(MetricName::DispatchCandidates, int(report.seen)));
    points.push(MetricPoint::int(MetricName::DispatchMaxConcurrent, int(max_concurrent)));
    points
}

/// The tick's span: a fresh sampled root trace covering `started_at..ended_at`.
#[must_use]
pub fn tick_span(
    report: &TickReport,
    max_concurrent: usize,
    started_at: DateTime<Utc>,
    ended_at: DateTime<Utc>,
) -> SpanRecord {
    let result = tick_result(report);
    let attributes: TraceAttributes = [
        ("loom.dispatch.result", result.to_string()),
        ("loom.dispatch.seen", report.seen.to_string()),
        ("loom.dispatch.dispatched", report.dispatched.to_string()),
        ("loom.dispatch.errors", report.errors.to_string()),
        ("loom.dispatch.max_concurrent", max_concurrent.to_string()),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value))
    .collect();
    SpanRecord {
        context: TraceContext::root(true),
        parent_span_id: None,
        name: SpanName::DispatchTick,
        started_at,
        ended_at: ended_at.max(started_at),
        status: if result == "error" {
            SpanStatus::Error
        } else {
            SpanStatus::Ok
        },
        attributes,
        events: Vec::new(),
        links: Vec::new(),
    }
}

/// Export one completed tick. Returns immediately when no ops sink is
/// registered (observability off, or no OTLP exporter).
pub fn record_tick(report: &TickReport, max_concurrent: usize, started_at: DateTime<Utc>) {
    let Some(sink) = super::global_ops_sink() else {
        return;
    };
    sink.emit_span(tick_span(report, max_concurrent, started_at, Utc::now()));
    sink.emit_metrics(tick_points(report, max_concurrent));
}
