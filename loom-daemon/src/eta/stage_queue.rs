//! The queue an in-flight item sits in (#10208): items ahead of it in its
//! stage, and that stage's recent drain rate. The input of `little-v0`.
//!
//! One definition, shared by serving (the tracker, from its fleet view) and
//! replay ([`super::backtest::ReplayCase::queue`]).
//!
//! - **Scope**: a PR in `merge_wait` queues behind its own repo's PRs (merge
//!   capacity is per repo); every other PR stage (`review_wait`, `doctor`)
//!   shares fleet capacity, so its queue is fleet-wide.
//! - **Items ahead**: other open PRs in the same stage and scope that entered
//!   it earlier (ties: repo, then PR number).
//! - **Drain rate**: departures from the stage in scope over the last
//!   [`WINDOW_SEC`], each weighted `2^(-age / HALF_LIFE_SEC)`, normalised so
//!   a steady `r` exits/hour reads as `r`.
//!
//! Knowability follows [`super::queue_features`]: a roster entry or event is
//! read only when `known_at < as_of` (and an event's own `at < as_of`).
//!
//! Pure: no clock, filesystem, environment or globals.

use super::queue_features::{is_pr_stage, EventLog, RosterEntry};
use super::Stage;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// Half life of the drain-rate weighting: 6 h.
pub const HALF_LIFE_SEC: i64 = 6 * 3600;

/// Exits older than this are not read: 24 h, four half lives.
pub const WINDOW_SEC: i64 = 24 * 3600;

/// Whose capacity the stage's queue shares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueScope {
    /// The item's own repo.
    Repo,
    /// Every repo in the fleet scope.
    Fleet,
}

impl QueueScope {
    /// The scope `stage`'s capacity is shared at.
    #[must_use]
    pub fn of(stage: Stage) -> Self {
        if stage == Stage::MergeWait {
            QueueScope::Repo
        } else {
            QueueScope::Fleet
        }
    }

    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            QueueScope::Repo => "repo",
            QueueScope::Fleet => "fleet",
        }
    }
}

/// The queue context of one stage at one instant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageQueue {
    /// The stage.
    pub stage: Stage,
    /// The scope of `items_ahead` and the exits.
    pub scope: QueueScope,
    /// Other open PRs in the stage and scope that entered it earlier.
    pub items_ahead: u32,
    /// Exponentially weighted exits per hour (six decimals).
    pub drain_rate_per_hr: f64,
    /// Half life of the weighting, seconds.
    pub half_life_sec: i64,
    /// Window the exits were read from, seconds.
    pub window_sec: i64,
    /// Exits in the window, unweighted.
    pub exits: u32,
    /// The Judge's real pick order for a `review_wait` PR (#10921,
    /// [`super::planner_queue`]), set by the caller after this FIFO view.
    /// Absent for every other stage and before the planner view existed, so
    /// a queue without it serializes as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planner: Option<super::planner_queue::PlannerPosition>,
}

/// The exponentially weighted exit rate, per hour, of exits `ages_sec` old.
///
/// The kernel `(ln 2 / H) · 2^(-a / H)` integrates to one over `[0, ∞)`;
/// truncated at the window it integrates to `1 - 2^(-W / H)`, which divides
/// the sum so a constant rate reads as itself.
#[must_use]
pub fn weighted_rate_per_hr(ages_sec: &[i64], half_life_sec: i64, window_sec: i64) -> f64 {
    let h = half_life_sec as f64;
    let norm = 1.0 - (-(window_sec as f64) / h * std::f64::consts::LN_2).exp();
    let per_sec: f64 = ages_sec
        .iter()
        .map(|&a| {
            std::f64::consts::LN_2 / h * (-(a.max(0) as f64) / h * std::f64::consts::LN_2).exp()
        })
        .sum();
    round6(per_sec / norm * 3600.0)
}

fn round6(x: f64) -> f64 {
    (x * 1_000_000.0).round() / 1_000_000.0
}

/// The queue context of the PR `pr` of `repo`, in `stage` since `entered_at`,
/// at `as_of`.
///
/// `scope` is the repos whose roster and events are complete. `None` when
/// `stage` is not a PR stage or the item's repo is outside `scope`.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn stage_queue(
    repo: &str,
    pr: u32,
    stage: Stage,
    entered_at: DateTime<Utc>,
    roster: &[RosterEntry],
    log: &EventLog,
    scope: &[String],
    as_of: DateTime<Utc>,
) -> Option<StageQueue> {
    if !is_pr_stage(stage) {
        return None;
    }
    let in_scope = |r: &str| scope.iter().any(|s| s.eq_ignore_ascii_case(r));
    if !in_scope(repo) {
        return None;
    }
    let queue_scope = QueueScope::of(stage);
    let shared = |r: &str| match queue_scope {
        QueueScope::Repo => r.eq_ignore_ascii_case(repo),
        QueueScope::Fleet => in_scope(r),
    };
    let me = (entered_at, repo.to_ascii_lowercase(), pr);
    let ahead = roster
        .iter()
        .filter(|r| r.known_at < as_of && r.stage == Some(stage) && shared(&r.repo))
        .filter(|r| (r.entered_at, r.repo.to_ascii_lowercase(), r.pr) < (me.0, me.1.clone(), me.2))
        .count();
    let floor = as_of - Duration::seconds(WINDOW_SEC);
    let ages: Vec<i64> = log
        .events
        .iter()
        .filter(|e| e.known_at < as_of && e.at < as_of && e.at >= floor)
        .filter(|e| e.stage == Some(stage) && shared(&e.repo))
        .map(|e| (as_of - e.at).num_seconds())
        .collect();
    Some(StageQueue {
        stage,
        scope: queue_scope,
        items_ahead: u32::try_from(ahead).unwrap_or(u32::MAX),
        drain_rate_per_hr: weighted_rate_per_hr(&ages, HALF_LIFE_SEC, WINDOW_SEC),
        half_life_sec: HALF_LIFE_SEC,
        window_sec: WINDOW_SEC,
        exits: u32::try_from(ages.len()).unwrap_or(u32::MAX),
        planner: None,
    })
}
