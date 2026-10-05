//! Queue, drain and friction features (#10201): **one** definition, shared by
//! serving and training so the two cannot drift.
//!
//! - **Serving**: the tracker calls [`queue_features`] for every estimate,
//!   from the last pass's review listings and the ETA stage journal
//!   (`tracker_features.rs`).
//! - **Training**: `eta fit` (#10221) calls the same function at each row's
//!   instant, from the webhook label stream.
//!
//! The contract table, the fleet-scope definition and the knowability rule
//! are documented in `defaults/docs/eta.md` → "Features".
//!
//! # Values are raw
//!
//! Counts, and integer seconds (the explanation's "seconds are integers"
//! rule). `log1p`, standardisation and `hour_sin`/`hour_cos` belong to the
//! model. The one exception is the 168 h cap on [`QueueFeatures::since_merge_sec`],
//! which is part of the definition, so the stored value is capped.
//!
//! # Knowability
//!
//! Every roster entry and event carries `known_at`. A feature at `as_of`
//! reads only entries with `known_at < as_of`, the strict-before rule of
//! `StageSamples::select`. An event also needs its own `at < as_of`. Adding
//! or changing anything known at or after `as_of` cannot change the result.
//!
//! # Purity
//!
//! [`queue_features`] reads its five arguments and nothing else: no clock,
//! no filesystem, no environment, no globals. A source scan in
//! `tests/queue_features.rs` keeps it that way.

use super::explanation::{FeatureOmitted, Features};
use super::Stage;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// The cap on [`QueueFeatures::since_merge_sec`]: 168 h.
pub const SINCE_MERGE_CAP_SEC: i64 = 7 * 24 * 3600;

/// Omission reasons this module assigns (`features_omitted[].reason`).
pub mod reason {
    /// The item is refused, so it has no current stage.
    pub const NO_STAGE: &str = "no_stage";
    /// A PR-stage feature, but the item has no PR yet.
    pub const NO_PR_YET: &str = "no_pr_yet";
    /// A PR-stage feature, but the item is in a pre-PR stage.
    pub const NOT_APPLICABLE_STAGE: &str = "not_applicable_stage";
    /// No merge is known in the repo, and the event history is shorter than
    /// [`super::SINCE_MERGE_CAP_SEC`], so the cap value is not justified.
    pub const HISTORY_SHORTER_THAN_CAP: &str = "history_shorter_than_cap";
    /// The item's repo is outside the fleet scope: its review listings were
    /// not read completely.
    pub const REPO_NOT_LISTED: &str = "repo_not_listed";
}

/// The PR-stage features, in [`Features`] field order.
pub const PR_STAGE_FEATURES: [&str; 9] = [
    "ahead",
    "n_stage_repo",
    "n_stage_fleet",
    "exits_repo_1h",
    "exits_repo_6h",
    "exits_repo_24h",
    "exits_fleet_1h",
    "exits_fleet_6h",
    "exits_fleet_24h",
];

/// Every feature [`queue_features`] produces, in [`Features`] field order.
pub const NAMES: [&str; 14] = [
    "ahead",
    "n_stage_repo",
    "n_stage_fleet",
    "exits_repo_1h",
    "exits_repo_6h",
    "exits_repo_24h",
    "exits_fleet_1h",
    "exits_fleet_6h",
    "exits_fleet_24h",
    "merges_repo_24h",
    "merges_fleet_6h",
    "since_merge_sec",
    "open_prs_repo",
    "fleet_scope_repos",
];

/// Whether `stage` is a PR stage: one an open PR sits in after the Builder.
///
/// Written as "not a pre-PR stage", so a new PR stage (`merge_hold`, #10218)
/// counts without an edit here.
#[must_use]
pub fn is_pr_stage(stage: Stage) -> bool {
    !matches!(stage, Stage::ReadyWait | Stage::SweepCurator | Stage::SweepBuilder)
}

/// The item the features describe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueSubject {
    /// `owner/repo` (compared case-insensitively).
    pub repo: String,
    /// Its PR, once one exists.
    pub pr: Option<u32>,
    /// Its current stage and when it entered it. `None` when refused.
    pub current: Option<(Stage, DateTime<Utc>)>,
}

/// One open PR under a review label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RosterEntry {
    /// `owner/repo`.
    pub repo: String,
    /// PR number.
    pub pr: u32,
    /// Its stage from its labels (an operator-held approved PR is
    /// `merge_hold`, #10218). `None` when the labels resolve to no stage (a
    /// refusal such as `loom:blocked`).
    pub stage: Option<Stage>,
    /// When it entered that stage (a lower bound for a first-seen PR).
    pub entered_at: DateTime<Utc>,
    /// When this entry was observed.
    pub known_at: DateTime<Utc>,
}

/// What a [`StageEvent`] records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// The PR left `stage` (any destination, including closed unmerged).
    Exit,
    /// The PR merged. With a `stage`, it is also a departure from it.
    Merge,
}

/// One stage departure or merge.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct StageEvent {
    /// `owner/repo`.
    pub repo: String,
    /// The PR, when known.
    pub pr: Option<u32>,
    /// The stage left, when known.
    pub stage: Option<Stage>,
    /// Exit or merge.
    pub kind: EventKind,
    /// When it happened.
    pub at: DateTime<Utc>,
    /// When it became known.
    pub known_at: DateTime<Utc>,
}

/// The events, and how far back they are complete.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventLog {
    /// The log has every event from this instant on. `None`: unknown.
    pub from: Option<DateTime<Utc>>,
    /// The events, in any order.
    pub events: Vec<StageEvent>,
}

/// The computed features. Each `None` has an entry in `omitted`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueFeatures {
    /// Other open PRs in the repo and stage that entered it earlier (ties:
    /// lower PR number first).
    pub ahead: Option<u32>,
    /// Other open PRs in the repo and stage.
    pub n_stage_repo: Option<u32>,
    /// Other open PRs in the stage, fleet scope.
    pub n_stage_fleet: Option<u32>,
    /// Departures from the stage in the repo, last hour.
    pub exits_repo_1h: Option<u32>,
    /// Same, last 6 h.
    pub exits_repo_6h: Option<u32>,
    /// Same, last 24 h.
    pub exits_repo_24h: Option<u32>,
    /// Departures from the stage, fleet scope, last hour.
    pub exits_fleet_1h: Option<u32>,
    /// Same, last 6 h.
    pub exits_fleet_6h: Option<u32>,
    /// Same, last 24 h.
    pub exits_fleet_24h: Option<u32>,
    /// PR merges in the repo, last 24 h.
    pub merges_repo_24h: Option<u32>,
    /// PR merges, fleet scope, last 6 h.
    pub merges_fleet_6h: Option<u32>,
    /// Seconds since the repo's last merge, capped at [`SINCE_MERGE_CAP_SEC`].
    pub since_merge_sec: Option<i64>,
    /// Open PRs under any review label in the repo, the subject included.
    pub open_prs_repo: Option<u32>,
    /// Repos the fleet-scope values cover.
    pub fleet_scope_repos: Option<u32>,
    /// Why each `None` above is null.
    pub omitted: Vec<FeatureOmitted>,
}

impl QueueFeatures {
    /// Every feature null, each for `why`.
    #[must_use]
    pub fn unavailable(why: &str) -> Self {
        let mut out = QueueFeatures::default();
        omit(&mut out.omitted, &NAMES, why);
        out
    }

    /// Copy the values onto `features` and the reasons onto `omitted`.
    pub fn write_to(&self, features: &mut Features, omitted: &mut Vec<FeatureOmitted>) {
        features.ahead = self.ahead;
        features.n_stage_repo = self.n_stage_repo;
        features.n_stage_fleet = self.n_stage_fleet;
        features.exits_repo_1h = self.exits_repo_1h;
        features.exits_repo_6h = self.exits_repo_6h;
        features.exits_repo_24h = self.exits_repo_24h;
        features.exits_fleet_1h = self.exits_fleet_1h;
        features.exits_fleet_6h = self.exits_fleet_6h;
        features.exits_fleet_24h = self.exits_fleet_24h;
        features.merges_repo_24h = self.merges_repo_24h;
        features.merges_fleet_6h = self.merges_fleet_6h;
        features.since_merge_sec = self.since_merge_sec;
        features.open_prs_repo = self.open_prs_repo;
        features.fleet_scope_repos = self.fleet_scope_repos;
        omitted.extend(self.omitted.iter().cloned());
    }
}

fn omit(omitted: &mut Vec<FeatureOmitted>, names: &[&str], why: &str) {
    for name in names {
        omitted.push(FeatureOmitted {
            name: (*name).to_string(),
            reason: why.to_string(),
        });
    }
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The subject's PR stage, entry instant and PR, or why it has none. The
/// order is fixed: `no_stage`, then `no_pr_yet`, then `not_applicable_stage`.
fn pr_position(subject: &QueueSubject) -> Result<(Stage, DateTime<Utc>, u32), &'static str> {
    let Some((stage, entered_at)) = subject.current else {
        return Err(reason::NO_STAGE);
    };
    let Some(pr) = subject.pr else {
        return Err(reason::NO_PR_YET);
    };
    if !is_pr_stage(stage) {
        return Err(reason::NOT_APPLICABLE_STAGE);
    }
    Ok((stage, entered_at, pr))
}

/// The queue, drain and friction features of `subject` at `as_of`.
///
/// - `roster`: open PRs under a review label.
/// - `log`: stage departures and merges.
/// - `scope`: the repos whose roster and events are complete (the fleet
///   scope). Entries of other repos are ignored. Its distinct count is
///   [`QueueFeatures::fleet_scope_repos`].
///
/// Pure: reads its arguments and nothing else.
#[must_use]
pub fn queue_features(
    subject: &QueueSubject,
    roster: &[RosterEntry],
    log: &EventLog,
    scope: &[String],
    as_of: DateTime<Utc>,
) -> QueueFeatures {
    let in_scope = |repo: &str| scope.iter().any(|r| r.eq_ignore_ascii_case(repo));
    let in_repo = |repo: &str| repo.eq_ignore_ascii_case(&subject.repo);
    let roster: Vec<&RosterEntry> = roster
        .iter()
        .filter(|r| r.known_at < as_of && in_scope(&r.repo))
        .collect();
    let events: Vec<&StageEvent> = log
        .events
        .iter()
        .filter(|e| e.known_at < as_of && e.at < as_of && in_scope(&e.repo))
        .collect();
    let since = |hours: i64| as_of - Duration::hours(hours);

    let mut scope_repos: Vec<String> = scope.iter().map(|r| r.to_ascii_lowercase()).collect();
    scope_repos.sort();
    scope_repos.dedup();
    let fleet_listed = !scope_repos.is_empty();
    let repo_listed = in_scope(&subject.repo);

    let mut out = QueueFeatures {
        fleet_scope_repos: Some(count(scope_repos.len())),
        ..QueueFeatures::default()
    };
    let merges = |repo_only: bool, hours: i64| {
        count(
            events
                .iter()
                .filter(|e| e.kind == EventKind::Merge && e.at >= since(hours))
                .filter(|e| !repo_only || in_repo(&e.repo))
                .count(),
        )
    };

    // Fleet scope.
    if fleet_listed {
        out.merges_fleet_6h = Some(merges(false, 6));
    } else {
        omit(&mut out.omitted, &["merges_fleet_6h"], reason::REPO_NOT_LISTED);
    }

    // The subject's repo.
    if repo_listed {
        out.merges_repo_24h = Some(merges(true, 24));
        out.open_prs_repo = Some(count(roster.iter().filter(|r| in_repo(&r.repo)).count()));
        let last_merge = events
            .iter()
            .filter(|e| e.kind == EventKind::Merge && in_repo(&e.repo))
            .map(|e| e.at)
            .max();
        out.since_merge_sec = match last_merge {
            Some(at) => Some((as_of - at).num_seconds().clamp(0, SINCE_MERGE_CAP_SEC)),
            None if log
                .from
                .is_some_and(|from| (as_of - from).num_seconds() >= SINCE_MERGE_CAP_SEC) =>
            {
                Some(SINCE_MERGE_CAP_SEC)
            }
            None => None,
        };
        if out.since_merge_sec.is_none() {
            omit(&mut out.omitted, &["since_merge_sec"], reason::HISTORY_SHORTER_THAN_CAP);
        }
    } else {
        omit(
            &mut out.omitted,
            &["merges_repo_24h", "since_merge_sec", "open_prs_repo"],
            reason::REPO_NOT_LISTED,
        );
    }

    // PR-stage features. The item's own reason comes first: it describes
    // the item, where `repo_not_listed` describes the pass.
    match pr_position(subject) {
        Err(why) => omit(&mut out.omitted, &PR_STAGE_FEATURES, why),
        Ok((stage, entered_at, pr)) => {
            let others: Vec<&RosterEntry> = roster
                .iter()
                .copied()
                .filter(|r| r.stage == Some(stage) && !(in_repo(&r.repo) && r.pr == pr))
                .collect();
            let exits = |repo_only: bool, hours: i64| {
                Some(count(
                    events
                        .iter()
                        .filter(|e| e.stage == Some(stage) && e.at >= since(hours))
                        .filter(|e| !repo_only || in_repo(&e.repo))
                        .count(),
                ))
            };
            if fleet_listed {
                out.n_stage_fleet = Some(count(others.len()));
                out.exits_fleet_1h = exits(false, 1);
                out.exits_fleet_6h = exits(false, 6);
                out.exits_fleet_24h = exits(false, 24);
            } else {
                omit(
                    &mut out.omitted,
                    &[
                        "n_stage_fleet",
                        "exits_fleet_1h",
                        "exits_fleet_6h",
                        "exits_fleet_24h",
                    ],
                    reason::REPO_NOT_LISTED,
                );
            }
            if repo_listed {
                let same_repo = || others.iter().filter(|r| in_repo(&r.repo));
                out.n_stage_repo = Some(count(same_repo().count()));
                out.ahead = Some(count(
                    same_repo()
                        .filter(|r| (r.entered_at, r.pr) < (entered_at, pr))
                        .count(),
                ));
                out.exits_repo_1h = exits(true, 1);
                out.exits_repo_6h = exits(true, 6);
                out.exits_repo_24h = exits(true, 24);
            } else {
                omit(
                    &mut out.omitted,
                    &[
                        "ahead",
                        "n_stage_repo",
                        "exits_repo_1h",
                        "exits_repo_6h",
                        "exits_repo_24h",
                    ],
                    reason::REPO_NOT_LISTED,
                );
            }
        }
    }
    // `omitted` in field order, whichever branch named each null.
    out.omitted
        .sort_by_key(|o| NAMES.iter().position(|n| *n == o.name));
    out
}
