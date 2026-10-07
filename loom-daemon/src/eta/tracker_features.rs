//! What an estimate records as `features` (#10201): the per-pass context the
//! tracker keeps, and the adapter from it to [`queue_features`].
//!
//! # The per-pass context
//!
//! One ETA pass (`observability::eta::record`) hands the tracker two views:
//!
//! - **The fleet view** ([`Tracker::on_fleet_context`]): every open PR of each
//!   repo whose review listings were read completely, including PRs that
//!   close no issue (they never become tracker items), plus the stage events:
//!   the fleet snapshots' log (`fleet_log::SnapshotLog`, the one `eta fit`
//!   counts over, #10500) before its horizon, the ETA stage journal's
//!   ([`events_from_journal`]) after it and for any repo no snapshot covers.
//!   The repos are the fleet scope.
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
//!   its hold entry, #10284), otherwise its entry in the fleet snapshots'
//!   label timeline ([`Tracker::on_fleet_snapshots`], #10500: the same
//!   episodes `eta fit` trains on), otherwise the listing's `updated_at` (a
//!   lower bound). A tracked PR is dated, and its rework counted, from the
//!   same timeline on every pass (`Tracker::model_view`), unless the tracker
//!   observed its entry after the snapshot was cut. Only a PR the timeline
//!   cannot date (not yet in a snapshot, or re-entered the stage since the
//!   cut) keeps the bound.
//! - A hold-aware model reads the **episode roster** captured beside the
//!   roster (#10312): a tracked `merge_wait` PR released from a hold enters
//!   at its release, as training's split episode does. A PR the tracker
//!   does not follow enters at its split episode's entry in the timeline,
//!   else the `updated_at` lower bound.
//! - Events come from the snapshots up to their horizon (one refresh
//!   interval behind at most). After it, and with no snapshot, they come from
//!   the journal, which records only PRs this host tracks (the PRs that close
//!   an issue): departures and merges of other PRs there are not counted.
//! - The log's `from` is the snapshots' (as training's), else the journal's
//!   oldest row. Daemon downtime inside the journal's span is not visible.
//! - Priority levels (#10333) are observed pass by pass: a level change
//!   dates from the first pass that showed it, and a PR already starred on
//!   the tracker's first pass of its repo has an unknown star instant (it
//!   orders by age, as dispatch does without a starred-at).

use super::hold;
use super::ready::plan_max_age_secs;
use super::timeline::{Dated, Timeline};
use super::{EstimateContext, Item, ItemKey, ReadyPlan, ReadyRow, Tracker};
use crate::eta::explanation::{FeatureOmitted, Features};
use crate::eta::fit::features_v2::PriorityInputs;
use crate::eta::fleet::FleetSnapshot;
use crate::eta::journal::JournalEntry;
use crate::eta::labels::stage_from_pr_labels;
use crate::eta::loop_features::FileSnapshot;
use crate::eta::pr_features::{FeatureRead, PrFeatureStore, Wanted};
use crate::eta::priority_features::{self, PriorityEntry, PriorityFeatures, PriorityState};
use crate::eta::priority_inputs::{priority_inputs, PriorityContext};
use crate::eta::queue_features::{
    self, is_pr_stage, reason, EventKind, EventLog, QueueFeatures, QueueSubject, RosterEntry,
    StageEvent, SINCE_MERGE_CAP_SEC,
};
use crate::eta::repo_priority::RosterRevision;
use crate::eta::stage_queue::{stage_queue, StageQueue};
use crate::eta::stall_features::{self, StallSnapshot};
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
    /// The modeled roster with each PR's star from its labels (#10333).
    priority_roster: Vec<PriorityEntry>,
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
    /// Each listed PR's star timeline as the passes observed it (#10333),
    /// carried from pass to pass; a PR that leaves the listings drops out.
    stars: BTreeMap<(String, u32), PriorityState>,
    /// The repos a pass has listed: a PR new to one of these was opened (or
    /// entered review) since, so a star it carries is about that new.
    star_repos: BTreeSet<String>,
    /// The feature reads' answers so far (#10232).
    reads: PrFeatureStore,
    /// The last pass's stall signals (#10232).
    stall: Option<StallSnapshot>,
    /// Each PR's label-transition timeline from the fleet snapshots (#10500).
    timeline: Timeline,
    /// The fleet roster's revisions, oldest first (#10508), for the
    /// roster-derived `eta-fit/v2` inputs; `None` leaves them unknown.
    fleet_history: Option<Vec<RosterRevision>>,
    /// Each open PR's changed-file list as logged (#10550); `None` until a
    /// host loads the log, which leaves the overlap predictor unknown.
    file_snapshots: Option<Vec<FileSnapshot>>,
}

impl PassContext {
    /// An open PR's `(head, base)` branches as the feature reads last saw
    /// them (#10526).
    pub(super) fn open_pull_refs(
        &self,
        repo: &str,
        pr: u32,
    ) -> Option<(Option<String>, Option<String>)> {
        self.reads.open_pull_refs(repo, pr)
    }

    /// `pr`'s current-stage entry from the timeline (see [`Timeline::current`]).
    pub(super) fn timeline_dated(
        &self,
        repo: &str,
        pr: u32,
        stage: Stage,
        now: DateTime<Utc>,
    ) -> Option<Dated> {
        self.timeline.current(repo, pr, stage, now)
    }

    /// `pr`'s hold entries from the timeline (see [`Timeline::held`]).
    pub(super) fn timeline_held(
        &self,
        repo: &str,
        pr: u32,
        now: DateTime<Utc>,
    ) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        self.timeline.held(repo, pr, now)
    }

    /// The fleet snapshots' timeline (#10500).
    pub(super) fn timeline(&self) -> &Timeline {
        &self.timeline
    }

    /// `pr`'s doctor rounds from the timeline.
    pub(super) fn timeline_rework(&self, repo: &str, pr: u32, now: DateTime<Utc>) -> u32 {
        self.timeline.doctor_rounds(repo, pr, now)
    }
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
    /// The fleet snapshots, loaded at `now` (#10500): the label-transition
    /// timeline the tracker dates a first-seen PR's stage entry and rework
    /// from, and the event log it counts departures and merges over, as
    /// `eta fit`'s training rows do. Replaces the previous load; call it
    /// before the pass's listings.
    pub fn on_fleet_snapshots(&mut self, snapshots: &[FleetSnapshot], now: DateTime<Utc>) {
        self.context.timeline = Timeline::from_snapshots(snapshots, now);
    }

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
        // #10500: each followed PR as the model reads it, reconciled
        // against the label timeline (`Tracker::model_view`).
        let tracked: BTreeMap<(&str, u32), Item> = self
            .items
            .iter()
            .filter_map(|(key, item)| {
                Some(((key.repo.as_str(), item.pr_number?), self.model_view(item, observed_at)))
            })
            .collect();
        let mut roster = Vec::new();
        let mut episode_roster = Vec::new();
        let mut priority_roster = Vec::new();
        let mut scope = Vec::new();
        let mut stars = BTreeMap::new();
        for (repo, prs) in listings {
            let repo = repo.to_ascii_lowercase();
            let repo_seen = self.context.star_repos.contains(&repo);
            for pr in prs {
                let star = PriorityState::observe(
                    self.context.stars.get(&(repo.clone(), pr.number)),
                    &pr.labels,
                    observed_at,
                    repo_seen,
                );
                let stage = stage_from_pr_labels(&pr.labels).ok();
                let item = tracked.get(&(repo.as_str(), pr.number));
                // A PR the tracker does not follow is dated from the label
                // timeline when it has it (#10500), else `updated_at`.
                let dated = stage.and_then(|s| {
                    self.context
                        .timeline
                        .current(&repo, pr.number, s, observed_at)
                });
                let entry = |followed: Option<DateTime<Utc>>, split: bool| RosterEntry {
                    repo: repo.clone(),
                    pr: pr.number,
                    stage,
                    entered_at: followed
                        .or(dated.map(|d| {
                            if split {
                                d.released_at.unwrap_or(d.entered_at)
                            } else {
                                d.entered_at
                            }
                        }))
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
                roster.push(entry(followed(false), false));
                let modeled = entry(followed(true), true);
                priority_roster.push(PriorityEntry {
                    repo: modeled.repo.clone(),
                    pr: modeled.pr,
                    stage: modeled.stage,
                    entered_at: modeled.entered_at,
                    known_at: modeled.known_at,
                    star: star.clone(),
                });
                episode_roster.push(modeled);
                stars.insert((repo.clone(), pr.number), star);
            }
            scope.push(repo);
        }
        self.context.star_repos.extend(scope.iter().cloned());
        self.context.stars = stars;
        // #10500: the snapshots' log (training's) where it reaches.
        let events = self.context.timeline.events(events, observed_at);
        self.context.fleet = Some(FleetView {
            roster,
            episode_roster,
            priority_roster,
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

    /// The priority-aware features (#10333) of `subject` at `as_of`, from the
    /// last fleet view observed before `as_of`, over the modeled (episode)
    /// roster, with the subject's star from the same view. Not part of
    /// [`Features`]: no shipped heuristic reads it (see
    /// [`priority_features`]'s module docs).
    ///
    /// Serving's star instants are the passes that first showed a change, so
    /// they trail the label event by at most one pass; a PR already starred
    /// on the tracker's first pass of its repo has an unknown star instant
    /// (ordered by age, no `starred_age_sec`), as a first-seen PR's stage
    /// entry is a lower bound.
    #[must_use]
    pub fn priority_features_at(
        &self,
        subject: &QueueSubject,
        as_of: DateTime<Utc>,
    ) -> PriorityFeatures {
        let Some(view) = self
            .context
            .fleet
            .as_ref()
            .filter(|v| v.observed_at < as_of)
        else {
            return PriorityFeatures::default();
        };
        // Each entry's linked-issue star (#10372) as of `as_of`.
        let roster: Vec<PriorityEntry> = view
            .priority_roster
            .iter()
            .map(|e| {
                let linked = self.linked_star(&e.repo, e.pr, as_of);
                PriorityEntry {
                    star: e.star.clone().with_linked(linked),
                    ..e.clone()
                }
            })
            .collect();
        let star = subject
            .pr
            .and_then(|pr| {
                roster
                    .iter()
                    .find(|e| e.pr == pr && e.repo.eq_ignore_ascii_case(&subject.repo))
            })
            .map(|e| e.star.clone())
            .unwrap_or_default();
        priority_features::priority_features(subject, &star, &roster, &view.scope, as_of)
    }

    /// [`Self::priority_features_at`] for listed PR `pr` of `repo`, its stage
    /// and entry from the view's modeled roster; `None` when the last view
    /// before `as_of` does not list it.
    #[must_use]
    pub fn priority_features_of(
        &self,
        repo: &str,
        pr: u32,
        as_of: DateTime<Utc>,
    ) -> Option<PriorityFeatures> {
        let view = self
            .context
            .fleet
            .as_ref()
            .filter(|v| v.observed_at < as_of)?;
        let entry = view
            .priority_roster
            .iter()
            .find(|e| e.pr == pr && e.repo.eq_ignore_ascii_case(repo))?;
        let subject = QueueSubject {
            repo: entry.repo.clone(),
            pr: Some(pr),
            current: entry.stage.map(|s| (s, entry.entered_at)),
        };
        Some(self.priority_features_at(&subject, as_of))
    }

    /// Hand the tracker the logged per-PR file lists (#10550), what the
    /// serving side's file-overlap predictor reads exactly as `eta fit` does
    /// (`None`: unknown, so the predictor stays unknown).
    pub fn set_file_snapshots(&mut self, files: Option<Vec<FileSnapshot>>) {
        self.context.file_snapshots = files;
    }

    /// The logged file lists (#10550), or `None` when none were loaded.
    pub(super) fn file_snapshots(&self) -> Option<&[FileSnapshot]> {
        self.context.file_snapshots.as_deref()
    }

    /// Hand the tracker the fleet roster's revisions, oldest first (#10508):
    /// what every later estimate's roster-derived `eta-fit/v2` inputs read
    /// (`None`: unknown). Never today's `repos.yml` standing in for history.
    pub fn set_fleet_history(&mut self, history: Option<Vec<RosterRevision>>) {
        self.context.fleet_history = history;
    }

    /// The `eta-fit/v2` priority inputs (#10508) of listed PR `pr` of `repo`
    /// at `as_of`, from the last fleet view observed before `as_of`, through
    /// the one builder the fit calls ([`priority_inputs`]): the PR's own
    /// label state, its linked-issue star (`None` without a fresh star
    /// observation), the modeled roster with each entry's linked star, and
    /// `fleet_history` (the fleet roster's revisions, oldest first; `None`
    /// leaves the roster-derived inputs unknown). `None` when the view does
    /// not list the PR. Recorded as [`Features::priority`];
    /// `land-2026-10-06-keen-wren` reads it.
    #[must_use]
    pub fn priority_inputs_of(
        &self,
        repo: &str,
        pr: u32,
        as_of: DateTime<Utc>,
        fleet_history: Option<&[RosterRevision]>,
    ) -> Option<PriorityInputs> {
        let view = self
            .context
            .fleet
            .as_ref()
            .filter(|v| v.observed_at < as_of)?;
        let entry = view
            .priority_roster
            .iter()
            .find(|e| e.pr == pr && e.repo.eq_ignore_ascii_case(repo))?;
        let subject = QueueSubject {
            repo: entry.repo.clone(),
            pr: Some(pr),
            current: entry.stage.map(|s| (s, entry.entered_at)),
        };
        let roster: Vec<PriorityEntry> = view
            .priority_roster
            .iter()
            .map(|e| PriorityEntry {
                star: e
                    .star
                    .clone()
                    .with_linked(self.linked_star(&e.repo, e.pr, as_of)),
                ..e.clone()
            })
            .collect();
        let linked = self.linked_star_known(&entry.repo, pr, as_of);
        let ctx = PriorityContext {
            roster: &roster,
            scope: &view.scope,
            fleet_history,
        };
        Some(priority_inputs(&subject, &entry.star, linked.as_ref(), &ctx, as_of))
    }

    /// This pass's feature reads (#10232), at most `budget` of them, for
    /// every live item; only items of `readable` repos (lowercased slugs
    /// with a checkout this pass) plan a read. Call it before the pass's
    /// `as_of`, so its answers are known to that pass's estimates.
    pub fn plan_feature_reads(
        &mut self,
        readable: &[String],
        now: DateTime<Utc>,
        budget: usize,
    ) -> Vec<FeatureRead> {
        let wanted: Vec<Wanted> = self
            .items
            .iter()
            .filter(|(_, item)| !item.landed)
            .map(|(key, item)| Wanted {
                repo: key.repo.clone(),
                issue: key.issue,
                pr: item.pr_number,
                readable: readable.iter().any(|r| r.eq_ignore_ascii_case(&key.repo)),
            })
            .collect();
        self.context.reads.plan(&wanted, now, budget)
    }

    /// The answers to [`Tracker::plan_feature_reads`]' reads, each with the
    /// instant it returned (`None` body: the read failed).
    pub fn on_feature_reads(
        &mut self,
        answers: &[(FeatureRead, Option<serde_json::Value>, DateTime<Utc>)],
    ) {
        for (read, body, at) in answers {
            self.context.reads.answer(read, body.as_ref(), *at);
        }
    }

    /// The host's stall signals (#10232), taken once per pass.
    pub fn on_stall_snapshot(&mut self, snapshot: StallSnapshot) {
        self.context.stall = Some(snapshot);
    }

    /// The per-stage queue context `little-v0` reads (#10208) for `item` at
    /// `now`: empty without a fleet view observed before `now`, a PR, or a PR
    /// stage.
    pub(super) fn stage_queue_for(
        &self,
        key: &ItemKey,
        item: &Item,
        current: &CurrentState,
        now: DateTime<Utc>,
    ) -> Vec<StageQueue> {
        let (Some(view), Some(pr), CurrentState::At(stage)) =
            (&self.context.fleet, item.pr_number, current)
        else {
            return Vec::new();
        };
        let Some(entered_at) = stage.entered_at else {
            return Vec::new();
        };
        if view.observed_at >= now {
            return Vec::new();
        }
        stage_queue(
            &key.repo,
            pr,
            stage.stage,
            entered_at,
            &view.roster,
            &view.events,
            &view.scope,
            now,
        )
        .into_iter()
        .collect()
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
        self.star_features(key, item, now, &mut features);
        // #10508: the v2 priority inputs, through the builder the fit calls.
        if let Some(pr) = item.pr_number {
            features.priority =
                self.priority_inputs_of(&key.repo, pr, now, self.context.fleet_history.as_deref());
            // #10521: the v3 friction predictors, through the builder the
            // fit calls, over the same timeline at `now − LAG`.
            features.loops = Some(self.loop_features_of(&key.repo, pr, now));
        }
        let pr = item.pr_number.ok_or(reason::NO_PR_YET);
        self.context
            .reads
            .write_to(&key.repo, key.issue, pr, now, &mut features, &mut omitted);
        stall_features::write_to(
            self.context.stall.as_ref(),
            &key.repo,
            now,
            &mut features,
            &mut omitted,
        );
        (features, omitted)
    }
}

#[cfg(test)]
impl Tracker {
    /// The last fleet view's described and episode rosters (#10500 parity).
    pub(crate) fn fleet_rosters(&self) -> Option<(&[RosterEntry], &[RosterEntry])> {
        let view = self.context.fleet.as_ref()?;
        Some((&view.roster, &view.episode_roster))
    }
}
