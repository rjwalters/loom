//! The label-transition timeline the serving side shares with training
//! (#10500).
//!
//! `eta fit` dates a PR's stage entry, counts its doctor rounds and counts
//! the fleet's departures and merges from the fleet snapshots
//! (`FleetSnapshot::episodes` / `merges`, derived from the forge's label
//! timeline and PR listing). A tracker that first sees a PR mid-stage (a
//! restart, or a PR that closes no issue) used to date it from the listing's
//! `updated_at`, a lower bound (median 6.4 h against 27.7 h of true stage
//! age), to count its rework as 0 or 1, and to count only the merges of PRs
//! this host tracks.
//!
//! [`Timeline`] holds those snapshots. With the training definitions, read at
//! `cutoff = now − LAG` exactly as a training row at `t` reads `t − LAG`:
//!
//! - **entry**: the entry of the PR's episode in the labels' stage that is
//!   open at the cutoff ([`is_open_at`]); a stage the PR left and re-entered
//!   since the snapshot is not visible, and keeps the `updated_at` bound;
//! - **pooled entry**: the pooled `merge_wait` the tracker's path-engine
//!   heuristics read begins at the approval, so a `merge_wait` episode that
//!   follows a hold chains back to the chain's first episode, and the hold's
//!   release is the split entry ([`Dated::released_at`]) a hold-aware model
//!   reads, as training's split episode is;
//! - **rework**: the PR's `doctor` episodes entered before the cutoff;
//! - **events**: [`SnapshotLog`], the log `eta fit` counts over.
//!
//! First sight writes the timeline's dating into the track
//! ([`Tracker::first_sight`]); every later estimate and fleet view reads
//! [`Tracker::model_view`], which reconciles a tracked PR against the
//! timeline again, so a refresh that lands after first sight (or a
//! transition observed a pass after its label) reaches the model too.

use super::{Item, ItemKey, PrView, StageTrack, Tracker};
use crate::eta::episodes::{EpisodeEnd, EpisodeNext, StageEpisode};
use crate::eta::fit::rows::{data_horizon, is_open_at};
use crate::eta::fit::KNOWABLE_LAG_SEC;
use crate::eta::fleet::FleetSnapshot;
use crate::eta::fleet_log::{one_per_repo, SnapshotLog};
use crate::eta::loop_features::{
    loop_features, repo_context, FileSnapshot, LoopFeatures, LoopInputs,
};
use crate::eta::queue_features::EventLog;
use crate::eta::scope_features::{churn_context, scope_features, ScopeFeatures, ScopeInputs};
use crate::eta::{AgeSource, Stage};
use chrono::{DateTime, Duration, Utc};
use std::collections::BTreeMap;

impl Tracker {
    /// The friction predictors of `pr` of `repo` at `now` (#10521), from the
    /// fleet snapshots' timeline at `now − LAG`, as a training row at `t`
    /// reads them at `t − LAG` ([`Timeline::loop_features`]).
    #[must_use]
    pub fn loop_features_of(&self, repo: &str, pr: u32, now: DateTime<Utc>) -> LoopFeatures {
        self.context
            .timeline()
            .loop_features(repo, pr, now, self.file_snapshots())
    }

    /// The size and scope predictors of `pr` of `repo` at `now` (#10960),
    /// from the logged file lists and the timeline's merges at `now − LAG`,
    /// as a training row at `t` reads them at `t − LAG`. `None` while no
    /// file log is loaded (every input would be unknown).
    #[must_use]
    pub fn scope_features_of(
        &self,
        repo: &str,
        pr: u32,
        now: DateTime<Utc>,
    ) -> Option<ScopeFeatures> {
        let files = self.file_snapshots()?;
        Some(self.context.timeline().scope_features(repo, pr, now, files))
    }

    /// Start `key`'s track on first sight of its PR mid-`stage` (a restart,
    /// or a PR new to this host), and return the item.
    ///
    /// The entry is the timeline's (see [`Timeline::current`]), else the
    /// listing's `updated_at` (a lower bound). The rework count is at least
    /// the timeline's ([`Timeline::doctor_rounds`]), and at least 1 in
    /// `doctor`. A released `merge_wait` PR takes its release from the
    /// timeline too, so a hold-aware model reads its split episode.
    pub(super) fn first_sight(
        &mut self,
        key: &ItemKey,
        pr: &PrView,
        stage: Stage,
        now: DateTime<Utc>,
    ) -> Item {
        let dated = self
            .context
            .timeline_dated(&key.repo, pr.number, stage, now);
        let rework = self.context.timeline_rework(&key.repo, pr.number, now);
        let item = self.items.entry(key.clone()).or_default();
        let floor = u32::from(stage == Stage::Doctor).max(rework);
        item.rework_rounds = item.rework_rounds.max(floor);
        item.hold.released_at = dated.and_then(|d| d.released_at);
        item.stage = Some(StageTrack {
            stage,
            entered_at: dated
                .map(|d| d.entered_at)
                .or(pr.updated_at)
                .unwrap_or(now)
                .min(now),
            source: if dated.is_some() {
                AgeSource::LabelEvent
            } else {
                AgeSource::UpdatedAtLowerBound
            },
            exact: false,
        });
        item.clone()
    }

    /// `item` as the model reads it at `now` (#10500): its stage entry,
    /// hold entry, release and rework reconciled against the timeline on
    /// **every** pass, not only on first sight, so a PR tracked before its
    /// snapshot existed (or across a transition observed a pass late) is
    /// dated and counted as `eta fit`'s training row is.
    ///
    /// Only the estimate's input and the fleet roster read this view. The
    /// item itself is untouched: its journal rows, observed durations and
    /// verdict attempts keep the tracker's own observations.
    ///
    /// The timeline applies only when its episode is the stage visit the
    /// tracker follows: open in the tracked stage at `now − LAG` (for a held
    /// PR, in `merge_hold`), and — when the tracker observed an entry or a
    /// release itself (an exact track) — known through at least that
    /// instant. A snapshot cut before the tracker saw the PR enter its stage
    /// describes an earlier visit, so it is never applied over that
    /// observation. A first-sight track (a lower bound, or an earlier
    /// timeline's dating) takes the timeline as [`Self::first_sight`] does.
    pub(super) fn model_view(&self, item: &Item, now: DateTime<Utc>) -> Item {
        let mut out = item.clone();
        let (Some(pr), Some(track)) = (item.pr_number, item.stage.as_ref()) else {
            return out;
        };
        let timeline = self.context.timeline();
        let covers = |t: &StageTrack, through: DateTime<Utc>| !t.exact || through >= t.entered_at;
        let held = item
            .hold
            .open
            .as_ref()
            .filter(|_| track.stage == Stage::MergeWait);
        if let Some(open) = held {
            let Some((pooled, hold_at, through)) = timeline.held_known(&item.repo, pr, now) else {
                return out;
            };
            if !(covers(track, through) && covers(open, through)) {
                return out;
            }
            if let Some(o) = out.hold.open.as_mut() {
                o.entered_at = hold_at.min(now);
                o.source = AgeSource::LabelEvent;
            }
            if let Some(s) = out.stage.as_mut() {
                s.entered_at = pooled.min(now);
                s.source = AgeSource::LabelEvent;
            }
        } else {
            let Some(dated) = timeline.current(&item.repo, pr, track.stage, now) else {
                return out;
            };
            // A release the tracker observed is an observation too.
            let released = item
                .hold
                .released_at
                .filter(|at| track.stage == Stage::MergeWait && *at > track.entered_at)
                .is_none_or(|at| dated.known_through >= at);
            if !(covers(track, dated.known_through) && released) {
                return out;
            }
            if track.stage == Stage::MergeWait {
                out.hold.released_at = dated.released_at;
            }
            if let Some(s) = out.stage.as_mut() {
                s.entered_at = dated.entered_at.min(now);
                s.source = AgeSource::LabelEvent;
            }
        }
        out.rework_rounds = timeline.doctor_rounds(&item.repo, pr, now);
        out
    }
}

/// What the timeline says about a PR's current stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Dated {
    /// The entry the tracker's pooled track uses.
    pub(super) entered_at: DateTime<Utc>,
    /// The split episode's entry, when it differs from `entered_at` (a
    /// `merge_wait` released from a hold).
    pub(super) released_at: Option<DateTime<Utc>>,
    /// The last instant the snapshot describes the open episode at: its
    /// cut, or its end when it ended after `now − LAG`. A tracker
    /// observation later than this is one the snapshot has not seen.
    pub(super) known_through: DateTime<Utc>,
}

/// Every PR's stage episodes, by `(lowercased repo, pr)`, ascending by entry,
/// and the snapshots' event log.
#[derive(Debug, Clone, Default)]
pub(super) struct Timeline {
    prs: BTreeMap<(String, u32), Vec<StageEpisode>>,
    log: SnapshotLog,
}

fn cutoff(now: DateTime<Utc>) -> DateTime<Utc> {
    now - Duration::seconds(KNOWABLE_LAG_SEC)
}

impl Timeline {
    /// The timeline of `snapshots`, loaded at `now`. A repo with two
    /// snapshots (a case variant) keeps the later one, as `fit::rows` does;
    /// the log's horizon is `fit::rows::data_horizon(snapshots, now)`.
    pub(super) fn from_snapshots(snapshots: &[FleetSnapshot], now: DateTime<Utc>) -> Self {
        let chosen = one_per_repo(snapshots);
        let mut prs: BTreeMap<(String, u32), Vec<StageEpisode>> = BTreeMap::new();
        for snapshot in &chosen {
            let repo = snapshot.repo.to_ascii_lowercase();
            for episode in &snapshot.episodes {
                prs.entry((repo.clone(), episode.pr_number))
                    .or_default()
                    .push(episode.clone());
            }
        }
        for episodes in prs.values_mut() {
            episodes.sort_by_key(|e| (e.entered_at, e.stage));
        }
        let log = if chosen.is_empty() {
            SnapshotLog::default()
        } else {
            SnapshotLog::new(&chosen, data_horizon(snapshots, now))
        };
        Timeline { prs, log }
    }

    fn episodes(&self, repo: &str, pr: u32) -> &[StageEpisode] {
        self.prs
            .get(&(repo.to_ascii_lowercase(), pr))
            .map_or(&[], Vec::as_slice)
    }

    /// `pr`'s episodes as a derivation cut at `cutoff` would have them: a
    /// snapshot cut later shows no later fact.
    fn view(&self, repo: &str, pr: u32, cutoff: DateTime<Utc>) -> Vec<StageEpisode> {
        self.episodes(repo, pr)
            .iter()
            .filter_map(|e| e.view_at(cutoff))
            .collect()
    }

    /// The index in `episodes` of the `stage` episode open at `cutoff`.
    fn open(episodes: &[StageEpisode], stage: Stage, cutoff: DateTime<Utc>) -> Option<usize> {
        episodes
            .iter()
            .rposition(|e| e.stage == stage && is_open_at(e, cutoff))
    }

    /// The doctor rounds of `pr` known at `now`: its `doctor` episodes
    /// entered before `now − LAG`, as `fit::rows` counts `rework`.
    pub(super) fn doctor_rounds(&self, repo: &str, pr: u32, now: DateTime<Utc>) -> u32 {
        let cutoff = cutoff(now);
        let n = self
            .episodes(repo, pr)
            .iter()
            .filter(|e| e.stage == Stage::Doctor && e.entered_at < cutoff)
            .count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// When `pr` entered `stage`, if the timeline has it running in `stage`
    /// at `now − LAG`.
    ///
    /// `stage` is the labels' stage as the tracker resolves it: the pooled
    /// `merge_wait`, or `merge_hold` for a held PR (whose pooled track is
    /// `merge_wait`, see [`Self::held`]).
    pub(super) fn current(
        &self,
        repo: &str,
        pr: u32,
        stage: Stage,
        now: DateTime<Utc>,
    ) -> Option<Dated> {
        let cutoff = cutoff(now);
        let episodes = self.view(repo, pr, cutoff);
        let at = Self::open(&episodes, stage, cutoff)?;
        let open = &episodes[at];
        let known_through = self.known_through(repo, pr, at);
        if stage != Stage::MergeWait {
            return Some(Dated {
                entered_at: open.entered_at,
                released_at: None,
                known_through,
            });
        }
        // Chain back over the hold: the pooled `merge_wait` begins at the
        // first episode of the run of `merge_wait` / `merge_hold` episodes.
        let (first, held) = chain_start(&episodes[..=at]);
        Some(Dated {
            entered_at: first,
            released_at: held.then_some(open.entered_at).filter(|r| *r > first),
            known_through,
        })
    }

    /// [`Dated::known_through`] of `pr`'s episode at `index`. The view at a
    /// cutoff is a prefix of the episodes (they are sorted by entry and
    /// [`StageEpisode::view_at`] drops only those entered at or after it),
    /// so the index is the same in both.
    fn known_through(&self, repo: &str, pr: u32, index: usize) -> DateTime<Utc> {
        self.episodes(repo, pr)[index].last_at()
    }

    /// For a PR held at `now − LAG`: `(pooled merge_wait entry, hold entry)`.
    pub(super) fn held(
        &self,
        repo: &str,
        pr: u32,
        now: DateTime<Utc>,
    ) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        self.held_known(repo, pr, now)
            .map(|(pooled, hold_at, _)| (pooled, hold_at))
    }

    /// [`Self::held`], with the hold episode's [`Dated::known_through`].
    pub(super) fn held_known(
        &self,
        repo: &str,
        pr: u32,
        now: DateTime<Utc>,
    ) -> Option<(DateTime<Utc>, DateTime<Utc>, DateTime<Utc>)> {
        let cutoff = cutoff(now);
        let episodes = self.view(repo, pr, cutoff);
        let at = Self::open(&episodes, Stage::MergeHold, cutoff)?;
        let (first, _) = chain_start(&episodes[..=at]);
        Some((first, episodes[at].entered_at, self.known_through(repo, pr, at)))
    }

    /// The friction predictors of `pr` at `now − LAG` (#10521): the one
    /// builder [`loop_features`] over the PR's episodes and its repo's, as
    /// `fit::rows` calls it. File lists are the logged ones (#10550; `None`
    /// until loaded), CI runs are not logged yet.
    pub(super) fn loop_features(
        &self,
        repo: &str,
        pr: u32,
        now: DateTime<Utc>,
        files: Option<&[FileSnapshot]>,
    ) -> LoopFeatures {
        let cutoff = cutoff(now);
        let key = repo.to_ascii_lowercase();
        let all: Vec<&StageEpisode> = self
            .prs
            .range((key.clone(), 0)..=(key, u32::MAX))
            .flat_map(|(_, episodes)| episodes.iter())
            .collect();
        let own: Vec<&StageEpisode> = self.episodes(repo, pr).iter().collect();
        let context = repo_context(&all, cutoff);
        loop_features(
            &LoopInputs {
                repo,
                pr,
                own: &own,
                repo_episodes: &context,
                files,
                ci: None,
            },
            cutoff,
        )
    }

    /// The size and scope predictors of `pr` at `now − LAG` (#10960): the one
    /// builder [`scope_features`] over the logged file lists and the repo's
    /// merges in the churn window, as `fit::rows` calls it.
    pub(super) fn scope_features(
        &self,
        repo: &str,
        pr: u32,
        now: DateTime<Utc>,
        files: &[FileSnapshot],
    ) -> ScopeFeatures {
        let cutoff = cutoff(now);
        let key = repo.to_ascii_lowercase();
        let all: Vec<&StageEpisode> = self
            .prs
            .range((key.clone(), 0)..=(key, u32::MAX))
            .flat_map(|(_, episodes)| episodes.iter())
            .collect();
        let merged = churn_context(&all, cutoff);
        scope_features(
            &ScopeInputs {
                repo,
                pr,
                repo_episodes: &merged,
                files: Some(files),
            },
            cutoff,
        )
    }

    /// The event log serving counts over at `observed_at`
    /// ([`SnapshotLog::serve`]).
    pub(super) fn events(&self, journal: EventLog, observed_at: DateTime<Utc>) -> EventLog {
        self.log.serve(journal, observed_at)
    }
}

/// The entry of the first episode of the `merge_wait` / `merge_hold` run that
/// ends with `episodes`' last, and whether the run holds a `merge_hold`
/// before that last episode. An episode joins the run when the previous one
/// left into it.
fn chain_start(episodes: &[StageEpisode]) -> (DateTime<Utc>, bool) {
    let last = &episodes[episodes.len() - 1];
    let mut first = last.entered_at;
    let mut next = last.stage;
    let mut held = false;
    for e in episodes[..episodes.len() - 1].iter().rev() {
        let pooled = matches!(e.stage, Stage::MergeWait | Stage::MergeHold);
        let leads = matches!(
            e.end,
            EpisodeEnd::Left { next: EpisodeNext::Stage(s), .. } if s == next
        );
        if !(pooled && leads) {
            break;
        }
        held |= e.stage == Stage::MergeHold;
        first = e.entered_at;
        next = e.stage;
    }
    (first, held)
}
