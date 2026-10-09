//! Host-level capacity asks (#10214): a resource-limited concurrency cap, and
//! a starred backlog that has outgrown the cap.
//!
//! On 2026-10-04 free disk on loom-worker-1 fell to 4 GB, disk headroom held
//! `max_concurrent` at 2, and 106 starred issues queued behind it for hours.
//! The only signal was per-issue "no progress" comments blaming the work
//! finder. Neither condition is an issue's problem, so neither belongs in a
//! per-issue comment: each is one host-level [`Condition`], de-duplicated and
//! delivered by the fleet-alert state machine and sinks (the event bus and,
//! independently of Safehouse, the loom-ui inbox).

use super::Condition;
use crate::star_liveness::queue::waiting_star;
use crate::types::{CapLimiter, WorkFinderTickSummary};

/// Condition key: disk or RAM headroom holds the cap below `maxConcurrent`.
pub const KEY_CAPACITY_LIMITED: &str = "capacity-limited";
/// Condition key: far more starred issues wait than the host has slots.
pub const KEY_STAR_BACKLOG: &str = "star-backlog";
/// A starred backlog larger than this multiple of the effective cap means a
/// star no longer means "next": it is a FIFO position.
pub const STAR_BACKLOG_FACTOR: usize = 3;

/// The host-level capacity conditions the last tick shows.
#[must_use]
pub fn conditions(tick: &WorkFinderTickSummary) -> Vec<Condition> {
    let mut out = Vec::new();
    let mut stars: Vec<_> = tick.queue.iter().filter(|r| waiting_star(r)).collect();
    stars.sort_by_key(|r| r.rank);
    let waiting = stars.len();
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
        let stars_part = if waiting > 0 {
            format!(", and {waiting} starred issue(s) are waiting")
        } else {
            String::new()
        };
        out.push(Condition {
            key: KEY_CAPACITY_LIMITED.to_string(),
            critical: false,
            headline: format!(
                "{what} is capping concurrency at {effective}, below the configured {}{stars_part}.",
                cap.configured
            ),
            fix: fix.to_string(),
        });
    }

    if waiting > STAR_BACKLOG_FACTOR.saturating_mul(effective.max(1)) {
        let oldest = stars.first().map_or_else(String::new, |r| {
            let at = r
                .operator_priority_at
                .as_deref()
                .map_or_else(String::new, |t| format!(", starred {t}"));
            format!(" The oldest is #{} in {}{at}.", r.issue, r.repo)
        });
        let rounds = waiting.div_ceil(effective.max(1));
        out.push(Condition {
            key: KEY_STAR_BACKLOG.to_string(),
            critical: false,
            headline: format!(
                "{waiting} starred issues are waiting for {effective} slot(s) on this host, more \
                 than {STAR_BACKLOG_FACTOR}x the cap: a new star is now position {} in a FIFO \
                 and waits about {rounds} sweep-lengths.{oldest}",
                waiting + 1
            ),
            fix: "Unstar or re-rank stars so the urgent ones lead, or add capacity.".to_string(),
        });
    }
    out
}
