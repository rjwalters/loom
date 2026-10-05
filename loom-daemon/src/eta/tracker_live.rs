//! The tracker's live items as plain values (Issue #10196, `fleet.state`).
//!
//! A read-only view. It clones what the tracker already holds in memory and
//! never touches the estimation path, the forge or the clock, so the
//! `fleet.state` emitter is a reader of tracker state and not a second tick
//! loop. An item is live when it has a current stage, has not landed, and is
//! still a member of the fleet's in-flight work: a running sweep, a PR in a
//! review listing, or a ready-queue row. An item that only awaits an outcome
//! read (its sweep ended, or its PR left the listing) keeps its stage for
//! the pending ETA outcomes but is not current state, so it is not exported.

use super::Tracker;
use crate::eta::{AgeSource, Stage};
use chrono::{DateTime, Utc};

/// One live item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveItem {
    /// Lowercased `owner/repo`.
    pub repo: String,
    /// Issue.
    pub issue: u32,
    /// Current stage.
    pub stage: Stage,
    /// When it entered `stage`.
    pub entered_at: DateTime<Utc>,
    /// `entered_at` is a lower bound, not an observed transition.
    pub entered_at_lower_bound: bool,
    /// The PR, when known.
    pub pr: Option<u32>,
    /// The sweep this host runs for it, while that sweep is running.
    pub running_sweep_id: Option<String>,
}

impl Tracker {
    /// Every live item, in `(repo, issue)` order.
    #[must_use]
    pub fn live_items(&self) -> Vec<LiveItem> {
        self.items
            .iter()
            .filter(|(_, item)| {
                !item.landed
                    && (item.sweep_running || item.in_review_listing || item.in_ready_queue)
            })
            .filter_map(|(key, item)| {
                let track = item.stage.as_ref()?;
                Some(LiveItem {
                    repo: key.repo.clone(),
                    issue: key.issue,
                    stage: track.stage,
                    entered_at: track.entered_at,
                    entered_at_lower_bound: !track.exact
                        || track.source == AgeSource::UpdatedAtLowerBound,
                    pr: item.pr_number,
                    running_sweep_id: if item.sweep_running {
                        item.sweep_id.clone()
                    } else {
                        None
                    },
                })
            })
            .collect()
    }
}
