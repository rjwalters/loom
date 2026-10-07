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
//!   open in an episode at `t − LAG`, known at `entered_at + LAG`. The log is
//!   [`SnapshotLog`] (#10500), the one definition serving reads too: one
//!   event per episode end before `H` (a merge one `merge`, never an exit
//!   plus a merge), plus every other forge merge of the repo (a PR with no
//!   loom review label has no episode), each known at `at + LAG`. The scope
//!   is every loaded snapshot's repo.
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
//! - **v2 priority inputs** (#10508, [`Assembled::priority_inputs`], not
//!   read by the v1 fit): [`crate::eta::priority_inputs::priority_inputs`],
//!   the one builder serving calls too, over the same roster, with the
//!   subject's own flag timeline, its linked-issue star at `t − LAG` (`None`
//!   when the cache does not cover it) and the fleet roster history given to
//!   [`build_with_context`] (`None` = every roster-derived input unknown).
//! - **Friction predictors** (#10521, [`Assembled::loops`], not model
//!   inputs): `loop_features` at `t − LAG` over the repo's episodes, the same
//!   builder serving calls. Files and CI are not logged, so those are `None`.
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

use super::features_v2::PriorityInputs;
use super::{
    clock, DwellEnd, DwellRow, FitStage, MergeLabel, ModelInputs, TrainingRow, EXIT_HORIZON_SEC,
    KNOWABLE_LAG_SEC, ROW_STEP_SEC, WINDOW_DAYS,
};
use crate::eta::episodes::{EpisodeEnd, EpisodeNext, StageEpisode};
use crate::eta::flag_timeline::{flags_before, FlagChange};
use crate::eta::fleet::FleetSnapshot;
use crate::eta::fleet_log::{one_per_repo, SnapshotLog};
use crate::eta::labels::{
    FLAG_BLOCKED, FLAG_CI_FAIL, FLAG_CONFLICT, FLAG_OP_HOLD, FLAG_SEQUENCED, FLAG_STARRED,
};
use crate::eta::loop_features::{
    loop_features, repo_context, FileSnapshot, LoopFeatures, LoopInputs,
};
use crate::eta::priority_features::{
    priority_features, PriorityEntry, PriorityFeatures, PriorityState,
};
use crate::eta::priority_inputs::{priority_inputs, PriorityContext};
use crate::eta::queue_features::{queue_features, QueueFeatures, QueueSubject, RosterEntry};
use crate::eta::repo_priority::RosterRevision;
use crate::eta::star::{LinkedStar, StarInputs, StarSource};
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
    /// `priority_inputs[i]` is the `eta-fit/v2` priority input set of
    /// `rows[i]` (#10508), from the builder serving shares. Not read by the
    /// v1 fit, so the coefficient file is unchanged.
    pub priority_inputs: Vec<PriorityInputs>,
    /// `loops[i]` is the friction-predictor set of `rows[i]` (#10521): the
    /// review-loop history, repo Judge rejection rate and cumulative stage
    /// age, from the one builder serving also calls. File overlap and own-CI
    /// are `None` (not logged yet). Not read by the current fit.
    pub loops: Vec<LoopFeatures>,
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

/// `pr`'s linked-issue star known before `cutoff` (#10372), for the priority
/// features (#10333): its current run and on/off instants. Empty when no
/// star inputs were given or they do not cover `cutoff`.
fn linked_star(star: Option<&StarInputs>, pr: &Pr<'_>, cutoff: DateTime<Utc>) -> LinkedStar {
    star.and_then(|s| s.repos.get(&pr.repo))
        .and_then(|r| r.linked_at(pr.number, cutoff))
        .unwrap_or_default()
}

/// [`build`], also recording each row's star state from `star` (#10372).
#[must_use]
pub fn build_with_star(
    snapshots: &[FleetSnapshot],
    as_of: DateTime<Utc>,
    star: Option<&StarInputs>,
) -> Assembled {
    build_with_context(snapshots, as_of, star, None)
}

/// [`build_with_star`], also reading the fleet roster's history (oldest
/// first) for the v2 priority inputs (#10508). `None` leaves every
/// roster-derived input unknown.
#[must_use]
pub fn build_with_context(
    snapshots: &[FleetSnapshot],
    as_of: DateTime<Utc>,
    star: Option<&StarInputs>,
    fleet_history: Option<&[RosterRevision]>,
) -> Assembled {
    build_with_files(snapshots, as_of, star, fleet_history, None)
}

/// [`build_with_context`], also reading each open PR's changed-file list as
/// logged before each row's cutoff (#10550) for the file-overlap predictor.
/// `None` leaves it unknown; `Some` reads only snapshots whose `known_at` is
/// before `t - lag`, never the PR's final diff.
#[must_use]
pub fn build_with_files(
    snapshots: &[FleetSnapshot],
    as_of: DateTime<Utc>,
    star: Option<&StarInputs>,
    fleet_history: Option<&[RosterRevision]>,
    files: Option<&[FileSnapshot]>,
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
    // #10500: the one event-log definition, shared with serving.
    let events = SnapshotLog::new(&chosen, horizon);
    let mut repo_episodes: BTreeMap<&str, Vec<&StageEpisode>> = BTreeMap::new();
    for pr in &prs {
        repo_episodes
            .entry(pr.repo.as_str())
            .or_default()
            .extend(pr.episodes.iter().copied());
    }

    let mut stats = RowStats::default();
    let mut keyed: Vec<KeyedRow> = Vec::new();
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
                        .with_linked(linked_star(star, pr, cutoff)),
                })
                .collect();
            let log = events.at(t);
            // The repo-level slice of the friction predictors, once per tick.
            let context: BTreeMap<&str, Vec<&StageEpisode>> = repo_episodes
                .iter()
                .map(|(repo, eps)| (*repo, repo_context(eps, cutoff)))
                .collect();
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
                let ctx = PriorityContext {
                    roster: &priority_roster,
                    scope: &scope,
                    fleet_history,
                };
                let linked = star
                    .and_then(|s| s.repos.get(&pr.repo))
                    .and_then(|r| r.linked_at(pr.number, cutoff));
                let own_flags = PriorityState::from_flags(&pr.flags, cutoff).unwrap_or_default();
                let prio_v2 = priority_inputs(&subject, &own_flags, linked.as_ref(), &ctx, t);
                let loops = loop_features(
                    &LoopInputs {
                        repo: &pr.repo,
                        pr: pr.number,
                        own: &pr.episodes,
                        repo_episodes: context.get(pr.repo.as_str()).map_or(&[], Vec::as_slice),
                        files,
                        ci: None,
                    },
                    cutoff,
                );
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
                        prio_v2,
                        loops,
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
    let mut priority_inputs = Vec::with_capacity(keyed.len());
    let mut loops = Vec::with_capacity(keyed.len());
    for ((key, _), (row, prio, prio_v2, lp)) in keyed {
        row_keys.push(key);
        rows.push(row);
        priority.push(prio);
        priority_inputs.push(prio_v2);
        loops.push(lp);
    }

    Assembled {
        rows,
        row_keys,
        dwells: dwells(&prs, window_start, horizon),
        priority,
        priority_inputs,
        loops,
        stats,
        data_through: horizon,
    }
}

/// A dwell's canonical sort key: `(stage, repo, pr, entered_at)`.
type DwellKey<'a> = (FitStage, &'a str, u32, DateTime<Utc>);

/// One assembled row with its sort key, v1 priority candidates, v2 inputs and
/// friction predictors (#10521).
type KeyedRow = (
    (RowKey, FitStage),
    (TrainingRow, PriorityFeatures, PriorityInputs, LoopFeatures),
);

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
