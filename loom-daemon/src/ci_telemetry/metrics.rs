//! Ops metrics and task liveness for the CI telemetry poller (Issue #12793).
//!
//! Before this, what a poll cycle concluded lived only in the daemon's text
//! log and `loom-daemon ci-telemetry status`, so "why did CI records stop?"
//! could not be answered from SigNoz (#12792). The daemon loop calls
//! [`record_cycle`] once per finished cycle. The CLI runs `run_cycle` too and
//! deliberately does not.

use std::io;
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::export::{load_cursor, ExportCursor};
use super::journal::MAX_LINE_BYTES;
use super::journal_path;
use super::ledger::{complete_len, scan_lines};
use super::poll::{CycleError, CycleReport};
use super::state::CycleSummary;
use crate::observability::ops::emit_metrics;
use crate::task_liveness::{self, CI_TELEMETRY};
use crate::telemetry::ops::{MetricName, MetricPoint};

/// `outcome` label of a cycle whose blocking task panicked.
pub const OUTCOME_PANICKED: &str = "panicked";

/// How long one cycle may run before the loop reads as stuck: a cycle pages
/// through every repo and downloads job logs, so it can legitimately outlast
/// an interval.
pub const CYCLE_OVERRUN: Duration = Duration::from_secs(30 * 60);

/// The liveness window of the poller loop: two intervals and the grace, plus
/// the longest a cycle may run.
#[must_use]
pub fn liveness_window(interval: Duration) -> Duration {
    task_liveness::default_stale_after(interval).saturating_add(CYCLE_OVERRUN)
}

/// Register the poller loop with the liveness registry.
pub fn register_liveness(interval: Duration) {
    task_liveness::register(CI_TELEMETRY, interval, liveness_window(interval));
}

/// The `outcome` label of a finished cycle.
#[must_use]
pub fn outcome_label(result: &Result<CycleReport, CycleError>) -> &'static str {
    match result {
        Ok(_) => "ok",
        Err(CycleError::Busy) => "busy",
        Err(CycleError::BackingOff { .. }) => "backing_off",
        Err(CycleError::RateLimited { .. }) => "rate_limited",
        Err(CycleError::CredentialRejected { .. }) => "credential_rejected",
        Err(CycleError::Discovery(_)) => "discovery_failed",
        Err(CycleError::Io(_)) => "io_failed",
    }
}

/// The summary a finished cycle carries, when it got far enough to have one.
fn summary_of(result: &Result<CycleReport, CycleError>) -> Option<&CycleSummary> {
    match result {
        Ok(report) => Some(&report.summary),
        Err(CycleError::CredentialRejected { progress, .. }) => Some(progress),
        Err(_) => None,
    }
}

/// Record one finished cycle (`None` = the blocking task panicked): the
/// per-cycle metrics plus the journal/cursor gauges. A no-op for the metrics
/// without the ops sink.
pub fn record_cycle(
    root: &Path,
    result: Option<&Result<CycleReport, CycleError>>,
    elapsed: Duration,
) {
    let outcome = result.map_or(OUTCOME_PANICKED, outcome_label);
    let mut points = cycle_points(outcome, result.and_then(summary_of), elapsed);
    points.extend(commit_points(root, Utc::now()));
    emit_metrics(points);
}

fn cycle_points(
    outcome: &str,
    summary: Option<&CycleSummary>,
    elapsed: Duration,
) -> Vec<MetricPoint> {
    let mut points = vec![
        MetricPoint::int(MetricName::CiTelemetryCycles, 1).label("outcome", outcome),
        MetricPoint::double(MetricName::CiTelemetryCycleDuration, elapsed.as_secs_f64())
            .label("outcome", outcome),
    ];
    if let Some(s) = summary {
        for (kind, value) in [
            ("repos_polled", s.repos_polled),
            ("runs_emitted", s.runs_emitted),
            ("jobs_emitted", s.jobs_emitted),
            ("recovered_units", s.recovered_units),
        ] {
            let value = i64::try_from(value).unwrap_or(i64::MAX);
            points.push(
                MetricPoint::int(MetricName::CiTelemetryCycleItems, value).label("kind", kind),
            );
        }
    }
    points
}

/// Journal size, export cursor offset and export lag. A missing journal
/// reads as all zeros (nothing to export); an unreadable one emits nothing
/// rather than a misleading zero.
fn commit_points(root: &Path, now: DateTime<Utc>) -> Vec<MetricPoint> {
    let path = journal_path(root);
    let len = match std::fs::metadata(&path) {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
        Err(_) => return Vec::new(),
    };
    let cursor = load_cursor(root);
    let Some(lag) = export_lag_secs(&path, &cursor, now) else {
        return Vec::new();
    };
    let int = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    vec![
        MetricPoint::int(MetricName::CiTelemetryJournalBytes, int(len)),
        MetricPoint::int(MetricName::CiTelemetryExportCursorOffset, int(cursor.byte_offset)),
        MetricPoint::int(MetricName::CiTelemetryExportLag, int(lag)),
    ]
}

/// Seconds since the oldest journal line past the cursor was emitted; `0`
/// when the exporter is caught up or the journal is absent. A cursor past the
/// journal's complete prefix means the journal was replaced, so the pass
/// restarts at `0`, as the exporter does. `None` on an I/O error.
fn export_lag_secs(path: &Path, cursor: &ExportCursor, now: DateTime<Utc>) -> Option<u64> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Some(0),
        Err(_) => return None,
    };
    let complete = complete_len(&mut file).ok()?;
    let start = if cursor.byte_offset > complete {
        0
    } else {
        cursor.byte_offset
    };
    if start >= complete {
        return Some(0);
    }
    let mut oldest: Option<DateTime<Utc>> = None;
    scan_lines(path, start, MAX_LINE_BYTES, |line| {
        if line.trim_ascii().is_empty() {
            return true;
        }
        match serde_json::from_slice::<EmittedAt>(line) {
            Ok(stamp) => {
                oldest = Some(stamp.emitted_at);
                false
            }
            // An unparseable line is skipped by the exporter too.
            Err(_) => true,
        }
    })
    .ok()?;
    Some(oldest.map_or(0, |at| u64::try_from((now - at).num_seconds()).unwrap_or(0)))
}

#[derive(serde::Deserialize)]
struct EmittedAt {
    emitted_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests;
