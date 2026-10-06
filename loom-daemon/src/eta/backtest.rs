//! Leak-free replay of a heuristic against real outcomes (#9325, the first
//! gate for promoting a heuristic — Phase 2 of #9289).
//!
//! # Where a replay case comes from
//!
//! A completed `sweep.outcome` record's own `phase_durations` sequence
//! answers, for every stage the sweep passed through, both the exact
//! remaining time to the sweep's terminal state (`finish`) and — when the
//! sequence ends with a `merge` phase — to the PR's landing (`land`).
//! [`cases_from_record`] reconstructs each stage's entry instant by summing
//! durations backward from the record's `emitted_at` (treated as the
//! sweep's completion instant), so a case's `as_of` is always strictly
//! before the record itself was observed.
//!
//! # Why that is leak-free
//!
//! [`run`] scores every case with [`crate::eta::Heuristic::estimate`], which
//! reaches history only through [`StageSamples::select`]/`select_at` — and
//! those refuse any sample observed at or after `as_of`. A record's own
//! stage samples all share its `observed_at` (the record's `emitted_at`),
//! which is never earlier than any case's `as_of` reconstructed from it, so
//! `select_at` excludes a case's own record by construction: a replay can
//! see other, earlier history, but never its own future. [`tests`] pins
//! this with a case whose answer provably does not move when a
//! would-be-leaking future sample is added to the history it replays
//! against.
//!
//! # Coverage: `land` cases from an in-sweep merge, or from the forge
//!
//! [`cases_from_record`] yields a [`Kind::Land`] case only where the record's
//! phase sequence ends with a `merge` — the one place a `sweep.outcome`
//! record witnesses the landing itself. Where the fleet merges out of sweep
//! (Champion's auto-merge), no local record ends in `merge`, so that source
//! alone gives `land-v1` zero cases even though `eta backfill` gives it
//! plenty of `merge_wait` *samples* to estimate from. Samples and cases are
//! different things.
//!
//! The second `land` source (#9579) is the merged PR's own forge label
//! timeline ([`pr_cases::cases_from_pr_history`]): every stage entry the
//! shared label resolver names (`merge_hold` included, #10305) is a replay
//! instant and the PR's `merged_at` its answer. It is opt-in at the CLI (`eta backtest --pr-history` /
//! `--forge-pr-cases`) so a default backtest stays offline, and its own
//! leak-freedom argument is made in [`pr_cases`] rather than inherited from
//! the one above. A case both sources answer is counted once
//! ([`merge_case_sets`]).
//!
//! # `start` cases come from the stage journal (#9326)
//!
//! No `sweep.outcome` record witnesses a queue wait, so [`Kind::Start`]
//! cases come from the tracker's own journal instead
//! ([`cases_from_journal`]): the plan inputs recorded at a ready item's
//! first sighting, and its dispatch.

use super::history::StageSamples;
use super::journal::JournalEntry;
use super::score::{score, EstimateSummary, OutcomeKind, Score};
use super::tracker::READY_FIRST_SEEN;
use super::{
    explanation, AgeSource, CurrentStage, CurrentState, DispatchInput, EstimateInput, Heuristic,
    Kind, Provenance, Stage, Subject,
};
use crate::telemetry::{SweepOutcomeRecord, SweepResult, TelemetryEnvelope, TelemetryRecord};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub mod pr_cases;
pub use pr_cases::{
    cases_from_pr_history, cases_from_pr_records, parse_pr_records, pr_case_entries, PrCaseEntries,
    PrCaseExclusion, PrCaseRecord, PrCaseSummary, RefusedEntry,
};

/// Bucket for a record whose `repo` slug was unresolved (Issue #9442),
/// mirroring [`super::history`]'s own sentinel.
const UNRESOLVED_REPO_BUCKET: &str = "(unresolved)";

/// One replayable instant: predict from here, using only what was known
/// then, and compare to what really happened.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayCase {
    /// What is estimated.
    pub subject: Subject,
    /// The replay instant: the reconstructed stage-entry time.
    pub as_of: DateTime<Utc>,
    /// The stage the case is replayed from.
    pub stage: Stage,
    /// Judge rejections already taken at this instant.
    pub rework_rounds: u32,
    /// Which heuristic kind this case answers.
    pub kind: Kind,
    /// How it actually resolved.
    pub outcome: OutcomeKind,
    /// When.
    pub actual_at: DateTime<Utc>,
    /// A `ready_wait` case only (#9326): the dispatch-plan inputs the
    /// tracker read at `as_of`, replayed verbatim.
    pub dispatch: Option<DispatchInput>,
    /// Seconds already spent in `stage` at `as_of` (#10210). Every case
    /// derived from a record replays from the stage's entry (`0`); a case
    /// replayed mid-stage — the only kind that can outlive its history —
    /// sets it.
    pub age_sec: i64,
    /// The per-stage queue context at `as_of` (#10208), replayed verbatim
    /// into [`EstimateInput::queue`]. Empty for a case built from a record
    /// that carries none (every `sweep.outcome`-derived case today); a
    /// `land` case source (#9579) fills it from the stage journal with
    /// [`super::stage_queue::stage_queue`], the same function the tracker
    /// serves from.
    pub queue: Vec<super::stage_queue::StageQueue>,
}

/// Every finish/land replay case one `sweep.outcome` record's own phase
/// sequence answers, observed (emitted) at `observed_at`.
///
/// A record whose only entry spans the whole sweep (the journal's fallback
/// when no phase was ever sampled, mirroring
/// [`StageSamples::push_outcome`]'s own skip) yields no case: its duration
/// is not attributable to the one phase it names.
#[must_use]
pub fn cases_from_record(
    record: &SweepOutcomeRecord,
    observed_at: DateTime<Utc>,
) -> Vec<ReplayCase> {
    let n = record.phase_durations.len();
    let fallback = n == 1
        && record.phase_durations[0].duration_sec == record.total_duration_sec
        && record.total_duration_sec > 0;
    if fallback || n == 0 {
        return Vec::new();
    }

    let repo = record
        .repo
        .clone()
        .unwrap_or_else(|| UNRESOLVED_REPO_BUCKET.to_string());
    let landed = record
        .phase_durations
        .last()
        .is_some_and(|p| p.phase == "merge");

    // `remaining[i]` = seconds from entering stage `i` to the record's own
    // completion instant: the sum of `i`'s own duration and every later one.
    let mut remaining = vec![0_i64; n];
    let mut acc = 0_i64;
    for i in (0..n).rev() {
        acc += record.phase_durations[i].duration_sec.max(0);
        remaining[i] = acc;
    }

    let mut subject = Subject::new(&repo, None, record.issue);
    subject.pr_number = record.pr_number;
    subject.sweep_id = Some(record.sweep_id.clone());

    let mut cases = Vec::new();
    let mut rework_rounds = 0_u32;
    for (i, phase) in record.phase_durations.iter().enumerate() {
        let Some(stage) = Stage::from_sweep_phase(&phase.phase) else {
            continue;
        };
        let as_of = observed_at - Duration::seconds(remaining[i]);
        cases.push(ReplayCase {
            subject: subject.clone(),
            as_of,
            stage,
            rework_rounds,
            kind: Kind::Finish,
            outcome: OutcomeKind::Finished,
            actual_at: observed_at,
            dispatch: None,
            age_sec: 0,
            queue: Vec::new(),
        });
        if landed {
            cases.push(ReplayCase {
                subject: subject.clone(),
                as_of,
                stage,
                rework_rounds,
                kind: Kind::Land,
                outcome: OutcomeKind::Landed,
                actual_at: observed_at,
                dispatch: None,
                age_sec: 0,
                queue: Vec::new(),
            });
        }
        if stage == Stage::Doctor {
            rework_rounds += 1;
        }
    }
    cases
}

/// Every case every `sweep.outcome` envelope answers.
#[must_use]
pub fn cases_from_envelopes<'a>(
    envelopes: impl IntoIterator<Item = &'a TelemetryEnvelope>,
) -> Vec<ReplayCase> {
    let mut cases = Vec::new();
    for envelope in envelopes {
        if let TelemetryRecord::SweepOutcome(record) = &envelope.record {
            if record.result == SweepResult::Success || record.result == SweepResult::Failure {
                cases.extend(cases_from_record(record, envelope.emitted_at));
            }
        }
    }
    cases
}

/// Every `start` case the ETA stage journal answers (#9326): a ready item's
/// `ready.first_seen` row carrying the plan inputs it was estimated from,
/// paired with the same item's next `sweep.dispatch` row leaving
/// `ready_wait`. The case replays from the first sighting, so the only
/// turnover samples it can see are ones observed before it (leak-free by
/// the same `select` rule as every other case). A first sighting the plan
/// gave no position, or one never dispatched, yields no case.
#[must_use]
pub fn cases_from_journal(entries: &[JournalEntry]) -> Vec<ReplayCase> {
    let mut open: BTreeMap<(String, u32), (DateTime<Utc>, DispatchInput)> = BTreeMap::new();
    let mut cases = Vec::new();
    for entry in entries {
        let Some(issue) = entry.issue else {
            continue;
        };
        let key = (entry.repo.clone(), issue);
        if entry.event == READY_FIRST_SEEN {
            match serde_json::from_value::<DispatchInput>(entry.raw["dispatch"].clone()) {
                Ok(dispatch) => {
                    open.insert(key, (entry.observed_at, dispatch));
                }
                Err(_) => {
                    open.remove(&key);
                }
            }
        } else if entry.event == "sweep.dispatch" && entry.stage == Some(Stage::ReadyWait) {
            let Some((as_of, dispatch)) = open.remove(&key) else {
                continue;
            };
            let actual_at = entry.left_at.unwrap_or(entry.observed_at);
            if actual_at < as_of {
                continue;
            }
            cases.push(ReplayCase {
                subject: Subject::new(&entry.repo, None, issue),
                as_of,
                stage: Stage::ReadyWait,
                rework_rounds: 0,
                kind: Kind::Start,
                outcome: OutcomeKind::Started,
                actual_at,
                dispatch: Some(dispatch),
                age_sec: 0,
                queue: Vec::new(),
            });
        }
    }
    cases
}

/// The identity a replay case has whichever source derived it: subject
/// (repo, case-insensitive, and issue), kind, stage, and which entry of that
/// stage it is — the *n*-th, by `as_of`, among the subject's cases of that
/// kind and stage in the same set. Two sources disagree on the exact `as_of`
/// (a sweep's is reconstructed from phase durations, the forge's is a label
/// event), so the instant itself cannot be the key; the lap ordinal can.
type CaseIdentity = (String, u32, Kind, Stage, usize);

fn identities(cases: &[ReplayCase]) -> Vec<CaseIdentity> {
    let mut groups: BTreeMap<(String, u32, Kind, Stage), Vec<usize>> = BTreeMap::new();
    for (i, c) in cases.iter().enumerate() {
        groups
            .entry((c.subject.repo.to_ascii_lowercase(), c.subject.issue, c.kind, c.stage))
            .or_default()
            .push(i);
    }
    let mut ids = vec![(String::new(), 0, Kind::Land, Stage::ReviewWait, 0); cases.len()];
    for ((repo, issue, kind, stage), mut members) in groups {
        members.sort_by_key(|&i| (cases[i].as_of, i));
        for (ordinal, i) in members.into_iter().enumerate() {
            ids[i] = (repo.clone(), issue, kind, stage, ordinal);
        }
    }
    ids
}

/// `primary` plus every `secondary` case that is not already in it (#9579).
/// Returns the merged set and how many `secondary` cases were dropped.
///
/// Every `primary` case is kept as-is. `secondary` is first reduced to
/// distinct cases (the same PR read twice — an offline file and a forge
/// fetch — is the same case, not two), then any case whose
/// [`CaseIdentity`] `primary` already holds is dropped: a PR merged inside a
/// sweep is answered by both its `sweep.outcome` record and its forge
/// timeline, and must be scored once. Genuine second laps keep distinct
/// ordinals and so are never collapsed.
#[must_use]
pub fn merge_case_sets(
    primary: Vec<ReplayCase>,
    secondary: Vec<ReplayCase>,
) -> (Vec<ReplayCase>, usize) {
    let before = secondary.len();
    let mut seen_exact = BTreeSet::new();
    let distinct: Vec<ReplayCase> = secondary
        .into_iter()
        .filter(|c| {
            seen_exact.insert((
                c.subject.repo.to_ascii_lowercase(),
                c.subject.issue,
                c.subject.pr_number,
                c.kind,
                c.stage,
                c.as_of,
            ))
        })
        .collect();
    let taken: BTreeSet<CaseIdentity> = identities(&primary).into_iter().collect();
    let ids = identities(&distinct);
    let mut merged = primary;
    let mut added = 0_usize;
    for (case, id) in distinct.into_iter().zip(ids) {
        if !taken.contains(&id) {
            merged.push(case);
            added += 1;
        }
    }
    (merged, before - added)
}

/// One dimension's aggregate: mean pinball loss, p25-p75 coverage and bias
/// (mean signed error) over every scored case, plus how many were refused.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Bucket {
    /// Cases replayed into this bucket.
    pub n: usize,
    /// Of those, how many the heuristic actually scored (not refused).
    pub scored: usize,
    /// `n - scored`: the heuristic had no estimate.
    pub refused: usize,
    /// Mean pinball loss over the scored cases, seconds. `None` when none
    /// scored.
    pub mean_pinball_loss_sec: Option<f64>,
    /// Fraction of scored cases whose actual fell in `[p25, p75]`.
    pub coverage: Option<f64>,
    /// Mean `actual − p50` over the scored cases: positive is a heuristic
    /// that runs early (underestimates), negative one that runs late.
    pub bias_sec: Option<f64>,
}

fn bucket_of(scores: &[&Score]) -> Bucket {
    let n = scores.len();
    let scored: Vec<&&Score> = scores
        .iter()
        .filter(|s| s.pinball_loss_sec.is_some())
        .collect();
    let scored_n = scored.len();
    let mean_pinball_loss_sec = (scored_n > 0).then(|| {
        scored
            .iter()
            .map(|s| s.pinball_loss_sec.unwrap_or(0.0))
            .sum::<f64>()
            / scored_n as f64
    });
    let coverage = (scored_n > 0).then(|| {
        scored.iter().filter(|s| s.covered == Some(true)).count() as f64 / scored_n as f64
    });
    let bias_sec = (scored_n > 0).then(|| {
        scored
            .iter()
            .map(|s| s.error_sec.unwrap_or(0) as f64)
            .sum::<f64>()
            / scored_n as f64
    });
    Bucket {
        n,
        scored: scored_n,
        refused: n - scored_n,
        mean_pinball_loss_sec,
        coverage,
        bias_sec,
    }
}

/// A heuristic's backtest, over every case its own kind answers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BacktestReport {
    /// The heuristic id backtested.
    pub heuristic: String,
    /// Which kind it predicts.
    pub kind: Kind,
    /// Every case replayed, before any bucketing.
    pub overall: Bucket,
    /// Per-repo breakdown.
    pub by_repo: BTreeMap<String, Bucket>,
    /// Per-horizon-bucket breakdown ([`super::score::bucket`] of the
    /// predicted `p50`).
    pub by_horizon: BTreeMap<String, Bucket>,
    /// How far the predicted landing instant moves between consecutive
    /// cases of one series that both answered (#10233). Diagnostic; not a gate.
    #[serde(default)]
    pub stability: Stability,
    /// Interval width per bucket of the actual lead (#10233). Diagnostic;
    /// not a gate.
    #[serde(default)]
    pub convergence: BTreeMap<String, Convergence>,
    /// Ordinary estimates (`normal`) apart from residual-life tail guesses
    /// (`tail_extrapolated`, #10210), so a tail guess never hides inside the
    /// ordinary accuracy. Present only when some case was tail-extrapolated;
    /// `overall` still counts every case.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_tail: BTreeMap<String, Bucket>,
}

/// `by_tail` key for an ordinary estimate (or a refusal).
pub const TAIL_NONE: &str = "normal";

/// `by_tail` key for a residual-life tail estimate (#10210).
pub const TAIL_EXTRAPOLATED: &str = "tail_extrapolated";

/// Cases [`run`] and [`compare`] accept in addition to `heuristic.kind()`:
/// narrows the replay set the same way `eta backtest`'s own flags do.
#[derive(Debug, Clone, Copy, Default)]
pub struct Filter<'a> {
    /// Only cases at or after this instant.
    pub since: Option<DateTime<Utc>>,
    /// Only this repo (case-insensitive).
    pub repo: Option<&'a str>,
}

fn matches(case: &ReplayCase, kind: Kind, filter: Filter<'_>) -> bool {
    case.kind == kind
        && filter.since.is_none_or(|s| case.as_of >= s)
        && filter
            .repo
            .is_none_or(|r| case.subject.repo.eq_ignore_ascii_case(r))
}

/// The estimator input a replay case describes: its own `as_of`, its stage
/// entered at that instant, no features.
fn case_input(case: &ReplayCase, loom: &Provenance) -> EstimateInput {
    EstimateInput {
        subject: case.subject.clone(),
        as_of: case.as_of,
        current: CurrentState::At(CurrentStage {
            stage: case.stage,
            entered_at: Some(case.as_of - Duration::seconds(case.age_sec.max(0))),
            age_sec: case.age_sec.max(0),
            age_source: AgeSource::TrackerObserved,
            rework_rounds: case.rework_rounds,
            episode_entered_at: None,
        }),
        features: explanation::Features::default(),
        features_omitted: Vec::new(),
        provenance: loom.clone(),
        dispatch: case.dispatch.clone(),
        // No point-in-time stall state is reconstructed for a replay.
        stalls: Vec::new(),
        held: None,
        queue: case.queue.clone(),
        // No point-in-time dependency graph is reconstructed here (#10510).
        dependencies: None,
    }
}

/// Replay every case matching `heuristic.kind()` and `filter` against
/// `history`, leak-free: each case's own `as_of` is what
/// [`super::Heuristic::estimate`] sees, and history excludes anything not
/// strictly earlier via [`StageSamples::select`]/`select_at`.
///
/// `loom` is the build stamped on every replayed estimate. It is carried,
/// never read by the numeric path, so a report does not move with the
/// binary under test — which is what lets a golden pin one.
#[must_use]
pub fn run(
    heuristic: &dyn Heuristic,
    history: &StageSamples,
    cases: &[ReplayCase],
    filter: Filter<'_>,
    loom: &Provenance,
) -> BacktestReport {
    let replayed = replay(heuristic, history, cases, filter, loom);
    report_of(heuristic, &replayed)
}

/// Every case matching `heuristic.kind()` and `filter`, replayed and scored,
/// in `cases` order.
fn replay(
    heuristic: &dyn Heuristic,
    history: &StageSamples,
    cases: &[ReplayCase],
    filter: Filter<'_>,
    loom: &Provenance,
) -> Vec<Replayed> {
    let kind = heuristic.kind();
    cases
        .iter()
        .filter(|case| matches(case, kind, filter))
        .map(|case| {
            let input = case_input(case, loom);
            let summary = EstimateSummary::of(&heuristic.estimate(&input, history));
            let score = score(&summary, case.outcome, case.actual_at, &[]);
            Replayed {
                case: case.clone(),
                summary,
                score,
            }
        })
        .collect()
}

fn report_of(heuristic: &dyn Heuristic, replayed: &[Replayed]) -> BacktestReport {
    let kind = heuristic.kind();
    let all: Vec<&Score> = replayed.iter().map(|r| &r.score).collect();
    let overall = bucket_of(&all);

    let mut by_repo_scores: BTreeMap<String, Vec<&Score>> = BTreeMap::new();
    let mut by_horizon_scores: BTreeMap<String, Vec<&Score>> = BTreeMap::new();
    let mut by_tail_scores: BTreeMap<String, Vec<&Score>> = BTreeMap::new();
    for Replayed {
        case,
        score: s,
        summary,
    } in replayed
    {
        let key = if summary.tail_extrapolated {
            TAIL_EXTRAPOLATED
        } else {
            TAIL_NONE
        };
        by_tail_scores.entry(key.to_string()).or_default().push(s);
        by_repo_scores
            .entry(case.subject.repo.clone())
            .or_default()
            .push(s);
        let horizon = s
            .horizon_bucket
            .clone()
            .unwrap_or_else(|| "refused".to_string());
        by_horizon_scores.entry(horizon).or_default().push(s);
    }
    let by_repo = by_repo_scores
        .into_iter()
        .map(|(k, v)| (k, bucket_of(&v)))
        .collect();
    let by_horizon = by_horizon_scores
        .into_iter()
        .map(|(k, v)| (k, bucket_of(&v)))
        .collect();
    // Only a report that has a tail-extrapolated case carries the split, so
    // every report without one (every pre-#10210 heuristic's) is unchanged.
    let by_tail = if by_tail_scores.contains_key(TAIL_EXTRAPOLATED) {
        by_tail_scores
            .into_iter()
            .map(|(k, v)| (k, bucket_of(&v)))
            .collect()
    } else {
        BTreeMap::new()
    };

    BacktestReport {
        heuristic: heuristic.id().to_string(),
        kind,
        overall,
        by_repo,
        by_horizon,
        stability: paired::stability_of(replayed),
        convergence: paired::convergence_of(replayed),
        by_tail,
    }
}

/// The calibration observations a replay of `base` over `cases` yields
/// (#10207): one per `land` case `base` estimated, landing at the case's
/// `actual_at` and known from that instant on.
///
/// This is what lets `eta backtest` score a recalibrating heuristic from the
/// journals alone, with no live outcome log. It stays leak-free because the
/// recalibrating heuristic fits its table at each case's own `as_of`
/// ([`super::recalibrate::fit_table`]): a case's own landing (and every
/// later one) is known only at or after its `actual_at`, which is never
/// before its `as_of`, so at most it enters as a censored lower bound.
#[must_use]
pub fn calibration_from_replay(
    base: &dyn Heuristic,
    history: &StageSamples,
    cases: &[ReplayCase],
    loom: &Provenance,
) -> Vec<super::recalibrate::CalibrationObservation> {
    cases
        .iter()
        .filter(|case| case.kind == Kind::Land && base.kind() == Kind::Land)
        .filter_map(|case| {
            let input = case_input(case, loom);
            let summary = EstimateSummary::of(&base.estimate(&input, history));
            let s = score(&summary, case.outcome, case.actual_at, &[]);
            super::recalibrate::CalibrationObservation::from_scored(&summary, &s, case.actual_at)
        })
        .collect()
}

/// A paired comparison of two heuristics on the identical replay set —
/// operator decision 2 on #9289: the first gate for promoting a heuristic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Comparison {
    /// The first heuristic's report.
    pub a: BacktestReport,
    /// The second heuristic's report.
    pub b: BacktestReport,
    /// Both on the union of cases, every figure on its common decidable
    /// subset, with the walk-forward daily folds (#10233).
    #[serde(default)]
    pub paired: Paired,
    /// The better heuristic id on [`Self::paired`] (see
    /// [`compare`]). `None` when neither may win, nothing was paired, or they
    /// tie exactly.
    pub better: Option<String>,
}

/// Backtest `a` and `b` on the same `cases`/`history`/`filter`, and rank
/// them on the **union** of cases, counting refusals (#10233).
///
/// Each report's own `overall` bucket is over the cases *that* heuristic
/// answered, so ranking on it let a heuristic that refuses the slowest cases
/// win by refusing them. The ranking reads [`Paired`] instead: the deciding
/// loss (`pinball4_loss_sec`, lower is better — the proper scoring rule this
/// module is built on) over the cases both scored, and a side may win only
/// if its answer rate over the union and its late-surprise rate do not
/// regress on the other's (the live gate's slacks). An exact loss tie goes to
/// the side that answered more.
///
/// # Errors
///
/// `a` and `b` predict different [`Kind`]s. [`run`] only ever scores cases
/// of its own heuristic's kind, so two kinds are two *disjoint* replay
/// sets — the losses would not be comparable and the "better" answer would
/// be meaningless. A promotion gate must not silently return one.
pub fn compare(
    a: &dyn Heuristic,
    b: &dyn Heuristic,
    history: &StageSamples,
    cases: &[ReplayCase],
    filter: Filter<'_>,
    loom: &Provenance,
) -> Result<Comparison, KindMismatch> {
    if a.kind() != b.kind() {
        return Err(KindMismatch {
            a: (a.id().to_string(), a.kind()),
            b: (b.id().to_string(), b.kind()),
        });
    }
    let replayed_a = replay(a, history, cases, filter, loom);
    let replayed_b = replay(b, history, cases, filter, loom);
    let paired = paired::paired_of(&replayed_a, &replayed_b);
    let better =
        paired::better_side(&paired).map(|a_wins| if a_wins { a.id() } else { b.id() }.to_string());
    Ok(Comparison {
        a: report_of(a, &replayed_a),
        b: report_of(b, &replayed_b),
        paired,
        better,
    })
}

#[path = "backtest_paired.rs"]
mod paired;

use paired::Replayed;
pub use paired::{Convergence, Fold, Paired, Stability};

/// [`compare`] was asked to rank two heuristics that predict different
/// kinds, whose replay sets do not overlap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindMismatch {
    /// The first heuristic's id and kind.
    pub a: (String, Kind),
    /// The second's.
    pub b: (String, Kind),
}

impl std::fmt::Display for KindMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "cannot pair {} ({}) against {} ({}): different kinds replay disjoint case sets, \
             so their losses are not comparable",
            self.a.0, self.a.1, self.b.0, self.b.1
        )
    }
}

impl std::error::Error for KindMismatch {}
