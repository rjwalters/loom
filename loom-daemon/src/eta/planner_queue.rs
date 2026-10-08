//! The review queue in the Judge's **real pick order** (#10921): how many PRs
//! the planner puts ahead of a PR in `review_wait`, and its repo's review
//! drain rate. The input of `land-2026-10-08-ranked-rook`.
//!
//! [`super::stage_queue`] counts the items ahead **first in, first out** by
//! stage entry. The Judge does not pick that way. It walks
//! `loom-daemon pr-queue --role judge`, which is
//! [`crate::pr_planning::ordered_queue`] over the open-PR listing:
//!
//! 1. the effective operator level, highest first (#10307: level 2, then the
//!    star, then everything unstarred);
//! 2. a trusted human-origin PR first, when `planning.preferHumanPrs`;
//! 3. the listing order, which is **newest first** (`sort=created`,
//!    `direction=desc`, so the higher PR number of one repo first).
//!
//! So a starred PR jumps the queue, and among unstarred PRs the newest is
//! reviewed first. This module calls that one function, so there is no
//! second ordering to drift from the planner.
//!
//! # The rows it orders
//!
//! Each PR of the subject's repo in `review_wait` at `as_of` becomes one
//! listing row: its number, `state: open`, and its labels as known strictly
//! before `as_of`: `loom:review-requested` (what puts it in `review_wait`)
//! and the operator label of its own level ([`crate::operator_levels`]). The
//! level is the PR's **own** labels' ([`PriorityState`] without a linked
//! issue's star), because `ordered_queue` reads the PR's labels and nothing
//! else. Rows are handed over newest first, the listing's own order.
//!
//! # `preferHumanPrs` is pinned off
//!
//! Origin is a trusted provenance marker in the PR body, checked against
//! the author's login and association. Neither the replay's label timeline
//! nor serving's roster carries them point-in-time, so origin cannot be
//! reconstructed at `as_of`. Serving and replay therefore both order with
//! [`PREFER_HUMAN_PRS`] `= false`: train/serve parity before fidelity
//! (#10921, curator Q1). With no origin on the rows, `true` would order the
//! same anyway; the pin makes that explicit.
//!
//! # Scope and drain
//!
//! The Judge runs one instance per (repository, role), so the queue and its
//! drain are **per repo**: the drain rate is the review departures of the
//! subject's repo, weighted as [`super::stage_queue::weighted_rate_per_hr`]
//! (half life 6 h, 24 h window).
//!
//! # Knowability
//!
//! As [`super::queue_features`]: a roster entry is read only when
//! `known_at < as_of`, a level change only when it happened before `as_of`
//! ([`PriorityState::known`]), an event only when `known_at < as_of` and
//! `at < as_of`.
//!
//! Pure: reads its arguments and nothing else.

use super::priority_features::{PriorityEntry, PriorityState};
use super::queue_features::EventLog;
use super::stage_queue::{weighted_rate_per_hr, HALF_LIFE_SEC, WINDOW_SEC};
use super::Stage;
use crate::comment_trust::TrustPolicy;
use crate::pr_planning::{ordered_queue, PrRole};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// `planning.preferHumanPrs` as the ETA orders with it: pinned off in
/// serving and replay alike (see the module docs).
pub const PREFER_HUMAN_PRS: bool = false;

/// The review label every row carries.
const REVIEW_REQUESTED: &str = "loom:review-requested";

/// A PR's position in its repo's Judge queue at one instant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlannerPosition {
    /// The role whose pick order this is (`judge`).
    pub role: String,
    /// PRs `ordered_queue` puts before the subject.
    pub items_ahead: u32,
    /// PRs in the queue, the subject included.
    pub queue_len: u32,
    /// The subject's own operator level as the planner read it.
    pub operator_level: u8,
    /// The `preferHumanPrs` the order was computed with.
    pub prefer_human_prs: bool,
    /// The repo's exponentially weighted review exits per hour (six
    /// decimals).
    pub drain_rate_per_hr: f64,
    /// The repo's review exits in the window, unweighted.
    pub exits: u32,
}

/// `state`'s own-label level (no linked-issue star) as known before `as_of`.
fn own_level(state: &PriorityState, as_of: DateTime<Utc>) -> u8 {
    PriorityState {
        linked_since: None,
        linked_changes: Vec::new(),
        ..state.known(as_of)
    }
    .level()
}

/// One listing row: the fields `ordered_queue` reads for the Judge.
fn row(pr: u32, level: u8) -> Value {
    let mut labels = vec![json!({ "name": REVIEW_REQUESTED })];
    if let Some(level) = crate::operator_levels::row(crate::operator_levels::table(), level) {
        labels.push(json!({ "name": level.operator_label }));
    }
    json!({ "number": pr, "state": "open", "draft": false, "labels": labels })
}

/// The Judge-queue position of PR `pr` of `repo`, in `stage` with own-label
/// priority `star`, at `as_of`.
///
/// - `roster`: open PRs with their own star state (any linked star on them
///   is ignored);
/// - `log`: stage departures;
/// - `scope`: the repos whose roster and events are complete.
///
/// `None` unless `stage` is `review_wait` and `repo` is in `scope`.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn planner_position(
    repo: &str,
    pr: u32,
    stage: Stage,
    star: &PriorityState,
    roster: &[PriorityEntry],
    log: &EventLog,
    scope: &[String],
    as_of: DateTime<Utc>,
) -> Option<PlannerPosition> {
    if stage != Stage::ReviewWait || !scope.iter().any(|s| s.eq_ignore_ascii_case(repo)) {
        return None;
    }
    let in_repo = |r: &str| r.eq_ignore_ascii_case(repo);
    let level = own_level(star, as_of);
    let mut rows: Vec<(u32, u8)> = roster
        .iter()
        .filter(|e| e.known_at < as_of && e.stage == Some(stage) && in_repo(&e.repo))
        .filter(|e| e.pr != pr)
        .map(|e| (e.pr, own_level(&e.star, as_of)))
        .collect();
    rows.push((pr, level));
    // The listing's order: newest first. One entry per PR (a duplicate
    // roster line is one listing row).
    rows.sort_by_key(|r| std::cmp::Reverse(r.0));
    rows.dedup_by_key(|r| r.0);
    let queue = ordered_queue(
        rows.iter().map(|&(n, l)| row(n, l)).collect(),
        PrRole::Judge,
        PREFER_HUMAN_PRS,
        &TrustPolicy::default(),
    );
    let ahead = queue
        .iter()
        .position(|r| r["number"].as_u64() == Some(u64::from(pr)))?;
    let floor = as_of - Duration::seconds(WINDOW_SEC);
    let ages: Vec<i64> = log
        .events
        .iter()
        .filter(|e| e.known_at < as_of && e.at < as_of && e.at >= floor)
        .filter(|e| e.stage == Some(stage) && in_repo(&e.repo))
        .map(|e| (as_of - e.at).num_seconds())
        .collect();
    let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    Some(PlannerPosition {
        role: PrRole::Judge.as_str().to_string(),
        items_ahead: count(ahead),
        queue_len: count(queue.len()),
        operator_level: level,
        prefer_human_prs: PREFER_HUMAN_PRS,
        drain_rate_per_hr: weighted_rate_per_hr(&ages, HALF_LIFE_SEC, WINDOW_SEC),
        exits: count(ages.len()),
    })
}
