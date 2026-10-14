//! Host-level capacity ask (#10214): a resource-limited concurrency cap.
//!
//! On 2026-10-04 free disk on loom-worker-1 fell to 4 GB, disk headroom held
//! `max_concurrent` at 2, and 106 issues queued behind it for hours. The only
//! signal was per-issue "no progress" comments blaming the work finder. The
//! condition is not an issue's problem, so it does not belong in a per-issue
//! comment: it is one host-level [`Condition`], de-duplicated and delivered by
//! the fleet-alert state machine and sinks (the event bus).
//!
//! The `star-backlog` ask (more starred issues waiting than 3x the cap) was
//! retired with the operator star (#11510): priority now only orders the
//! queue (#11103), and `loom:very-important` is capped at one per workspace,
//! so a priority backlog is no longer a FIFO the operator must re-rank.

use super::Condition;
use crate::types::{CapLimiter, WorkFinderTickSummary};

/// Condition key: disk or RAM headroom holds the cap below `maxConcurrent`.
pub const KEY_CAPACITY_LIMITED: &str = "capacity-limited";

/// The host-level capacity conditions the last tick shows.
#[must_use]
pub fn conditions(tick: &WorkFinderTickSummary) -> Vec<Condition> {
    let mut out = Vec::new();
    // Issues waiting only on a capacity-style gate (`ready` rows).
    let waiting = tick
        .queue
        .iter()
        .filter(|r| r.disposition.state() == "ready")
        .count();
    let effective = tick.cap.map_or(tick.max_concurrent, |c| c.effective());

    if let Some(cap) = tick.cap.filter(crate::types::CapView::resource_limited) {
        let (what, fix) = match cap.limiter() {
            CapLimiter::Ram => (
                "Available RAM",
                "Free memory on this host (stop stray processes) or add RAM / a host.",
            ),
            _ => (
                "Disk headroom on the worktree volume",
                "Free disk on the worktree volume (reclaim stale worktrees and build \
                 targets) or add disk / a host.",
            ),
        };
        let waiting_part = if waiting > 0 {
            format!(", and {waiting} ready issue(s) are waiting")
        } else {
            String::new()
        };
        out.push(Condition {
            key: KEY_CAPACITY_LIMITED.to_string(),
            critical: false,
            headline: format!(
                "{what} is capping concurrency at {effective}, below the configured {}{waiting_part}.",
                cap.configured
            ),
            fix: fix.to_string(),
        });
    }
    out
}
