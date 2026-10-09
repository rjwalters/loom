//! Historic repo priority (#10508): the fleet store's roster as it was
//! knowable at a cutoff, and a repo's normalized dispatch rank within it.
//!
//! The candidate input `repo_rank` of the `eta-fit/v2` feature set
//! ([`super::fit::features_v2`]) and the `workspace_priority` key of its
//! fleet-wide dispatch position ([`super::priority_inputs`]) both read a
//! [`RosterRevision`] picked by [`revision_at`]. Pure: no clock, no I/O.
//! The history reader that produces revisions from the fleet store's commit
//! log is a separate step; this module only defines what a revision means
//! and how one is chosen.
//!
//! # Knowability
//!
//! A commit's date is not proof its contents were observed then: a commit
//! can be backdated, or pushed long after it was authored. So a revision
//! carries both its `committed_at` and, when the history source recorded it,
//! the instant the store first **observed** it (`observed_at`, e.g. the
//! daemon's reload of the store). It is knowable from
//! [`RosterRevision::knowable_at`] = the later of the two. A revision with no
//! observation record falls back to its commit date and says so
//! ([`KnowBasis::CommitDate`]), so coverage reports can separate the two.
//!
//! [`revision_at`] returns the **last revision in history order** knowable
//! strictly before the cutoff. When none is (the history starts after the
//! cutoff, or there is no history), the answer is `None`: unknown. It never
//! falls back to today's roster.
//!
//! # Membership and rank
//!
//! A revision's members are the store's desired set at that revision
//! ([`crate::fleet_store::roster::Roster::desired`]: `fleet: true`, not
//! `firewall: true`), each with its `fleet_priority` (default
//! [`DEFAULT_WORKSPACE_PRIORITY`], the dispatch comparator's own default).
//! Lower priority dispatches first, so [`repo_rank`] maps the repo to
//! `[0, 1]` with **0 = dispatched first** and 1 = last:
//!
//! `rank = (members with a lower priority + ½ · other members with an equal
//! priority) / (members − 1)`, and `0` for a one-member fleet.
//!
//! Ties take the mid-rank, so tied repos get one value. The denominator is
//! the membership **at that revision**, not today's. A repo that is not a
//! member has no rank (`None`).

use crate::fleet_store::roster::Roster;
use crate::release_resolve::host::slug_from_remote_url;
use crate::workspace_registry::DEFAULT_WORKSPACE_PRIORITY;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One member of the fleet at a revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetMember {
    /// `owner/repo`, lowercased; `None` when its remote names no GitHub
    /// repo. It still counts toward the membership denominator.
    pub repo: Option<String>,
    /// Its `fleet_priority`, defaulted ([`DEFAULT_WORKSPACE_PRIORITY`]).
    pub priority: u32,
}

/// What a revision's knowable-at instant rests on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowBasis {
    /// The later of the commit date and a recorded observation.
    Observed,
    /// The commit date alone: no observation was recorded.
    CommitDate,
}

/// The fleet store's roster at one commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RosterRevision {
    /// The commit's date.
    pub committed_at: DateTime<Utc>,
    /// When the store first observed this commit, when recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<DateTime<Utc>>,
    /// The desired set at this commit, in file order.
    pub members: Vec<FleetMember>,
}

impl RosterRevision {
    /// A revision from a parsed roster: its desired set, slugs from each
    /// record's `remote`.
    #[must_use]
    pub fn from_roster(
        roster: &Roster,
        committed_at: DateTime<Utc>,
        observed_at: Option<DateTime<Utc>>,
    ) -> Self {
        let members = roster
            .records
            .iter()
            .filter(|r| r.fleet && !r.firewall)
            .map(|r| FleetMember {
                repo: r
                    .remote
                    .as_deref()
                    .and_then(slug_from_remote_url)
                    .map(|s| s.to_ascii_lowercase()),
                priority: r.fleet_priority.unwrap_or(DEFAULT_WORKSPACE_PRIORITY),
            })
            .collect();
        RosterRevision {
            committed_at,
            observed_at,
            members,
        }
    }

    /// When this revision becomes knowable: the later of its commit date
    /// and its observation, or the commit date when none was recorded.
    #[must_use]
    pub fn knowable_at(&self) -> DateTime<Utc> {
        self.observed_at
            .map_or(self.committed_at, |o| o.max(self.committed_at))
    }

    /// What [`Self::knowable_at`] rests on.
    #[must_use]
    pub fn basis(&self) -> KnowBasis {
        if self.observed_at.is_some() {
            KnowBasis::Observed
        } else {
            KnowBasis::CommitDate
        }
    }

    /// `repo`'s member record, matched case-insensitively.
    fn member(&self, repo: &str) -> Option<&FleetMember> {
        self.members.iter().find(|m| {
            m.repo
                .as_deref()
                .is_some_and(|r| r.eq_ignore_ascii_case(repo))
        })
    }

    /// `repo`'s dispatch priority at this revision: its `fleet_priority`,
    /// or [`DEFAULT_WORKSPACE_PRIORITY`] when it is not a member (what the
    /// dispatch comparator uses for an unconfigured workspace).
    #[must_use]
    pub fn priority_of(&self, repo: &str) -> u32 {
        self.member(repo)
            .map_or(DEFAULT_WORKSPACE_PRIORITY, |m| m.priority)
    }
}

/// The last revision in `history` (oldest first) knowable strictly before
/// `cutoff`; `None` when there is none (unknown, never today's file).
#[must_use]
pub fn revision_at(history: &[RosterRevision], cutoff: DateTime<Utc>) -> Option<&RosterRevision> {
    history.iter().rev().find(|r| r.knowable_at() < cutoff)
}

/// `repo`'s normalized dispatch rank at `revision` (see the module docs):
/// 0 dispatched first, 1 last, ties at the mid-rank. `None` when `repo` is
/// not a member.
#[must_use]
pub fn repo_rank(revision: &RosterRevision, repo: &str) -> Option<f64> {
    let me = revision.member(repo)?;
    let n = revision.members.len();
    if n <= 1 {
        return Some(0.0);
    }
    let lower = revision
        .members
        .iter()
        .filter(|m| m.priority < me.priority)
        .count();
    // Equal priority, minus the member itself.
    let equal = revision
        .members
        .iter()
        .filter(|m| m.priority == me.priority)
        .count()
        - 1;
    Some((lower as f64 + 0.5 * equal as f64) / (n - 1) as f64)
}
