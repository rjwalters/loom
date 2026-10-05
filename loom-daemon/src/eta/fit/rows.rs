//! Training rows and dwells from fleet snapshots (#10245).
//!
//! [`build`] is pure: no clock, no I/O, and the output does not depend on the
//! order of the snapshots, their episodes or their flag changes. Everything a
//! row reads comes from [`FleetSnapshot::episodes`] (#10218) and
//! [`FleetSnapshot::flag_changes`]; the queue features come from the one
//! shared definition, [`queue_features`], called exactly as serving calls it.
//!
//! # The cutoff, knowability and the data horizon
//!
//! A fit at cutoff `T` reads facts knowable before `T`. A fact at instant `a`
//! is usable at row instant `t` iff `a + LAG < t` (`LAG` =
//! [`KNOWABLE_LAG_SEC`]); for an episode that is
//! [`StageEpisode::view_at`]`(t − LAG)`.
//!
//! Labels are censored at the **data horizon** `H = min(T − LAG, D)`, where
//! `D` is the oldest loaded snapshot's `as_of`, and `H` is recorded as the
//! file's `window.data_through`. Censoring at `T` itself would be wrong twice:
//! a merge in `[T − LAG, T)` is not knowable at `T`, so "not merged by `T`"
//! would be false; and a stale snapshot (`D < T`) has observed nothing after
//! `D`, so censoring at `T` would claim survival through days nobody saw.
//! An episode `open` in a snapshot is running at that snapshot's `as_of`
//! (whatever its stored `open.at`), so it is running at every instant `< H`.
//!
//! # Rows
//!
//! Row instants are `t = W + k·`[`ROW_STEP_SEC`] for `t < H`, with
//! `W = T −` [`WINDOW_DAYS`]. The subjects at `t` are the PRs with an episode
//! open at `t − LAG` in a [`FitStage`].
//!
//! - **Queue features**: `queue_features(subject, roster, log, scope, t)`.
//!   The subject's entry is the **split** episode's. The roster is every PR
//!   open in an episode at `t − LAG`, known at `entered_at + LAG`. The log has
//!   one event per episode end before `H`, known at `at + LAG`: a merge is one
//!   `merge` event (never an exit plus a merge, which would double-count the
//!   departure), every other end (`left` for a stage or a close, `unstaged`)
//!   an `exit`, both with the episode's stage. So `merge_wait → merge_hold` is
//!   a `merge_wait` exit, as serving journals it. `log.from` is the latest of
//!   the snapshots' earliest episode entries, the instant from which every
//!   repo's log is complete. The scope is every loaded snapshot's repo.
//! - **`age_h`**: hours since the split episode's entry.
//! - **`rework`**: the PR's `doctor` episodes entered before `t − LAG`, the
//!   current one included: the Judge rejections knowable at `t`, which is
//!   what serving's `doctor_cycles_so_far` (one per entry into `doctor`)
//!   counts.
//! - **Flags**: the last [`FlagChange`] before `t − LAG`. A PR with episodes
//!   but no flag timeline (its snapshot predates #10245) is dropped and
//!   counted in [`RowStats::rows_dropped_no_flags`], never zero-flagged.
//! - **Priority features** (#10333, [`Assembled::priority`], not model
//!   inputs): `priority_features` over the same roster, each PR's level
//!   timeline read from its flag changes before `t − LAG`
//!   ([`PriorityState::from_flags`]), plus the linked-issue star run
//!   (#10372) when star inputs are given.
//! - A row whose needed queue feature is `None` is dropped and counted in
//!   [`RowStats::rows_dropped_missing`].
//! - **Exit label**: `Some` iff `t + `[`EXIT_HORIZON_SEC`]` < H`; then whether
//!   the episode open at `t − LAG` ended (any end) by `t + horizon`, an end in
//!   `[t − LAG, t)` included (not knowable at `t`, exactly as when serving).
//! - **Merge label**, in hours: a merge at `m < H` is `(max(0, m − t),
//!   merged)`; else a close at `c < H` is `(max(0, c − t), censored)`; else
//!   `(H − t, censored)`.
//!
//! - **Star** (#10372): `starred_any` / `star_source` come from
//!   [`crate::eta::star::star_state_at`] at `cutoff = t − LAG` (PR or
//!   linked-issue star). They are recorded, not model inputs: the model's
//!   `starred` stays the PR's own flag. `None` when no star inputs were
//!   given or the repo's raw event cache does not cover the cutoff.
//!
//! # Dwells
//!
//! One per episode in a fit stage that entered before `H` and had not ended
//! before `W`: `entry_h = max(0, W − entered)`, `dwell_h` from entry to its
//! end when that is before `H`, else to `H`. A `left` for a fit stage is
//! `next`, a merge `merged`, a close `closed`; `unstaged` (at its own instant),
//! a `left` for a non-fit stage and anything still running at `H` are
//! `censored`.
//!
//! # Order
//!
//! Rows are sorted by `(t, repo, pr, stage)` and dwells by `(stage, repo, pr,
//! entered_at)`, repos lowercased: total keys, never input order.

use super::{
    clock, DwellEnd, DwellRow, FitStage, MergeLabel, ModelInputs, TrainingRow, EXIT_HORIZON_SEC,
    KNOWABLE_LAG_SEC, ROW_STEP_SEC, WINDOW_DAYS,
};
use crate::eta::episodes::{EpisodeEnd, EpisodeNext, StageEpisode};
use crate::eta::flag_timeline::{flags_before, FlagChange};
use crate::eta::fleet::FleetSnapshot;
use crate::eta::labels::{
    FLAG_BLOCKED, FLAG_CI_FAIL, FLAG_CONFLICT, FLAG_OP_HOLD, FLAG_SEQUENCED, FLAG_STARRED,
};
use crate::eta::priority_features::{
    priority_features, PriorityEntry, PriorityFeatures, PriorityState,
};
use crate::eta::queue_features::{
    queue_features, EventKind, EventLog, QueueFeatures, QueueSubject, RosterEntry, StageEvent,
};
use crate::eta::star::{StarInputs, StarSource};
use crate::eta::Stage;
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use std::collections::BTreeMap;

/// Rows left out, and why.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RowStats {
    /// Rows with a needed queue feature `None`.
    pub rows_dropped_missing: usize,
    /// Rows of a PR with episodes but no flag timeline: its snapshot was
    /// written before #10245 (`eta fleet backfill` rebuilds it).
    pub rows_dropped_no_flags: usize,
    /// Rows kept whose star state is unknown (#10372): the repo's raw event
    /// cache does not cover the row's cutoff. Their `starred_any` is `None`,
    /// never `false`. Counted only when star inputs were supplied.
    pub rows_star_unknown: usize,
    /// Rows kept whose star state is known and starred, by either source
    /// (#10389). Counted only when star inputs were supplied.
    pub rows_starred_any: usize,
    /// Of those, rows starred only through a linked issue (`StarSource::Issue`):
    /// the rows the model's PR-only `starred` misses.
    pub rows_star_issue_only: usize,
}

/// The PR and instant one row describes (a [`TrainingRow`] carries only its
/// group).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct RowKey {
    /// The row instant `t`.
    pub at: DateTime<Utc>,
    /// `owner/repo`, lowercased.
    pub repo: String,
    /// The PR.
    pub pr: u32,
}

/// Everything [`build`] assembles.
#[derive(Debug, Clone, PartialEq)]
pub struct Assembled {
    /// The training rows, in canonical order.
    pub rows: Vec<TrainingRow>,
    /// `row_keys[i]` names `rows[i]`.
    pub row_keys: Vec<RowKey>,
    /// The dwells, in canonical order.
    pub dwells: Vec<DwellRow>,
    /// `priority[i]` is the priority-aware feature set of `rows[i]` (#10333):
    /// candidate inputs for the next model version, not read by the current
    /// fit, so the coefficient file is unchanged.
    pub priority: Vec<PriorityFeatures>,
    /// What was dropped.
    pub stats: RowStats,
    /// The data horizon `H` (see the module docs).
    pub data_through: DateTime<Utc>,
}

/// `H = min(as_of − LAG, oldest snapshot as_of)`; `as_of − LAG` with no
/// snapshot.
#[must_use]
pub fn data_horizon(snapshots: &[FleetSnapshot], as_of: DateTime<Utc>) -> DateTime<Utc> {
    let lagged = as_of - Duration::seconds(KNOWABLE_LAG_SEC);
    snapshots
        .iter()
        .map(|s| s.as_of)
        .min()
        .map_or(lagged, |oldest| oldest.min(lagged))
}

/// Whether `episode` is running at `cutoff`: exactly when
/// [`StageEpisode::view_at`]`(cutoff)` is [`EpisodeEnd::Open`], without the
/// clone.
#[must_use]
pub fn is_open_at(episode: &StageEpisode, cutoff: DateTime<Utc>) -> bool {
    episode.entered_at < cutoff && episode.ended_at().is_none_or(|at| at >= cutoff)
}

/// One PR, gathered from its snapshot.
struct Pr<'a> {
    repo: String,
    number: u32,
    episodes: Vec<&'a StageEpisode>,
    flags: Vec<FlagChange>,
    merged_at: Option<DateTime<Utc>>,
    closed_at: Option<DateTime<Utc>>,
}

impl<'a> Pr<'a> {
    fn open_at(&self, cutoff: DateTime<Utc>) -> Option<&'a StageEpisode> {
        self.episodes
            .iter()
            .copied()
            .find(|e| is_open_at(e, cutoff))
    }
}

/// One snapshot per repo: two files for one repo (a case variant) would
/// otherwise double every PR. The later `(as_of, snapshot_id)` wins, which is
/// independent of load order.
fn one_per_repo(snapshots: &[FleetSnapshot]) -> Vec<&FleetSnapshot> {
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

/// Every PR with an episode, in `(repo, pr)` order, its episodes by entry and
/// its flag changes by instant.
fn gather<'a>(snapshots: &[&'a FleetSnapshot]) -> Vec<Pr<'a>> {
    let mut prs: BTreeMap<(String, u32), Pr<'a>> = BTreeMap::new();
    for snapshot in snapshots {
        let repo = snapshot.repo.to_ascii_lowercase();
        for episode in &snapshot.episodes {
            prs.entry((repo.clone(), episode.pr_number))
                .or_insert_with(|| Pr {
                    repo: repo.clone(),
                    number: episode.pr_number,
                    episodes: Vec::new(),
                    flags: Vec::new(),
                    merged_at: None,
                    closed_at: None,
                })
                .episodes
                .push(episode);
        }
        for change in &snapshot.flag_changes {
            if let Some(pr) = prs.get_mut(&(repo.clone(), change.pr_number)) {
                pr.flags.push(*change);
            }
        }
    }
    for pr in prs.values_mut() {
        pr.episodes.sort_by(|a, b| {
            (a.entered_at, a.stage)
                .cmp(&(b.entered_at, b.stage))
                .then_with(|| a.digest_line().cmp(&b.digest_line()))
        });
        pr.flags.sort();
        for episode in &pr.episodes {
            if let EpisodeEnd::Left { at, next } = episode.end {
                match next {
                    EpisodeNext::Merged => {
                        pr.merged_at = Some(pr.merged_at.map_or(at, |m| m.min(at)))
                    }
                    EpisodeNext::Closed => {
                        pr.closed_at = Some(pr.closed_at.map_or(at, |c| c.min(at)))
                    }
                    EpisodeNext::Stage(_) => {}
                }
            }
        }
    }
    prs.into_values().collect()
}

/// The stage events: one per episode end before `horizon`, sorted by instant,
/// plus each repo's merges for the since-merge lookup.
struct Events {
    sorted: Vec<StageEvent>,
    merges_by_repo: BTreeMap<String, Vec<StageEvent>>,
    from: Option<DateTime<Utc>>,
}

impl Events {
    fn new(prs: &[Pr<'_>], horizon: DateTime<Utc>, from: Option<DateTime<Utc>>) -> Self {
        let lag = Duration::seconds(KNOWABLE_LAG_SEC);
        let mut sorted: Vec<StageEvent> = Vec::new();
        for pr in prs {
            for episode in &pr.episodes {
                let Some(at) = episode.ended_at().filter(|at| *at < horizon) else {
                    continue;
                };
                let kind = match episode.end {
                    EpisodeEnd::Left {
                        next: EpisodeNext::Merged,
                        ..
                    } => EventKind::Merge,
                    _ => EventKind::Exit,
                };
                sorted.push(StageEvent {
                    repo: pr.repo.clone(),
                    pr: Some(pr.number),
                    stage: Some(episode.stage),
                    kind,
                    at,
                    known_at: at + lag,
                });
            }
        }
        sorted.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.cmp(b)));
        let mut merges_by_repo: BTreeMap<String, Vec<StageEvent>> = BTreeMap::new();
        for event in sorted.iter().filter(|e| e.kind == EventKind::Merge) {
            merges_by_repo
                .entry(event.repo.clone())
                .or_default()
                .push(event.clone());
        }
        Events {
            sorted,
            merges_by_repo,
            from,
        }
    }

    /// The log [`queue_features`] reads at `t`. Equal in effect to the whole
    /// log: every window it counts over is at most 24 h, and `since_merge`
    /// reads only each repo's last merge, so the events in `[t − 24 h, t)`
    /// plus each repo's last merge before that give the same features.
    fn at(&self, t: DateTime<Utc>) -> EventLog {
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
}

fn hours(from: DateTime<Utc>, to: DateTime<Utc>) -> f64 {
    (to - from).num_seconds() as f64 / 3600.0
}

/// The model inputs, or `None` when a needed queue feature is missing.
fn model_inputs(
    q: &QueueFeatures,
    age_h: f64,
    t: DateTime<Utc>,
    rework: u32,
    flags: u8,
) -> Option<ModelInputs> {
    let (hour_utc, weekend) = clock(t);
    let flag = |bit: u8| flags & bit != 0;
    Some(ModelInputs {
        age_h,
        ahead: q.ahead?,
        n_stage_repo: q.n_stage_repo?,
        exits_repo_6h: q.exits_repo_6h?,
        exits_repo_24h: q.exits_repo_24h?,
        exits_fleet_6h: q.exits_fleet_6h?,
        merges_repo_24h: q.merges_repo_24h?,
        merges_fleet_6h: q.merges_fleet_6h?,
        since_merge_h: q.since_merge_sec? as f64 / 3600.0,
        n_stage_fleet: q.n_stage_fleet?,
        hour_utc,
        weekend,
        rework,
        op_hold: flag(FLAG_OP_HOLD),
        sequenced: flag(FLAG_SEQUENCED),
        starred: flag(FLAG_STARRED),
        conflict: flag(FLAG_CONFLICT),
        ci_fail: flag(FLAG_CI_FAIL),
        blocked: flag(FLAG_BLOCKED),
    })
}

fn merge_label(pr: &Pr<'_>, t: DateTime<Utc>, horizon: DateTime<Utc>) -> MergeLabel {
    match (pr.merged_at.filter(|m| *m < horizon), pr.closed_at.filter(|c| *c < horizon)) {
        (Some(m), _) => MergeLabel {
            dur_h: hours(t, m).max(0.0),
            merged: true,
        },
        (None, Some(c)) => MergeLabel {
            dur_h: hours(t, c).max(0.0),
            merged: false,
        },
        (None, None) => MergeLabel {
            dur_h: hours(t, horizon),
            merged: false,
        },
    }
}

/// The training rows and dwells of `snapshots` at cutoff `as_of` (see the
/// module docs for every rule).
#[must_use]
pub fn build(snapshots: &[FleetSnapshot], as_of: DateTime<Utc>) -> Assembled {
    build_with_star(snapshots, as_of, None)
}

/// The start of the PR's current star run before `cutoff`, from its flag
/// changes (any order): the first change of the trailing run of masks that
/// carry the star. `None` when the star is not in force.
fn pr_star_since(flags: &[FlagChange], cutoff: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let mut known: Vec<&FlagChange> = flags.iter().filter(|c| c.at < cutoff).collect();
    known.sort_by_key(|c| (c.at, c.flags));
    let mut since = None;
    for change in known {
        if change.flags & FLAG_STARRED == 0 {
            since = None;
        } else if since.is_none() {
            since = Some(change.at);
        }
    }
    since
}

/// The start of `pr`'s linked-issue star run known before `cutoff` (#10372),
/// for the priority features (#10333): `None` when no linked issue is
/// starred, or no star inputs were given or they do not cover `cutoff`.
fn linked_star_since(
    star: Option<&StarInputs>,
    pr: &Pr<'_>,
    cutoff: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let state = star?
        .repos
        .get(&pr.repo)?
        .state_at(pr.number, 0, None, cutoff)?;
    state.source.starred().then_some(state.since).flatten()
}

/// [`build`], also recording each row's star state from `star` (#10372).
#[must_use]
pub fn build_with_star(
    snapshots: &[FleetSnapshot],
    as_of: DateTime<Utc>,
    star: Option<&StarInputs>,
) -> Assembled {
    let lag = Duration::seconds(KNOWABLE_LAG_SEC);
    let step = Duration::seconds(ROW_STEP_SEC);
    let exit_horizon = Duration::seconds(EXIT_HORIZON_SEC);
    let window_start = as_of - Duration::days(WINDOW_DAYS);
    let horizon = data_horizon(snapshots, as_of);

    let chosen = one_per_repo(snapshots);
    let prs = gather(&chosen);
    let mut scope: Vec<String> = snapshots
        .iter()
        .map(|s| s.repo.to_ascii_lowercase())
        .collect();
    scope.sort();
    scope.dedup();
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
    let events = Events::new(&prs, horizon, from);

    let mut stats = RowStats::default();
    let mut keyed: Vec<((RowKey, FitStage), (TrainingRow, PriorityFeatures))> = Vec::new();
    let mut t = window_start;
    while t < horizon {
        let cutoff = t - lag;
        let open: Vec<(&Pr<'_>, &StageEpisode)> = prs
            .iter()
            .filter_map(|pr| pr.open_at(cutoff).map(|e| (pr, e)))
            .collect();
        if !open.is_empty() {
            let roster: Vec<RosterEntry> = open
                .iter()
                .map(|(pr, e)| RosterEntry {
                    repo: pr.repo.clone(),
                    pr: pr.number,
                    stage: Some(e.stage),
                    entered_at: e.entered_at,
                    known_at: e.entered_at + lag,
                })
                .collect();
            let priority_roster: Vec<PriorityEntry> = open
                .iter()
                .map(|(pr, e)| PriorityEntry {
                    repo: pr.repo.clone(),
                    pr: pr.number,
                    stage: Some(e.stage),
                    entered_at: e.entered_at,
                    known_at: e.entered_at + lag,
                    star: PriorityState::from_flags(&pr.flags, cutoff)
                        .unwrap_or_default()
                        .with_linked(linked_star_since(star, pr, cutoff)),
                })
                .collect();
            let log = events.at(t);
            for (pr, episode) in &open {
                let Some(stage) = FitStage::from_stage(episode.stage) else {
                    continue;
                };
                let Some(flags) = flags_before(&pr.flags, cutoff) else {
                    stats.rows_dropped_no_flags += 1;
                    continue;
                };
                let subject = QueueSubject {
                    repo: pr.repo.clone(),
                    pr: Some(pr.number),
                    current: Some((episode.stage, episode.entered_at)),
                };
                let (starred_any, star_source) = match star {
                    None => (None, None),
                    Some(inputs) => {
                        let state = inputs.repos.get(&pr.repo).and_then(|r| {
                            r.state_at(pr.number, flags, pr_star_since(&pr.flags, cutoff), cutoff)
                        });
                        match state.map(|s| s.source) {
                            None => stats.rows_star_unknown += 1,
                            Some(StarSource::None) => {}
                            Some(source) => {
                                stats.rows_starred_any += 1;
                                if source == StarSource::Issue {
                                    stats.rows_star_issue_only += 1;
                                }
                            }
                        }
                        (state.map(|s| s.source.starred()), state.map(|s| s.source))
                    }
                };
                let q = queue_features(&subject, &roster, &log, &scope, t);
                let own = priority_roster
                    .iter()
                    .find(|e| e.pr == pr.number && e.repo == pr.repo)
                    .map(|e| e.star.clone())
                    .unwrap_or_default();
                let prio = priority_features(&subject, &own, &priority_roster, &scope, t);
                let rework = pr
                    .episodes
                    .iter()
                    .filter(|e| e.stage == Stage::Doctor && e.entered_at < cutoff)
                    .count();
                let rework = u32::try_from(rework).unwrap_or(u32::MAX);
                let age_h = hours(episode.entered_at, t);
                let Some(inputs) = model_inputs(&q, age_h, t, rework, flags) else {
                    stats.rows_dropped_missing += 1;
                    continue;
                };
                let exit = (t + exit_horizon < horizon)
                    .then(|| episode.ended_at().is_some_and(|at| at <= t + exit_horizon));
                let key = RowKey {
                    at: t,
                    repo: pr.repo.clone(),
                    pr: pr.number,
                };
                keyed.push((
                    (key, stage),
                    (
                        TrainingRow {
                            stage,
                            group: format!("{}#{}", pr.repo, pr.number),
                            inputs,
                            starred_any,
                            star_source,
                            exit,
                            merge: merge_label(pr, t, horizon),
                        },
                        prio,
                    ),
                ));
            }
        }
        t += step;
    }
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    let mut row_keys = Vec::with_capacity(keyed.len());
    let mut rows = Vec::with_capacity(keyed.len());
    let mut priority = Vec::with_capacity(keyed.len());
    for ((key, _), (row, prio)) in keyed {
        row_keys.push(key);
        rows.push(row);
        priority.push(prio);
    }

    Assembled {
        rows,
        row_keys,
        dwells: dwells(&prs, window_start, horizon),
        priority,
        stats,
        data_through: horizon,
    }
}

/// A dwell's canonical sort key: `(stage, repo, pr, entered_at)`.
type DwellKey<'a> = (FitStage, &'a str, u32, DateTime<Utc>);

/// The dwells, in canonical order (see the module docs).
fn dwells(prs: &[Pr<'_>], window_start: DateTime<Utc>, horizon: DateTime<Utc>) -> Vec<DwellRow> {
    let mut keyed: Vec<(DwellKey<'_>, DwellRow)> = Vec::new();
    for pr in prs {
        for episode in &pr.episodes {
            let Some(stage) = FitStage::from_stage(episode.stage) else {
                continue;
            };
            if episode.entered_at >= horizon
                || episode.ended_at().is_some_and(|at| at < window_start)
            {
                continue;
            }
            let known_end = episode.ended_at().filter(|at| *at < horizon);
            let end = match (known_end, episode.end) {
                (Some(_), EpisodeEnd::Left { next, .. }) => match next {
                    EpisodeNext::Stage(s) => {
                        FitStage::from_stage(s).map_or(DwellEnd::Censored, DwellEnd::Next)
                    }
                    EpisodeNext::Merged => DwellEnd::Merged,
                    EpisodeNext::Closed => DwellEnd::Closed,
                },
                _ => DwellEnd::Censored,
            };
            keyed.push((
                (stage, pr.repo.as_str(), pr.number, episode.entered_at),
                DwellRow {
                    stage,
                    entry_h: hours(episode.entered_at, window_start).max(0.0),
                    dwell_h: hours(episode.entered_at, known_end.unwrap_or(horizon)),
                    end,
                },
            ));
        }
    }
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    keyed.into_iter().map(|(_, dwell)| dwell).collect()
}
