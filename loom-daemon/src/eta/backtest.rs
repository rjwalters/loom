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
//! # Coverage: `land` cases need an in-sweep merge
//!
//! A [`Kind::Land`] case exists only where the record's phase sequence ends
//! with a `merge` — the one place a `sweep.outcome` record witnesses the
//! landing itself. Where the fleet merges out of sweep (Champion's
//! auto-merge), no local record ends in `merge` and `land-v1` backtests
//! against zero cases, even though `eta backfill` gives it plenty of
//! `merge_wait` *samples* to estimate from. Samples and cases are different
//! things, and only the latter is missing. Deriving `land` cases from the
//! same `pr_latency` histories backfill already reads (the real
//! `review-requested → merged` lead time) is the fix, tracked separately;
//! it is not a leakage hazard, just a gap in what can be scored.
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
use std::collections::BTreeMap;

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
            });
        }
    }
    cases
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
}

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
    let kind = heuristic.kind();
    let mut scores: Vec<(ReplayCase, Score)> = Vec::new();
    for case in cases {
        if !matches(case, kind, filter) {
            continue;
        }
        let input = EstimateInput {
            subject: case.subject.clone(),
            as_of: case.as_of,
            current: CurrentState::At(CurrentStage {
                stage: case.stage,
                entered_at: Some(case.as_of),
                age_sec: 0,
                age_source: AgeSource::TrackerObserved,
                rework_rounds: case.rework_rounds,
            }),
            features: explanation::Features::default(),
            features_omitted: Vec::new(),
            provenance: loom.clone(),
            dispatch: case.dispatch.clone(),
        };
        let explanation = heuristic.estimate(&input, history);
        let summary = EstimateSummary::of(&explanation);
        let s = score(&summary, case.outcome, case.actual_at, &[]);
        scores.push((case.clone(), s));
    }

    let all: Vec<&Score> = scores.iter().map(|(_, s)| s).collect();
    let overall = bucket_of(&all);

    let mut by_repo_scores: BTreeMap<String, Vec<&Score>> = BTreeMap::new();
    let mut by_horizon_scores: BTreeMap<String, Vec<&Score>> = BTreeMap::new();
    for (case, s) in &scores {
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

    BacktestReport {
        heuristic: heuristic.id().to_string(),
        kind,
        overall,
        by_repo,
        by_horizon,
    }
}

/// A paired comparison of two heuristics on the identical replay set —
/// operator decision 2 on #9289: the first gate for promoting a heuristic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Comparison {
    /// The first heuristic's report.
    pub a: BacktestReport,
    /// The second heuristic's report.
    pub b: BacktestReport,
    /// The heuristic id with the lower `overall.mean_pinball_loss_sec`.
    /// `None` when neither scored anything, or they tie exactly.
    pub better: Option<String>,
}

/// Backtest `a` and `b` on the same `cases`/`history`/`filter`, and rank
/// them by overall mean pinball loss — lower is better, the proper scoring
/// rule this whole module is built on.
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
    let ra = run(a, history, cases, filter, loom);
    let rb = run(b, history, cases, filter, loom);
    let better = match (ra.overall.mean_pinball_loss_sec, rb.overall.mean_pinball_loss_sec) {
        (Some(pa), Some(pb)) if pa < pb => Some(ra.heuristic.clone()),
        (Some(pa), Some(pb)) if pb < pa => Some(rb.heuristic.clone()),
        _ => None,
    };
    Ok(Comparison {
        a: ra,
        b: rb,
        better,
    })
}

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
