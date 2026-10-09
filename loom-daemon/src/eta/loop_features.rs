//! Friction predictors from the 2026-10-06 error analysis (#10521): the
//! review-loop history, the repo's Judge rejection rate, the cumulative
//! stage-age form, the open-PR file overlap and the PR's own last CI failure.
//! **One** definition for training and serving, kept apart from
//! [`super::fit::features`] on purpose (the model of
//! [`super::priority_features`], #10333).
//!
//! # Not enabled for any shipped heuristic
//!
//! [`LOOP_FEATURES`] are the candidate inputs of the *next* datestamped
//! shadow heuristic. The fit's schema (`eta-fit/v1`,
//! [`super::fit::FEATURES`]) does not list them, `age_h` keeps its meaning
//! (time since the last stage entry), and no coefficient file, explanation or
//! fixture changes. A new heuristic opts in under its own schema version.
//!
//! # Two call sites, one builder
//!
//! - **Fit**: [`super::fit::rows`] records one [`LoopFeatures`] per training
//!   row (`Assembled::loops`) at `t - lag`, from the fleet snapshots'
//!   episodes.
//! - **Serving**: `Tracker::loop_features_of` reads the same fleet
//!   snapshots' label timeline (#10500) at `now - lag`.
//!
//! Both go through [`loop_features`]; the parity test pins that the same
//! episodes give the same answer on either path.
//!
//! # The predictors
//!
//! | # | feature | source |
//! |---|---|---|
//! | 1 | [`LoopFeatures::overlap_prs`], [`LoopFeatures::overlap_files`] | each open PR's changed-file list **as known at `as_of`** ([`FileSnapshot`]); `None` if the subject's or any open peer's list is unknown then |
//! | 2 | [`LoopFeatures::review_requests`], [`LoopFeatures::approvals_lost`] | stage episodes |
//! | 3 | [`LoopFeatures::own_ci_failed`] | the PR's last CI run before `as_of` ([`CiObservation`]; SigNoz `ci.run` first) |
//! | 4 | [`LoopFeatures::judge_reject_rate_7d`] | the repo's `review_wait` exits over the trailing 7 days |
//! | - | [`LoopFeatures::cum_stage_h`] | stage episodes: time in the current stage summed over **every** visit |
//!
//! `cum_stage_h` is the stage-age fix: the stage's own `age_h` restarts at
//! every loop, so a PR on its fourth approval looks minutes old. The
//! cumulative form is fed through `ln(1 + h)`, which flattens the hazard with
//! age instead of growing it linearly.
//!
//! # Knowability
//!
//! Everything reads facts strictly before `as_of` (callers pass `t - lag`
//! when training, the serving instant when serving): an episode is counted
//! once entered before `as_of`, as ended only if it ended before `as_of`, and
//! only the latest file list / CI run observed before `as_of` is used, never
//! the PR's final diff. A fact that is not logged (`files` or `ci` of `None`)
//! is `None`, never defaulted, and the vector carries an explicit `*_known`
//! indicator so a missing source cannot masquerade as "no overlap".
//!
//! Pure: reads its arguments and nothing else.

use super::episodes::{EpisodeEnd, EpisodeNext, StageEpisode};
use super::fit::rows::is_open_at;
use super::Stage;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// The candidate feature names, in [`loop_vector`] order. None is in
/// [`super::fit::FEATURES`] (pinned by a test).
pub const LOOP_FEATURES: [&str; 11] = [
    "log_cum_stage",
    "log_review_requests",
    "log_approvals_lost",
    "judge_reject_rate_7d",
    "judge_rate_known",
    "log_overlap_prs",
    "log_overlap_files",
    "overlap_known",
    "own_ci_failed",
    "ci_known",
    "stage_looped",
];

/// `LOOP_FEATURES.len()`.
pub const N_LOOP_FEATURES: usize = LOOP_FEATURES.len();

/// The trailing window of the Judge rejection rate, in days.
pub const JUDGE_WINDOW_DAYS: i64 = 7;

/// Fewest Judge verdicts in the window for a rate; fewer is `None`, not a
/// noisy ratio.
pub const MIN_JUDGE_VERDICTS: u32 = 5;

/// One PR's changed-file list as observed at `known_at`: the head commit's
/// files then, never the final diff. A PR may have several snapshots; the
/// builder takes the latest before `as_of`, and an incomplete latest one
/// (`complete: false`) makes the list unknown then, never an older complete
/// list served as current.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSnapshot {
    /// `owner/repo`.
    pub repo: String,
    /// The PR.
    pub pr: u32,
    /// When the list was read.
    pub known_at: DateTime<Utc>,
    /// The changed paths; empty and meaningless when not `complete`.
    pub files: Vec<String>,
    /// The head commit the list describes, when the read could tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    /// Whether `files` is the PR's whole list at that head. `false` records
    /// that the read returned a possibly truncated or head-inconsistent page:
    /// the list is unknown from `known_at` on. Absent (older lines) is
    /// `true`: only complete lists were logged before the field existed.
    #[serde(default = "complete_default")]
    pub complete: bool,
    /// Lines added across the PR at that head, summed from the per-file
    /// entries of the same page (no extra forge call). `None` when unknown:
    /// an older line, an incomplete page, or an entry without the stat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additions: Option<u32>,
    /// Lines deleted; same provenance and `None` rule as `additions`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletions: Option<u32>,
    /// How many file entries the page listed. Known even when `complete` is
    /// `false`, so a PR of `MAX_LISTED_FILES` or more reads as huge, never as
    /// unknown-and-small. `None` on older lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listed: Option<u32>,
}

const fn complete_default() -> bool {
    true
}

/// One finished CI run of a PR's head, as read from SigNoz `ci.run` (or the
/// forge to fill a gap).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiObservation {
    /// The PR.
    pub pr: u32,
    /// When the run finished.
    pub at: DateTime<Utc>,
    /// Whether it failed.
    pub failed: bool,
}

/// What the builder reads. `files` and `ci` are `None` when the source is
/// not logged for the caller: the dependent features are then `None`. File
/// lists are logged since #10550 ([`super::pr_file_log`]); CI runs are not
/// yet.
#[derive(Debug, Clone, Copy)]
pub struct LoopInputs<'a> {
    /// `owner/repo` of the subject.
    pub repo: &'a str,
    /// The subject PR.
    pub pr: u32,
    /// The subject's own episodes, every one of them (the loop history).
    pub own: &'a [&'a StageEpisode],
    /// The repo's episodes (the subject's may be included): the Judge
    /// rejection rate and which other PRs are open. Only those
    /// [`repo_relevant_at`] `as_of` are read, so a caller may pass that
    /// subset ([`repo_context`]) instead of the whole history.
    pub repo_episodes: &'a [&'a StageEpisode],
    /// File snapshots of the subject and the repo's other PRs.
    pub files: Option<&'a [FileSnapshot]>,
    /// CI runs of the subject.
    pub ci: Option<&'a [CiObservation]>,
}

/// The friction predictors of one PR at one instant.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LoopFeatures {
    /// Hours in the current stage summed over every visit up to `as_of`;
    /// `None` when the PR has no episode open at `as_of`.
    pub cum_stage_h: Option<f64>,
    /// Whether the current stage has been visited before (a loop).
    pub stage_looped: bool,
    /// `review_wait` entries so far: the review requests.
    pub review_requests: u32,
    /// Approvals lost so far: `merge_wait` or `merge_hold` left for
    /// `review_wait` (a stale-main re-review).
    pub approvals_lost: u32,
    /// Share of the repo's Judge verdicts in the trailing window that sent a
    /// PR to Doctor; `None` under [`MIN_JUDGE_VERDICTS`].
    pub judge_reject_rate_7d: Option<f64>,
    /// Other open PRs of the repo whose files (as known at `as_of`) share a
    /// path with the subject's; `None` when the subject has no list or any
    /// other open PR has none known before `as_of` (a partly observed
    /// roster is unknown, not zero).
    pub overlap_prs: Option<u32>,
    /// Distinct subject paths touched by another open PR; `None` exactly
    /// when [`Self::overlap_prs`] is.
    pub overlap_files: Option<u32>,
    /// Whether the subject's last CI run before `as_of` failed; `None` when
    /// none is known.
    pub own_ci_failed: Option<bool>,
}

fn hours(from: DateTime<Utc>, to: DateTime<Utc>) -> f64 {
    ((to - from).num_milliseconds().max(0) as f64) / 3_600_000.0
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Whether a repo episode can move [`loop_features`]' repo-level answers at
/// `as_of`: open then (the overlap roster), or a `review_wait` that ended in
/// the Judge window `[as_of - 7 d, as_of)`.
#[must_use]
pub fn repo_relevant_at(e: &StageEpisode, as_of: DateTime<Utc>) -> bool {
    if is_open_at(e, as_of) {
        return true;
    }
    let from = as_of - Duration::days(JUDGE_WINDOW_DAYS);
    e.stage == Stage::ReviewWait && e.ended_at().is_some_and(|at| at >= from && at < as_of)
}

/// The subset of `episodes` [`repo_relevant_at`] `as_of`: what a caller
/// evaluating many PRs at one instant computes once per repo.
#[must_use]
pub fn repo_context<'a>(
    episodes: &[&'a StageEpisode],
    as_of: DateTime<Utc>,
) -> Vec<&'a StageEpisode> {
    episodes
        .iter()
        .copied()
        .filter(|e| repo_relevant_at(e, as_of))
        .collect()
}

/// The latest snapshot of `pr` known strictly before `as_of`, or `None` when
/// there is none or the latest is an incomplete (unknown) observation: an
/// older complete list is not current once a later read could not confirm it.
fn latest_files<'a>(
    files: &'a [FileSnapshot],
    repo: &str,
    pr: u32,
    as_of: DateTime<Utc>,
) -> Option<&'a FileSnapshot> {
    files
        .iter()
        .filter(|s| s.pr == pr && s.repo.eq_ignore_ascii_case(repo) && s.known_at < as_of)
        .max_by_key(|s| s.known_at)
        .filter(|s| s.complete)
}

/// The repo's Judge rejection rate over the trailing window before `as_of`.
fn judge_rate(episodes: &[&StageEpisode], as_of: DateTime<Utc>) -> Option<f64> {
    let from = as_of - Duration::days(JUDGE_WINDOW_DAYS);
    let (mut rejected, mut total) = (0_u32, 0_u32);
    for e in episodes.iter().filter(|e| e.stage == Stage::ReviewWait) {
        if let EpisodeEnd::Left {
            at,
            next: EpisodeNext::Stage(next),
        } = e.end
        {
            if at < as_of && at >= from {
                match next {
                    Stage::Doctor => {
                        rejected += 1;
                        total += 1;
                    }
                    Stage::MergeWait | Stage::MergeHold => total += 1,
                    _ => {}
                }
            }
        }
    }
    (total >= MIN_JUDGE_VERDICTS).then(|| f64::from(rejected) / f64::from(total))
}

/// The friction predictors of the subject at `as_of`.
#[must_use]
pub fn loop_features(inputs: &LoopInputs<'_>, as_of: DateTime<Utc>) -> LoopFeatures {
    let same_repo = |e: &StageEpisode| e.repo.eq_ignore_ascii_case(inputs.repo);
    let mine: Vec<&StageEpisode> = inputs
        .own
        .iter()
        .copied()
        .filter(|e| same_repo(e) && e.pr_number == inputs.pr && e.entered_at < as_of)
        .collect();
    let mut out = LoopFeatures::default();

    // The last open episode, as `Timeline::open` picks (`rposition`); only
    // differs from the first when open episodes overlap.
    if let Some(current) = mine.iter().rev().find(|e| is_open_at(e, as_of)) {
        let visits: Vec<&&StageEpisode> =
            mine.iter().filter(|e| e.stage == current.stage).collect();
        out.cum_stage_h = Some(
            visits
                .iter()
                .map(|e| {
                    let end = e.ended_at().filter(|at| *at < as_of).unwrap_or(as_of);
                    hours(e.entered_at, end)
                })
                .sum(),
        );
        out.stage_looped = visits.len() > 1;
    }
    out.review_requests = count(mine.iter().filter(|e| e.stage == Stage::ReviewWait).count());
    out.approvals_lost = count(
        mine.iter()
            .filter(|e| matches!(e.stage, Stage::MergeWait | Stage::MergeHold))
            .filter(|e| {
                matches!(
                    e.end,
                    EpisodeEnd::Left { at, next: EpisodeNext::Stage(Stage::ReviewWait) }
                        if at < as_of
                )
            })
            .count(),
    );
    let repo_eps: Vec<&StageEpisode> = inputs
        .repo_episodes
        .iter()
        .copied()
        .filter(|e| same_repo(e) && repo_relevant_at(e, as_of))
        .collect();
    out.judge_reject_rate_7d = judge_rate(&repo_eps, as_of);

    if let Some(files) = inputs.files {
        if let Some(mine_files) = latest_files(files, inputs.repo, inputs.pr, as_of) {
            let subject: BTreeSet<&str> = mine_files.files.iter().map(String::as_str).collect();
            let open_others: BTreeSet<u32> = repo_eps
                .iter()
                .filter(|e| e.pr_number != inputs.pr && is_open_at(e, as_of))
                .map(|e| e.pr_number)
                .collect();
            let mut prs = 0_usize;
            let mut touched: BTreeSet<&str> = BTreeSet::new();
            // A peer whose list is not known before `as_of` makes the roster
            // partly observed: the counts stay `None` rather than reading as
            // a confident "no overlap".
            let mut complete = true;
            for pr in open_others {
                let Some(snap) = latest_files(files, inputs.repo, pr, as_of) else {
                    complete = false;
                    break;
                };
                let shared: Vec<&str> = snap
                    .files
                    .iter()
                    .map(String::as_str)
                    .filter(|f| subject.contains(f))
                    .collect();
                if !shared.is_empty() {
                    prs += 1;
                    touched.extend(shared);
                }
            }
            if complete {
                out.overlap_prs = Some(count(prs));
                out.overlap_files = Some(count(touched.len()));
            }
        }
    }
    if let Some(ci) = inputs.ci {
        out.own_ci_failed = ci
            .iter()
            .filter(|c| c.pr == inputs.pr && c.at < as_of)
            .max_by_key(|c| c.at)
            .map(|c| c.failed);
    }
    out
}

/// How many of a set of rows know each friction input: the coverage a fit
/// or a backtest reports next to its numbers (#10521). File overlap
/// is 0 for rows older than the file-list log (#10550) and own CI everywhere
/// until its source is logged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopCoverage {
    /// Rows counted.
    pub rows: usize,
    /// Rows with an open episode (a cumulative stage age).
    pub cum_stage_known: usize,
    /// Rows whose current stage was visited before.
    pub stage_looped: usize,
    /// Rows with a repo Judge rejection rate.
    pub judge_rate_known: usize,
    /// Rows with a complete file-overlap roster.
    pub overlap_known: usize,
    /// Rows with a known last CI run.
    pub ci_known: usize,
}

impl LoopCoverage {
    /// The coverage of `loops`.
    #[must_use]
    pub fn of(loops: &[LoopFeatures]) -> Self {
        let n = |f: fn(&LoopFeatures) -> bool| loops.iter().filter(|l| f(l)).count();
        LoopCoverage {
            rows: loops.len(),
            cum_stage_known: n(|l| l.cum_stage_h.is_some()),
            stage_looped: n(|l| l.stage_looped),
            judge_rate_known: n(|l| l.judge_reject_rate_7d.is_some()),
            overlap_known: n(|l| l.overlap_prs.is_some()),
            ci_known: n(|l| l.own_ci_failed.is_some()),
        }
    }
}

/// The model-ready vector in [`LOOP_FEATURES`] order: counts and hours as
/// `ln(1 + x)`, the rate raw, and a `*_known` indicator beside every feature
/// that can be missing (a missing value is 0 with its indicator 0).
#[must_use]
pub fn loop_vector(f: &LoopFeatures) -> [f64; N_LOOP_FEATURES] {
    let flag = |b: bool| if b { 1.0 } else { 0.0 };
    [
        f.cum_stage_h.unwrap_or(0.0).ln_1p(),
        f64::from(f.review_requests).ln_1p(),
        f64::from(f.approvals_lost).ln_1p(),
        f.judge_reject_rate_7d.unwrap_or(0.0),
        flag(f.judge_reject_rate_7d.is_some()),
        f64::from(f.overlap_prs.unwrap_or(0)).ln_1p(),
        f64::from(f.overlap_files.unwrap_or(0)).ln_1p(),
        flag(f.overlap_prs.is_some()),
        flag(f.own_ci_failed.unwrap_or(false)),
        flag(f.own_ci_failed.is_some()),
        flag(f.stage_looped),
    ]
}
