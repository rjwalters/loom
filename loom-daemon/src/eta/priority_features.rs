//! Priority-aware queue features (#10333): **one** definition for serving and
//! the fit, kept apart from [`super::queue_features`] on purpose.
//!
//! # Not enabled for twin-otter
//!
//! These are the candidate inputs of the *next* twin-otter version
//! ([`PRIORITY_FEATURES`]). The fit's feature schema (`eta-fit/v1`,
//! [`super::fit::FEATURES`]) does not list them, they are not written to
//! [`super::explanation::Features`], and no shipped heuristic reads them, so
//! every current `ahead`, coefficient file, explanation and fixture is
//! unchanged. FIFO `ahead` keeps its meaning;
//! [`PriorityFeatures::ahead_dispatch`] is its dispatch-ordered sibling. A
//! new datestamped heuristic opts in by adding them to its own schema.
//!
//! # One ordering
//!
//! Queue position goes through the work finder's own comparator keys
//! ([`crate::work_finder::ready_queue::keyed_cmp`] over `candidate_keys`):
//! each PR is mapped to a [`PriorityCandidate`] and compared on
//! [`ETA_POSITION_KEYS`] — level, star bucket, starred-at, age, number. It
//! ignores [`crate::work_finder::ready_queue::ETA_IGNORED_KEYS`]:
//! `main_red_fix` (a per-tick red-main verdict with no point-in-time record)
//! and `workspace_priority` (constant inside one repo; the position counts
//! same-repo PRs only). `complexity` is not an ordering key at all.
//!
//! # Issue vs PR
//!
//! The work finder orders issues; the roster holds PRs. A PR's priority is
//! the level its own labels carry ([`crate::operator_levels::level`]: the
//! operator labels or their `*-inherited` twins), raised to at least the star
//! while an issue it links is starred ([`PriorityState::linked_since`], from
//! the one PR-or-linked-issue star rule of [`super::star`], #10372): the
//! operator usually stars the issue, not the PR. The linked star's on/off
//! instants ([`PriorityState::linked_changes`]) are kept too, so a linked
//! star that ended still counts for `star_changed_in_stage`. A PR's age key is its stage
//! entry, the analogue of `createdAt`.
//!
//! # Levels
//!
//! [`PriorityState::level`] is the effective operator priority level (#10307):
//! 0 none, 1 the star, 2 `loom:operator-high-priority`. Absent means 0. The
//! fit reads it from the flag timeline ([`super::labels::level_from_flags`]);
//! a level 3 is one more flag bit. A new level changes no existing column:
//! `priority_level` is already a column, the ordering already leads with the
//! level key, and the counts read only "starred or not".
//!
//! # Knowability
//!
//! As [`super::queue_features`]: roster entries with `known_at >= as_of` are
//! ignored, and a level change at or after `as_of` is not applied. The fit
//! builds each state from the flag timeline strictly before `t - lag`
//! ([`PriorityState::from_flags`]); serving from its own listing passes, each
//! observed before `as_of`.
//!
//! Pure: reads its arguments and nothing else.

use super::flag_timeline::FlagChange;
use super::labels::{level_from_flags, pr_flags};
use super::queue_features::{is_pr_stage, QueueSubject};
use super::star::LinkedStar;
use super::Stage;
use crate::work_finder::ready_queue::{keyed_cmp, ETA_POSITION_KEYS};
use crate::work_finder::PriorityCandidate;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

/// The level of a plain star.
pub const LEVEL_STARRED: u8 = 1;

/// The candidate feature names, in [`PriorityFeatures`] field order. None of
/// them is in the fit's [`super::fit::FEATURES`] (pinned by a test).
pub const PRIORITY_FEATURES: [&str; 7] = [
    "ahead_dispatch",
    "ahead_starred",
    "n_starred_repo",
    "n_starred_fleet",
    "starred_age_sec",
    "star_changed_in_stage",
    "priority_level",
];

/// One level change: the instant, and the effective level from then.
pub type LevelChange = (DateTime<Utc>, u8);

/// An item's priority level, with what is known of its timeline.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriorityState {
    /// The level in force before the first recorded change: 0 for a fit
    /// timeline (which starts unstarred), the observed level for a PR serving
    /// first saw with no known history.
    pub initial_level: u8,
    /// Each instant its own effective level changed and the level from then,
    /// ascending.
    pub changes: Vec<LevelChange>,
    /// The start of a linked issue's current star run (#10372), when one is
    /// starred, as known at the caller's cutoff. It makes the PR at least
    /// starred.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linked_since: Option<DateTime<Utc>>,
    /// Each instant the linked-issue star turned on or off, ascending, as
    /// known at the caller's cutoff ([`LinkedStar::changes`]). It outlives
    /// an unstar, which empties `linked_since`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub linked_changes: Vec<DateTime<Utc>>,
}

impl PriorityState {
    /// A state at `level` with no known history.
    #[must_use]
    pub fn at_level(level: u8) -> Self {
        PriorityState {
            initial_level: level,
            ..PriorityState::default()
        }
    }

    /// The state from a PR's flag timeline ([`level_from_flags`]), reading
    /// only changes strictly before `cutoff`. `None` when no change is that
    /// old (no timeline).
    ///
    /// The timeline's first entry (the PR's first label event) is a change
    /// when it carries a level. A retention prune
    /// ([`super::flag_timeline::prune`]) can move an old star's instant up to
    /// the last entry kept before the floor: `starred_at` is then a later
    /// bound.
    #[must_use]
    pub fn from_flags(changes: &[FlagChange], cutoff: DateTime<Utc>) -> Option<Self> {
        let mut known: Vec<&FlagChange> = changes.iter().filter(|c| c.at < cutoff).collect();
        known.sort_by_key(|c| (c.at, c.flags));
        known.last()?;
        let mut out = PriorityState::default();
        for change in known {
            out.push(change.at, level_from_flags(change.flags));
        }
        Some(out)
    }

    /// A state from a label listing alone: the level, with no timeline.
    #[must_use]
    pub fn from_labels(labels: &[String]) -> Self {
        Self::at_level(level_from_flags(pr_flags(labels)))
    }

    /// Serving: the state after one more listing pass at `observed_at` that
    /// shows `labels`. `first_sighting` is a PR new to a repo the tracker had
    /// already listed, so a level it carries was set about now; a PR seen on
    /// the tracker's first pass of its repo has an unknown star instant.
    #[must_use]
    pub fn observe(
        prev: Option<&PriorityState>,
        labels: &[String],
        observed_at: DateTime<Utc>,
        first_sighting: bool,
    ) -> PriorityState {
        let level = level_from_flags(pr_flags(labels));
        let mut out = match prev {
            Some(prev) => prev.clone(),
            None if first_sighting => PriorityState::default(),
            None => return Self::at_level(level),
        };
        out.push(observed_at, level);
        out
    }

    /// This state with its linked issues' star (#10372): the current run's
    /// start and every on/off instant.
    #[must_use]
    pub fn with_linked(mut self, linked: LinkedStar) -> Self {
        self.linked_since = linked.since;
        self.linked_changes = linked.changes;
        self
    }

    /// Records `level` from `at` when it differs from the PR's own one.
    fn push(&mut self, at: DateTime<Utc>, level: u8) {
        if level != self.own_level() {
            self.changes.push((at, level));
        }
    }

    /// The level the PR's own labels carry now.
    fn own_level(&self) -> u8 {
        self.changes.last().map_or(self.initial_level, |c| c.1)
    }

    /// The effective level now: its own, and at least the star while a
    /// linked issue is starred.
    #[must_use]
    pub fn level(&self) -> u8 {
        let linked = if self.linked_since.is_some() {
            LEVEL_STARRED
        } else {
            0
        };
        self.own_level().max(linked)
    }

    /// Whether the item is starred (at any level) now.
    #[must_use]
    pub fn starred(&self) -> bool {
        self.level() >= LEVEL_STARRED
    }

    /// When its current level was set, when starred and known: its own
    /// level's instant, or the earlier of that and the linked star when the
    /// own level is no more than a star. A starred item with none orders by
    /// age, as the dispatch ordering falls back to `createdAt`.
    #[must_use]
    pub fn starred_at(&self) -> Option<DateTime<Utc>> {
        let own = (self.own_level() >= LEVEL_STARRED)
            .then(|| self.changes.last().map(|c| c.0))
            .flatten();
        if self.own_level() > LEVEL_STARRED {
            return own;
        }
        [own, self.linked_since].into_iter().flatten().min()
    }

    /// Whether its level changed after `entered_at`: its own, or a linked
    /// star that began or ended then (one that ended is gone from
    /// `linked_since` but not from `linked_changes`).
    fn changed_after(&self, entered_at: DateTime<Utc>) -> bool {
        self.changes.iter().any(|c| c.0 > entered_at)
            || self.linked_since.is_some_and(|at| at > entered_at)
            || self.linked_changes.iter().any(|at| *at > entered_at)
    }

    /// This state as knowable strictly before `as_of`.
    #[must_use]
    pub fn known(&self, as_of: DateTime<Utc>) -> PriorityState {
        PriorityState {
            initial_level: self.initial_level,
            changes: self
                .changes
                .iter()
                .copied()
                .filter(|c| c.0 < as_of)
                .collect(),
            linked_since: self.linked_since.filter(|at| *at < as_of),
            linked_changes: self
                .linked_changes
                .iter()
                .copied()
                .filter(|at| *at < as_of)
                .collect(),
        }
    }
}

/// One open PR with its star state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriorityEntry {
    /// `owner/repo`.
    pub repo: String,
    /// PR number.
    pub pr: u32,
    /// Its stage; `None` when its labels resolve to none.
    pub stage: Option<Stage>,
    /// When it entered that stage.
    pub entered_at: DateTime<Utc>,
    /// When this entry was observed.
    pub known_at: DateTime<Utc>,
    /// Its priority.
    pub star: PriorityState,
}

/// The new features. A `None` is not computable for the subject (no PR, a
/// pre-PR stage, or its repo outside the scope); `priority_level` is always
/// present and 0 when unknown.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriorityFeatures {
    /// Other open PRs in the repo and stage that dispatch order puts before
    /// the subject (the dispatch-ordered sibling of FIFO `ahead`).
    pub ahead_dispatch: Option<u32>,
    /// Of those, the starred ones (any level).
    pub ahead_starred: Option<u32>,
    /// Other starred open PRs in the repo and stage.
    pub n_starred_repo: Option<u32>,
    /// Other starred open PRs in the stage, fleet scope.
    pub n_starred_fleet: Option<u32>,
    /// Seconds since the subject's current level was set; `None` when
    /// unstarred or the instant is unknown.
    pub starred_age_sec: Option<i64>,
    /// Whether its level changed after it entered its stage.
    pub star_changed_in_stage: Option<bool>,
    /// Its effective priority level at `as_of`.
    pub priority_level: u8,
}

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// The dispatch candidate a PR stands for: its level, star, starred-at, and
/// stage entry as `createdAt`.
#[must_use]
pub fn candidate(pr: u32, entered_at: DateTime<Utc>, star: &PriorityState) -> PriorityCandidate {
    PriorityCandidate {
        operator_level: star.level(),
        operator_priority: star.starred(),
        operator_priority_at: star.starred_at().map(stamp),
        created_at: Some(stamp(entered_at)),
        number: pr,
        ..PriorityCandidate::default()
    }
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The priority features of `subject` (with `star`) at `as_of`.
///
/// `roster` and `scope` are as for [`super::queue_features::queue_features`].
#[must_use]
pub fn priority_features(
    subject: &QueueSubject,
    star: &PriorityState,
    roster: &[PriorityEntry],
    scope: &[String],
    as_of: DateTime<Utc>,
) -> PriorityFeatures {
    let star = star.known(as_of);
    let mut out = PriorityFeatures {
        priority_level: star.level(),
        ..PriorityFeatures::default()
    };
    let (Some((stage, entered_at)), Some(pr)) = (subject.current, subject.pr) else {
        return out;
    };
    if !is_pr_stage(stage) {
        return out;
    }
    out.star_changed_in_stage = Some(star.changed_after(entered_at));
    out.starred_age_sec = star
        .starred_at()
        .map(|at| (as_of - at).num_seconds().max(0));
    let in_scope = |repo: &str| scope.iter().any(|r| r.eq_ignore_ascii_case(repo));
    let in_repo = |repo: &str| repo.eq_ignore_ascii_case(&subject.repo);
    let me = candidate(pr, entered_at, &star);
    let others: Vec<(PriorityCandidate, &str)> = roster
        .iter()
        .filter(|r| r.known_at < as_of && in_scope(&r.repo) && r.stage == Some(stage))
        .filter(|r| !(in_repo(&r.repo) && r.pr == pr))
        .map(|r| {
            let s = r.star.known(as_of);
            (candidate(r.pr, r.entered_at, &s), r.repo.as_str())
        })
        .collect();
    let starred = |o: &&(PriorityCandidate, &str)| o.0.operator_priority;
    if !scope.is_empty() {
        out.n_starred_fleet = Some(count(others.iter().filter(starred).count()));
    }
    if in_scope(&subject.repo) {
        let same = || others.iter().filter(|o| in_repo(o.1));
        out.n_starred_repo = Some(count(same().filter(starred).count()));
        let ahead =
            || same().filter(|o| keyed_cmp(&o.0, &me, &ETA_POSITION_KEYS) == Ordering::Less);
        out.ahead_dispatch = Some(count(ahead().count()));
        out.ahead_starred = Some(count(ahead().filter(starred).count()));
    }
    out
}
