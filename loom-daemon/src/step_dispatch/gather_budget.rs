//! Which repos one step tick gathers before its deadline (#12074).
//!
//! The gather walks every managed repo, and each costs forge reads. A tick
//! has a wall-clock deadline (`autonomous.workFinder.tickDeadlineSecs`); once
//! it passes, the tick stops gathering, draws from what it gathered, and
//! reports the rest as skipped. The walk starts where the last truncated
//! tick stopped ([`GatherWalk::next_start`]), so a run of slow ticks still
//! covers every repo in turn instead of re-reading the first few forever.
//!
//! At least one repo is gathered every tick, deadline or not, so a tick whose
//! pre-dispatch section used the whole budget still makes progress (its reads
//! then fail fast and fall back to the last good listing).

use std::time::{Duration, Instant};

/// The cost and coverage of one tick's gather, for the tick report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GatherStats {
    /// Wall-clock time spent gathering.
    pub gather_ms: u64,
    /// Repos whose items were read (or attempted) this tick.
    pub repos_gathered: usize,
    /// Repos not read because the deadline passed.
    pub repos_skipped: usize,
    /// The deadline cut the gather short.
    pub truncated: bool,
}

/// One tick's walk over `n` repos from `start`.
#[derive(Debug)]
pub struct GatherWalk {
    n: usize,
    start: usize,
    gathered: usize,
    skipped: Vec<usize>,
    began: Instant,
}

impl GatherWalk {
    /// A walk over `n` repos starting at `start` (taken modulo `n`).
    #[must_use]
    pub fn new(n: usize, start: usize, began: Instant) -> Self {
        Self {
            n,
            start: if n == 0 { 0 } else { start % n },
            gathered: 0,
            skipped: Vec::new(),
            began,
        }
    }

    /// The repo indices in walk order.
    pub fn order(&self) -> impl Iterator<Item = usize> {
        let (n, start) = (self.n, self.start);
        (0..n).map(move |k| (start + k) % n)
    }

    /// Whether to gather repo `idx` when the deadline has `passed`: yes for
    /// the first repo of the tick and for any repo before the deadline;
    /// otherwise it is recorded as skipped.
    pub fn admit(&mut self, idx: usize, passed: bool) -> bool {
        if passed && self.gathered > 0 {
            self.skipped.push(idx);
            return false;
        }
        self.gathered += 1;
        true
    }

    /// Where the next tick's walk starts: the first repo skipped this tick,
    /// else where this one started.
    #[must_use]
    pub fn next_start(&self) -> usize {
        self.skipped.first().copied().unwrap_or(self.start)
    }

    /// This walk's figures, measured at `now`.
    #[must_use]
    pub fn stats(&self, now: Instant) -> GatherStats {
        GatherStats {
            gather_ms: u64::try_from(now.saturating_duration_since(self.began).as_millis())
                .unwrap_or(u64::MAX),
            repos_gathered: self.gathered,
            repos_skipped: self.skipped.len(),
            truncated: !self.skipped.is_empty(),
        }
    }
}

/// Whether `deadline` (if any) has passed at `now`.
#[must_use]
pub fn deadline_passed(deadline: Option<Instant>, now: Instant) -> bool {
    deadline.is_some_and(|d| now >= d)
}

/// The default work-finder tick deadline.
pub const DEFAULT_TICK_DEADLINE: Duration = Duration::from_secs(90);

/// `autonomous.workFinder.tickDeadlineSecs` from an effective config: a
/// positive integer, else [`DEFAULT_TICK_DEADLINE`]. Pure.
#[must_use]
pub fn tick_deadline_from(effective: &serde_json::Value) -> Duration {
    crate::config_resolver::get_path(effective, "autonomous.workFinder.tickDeadlineSecs")
        .and_then(serde_json::Value::as_u64)
        .filter(|s| *s > 0)
        .map_or(DEFAULT_TICK_DEADLINE, Duration::from_secs)
}

/// The tick deadline for the daemon rooted at `root`, read live each tick.
#[must_use]
pub fn tick_deadline(root: &std::path::Path) -> Duration {
    tick_deadline_from(&crate::config_resolver::resolve_effective_config(root))
}

/// Start one work-finder tick body: log it at INFO, so a tick that never
/// reaches its dispatch line can be placed, and return its deadline (#12074).
#[must_use]
pub fn begin_tick(root: &std::path::Path) -> Instant {
    static TICKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = TICKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    let budget = tick_deadline(root);
    log::info!("work_finder: tick {n} start (deadline {}s, #12074)", budget.as_secs());
    Instant::now() + budget
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "gather_budget_tests.rs"]
mod tests;
