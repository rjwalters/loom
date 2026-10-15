//! Per-repo operator-action recency and hold-release burst markers on
//! `fleet.state` (#11956), derived from the delta between consecutive
//! collector passes' operator-held sets. Telemetry only.
//!
//! Approximate by construction: a release cannot be attributed to a human, and
//! the state is in memory, so it resets on daemon restart (the first pass after
//! one only seeds the baseline). A held item whose row left the view entirely
//! (the PR merged or the issue closed) is not a release.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};

use super::{FleetView, RepoView};
use crate::telemetry::kinds::fleet_state::{
    FleetHoldKind, FleetHoldReleaseBurst, FleetOperatorRecency, FleetStage,
};

/// Releases within [`BURST_WINDOW_MINUTES`] that make a burst.
pub(super) const BURST_MIN_RELEASES: u32 = 10;
/// The trailing window the burst count is taken over.
pub(super) const BURST_WINDOW_MINUTES: u32 = 60;

/// What is remembered of one repo between passes.
#[derive(Debug, Clone, Default)]
struct Track {
    /// The previous complete pass's operator-held issues; `None` until a pass
    /// read the census whole (so nothing is diffed against a guess).
    prev: Option<BTreeSet<u32>>,
    /// The latest observed release.
    last_release: Option<DateTime<Utc>>,
    /// Release instants inside the burst window, oldest first.
    recent: Vec<DateTime<Utc>>,
}

static TRACKS: Mutex<BTreeMap<String, Track>> = Mutex::new(BTreeMap::new());

/// The issues under an operator hold in `repo`: `merge_hold` rows and rows with
/// an operator-kind hold. `None` when the census was not read whole.
fn held_set(repo: &RepoView) -> Option<BTreeSet<u32>> {
    repo.census.as_ref()?;
    let is_operator = |kind: Option<FleetHoldKind>| {
        matches!(
            kind,
            Some(
                FleetHoldKind::Operator
                    | FleetHoldKind::OperatorOnly
                    | FleetHoldKind::OperatorDecision
            )
        )
    };
    Some(
        repo.rows
            .values()
            .filter(|row| row.stage == FleetStage::MergeHold || is_operator(row.hold_kind))
            .map(|row| row.issue)
            .collect(),
    )
}

impl Track {
    /// Fold one pass: items that left the held set but are still open rows are
    /// releases at `now`.
    fn observe(&mut self, repo: &RepoView, now: DateTime<Utc>) {
        let held = held_set(repo);
        if let (Some(prev), Some(held)) = (&self.prev, &held) {
            let released = prev
                .iter()
                .filter(|issue| !held.contains(issue) && repo.rows.contains_key(issue))
                .count();
            if released > 0 {
                self.last_release = Some(now);
                self.recent.extend(std::iter::repeat_n(now, released));
            }
        }
        self.prev = held;
    }

    /// The markers as of `now`; `None` while no release was ever observed.
    fn markers(&mut self, now: DateTime<Utc>) -> Option<FleetOperatorRecency> {
        let cutoff = now - Duration::minutes(i64::from(BURST_WINDOW_MINUTES));
        self.recent.retain(|at| *at > cutoff);
        let last_hold_release_at = self.last_release?;
        let count = u32::try_from(self.recent.len()).unwrap_or(u32::MAX);
        Some(FleetOperatorRecency {
            last_hold_release_at,
            hold_release_burst: (count >= BURST_MIN_RELEASES).then_some(FleetHoldReleaseBurst {
                count_in_window: count,
                window_minutes: BURST_WINDOW_MINUTES,
            }),
        })
    }
}

/// Collector pass: diff each repo's held set against the previous pass, then
/// stamp the view.
pub(super) fn observe(view: &mut FleetView, now: DateTime<Utc>) {
    let mut tracks = TRACKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (name, repo) in &mut view.repos {
        let track = tracks.entry(name.clone()).or_default();
        track.observe(repo, now);
        repo.operator_recency = track.markers(now);
    }
}

/// Tick pass: re-stamp from what the last collector pass saw (no diff).
pub(super) fn restamp(view: &mut FleetView, now: DateTime<Utc>) {
    let mut tracks = TRACKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (name, repo) in &mut view.repos {
        repo.operator_recency = tracks.get_mut(name).and_then(|t| t.markers(now));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "operator_recency_tests.rs"]
mod tests;
