//! Ready-queue depth gauges (Issue #8852, phase 2) for OTLP/SigNoz.
//!
//! Each work-finder tick's ranked ready queue (the rows on
//! `last_work_finder_tick.queue`) is reduced to one `loom.queue.issues` gauge
//! per known disposition. Each gauge is labelled `state` (`running`, `ready`
//! or `blocked`) and `reason` (the disposition's wire name). A second gauge,
//! `loom.queue.listing_failed_repos`, counts the repos whose ready listing
//! failed, which means the counts are incomplete rather than low. A third,
//! `loom.queue.listing_incomplete_repos` (#11139), counts the repos whose
//! listing came back partial: their rows are counted, but not all of them.
//! A fourth, `loom.queue.already_landed` (#11858), splits the
//! `already_landed` rows by the evidence the landed guard found (the row's
//! `detail`: `forge_closed`, `forge_closed_not_planned` or `journal_landed`;
//! `unknown` for a row with none of these), so the split is measurable without
//! the native `queue.snapshot`.
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

/// The `already_landed` evidence tokens `loom.queue.already_landed` names
/// (the landed guard's row details, #11429), plus [`UNKNOWN_EVIDENCE`].
pub const LANDED_EVIDENCE: [&str; 3] =
    ["forge_closed", "forge_closed_not_planned", "journal_landed"];

/// The evidence label for an `already_landed` row whose detail names none.
pub const UNKNOWN_EVIDENCE: &str = "unknown";

/// The tick's queue gauges: one `loom.queue.issues{state,reason}` point per
/// entry of [`QueueDisposition::ALL`], plus `loom.queue.listing_failed_repos`,
/// `loom.queue.listing_incomplete_repos` and one
/// `loom.queue.already_landed{reason}` point per evidence (#11858).
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
    // #11139: a partial listing is not a whole one either.
    points.push(MetricPoint::int(
        MetricName::QueueListingIncompleteRepos,
        int(summary.listing_incomplete.len()),
    ));
    points.extend(already_landed_points(summary));
    points
}

/// One `loom.queue.already_landed{reason}` point per evidence, zeros
/// included (#11858).
fn already_landed_points(summary: &WorkFinderTickSummary) -> Vec<MetricPoint> {
    let evidence = |detail: Option<&str>| {
        detail
            .and_then(|d| LANDED_EVIDENCE.iter().find(|e| **e == d))
            .copied()
            .unwrap_or(UNKNOWN_EVIDENCE)
    };
    LANDED_EVIDENCE
        .iter()
        .chain(std::iter::once(&UNKNOWN_EVIDENCE))
        .map(|reason| {
            let count = summary
                .queue
                .iter()
                .filter(|r| r.disposition == QueueDisposition::AlreadyLanded)
                .filter(|r| evidence(r.detail.as_deref()) == *reason)
                .count();
            MetricPoint::int(MetricName::QueueAlreadyLanded, int(count)).label("reason", *reason)
        })
        .collect()
}

/// Export one tick's queue gauges. Returns immediately when no ops sink is
/// registered (observability off, or no OTLP exporter).
pub fn record_queue(summary: &WorkFinderTickSummary) {
    if let Some(sink) = super::global_ops_sink() {
        sink.emit_metrics(queue_points(summary));
    }
}
