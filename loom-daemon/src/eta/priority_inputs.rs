//! The **one** raw-input builder of the `eta-fit/v2` priority inputs
//! (#10508), called by the fit ([`super::fit::rows::build_with_context`])
//! and by serving ([`super::tracker::Tracker::priority_inputs_of`]) with the
//! same arguments, so the train/serve skew of #10500 cannot recur for these
//! inputs: there is no second definition to drift.
//!
//! # Inputs and their unknowns
//!
//! - **`starred_any` / `priority_level`**: the PR's own level (its labels,
//!   [`PriorityState`]) raised to at least the star while a linked issue is
//!   starred ([`super::star`], #10372). `linked` is `None` when the linked-star
//!   history does not cover the instant (training: the raw event cache;
//!   serving: no fresh star observation). Then a PR starred by its own labels
//!   is still known starred at its own level (a linked star can only raise an
//!   unstarred PR to 1), and any other PR is **unknown**, never unstarred.
//! - **`repo_rank`**: [`repo_rank`] in the fleet roster revision knowable
//!   before `as_of − LAG` ([`revision_at`], LAG = [`KNOWABLE_LAG_SEC`]).
//!   `None` when no revision is (no history, or it starts after the
//!   cutoff) or the repo is not a member then. Never today's `repos.yml`.
//! - **`ahead_dispatch_fleet`**: the other open PRs in the subject's stage,
//!   across every repo in `scope`, that cross-repo dispatch order puts first:
//!   [`keyed_cmp`] over [`ETA_FLEET_POSITION_KEYS`] (the real comparator
//!   minus `main_red_fix`), with each PR's `workspace_priority` from the same
//!   revision ([`RosterRevision::priority_of`]: a non-member gets the
//!   comparator's default). `None` when the revision is unknown, the subject's
//!   star is unknown (its own position is then uncertain), it has no PR or a
//!   pre-PR stage, or its repo is outside `scope`. The roster entries' own
//!   stars are taken as given: an entry whose linked star is unknown counts
//!   at its own level.
//!
//! Every input reads only roster entries with `known_at < as_of` and level
//! changes before `as_of` ([`PriorityState::known`]), as
//! [`super::priority_features`] does.
//!
//! Pure: reads its arguments and nothing else.

use super::fit::features_v2::PriorityInputs;
use super::fit::KNOWABLE_LAG_SEC;
use super::priority_features::{candidate, PriorityEntry, PriorityState, LEVEL_STARRED};
use super::queue_features::{is_pr_stage, QueueSubject};
use super::repo_priority::{repo_rank, revision_at, RosterRevision};
use super::star::LinkedStar;
use crate::work_finder::ready_queue::{keyed_cmp, ETA_FLEET_POSITION_KEYS};
use chrono::{DateTime, Duration, Utc};
use std::cmp::Ordering;

/// What the builder reads besides the subject.
#[derive(Debug, Clone, Copy)]
pub struct PriorityContext<'a> {
    /// Every open PR, each with its own star state and its linked star as
    /// known at the caller's cutoff (empty when unknown).
    pub roster: &'a [PriorityEntry],
    /// The repos (lowercased) whose PRs the roster lists completely.
    pub scope: &'a [String],
    /// The fleet roster's history, oldest first; `None` when the caller has
    /// none (every roster-derived input is then unknown).
    pub fleet_history: Option<&'a [RosterRevision]>,
}

/// `state` without its linked-issue star: the PR's own labels alone.
fn own_only(state: &PriorityState) -> PriorityState {
    PriorityState {
        linked_since: None,
        linked_changes: Vec::new(),
        ..state.clone()
    }
}

/// The v2 priority inputs of `subject` at `as_of` (see the module docs).
///
/// `own` is the subject's own-label star state (any linked star on it is
/// ignored); `linked` its linked-issue star as known at the caller's cutoff,
/// or `None` when unknown.
#[must_use]
pub fn priority_inputs(
    subject: &QueueSubject,
    own: &PriorityState,
    linked: Option<&LinkedStar>,
    ctx: &PriorityContext<'_>,
    as_of: DateTime<Utc>,
) -> PriorityInputs {
    let own = own_only(own).known(as_of);
    let state = linked.map(|l| own.clone().with_linked(l.clone()).known(as_of));
    let (starred_any, priority_level) = match &state {
        Some(s) => (Some(s.starred()), Some(s.level())),
        None if own.level() >= LEVEL_STARRED => (Some(true), Some(own.level())),
        None => (None, None),
    };
    let revision = ctx
        .fleet_history
        .and_then(|h| revision_at(h, as_of - Duration::seconds(KNOWABLE_LAG_SEC)));
    let mut out = PriorityInputs {
        starred_any,
        priority_level,
        repo_rank: revision.and_then(|r| repo_rank(r, &subject.repo)),
        ahead_dispatch_fleet: None,
    };
    let (Some(revision), Some(pr), Some((stage, entered_at))) =
        (revision, subject.pr, subject.current)
    else {
        return out;
    };
    let in_scope = |repo: &str| ctx.scope.iter().any(|r| r.eq_ignore_ascii_case(repo));
    if !is_pr_stage(stage) || !in_scope(&subject.repo) || priority_level.is_none() {
        return out;
    }
    // A known level with an unknown linked star is a PR starred by its own
    // labels: the linked star cannot change its level, only its starred-at.
    let me_state = state.unwrap_or(own);
    let mut me = candidate(pr, entered_at, &me_state);
    me.workspace_priority = revision.priority_of(&subject.repo);
    let ahead = ctx
        .roster
        .iter()
        .filter(|r| r.known_at < as_of && in_scope(&r.repo) && r.stage == Some(stage))
        .filter(|r| !(r.repo.eq_ignore_ascii_case(&subject.repo) && r.pr == pr))
        .filter(|r| {
            let mut other = candidate(r.pr, r.entered_at, &r.star.known(as_of));
            other.workspace_priority = revision.priority_of(&r.repo);
            keyed_cmp(&other, &me, &ETA_FLEET_POSITION_KEYS) == Ordering::Less
        })
        .count();
    out.ahead_dispatch_fleet = Some(u32::try_from(ahead).unwrap_or(u32::MAX));
    out
}
