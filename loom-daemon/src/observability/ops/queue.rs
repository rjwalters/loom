//! Ready-queue depth gauges (Issue #8852, phase 2) for OTLP/SigNoz.
//!
//! Each work-finder tick's ranked ready queue (the rows on
//! `last_work_finder_tick.queue`) is reduced to one `loom.queue.issues` gauge
//! per known disposition. Each gauge is labelled `state` (`running`, `ready`
//! or `blocked`) and `reason` (the disposition's wire name). A second gauge,
//! `loom.queue.listing_failed_repos`, counts the repos whose ready listing
//! failed, which means the counts are incomplete rather than low.
//!
//! Every disposition is emitted every tick, **zeros included**, so an empty
//! queue reads as `0` and not as a missing series. A missing series then
//! means only one thing: this host stopped ticking or stopped exporting. The
//! labels are two small closed vocabularies. An issue number or a repo never
//! becomes a label; the per-issue rows go to the native backend as
//! `queue.snapshot` (`crate::telemetry::queue_snapshot`).

use crate::telemetry::ops::{MetricName, MetricPoint};
use crate::types::{QueueDisposition, WorkFinderTickSummary};

fn int(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// The tick's queue gauges: one `loom.queue.issues{state,reason}` point per
/// entry of [`QueueDisposition::ALL`], plus `loom.queue.listing_failed_repos`.
#[must_use]
pub fn queue_points(summary: &WorkFinderTickSummary) -> Vec<MetricPoint> {
    let mut points: Vec<MetricPoint> = QueueDisposition::ALL
        .iter()
        .map(|d| {
            let count = summary.queue.iter().filter(|r| r.disposition == *d).count();
            MetricPoint::int(MetricName::QueueIssues, int(count))
                .label("state", d.state())
                .label("reason", d.as_str())
        })
        .collect();
    points.push(MetricPoint::int(
        MetricName::QueueListingFailedRepos,
        int(summary.listing_failed.len()),
    ));
    points
}

/// Export one tick's queue gauges. Returns immediately when no ops sink is
/// registered (observability off, or no OTLP exporter).
pub fn record_queue(summary: &WorkFinderTickSummary) {
    if let Some(sink) = super::global_ops_sink() {
        sink.emit_metrics(queue_points(summary));
    }
}
