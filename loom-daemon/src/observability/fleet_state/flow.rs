//! Per-repo `ready_wait` queue flow (#11645): the arrivals and drains of each
//! repo's dispatch queue over a trailing window, so a reader can predict
//! backlog via Little's Law (wait ≈ `queue_depth` / drain_rate).
//!
//! The tracker is fed the **complete** ready-issue set of a repo every pass.
//! A diff against the last set it saw for the repo is that pass's arrivals
//! (issues now present that were not) and drains (issues that were present and
//! are now gone). The first sight of a repo only seeds the baseline — its
//! queue predates our watch, so no arrival or drain is counted for it. Each
//! event is stamped with the pass instant and expired once older than the
//! window.
//!
//! Counts are discrete, not a continuous rate: an idle repo reports the same
//! numbers pass after pass, so its `fleet.state` row stays unchanged (a float
//! rate would make every pass a delta — cf. `FleetCapacity`). The window start
//! is reported as an instant so a reader can compute the effective window
//! without the tracker emitting a fresh elapsed span every pass.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use chrono::{DateTime, Duration, Utc};

use crate::telemetry::kinds::fleet_state::FleetQueueFlow;

/// The default trailing window `fleet.state` counts arrivals and drains over
/// (6 h). Chosen for the `ready_wait` long tail: the evidence behind #11645
/// used a Little's-Law repo wait of `queue_depth / drain_rate` over roughly
/// this horizon.
pub const QUEUE_FLOW_WINDOW_SECS: i64 = 6 * 60 * 60;

/// One repo's running flow state.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RepoFlow {
    /// When this host first observed the repo's queue.
    since: DateTime<Utc>,
    /// The last complete ready set seen, to diff the next pass against.
    last_set: BTreeSet<u32>,
    /// Arrival instants, oldest first.
    arrivals: VecDeque<DateTime<Utc>>,
    /// Drain instants, oldest first.
    drains: VecDeque<DateTime<Utc>>,
}

/// A per-repo trailing-window arrival/drain tracker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueFlowTracker {
    window_secs: i64,
    repos: BTreeMap<String, RepoFlow>,
}

impl Default for QueueFlowTracker {
    fn default() -> Self {
        Self::new(QUEUE_FLOW_WINDOW_SECS)
    }
}

impl QueueFlowTracker {
    /// A tracker with an empty history and the given trailing window.
    #[must_use]
    pub const fn new(window_secs: i64) -> Self {
        Self {
            window_secs,
            repos: BTreeMap::new(),
        }
    }

    fn window(&self) -> Duration {
        Duration::seconds(self.window_secs)
    }

    /// Record `repo`'s complete ready set at `now`. Only call for a repo whose
    /// ready listing was read whole this pass — a partial set would read its
    /// missing rows as drains. The first sight of a repo seeds the baseline
    /// with no counts; later sights count the issues that entered and left
    /// since the last set.
    pub fn observe(&mut self, repo: &str, ready: &BTreeSet<u32>, now: DateTime<Utc>) {
        let cutoff = now - self.window();
        let Some(flow) = self.repos.get_mut(repo) else {
            self.repos.insert(
                repo.to_string(),
                RepoFlow {
                    since: now,
                    last_set: ready.clone(),
                    arrivals: VecDeque::new(),
                    drains: VecDeque::new(),
                },
            );
            return;
        };
        let arrived = ready.difference(&flow.last_set).count();
        let drained = flow.last_set.difference(ready).count();
        for _ in 0..arrived {
            flow.arrivals.push_back(now);
        }
        for _ in 0..drained {
            flow.drains.push_back(now);
        }
        flow.last_set.clone_from(ready);
        expire(&mut flow.arrivals, cutoff);
        expire(&mut flow.drains, cutoff);
    }

    /// The size of the last set observed for `repo` (its queue depth as of
    /// the last [`Self::observe`]), or `None` when it was never observed. Used
    /// for a queue read only on some passes (e.g. the triage queue, #11642):
    /// between reads the depth is the last observed set, never a stale guess.
    #[must_use]
    pub fn depth(&self, repo: &str) -> Option<u32> {
        self.repos
            .get(repo)
            .map(|flow| u32::try_from(flow.last_set.len()).unwrap_or(u32::MAX))
    }

    /// Every repo this tracker has observed, so a pass with no fresh read can
    /// re-stamp each from the last observed set (#11642).
    pub fn observed_repos(&self) -> impl Iterator<Item = &str> {
        self.repos.keys().map(String::as_str)
    }

    /// The repo's flow at `now`, or `None` when it was never observed. Counts
    /// are filtered to the window on read, so a repo not observed this pass
    /// still reports correctly.
    #[must_use]
    pub fn flow(&self, repo: &str, now: DateTime<Utc>) -> Option<FleetQueueFlow> {
        let flow = self.repos.get(repo)?;
        let cutoff = now - self.window();
        let within = |q: &VecDeque<DateTime<Utc>>| {
            u32::try_from(q.iter().filter(|&&t| t >= cutoff).count()).unwrap_or(u32::MAX)
        };
        Some(FleetQueueFlow {
            arrivals: within(&flow.arrivals),
            drains: within(&flow.drains),
            window_secs: u64::try_from(self.window_secs).unwrap_or(0),
            since: flow.since,
        })
    }
}

/// Drop every front event older than `cutoff` (events are pushed in
/// nondecreasing time order, so the front is the oldest).
fn expire(q: &mut VecDeque<DateTime<Utc>>, cutoff: DateTime<Utc>) {
    while q.front().is_some_and(|&t| t < cutoff) {
        q.pop_front();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "flow_tests.rs"]
mod tests;
