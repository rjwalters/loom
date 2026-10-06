//! The stage-event log of the fleet snapshots (#10500): **one** definition of
//! the departures and merges the queue features count, read by `eta fit`'s
//! training rows ([`super::fit::rows`]) and by serving (the tracker's fleet
//! view), so the two cannot count differently.
//!
//! # The log
//!
//! From one snapshot per repo ([`one_per_repo`]), with every event before the
//! data horizon `H`, each known at `at + LAG` (`LAG` =
//! [`KNOWABLE_LAG_SEC`]):
//!
//! - one event per episode end: a merge is one `merge` event (never an exit
//!   plus a merge, which would double-count the departure), every other end
//!   (`left` for a stage or a close, `unstaged`) an `exit`, both with the
//!   episode's stage. So `merge_wait → merge_hold` is a `merge_wait` exit;
//! - one stage-less `merge` per [`FleetSnapshot::merges`] entry of a PR with
//!   no episode ending in that merge: **every forge merge** counts, not only
//!   those of PRs that carried a loom review label. A snapshot written before
//!   #10500 has no `merges` and counts the episode merges only.
//!
//! `from` is the latest of the snapshots' earliest episode entries before
//! `H`, the instant from which every repo's log is complete.
//!
//! # Serving
//!
//! The snapshots end at `H`; the pass is later. [`SnapshotLog::serve`] takes
//! this log for every event before `H` in a repo it covers, and the ETA
//! stage journal's events for the rest (a repo no snapshot covers, or an
//! event at or after `H`). The journal sees only PRs the host tracks, so an
//! unlabelled merge after `H` is missing until the next refresh moves `H`.

use super::episodes::{EpisodeEnd, EpisodeNext};
use super::fit::KNOWABLE_LAG_SEC;
use super::fleet::FleetSnapshot;
use super::queue_features::{EventKind, EventLog, StageEvent};
use chrono::{DateTime, Duration, Utc};
use std::collections::{BTreeMap, BTreeSet};

/// One snapshot per repo: two files for one repo (a case variant) would
/// otherwise double every PR. The later `(as_of, snapshot_id)` wins, which is
/// independent of load order.
#[must_use]
pub fn one_per_repo(snapshots: &[FleetSnapshot]) -> Vec<&FleetSnapshot> {
    let mut by_repo: BTreeMap<String, &FleetSnapshot> = BTreeMap::new();
    for snapshot in snapshots {
        let slot = by_repo
            .entry(snapshot.repo.to_ascii_lowercase())
            .or_insert(snapshot);
        if (snapshot.as_of, &snapshot.snapshot_id) > (slot.as_of, &slot.snapshot_id) {
            *slot = snapshot;
        }
    }
    by_repo.into_values().collect()
}

/// The stage events of a set of snapshots (see the module docs).
#[derive(Debug, Clone, Default)]
pub struct SnapshotLog {
    sorted: Vec<StageEvent>,
    merges_by_repo: BTreeMap<String, Vec<StageEvent>>,
    from: Option<DateTime<Utc>>,
    /// `H`: no event at or after it is in the log.
    horizon: Option<DateTime<Utc>>,
    /// The repos (lowercased) a snapshot covers.
    repos: BTreeSet<String>,
}

impl SnapshotLog {
    /// The log of `chosen` (one snapshot per repo) before `horizon`.
    #[must_use]
    pub fn new(chosen: &[&FleetSnapshot], horizon: DateTime<Utc>) -> Self {
        let lag = Duration::seconds(KNOWABLE_LAG_SEC);
        let mut sorted: Vec<StageEvent> = Vec::new();
        let mut repos = BTreeSet::new();
        for snapshot in chosen {
            let repo = snapshot.repo.to_ascii_lowercase();
            let mut merged: BTreeSet<u32> = BTreeSet::new();
            for episode in &snapshot.episodes {
                let Some(at) = episode.ended_at().filter(|at| *at < horizon) else {
                    continue;
                };
                let kind = match episode.end {
                    EpisodeEnd::Left {
                        next: EpisodeNext::Merged,
                        ..
                    } => {
                        merged.insert(episode.pr_number);
                        EventKind::Merge
                    }
                    _ => EventKind::Exit,
                };
                sorted.push(StageEvent {
                    repo: repo.clone(),
                    pr: Some(episode.pr_number),
                    stage: Some(episode.stage),
                    kind,
                    at,
                    known_at: at + lag,
                });
            }
            for merge in &snapshot.merges {
                if merge.at >= horizon || merged.contains(&merge.pr_number) {
                    continue;
                }
                sorted.push(StageEvent {
                    repo: repo.clone(),
                    pr: Some(merge.pr_number),
                    stage: None,
                    kind: EventKind::Merge,
                    at: merge.at,
                    known_at: merge.at + lag,
                });
            }
            repos.insert(repo);
        }
        sorted.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.cmp(b)));
        let mut merges_by_repo: BTreeMap<String, Vec<StageEvent>> = BTreeMap::new();
        for event in sorted.iter().filter(|e| e.kind == EventKind::Merge) {
            merges_by_repo
                .entry(event.repo.clone())
                .or_default()
                .push(event.clone());
        }
        // Only entries before the horizon, so a post-cutoff PR cannot move it.
        let from = chosen
            .iter()
            .filter_map(|s| {
                s.episodes
                    .iter()
                    .map(|e| e.entered_at)
                    .filter(|at| *at < horizon)
                    .min()
            })
            .max();
        SnapshotLog {
            sorted,
            merges_by_repo,
            from,
            horizon: Some(horizon),
            repos,
        }
    }

    /// The log [`super::queue_features::queue_features`] reads at `t`. Equal
    /// in effect to the whole log: every window it counts over is at most
    /// 24 h, and `since_merge` reads only each repo's last merge, so the
    /// events in `[t − 24 h, t)` plus each repo's last merge before that give
    /// the same features.
    #[must_use]
    pub fn at(&self, t: DateTime<Utc>) -> EventLog {
        let window_start = t - Duration::hours(24);
        let lo = self.sorted.partition_point(|e| e.at < window_start);
        let hi = self.sorted.partition_point(|e| e.at < t);
        let mut events: Vec<StageEvent> = self.sorted[lo..hi].to_vec();
        for merges in self.merges_by_repo.values() {
            let before = merges.partition_point(|e| e.at < window_start);
            if let Some(last) = before.checked_sub(1).and_then(|i| merges.get(i)) {
                events.push(last.clone());
            }
        }
        EventLog {
            from: self.from,
            events,
        }
    }

    /// Serving's log at `observed_at` (see the module docs): this log before
    /// `H` in the repos it covers, `journal` everywhere else. `journal`
    /// unchanged when no snapshot was loaded.
    #[must_use]
    pub fn serve(&self, journal: EventLog, observed_at: DateTime<Utc>) -> EventLog {
        let Some(horizon) = self.horizon.filter(|_| !self.repos.is_empty()) else {
            return journal;
        };
        let mut log = self.at(observed_at.min(horizon));
        log.events.extend(
            journal
                .events
                .into_iter()
                .filter(|e| e.at >= horizon || !self.repos.contains(&e.repo.to_ascii_lowercase())),
        );
        log.events.sort();
        log.events.dedup();
        log.from = self.from.or(journal.from);
        log
    }
}
