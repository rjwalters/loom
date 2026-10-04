//! What an estimate records as `features` (#10201): the per-pass context the
//! tracker keeps, and the adapter from it to [`queue_features`].
//!
//! # The per-pass context
//!
//! One ETA pass (`observability::eta::record`) hands the tracker two views:
//!
//! - **The fleet view** ([`Tracker::on_fleet_context`]): every open PR of each
//!   repo whose review listings were read completely, including PRs that
//!   close no issue (they never become tracker items), plus the stage events
//!   derived from the ETA stage journal ([`events_from_journal`]). The repos
//!   are the fleet scope.
//! - **The dispatch plan** ([`Tracker::on_ready_queue`]): the host-level queue
//!   values `queue_ready`, `queue_running`, `max_concurrent`,
//!   `active_sweeps_host` and `repo_pr_open_skip`, now recorded on **every**
//!   item rather than only ready ones. `queue_rank` stays ready-only: it is a
//!   position in the ready queue.
//!
//! An estimate between two passes (a bus event) reads the last pass's
//! context, which was observed before its `as_of`. A view observed at or
//! after `as_of` is not used.
//!
//! # Serving-side approximations
//!
//! - A roster PR's `entered_at` is the tracker's own stage entry when it
//!   tracks the PR in the stage its labels show (for a held PR, `merge_hold`:
//!   its hold entry, #10284), otherwise the listing's `updated_at` (a lower
//!   bound). So `ahead` is approximate for first-seen PRs until exact entry
//!   times come from the label stream (#10218).
//! - A hold-aware model reads the **episode roster** captured beside the
//!   roster (#10312): a tracked `merge_wait` PR released from a hold enters
//!   at its release, as training's split episode does. A PR the tracker
//!   does not follow keeps the `updated_at` lower bound there too.
//! - The journal records only PRs this host tracks, which are the PRs that
//!   close an issue. Departures and merges of other PRs are not counted.
//! - The log's `from` is the journal's oldest row. Daemon downtime inside
//!   the journal's span is not visible.

use super::hold;
use super::ready::plan_max_age_secs;
use super::{EstimateContext, Item, ItemKey, ReadyPlan, ReadyRow, Tracker};
use crate::eta::explanation::{FeatureOmitted, Features};
use crate::eta::journal::JournalEntry;
use crate::eta::labels::stage_from_pr_labels;
use crate::eta::queue_features::{
    self, is_pr_stage, reason, EventKind, EventLog, QueueFeatures, QueueSubject, RosterEntry,
    StageEvent, SINCE_MERGE_CAP_SEC,
};
use crate::eta::{CurrentState, NoEstimateReason, Stage};
use crate::types::{PlanState, QueueDisposition};
use chrono::{DateTime, Datelike, Duration, Timelike, Utc};
use std::collections::{BTreeMap, BTreeSet};

/// Omission reason: no fleet view was observed before `as_of` (no pass yet).
pub const NOT_LISTED_YET: &str = "not_listed_yet";

/// The host-level plan features, in [`Features`] field order.
const PLAN_FEATURES: [&str; 5] = [
    "queue_ready",
    "queue_running",
    "max_concurrent",
    "active_sweeps_host",
    "repo_pr_open_skip",
];

/// One open PR from a review listing, whatever it closes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedPr {
    /// PR number.
    pub number: u32,
    /// Its labels.
    pub labels: Vec<String>,
    /// Last updated: a lower bound on the current stage's entry.
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
struct FleetView {
    roster: Vec<RosterEntry>,
    /// The roster as a hold-aware model's training sees it (#10312): a
    /// tracked released `merge_wait` PR enters at its release. Captured with
    /// `roster`, from the tracker's state at the same observation.
    episode_roster: Vec<RosterEntry>,
    events: EventLog,
    scope: Vec<String>,
    observed_at: DateTime<Utc>,
}

/// The host-level values of the last dispatch plan.
#[derive(Debug, Clone)]
pub(super) struct PlanView {
    at: DateTime<Utc>,
    max_age_secs: i64,
    queue_ready: u32,
    queue_running: u32,
    max_concurrent: u32,
    occupancy: Option<u32>,
    /// Repos (lowercased) with a `pr-open-skip` row on the plan.
    pr_open_skip: Vec<String>,
    /// Repos (lowercased) whose ready listing failed on the tick.
    unlisted: Vec<String>,
}

/// Whether `item` is only a ready (`loom:issue`) row: no running sweep, no
/// PR. Such an item gets `start` estimates and a `queue_rank`.
pub(super) fn ready_only(item: &Item) -> bool {
    item.in_ready_queue && !item.sweep_running && item.pr_number.is_none()
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

impl PlanView {
    /// The host-level values of `plan`, whose rows are `rows`.
    pub(super) fn of(rows: &[ReadyRow], plan: &ReadyPlan) -> Self {
        let slots = &plan.context.slots;
        let repos = |wanted: QueueDisposition| {
            let mut repos: Vec<String> = rows
                .iter()
                .filter(|r| r.disposition == wanted)
                .map(|r| r.repo.to_ascii_lowercase())
                .collect();
            repos.sort();
            repos.dedup();
            repos
        };
        PlanView {
            at: plan.at,
            max_age_secs: plan_max_age_secs(&plan.context),
            queue_ready: to_u32(rows.len()),
            queue_running: to_u32(
                rows.iter()
                    .filter(|r| r.plan.plan_state == PlanState::Running)
                    .count(),
            ),
            max_concurrent: to_u32(slots.max_concurrent),
            occupancy: slots.occupancy.map(to_u32),
            pr_open_skip: repos(QueueDisposition::OpenPr),
            unlisted: plan
                .listing_failed
                .iter()
                .map(|r| r.to_ascii_lowercase())
                .collect(),
        }
    }
}

/// What the last pass observed.
#[derive(Debug, Clone, Default)]
pub(super) struct PassContext {
    fleet: Option<FleetView>,
    pub(super) plan: Option<PlanView>,
}

fn omission(name: &str, why: &str) -> FeatureOmitted {
    FeatureOmitted {
        name: name.to_string(),
        reason: why.to_string(),
    }
}

/// The stage events in ETA stage-journal `rows`, each known at `known_at`
/// (the instant the pass read them).
///
/// - A row with a `stage` and a `left_at` is a departure from that stage, a
///   `pr.resolved` row for a PR closed unmerged included. Only PR stages are
///   kept.
/// - A `pr.resolved` row that is neither a close nor `open` (the tracker's
///   `merged`, or a backfilled row with no state) and a `sweep.phase`
///   `merge` row are merges. An `open` row (a held PR that left the listings
///   still open, #10218) is a departure only.
/// - A hold's end is counted once (#10218). A PR's `merge_hold` row carries
///   that departure, and the merge when there was one. A `merge_wait` row at
///   the same instant is the pooled track's shadow of the same end, so it is
///   skipped. A hold's entry (`merge_wait` → `merge_hold`) is a `merge_wait`
///   departure, and a release-then-merge keeps both rows (their instants
///   differ).
///
/// An event's `at` is the row's `left_at`. `observed_at` is no knowability
/// stamp: a `pr.resolved` row's `observed_at` is the merge instant even when
/// the read that found it came passes later.
///
/// Departures more than 24 h and merges more than 168 h before `known_at`
/// are dropped. No window reaches them at any `as_of` after `known_at`, and a
/// merge that old reads as the cap either way, because `from` (the oldest
/// row's instant) is at or before it.
#[must_use]
pub fn events_from_journal(rows: &[JournalEntry], known_at: DateTime<Utc>) -> EventLog {
    let pr_instant =
        |row: &JournalEntry| Some((row.repo.to_ascii_lowercase(), row.pr_number?, row.left_at?));
    let hold_ends: BTreeSet<_> = rows
        .iter()
        .filter(|row| row.stage == Some(Stage::MergeHold))
        .filter_map(pr_instant)
        .collect();
    let mut from: Option<DateTime<Utc>> = None;
    let mut events = Vec::new();
    for row in rows {
        let oldest = row
            .left_at
            .map_or(row.observed_at, |l| l.min(row.observed_at));
        from = Some(from.map_or(oldest, |f| f.min(oldest)));
        let Some(at) = row.left_at else {
            continue;
        };
        if row.stage == Some(Stage::MergeWait)
            && pr_instant(row).is_some_and(|end| hold_ends.contains(&end))
        {
            continue;
        }
        let merged = match row.event.as_str() {
            "pr.resolved" => !matches!(row.raw["state"].as_str(), Some("closed" | "open")),
            "sweep.phase" => row.raw["phase"] == "merge",
            _ => false,
        };
        let stage = row.stage.filter(|s| is_pr_stage(*s));
        let (kind, horizon_sec) = match (merged, stage) {
            (true, _) => (EventKind::Merge, SINCE_MERGE_CAP_SEC),
            (false, Some(_)) => (EventKind::Exit, 24 * 3600),
            (false, None) => continue,
        };
        if at < known_at - Duration::seconds(horizon_sec) {
            continue;
        }
        events.push(StageEvent {
            repo: row.repo.to_ascii_lowercase(),
            pr: row.pr_number,
            stage,
            kind,
            at,
            known_at,
        });
    }
    events.sort();
    events.dedup();
    EventLog { from, events }
}

impl Tracker {
    /// The fleet view one pass observed at `observed_at`: each completely
    /// listed repo's open review-label PRs (`listings`, by `owner/repo`), and
    /// the stage events. Replaces the previous pass's view.
    ///
    /// Call it after the pass's [`Tracker::on_listing`] calls, so a roster
    /// PR the tracker follows takes its stage entry from the tracker.
    pub fn on_fleet_context(
        &mut self,
        listings: &[(String, Vec<ListedPr>)],
        events: EventLog,
        observed_at: DateTime<Utc>,
    ) {
        let tracked: BTreeMap<(&str, u32), &Item> = self
            .items
            .iter()
            .filter_map(|(key, item)| Some(((key.repo.as_str(), item.pr_number?), item)))
            .collect();
        let mut roster = Vec::new();
        let mut episode_roster = Vec::new();
        let mut scope = Vec::new();
        for (repo, prs) in listings {
            let repo = repo.to_ascii_lowercase();
            for pr in prs {
                let stage = stage_from_pr_labels(&pr.labels).ok();
                let item = tracked.get(&(repo.as_str(), pr.number));
                let entry = |followed: Option<DateTime<Utc>>| RosterEntry {
                    repo: repo.clone(),
                    pr: pr.number,
                    stage,
                    entered_at: followed
                        .or(pr.updated_at)
                        .unwrap_or(observed_at)
                        .min(observed_at),
                    known_at: observed_at,
                };
                let followed = |episode: bool| {
                    let (item, stage) = (item?, stage?);
                    if episode {
                        hold::episode_roster_entry(item, stage)
                    } else {
                        hold::roster_entry(item, stage)
                    }
                };
                roster.push(entry(followed(false)));
                episode_roster.push(entry(followed(true)));
            }
            scope.push(repo);
        }
        self.context.fleet = Some(FleetView {
            roster,
            episode_roster,
            events,
            scope,
            observed_at,
        });
    }

    /// The queue features of `subject` at `as_of`, from the last fleet view
    /// observed before `as_of`.
    #[must_use]
    pub fn queue_features_at(&self, subject: &QueueSubject, as_of: DateTime<Utc>) -> QueueFeatures {
        self.queue_features_view(subject, as_of, false)
    }

    /// [`Self::queue_features_at`] for the described view (`episode` false)
    /// or the modeled one (`episode` true, #10312: the episode roster).
    fn queue_features_view(
        &self,
        subject: &QueueSubject,
        as_of: DateTime<Utc>,
        episode: bool,
    ) -> QueueFeatures {
        match &self.context.fleet {
            Some(view) if view.observed_at < as_of => queue_features::queue_features(
                subject,
                if episode {
                    &view.episode_roster
                } else {
                    &view.roster
                },
                &view.events,
                &view.scope,
                as_of,
            ),
            _ => QueueFeatures::unavailable(NOT_LISTED_YET),
        }
    }

    /// The host-level plan features for an item of `repo` at `now`.
    fn plan_features(
        &self,
        repo: &str,
        now: DateTime<Utc>,
        features: &mut Features,
        omitted: &mut Vec<FeatureOmitted>,
    ) {
        let plan = match &self.context.plan {
            Some(p) if p.at < now && (now - p.at).num_seconds() <= p.max_age_secs => p,
            other => {
                let why = match other {
                    Some(p) if p.at < now => NoEstimateReason::StaleInputs,
                    _ => NoEstimateReason::NoDispatchPlan,
                };
                omitted.extend(PLAN_FEATURES.iter().map(|n| omission(n, why.as_str())));
                return;
            }
        };
        features.queue_ready = Some(plan.queue_ready);
        features.queue_running = Some(plan.queue_running);
        features.max_concurrent = Some(plan.max_concurrent);
        features.active_sweeps_host = plan.occupancy;
        if plan.occupancy.is_none() {
            omitted.push(omission("active_sweeps_host", NoEstimateReason::NoDispatchPlan.as_str()));
        }
        if plan.unlisted.iter().any(|r| r == repo) {
            omitted.push(omission("repo_pr_open_skip", reason::REPO_NOT_LISTED));
        } else {
            features.repo_pr_open_skip = Some(plan.pr_open_skip.iter().any(|r| r == repo));
        }
    }

    /// The features an estimate of `item` in `current` at `now` records,
    /// friction included, and why each null one is null: `input_for`'s
    /// described view and `tracker_hold.rs`'s modeled one (#10284).
    pub(super) fn recorded_features(
        &self,
        key: &ItemKey,
        item: &Item,
        current: &CurrentState,
        ctx: &EstimateContext<'_>,
        now: DateTime<Utc>,
        episode: bool,
    ) -> (Features, Vec<FeatureOmitted>) {
        let (mut features, mut omitted) = self.features_for(key, item, current, ctx, now, episode);
        let labels = (!item.labels.is_empty()).then_some(item.labels.as_slice());
        self.friction
            .apply(&item.repo, item.pr_number, labels, now, &mut features, &mut omitted);
        (features, omitted)
    }

    /// [`Self::recorded_features`] before friction.
    fn features_for(
        &self,
        key: &ItemKey,
        item: &Item,
        current: &CurrentState,
        ctx: &EstimateContext<'_>,
        now: DateTime<Utc>,
        episode: bool,
    ) -> (Features, Vec<FeatureOmitted>) {
        let ready_only = ready_only(item);
        let mut features = Features {
            labels: (!item.labels.is_empty()).then(|| item.labels.clone()),
            doctor_cycles_so_far: Some(item.rework_rounds),
            sweep_internal: Some(item.sweep_running),
            pr_created_at: item.pr_created_at,
            hour_utc: Some(now.hour()),
            weekday_utc: Some(now.weekday().num_days_from_monday()),
            host_id: ctx.host_id.map(str::to_string),
            queue_rank: item
                .ready
                .as_ref()
                .map(|r| r.position)
                .filter(|_| ready_only),
            ..Features::default()
        };
        let mut omitted = Vec::new();
        if item.labels.is_empty() {
            omitted.push(omission("labels", NOT_LISTED_YET));
        }
        if features.queue_rank.is_none() {
            let why = if ready_only {
                item.refused
                    .unwrap_or(NoEstimateReason::NoDispatchPlan)
                    .as_str()
            } else {
                reason::NOT_APPLICABLE_STAGE
            };
            omitted.push(omission("queue_rank", why));
        }
        self.plan_features(&key.repo, now, &mut features, &mut omitted);
        let subject = QueueSubject {
            repo: key.repo.clone(),
            pr: item.pr_number,
            current: match current {
                CurrentState::At(stage) => stage.entered_at.map(|at| {
                    (
                        stage.stage,
                        if episode {
                            stage.episode_entered_at.unwrap_or(at)
                        } else {
                            at
                        },
                    )
                }),
                CurrentState::Refused(_) => None,
            },
        };
        self.queue_features_view(&subject, now, episode)
            .write_to(&mut features, &mut omitted);
        self.item_features(key, item, ctx.history, now, &mut features, &mut omitted);
        (features, omitted)
    }
}
