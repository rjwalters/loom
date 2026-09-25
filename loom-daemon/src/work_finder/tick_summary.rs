//! Last-tick publication (Issue #4761) and per-tick OTLP export (Issue
//! #8860), split out of `work_finder.rs` (which is frozen by the file-size
//! ratchet) so both tick loops share one publication seam.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use super::{ready_queue, TickReport};
use crate::types::WorkFinderTickSummary;

/// Process-global slot holding the most recent completed tick's summary.
///
/// Mirrors the "loop publishes, status reads" discipline
/// [`crate::auto_update::global_status_snapshot`] and
/// [`crate::host_breaker::global_snapshot`] already use: the work-finder loop
/// writes here at the end of every tick, and `build_daemon_status` reads it
/// back so a cross-process consumer (`loom-daemon health`) can see the last
/// tick's dispatch/skip breakdown without scraping the daemon log.
///
/// `None` (the initial value) honestly means "no tick has completed in this
/// process yet" — never "nothing was dispatched".
static LAST_TICK: OnceLock<Mutex<Option<WorkFinderTickSummary>>> = OnceLock::new();

fn last_tick_slot() -> &'static Mutex<Option<WorkFinderTickSummary>> {
    LAST_TICK.get_or_init(|| Mutex::new(None))
}

/// Publish `report` (as run under `max_concurrent`, completed at `at`) as the
/// most recent work-finder tick (Issue #4761). Called by both the
/// single-workspace and multi-workspace loops so the two can never diverge on
/// what "the last tick" means.
pub fn publish_tick_summary_at(
    report: &TickReport,
    max_concurrent: usize,
    at: chrono::DateTime<chrono::Utc>,
) {
    publish_tick_summary_with_roots_at(report, max_concurrent, at, &[]);
}

/// [`publish_tick_summary_at`] that also names each queue row's repo from
/// `roots` (workspace index -> repo root, Issue #8852).
pub fn publish_tick_summary_with_roots_at(
    report: &TickReport,
    max_concurrent: usize,
    at: chrono::DateTime<chrono::Utc>,
    roots: &[PathBuf],
) {
    let summary = tick_summary(report, max_concurrent, at, roots);
    *last_tick_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(summary);
}

/// The wire summary of `report` (pure; [`publish_tick_summary_with_roots_at`]
/// stores it).
#[must_use]
pub fn tick_summary(
    report: &TickReport,
    max_concurrent: usize,
    at: chrono::DateTime<chrono::Utc>,
    roots: &[PathBuf],
) -> WorkFinderTickSummary {
    WorkFinderTickSummary {
        queue: ready_queue::finish(&report.queue, roots),
        listing_failed: ready_queue::repo_names(&report.listing_failed, roots),
        at,
        max_concurrent,
        seen: report.seen,
        dispatched: report.dispatched,
        skipped_labeled: report.skipped_labeled,
        skipped_in_flight: report.skipped_in_flight,
        skipped_quarantined: report.skipped_quarantined,
        skipped_workspace_commands_missing: report.skipped_workspace_commands_missing,
        skipped_pr_open: report.skipped_pr_open,
        skipped_peer_claim: report.skipped_peer_claim,
        skipped_backoff: report.skipped_backoff,
        skipped_pr_open_backoff: report.skipped_pr_open_backoff,
        skipped_noop_cooldown: report.skipped_noop_cooldown,
        skipped_declined: report.skipped_declined,
        // #7972: `skipped_prless_retry` is deliberately NOT carried on the
        // cross-process `WorkFinderTickSummary` yet — `types.rs` is over the
        // file-size ratchet's threshold and frozen at its current size
        // (.loom/docs/file-size-policy.md), and a wire field is not worth
        // displacing unrelated code for. The counter IS on `TickReport` and
        // appears as `prless-retry-skip` on the per-tick summary log line.
        skipped_recheck_interval: report.skipped_recheck_interval,
        skipped_host_constraint: report.skipped_host_constraint,
        deferred_capacity: report.deferred_capacity,
        deferred_ramp_cap: report.deferred_ramp_cap,
        deferred_saturation: report.deferred_saturation,
        errors: report.errors,
        halted: report.halted,
        saturation_held: report.saturation_held,
        collisions: report.collisions,
    }
}

/// [`publish_tick_summary_at`] stamped with the current wall clock.
pub fn publish_tick_summary(report: &TickReport, max_concurrent: usize) {
    publish_tick_summary_at(report, max_concurrent, chrono::Utc::now());
}

/// Read back the most recently published tick summary, or `None` when no tick
/// has completed in this process (Issue #4761).
#[must_use]
pub fn last_tick_summary() -> Option<WorkFinderTickSummary> {
    last_tick_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Test-only reset of the process-global last-tick slot.
#[cfg(test)]
pub(super) fn reset_last_tick_summary() {
    *last_tick_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

/// End-of-tick seam for both work-finder loops: publish the summary for
/// `loom-daemon health` (#4761), then export the tick as OTLP telemetry
/// (#8860 — a no-op unless an OTLP exporter is running). `started_at` is when
/// the tick's candidate evaluation began; `roots` names each ready-queue row's
/// repo (#8852 — empty for the single-workspace loop).
pub fn publish_tick(
    report: &TickReport,
    max_concurrent: usize,
    started_at: chrono::DateTime<chrono::Utc>,
    roots: &[PathBuf],
) {
    publish_tick_summary_with_roots_at(report, max_concurrent, chrono::Utc::now(), roots);
    crate::observability::ops::dispatch::record_tick(report, max_concurrent, started_at);
}
