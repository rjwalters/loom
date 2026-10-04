//! When an estimate is emitted (operator decision on #9289): on every stage
//! transition immediately, and otherwise refreshed every `refresh_secs`
//! (5 minutes by default), each with its full explanation. A refusal is
//! emitted when its reason first appears and is not refreshed. A per-series
//! hourly cap bounds a flapping item.
//!
//! The signature is per series: an item's series share its stage, rework
//! count and refusal reason, except a hold-aware series of a held item
//! (#10284), whose signature is `merge_hold` with no reason, so it refreshes
//! while the item's other series keep its `blocked` refusal unrefreshed
//! (`tracker_hold.rs`).

use super::{NoEstimateReason, Stage};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// Most emissions per `(item, kind, heuristic)` in any rolling hour: twelve
/// 5-minute refreshes plus headroom for transitions.
pub const HOURLY_CAP: usize = 20;

/// Why an estimate was emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// The first estimate of this series.
    First,
    /// The stage, the rework count or the refusal reason changed.
    Transition,
    /// The periodic refresh of an unchanged estimate.
    Refresh,
}

impl Trigger {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Trigger::First => "first",
            Trigger::Transition => "transition",
            Trigger::Refresh => "refresh",
        }
    }
}

/// What identifies "the same" estimate state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    /// Current stage.
    pub stage: Option<Stage>,
    /// Rework rounds taken.
    pub rework_rounds: u32,
    /// Refusal reason.
    pub reason: Option<NoEstimateReason>,
}

/// One series' emission history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EmitState {
    last_at: Option<DateTime<Utc>>,
    last: Option<Signature>,
    recent: VecDeque<DateTime<Utc>>,
}

impl EmitState {
    /// Whether to emit an estimate with `signature` at `now`, and why. Does
    /// not record anything; call [`Self::record`] after emitting.
    #[must_use]
    pub fn decide(
        &self,
        signature: Signature,
        now: DateTime<Utc>,
        refresh_secs: u64,
    ) -> Option<Trigger> {
        let window_start = now - Duration::hours(1);
        let in_window = self.recent.iter().filter(|t| **t > window_start).count();
        if in_window >= HOURLY_CAP {
            return None;
        }
        match self.last {
            None => return Some(Trigger::First),
            Some(last) if last != signature => return Some(Trigger::Transition),
            Some(_) => {}
        }
        if signature.reason.is_some() {
            return None;
        }
        // A tenth of the interval as slack, so a pass that fires a moment
        // early still refreshes instead of skipping to the next one.
        let due = refresh_secs.saturating_sub(refresh_secs / 10) as i64;
        match self.last_at {
            Some(at) if (now - at).num_seconds() >= due => Some(Trigger::Refresh),
            Some(_) => None,
            None => Some(Trigger::Refresh),
        }
    }

    /// Record an emission of `signature` at `now`.
    pub fn record(&mut self, signature: Signature, now: DateTime<Utc>) {
        self.last = Some(signature);
        self.last_at = Some(now);
        self.recent.push_back(now);
        let window_start = now - Duration::hours(1);
        while self.recent.front().is_some_and(|t| *t <= window_start) {
            self.recent.pop_front();
        }
    }
}
