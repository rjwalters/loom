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

use super::{Item, ItemKey, PrView, StageTrack, Tracker};
use crate::eta::episodes::{EpisodeEnd, EpisodeNext, StageEpisode};
use crate::eta::fit::rows::{data_horizon, is_open_at};
use crate::eta::fit::KNOWABLE_LAG_SEC;
use crate::eta::fleet::FleetSnapshot;
use crate::eta::fleet_log::{one_per_repo, SnapshotLog};
use crate::eta::queue_features::EventLog;
use crate::eta::{AgeSource, Stage};
use chrono::{DateTime, Duration, Utc};
use std::collections::BTreeMap;

impl Tracker {
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
}

/// What the timeline says about a PR's current stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Dated {
    /// The entry the tracker's pooled track uses.
    pub(super) entered_at: DateTime<Utc>,
    /// The split episode's entry, when it differs from `entered_at` (a
    /// `merge_wait` released from a hold).
    pub(super) released_at: Option<DateTime<Utc>>,
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
        if stage != Stage::MergeWait {
            return Some(Dated {
                entered_at: open.entered_at,
                released_at: None,
            });
        }
        // Chain back over the hold: the pooled `merge_wait` begins at the
        // first episode of the run of `merge_wait` / `merge_hold` episodes.
        let (first, held) = chain_start(&episodes[..=at]);
        Some(Dated {
            entered_at: first,
            released_at: held.then_some(open.entered_at).filter(|r| *r > first),
        })
    }

    /// For a PR held at `now − LAG`: `(pooled merge_wait entry, hold entry)`.
    pub(super) fn held(
        &self,
        repo: &str,
        pr: u32,
        now: DateTime<Utc>,
    ) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let cutoff = cutoff(now);
        let episodes = self.view(repo, pr, cutoff);
        let at = Self::open(&episodes, Stage::MergeHold, cutoff)?;
        let (first, _) = chain_start(&episodes[..=at]);
        Some((first, episodes[at].entered_at))
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
