//! The walk-forward protocol, the censoring-aware scorer and the
//! issue-level bootstrap.
//!
//! # Protocol (all instants relative to the evaluation instant `N`)
//!
//! - **Fold for day `d`**: cutoff `T_d` = start of `d`. Train on estimates
//!   with `as_of ∈ [T_d − train_days, T_d)`, labels censored at `T_d`.
//!   Validate on `as_of ∈ [T_d, T_d + 24h)`.
//! - **Selection folds** (`N − (reported + selection)d` …): validation
//!   outcomes observed only through the **freeze** instant
//!   `N − reported·24h`, and censored there. Settings are frozen there.
//! - **Reported folds** (the last `reported` days): frozen settings,
//!   outcomes observed through `N`, still-open items censored at `N`.
//!
//! # Scoring a censored row
//!
//! An item still open at the horizon has a known lower bound on its
//! remaining time, `c = horizon − as_of`. The primary metric is the
//! **truncated pinball loss** `Σ_q ρ_q(min(y, c) − min(q̂, c))`, known for
//! every row, landed or not; for a landed row it is the ordinary pinball loss
//! ([`crate::eta::score::pinball`]). Coverage is counted only where it is
//! decidable: a landed row, or an open row already past its p75 (a known
//! miss). MAE and bias are over landed rows only, and say so: on their own
//! they reward optimism.

use super::candidate::{Fitted, Settings};
use super::dataset::{assert_point_in_time, Label, LeakError, Logged, Row};
use crate::eta::score::pinball;
use crate::eta::simulate::SplitMix64;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// The protocol's shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Protocol {
    /// The evaluation instant `N`.
    pub now: DateTime<Utc>,
    /// Training window length, days.
    pub train_days: i64,
    /// Selection folds (days), immediately before the reported ones.
    pub selection_folds: u32,
    /// Reported folds (days), ending at `now`.
    pub reported_folds: u32,
}

impl Protocol {
    /// The issue's protocol: 14-day training, 2 selection and 2 reported
    /// daily folds ending at `now`.
    #[must_use]
    pub fn standard(now: DateTime<Utc>) -> Self {
        Protocol {
            now,
            train_days: 14,
            selection_folds: 2,
            reported_folds: 2,
        }
    }

    /// When settings are frozen: `now − reported days`.
    #[must_use]
    pub fn freeze_at(&self) -> DateTime<Utc> {
        self.now - Duration::days(i64::from(self.reported_folds))
    }
}

/// Whether a fold chooses settings or is reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FoldRole {
    /// Chooses settings; scored with outcomes observed up to the freeze.
    Selection,
    /// Scored once, with frozen settings.
    Reported,
}

/// One walk-forward fold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fold {
    /// Selection or reported.
    pub role: FoldRole,
    /// Training `as_of` lower bound.
    pub train_from: DateTime<Utc>,
    /// The cutoff: training data knowable before it; validation starts here.
    pub cutoff: DateTime<Utc>,
    /// Validation `as_of` upper bound (exclusive).
    pub validate_until: DateTime<Utc>,
    /// Validation outcomes are observed (and censored) here.
    pub observe_until: DateTime<Utc>,
}

/// A protocol that would mix time between selection and reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolError(pub String);

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid protocol: {}", self.0)
    }
}

impl std::error::Error for ProtocolError {}

/// The folds of `p`, oldest first. Refuses a protocol whose selection
/// outcomes could be observed after the freeze, or whose freeze is after a
/// reported fold's cutoff.
pub fn plan(p: &Protocol) -> Result<Vec<Fold>, ProtocolError> {
    if p.train_days <= 0 || p.selection_folds == 0 || p.reported_folds == 0 {
        return Err(ProtocolError(
            "training days, selection folds and reported folds must all be positive".into(),
        ));
    }
    let freeze = p.freeze_at();
    let total = p.selection_folds + p.reported_folds;
    let folds: Vec<Fold> = (0..total)
        .map(|i| {
            let cutoff = p.now - Duration::days(i64::from(total - i));
            let role = if i < p.selection_folds {
                FoldRole::Selection
            } else {
                FoldRole::Reported
            };
            Fold {
                role,
                train_from: cutoff - Duration::days(p.train_days),
                cutoff,
                validate_until: cutoff + Duration::days(1),
                observe_until: match role {
                    FoldRole::Selection => freeze,
                    FoldRole::Reported => p.now,
                },
            }
        })
        .collect();
    for fold in &folds {
        let ok = match fold.role {
            FoldRole::Selection => fold.observe_until <= freeze && fold.validate_until <= freeze,
            FoldRole::Reported => fold.cutoff >= freeze,
        };
        if !ok || fold.validate_until > fold.observe_until {
            return Err(ProtocolError(format!(
                "fold at {} breaks temporal nesting around the freeze at {freeze}",
                fold.cutoff
            )));
        }
    }
    Ok(folds)
}

/// A fold's training and validation rows, each asserted point-in-time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FoldRows {
    /// The fold.
    pub fold: Fold,
    /// Training rows, censored at the cutoff.
    pub train: Vec<Row>,
    /// Validation rows, censored at `observe_until`.
    pub validate: Vec<Row>,
}

/// Build `fold`'s rows from `logged`, asserting both sets point-in-time.
pub fn fold_rows(logged: &Logged, fold: &Fold) -> Result<FoldRows, LeakError> {
    let train = logged.rows(fold.train_from, fold.cutoff, fold.cutoff);
    assert_point_in_time(&train, fold.cutoff)?;
    let validate = logged.rows(fold.cutoff, fold.validate_until, fold.observe_until);
    assert_point_in_time(&validate, fold.observe_until)?;
    Ok(FoldRows {
        fold: *fold,
        train,
        validate,
    })
}

/// One row's score under one predictor.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RowScore {
    /// Truncated pinball loss (sum over the three quartiles), seconds.
    pub truncated_pinball: f64,
    /// Landed rows: `actual − p50`.
    pub error: Option<f64>,
    /// `p25 ≤ actual ≤ p75`, where decidable.
    pub covered: Option<bool>,
}

/// Score quantiles `q` against `label`. `None` for an abandoned row.
#[must_use]
pub fn score_row(q: (i64, i64, i64), label: &Label) -> Option<RowScore> {
    let (actual, bound) = match *label {
        Label::Landed { remaining_sec, .. } => (remaining_sec as f64, f64::INFINITY),
        Label::Censored { elapsed_sec, .. } => (elapsed_sec as f64, elapsed_sec as f64),
        Label::Abandoned { .. } => return None,
    };
    let landed = bound.is_infinite();
    let (p25, p50, p75) = (q.0 as f64, q.1 as f64, q.2 as f64);
    let truncated_pinball = [(0.25, p25), (0.5, p50), (0.75, p75)]
        .iter()
        .map(|(level, qh)| pinball(*level, actual.min(bound) - qh.min(bound)))
        .sum();
    let covered = if landed {
        Some(p25 <= actual && actual <= p75)
    } else if bound > p75 {
        Some(false)
    } else {
        None
    };
    Some(RowScore {
        truncated_pinball,
        error: landed.then_some(actual - p50),
        covered,
    })
}

/// A metric with its issue-bootstrap 95% interval.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Estimate {
    /// Point value over every issue.
    pub value: Option<f64>,
    /// 2.5th bootstrap percentile.
    pub lo: Option<f64>,
    /// 97.5th bootstrap percentile.
    pub hi: Option<f64>,
    /// Rows contributing.
    pub n: usize,
}

/// Per-issue `(sum, count)` of one metric.
pub type IssueSums = BTreeMap<String, (f64, usize)>;

/// Ratio-of-sums mean over `sums`, with a 95% percentile interval from
/// `resamples` issue-bootstrap draws seeded with `seed`. Whole issues are
/// resampled, so many refreshed estimates of one issue never inflate `n`.
#[must_use]
pub fn bootstrap(sums: &IssueSums, resamples: usize, seed: u64) -> Estimate {
    let issues: Vec<(f64, usize)> = sums.values().copied().collect();
    let n: usize = issues.iter().map(|i| i.1).sum();
    let ratio = |s: f64, c: usize| (c > 0).then(|| s / c as f64);
    let value = ratio(issues.iter().map(|i| i.0).sum(), n);
    if value.is_none() {
        return Estimate {
            value,
            lo: None,
            hi: None,
            n,
        };
    }
    let mut rng = SplitMix64::new(seed);
    let len = issues.len() as u64;
    let mut draws: Vec<f64> = (0..resamples)
        .filter_map(|_| {
            let (mut s, mut c) = (0.0, 0_usize);
            for _ in 0..issues.len() {
                let (is, ic) = issues[(rng.next_u64() % len) as usize];
                s += is;
                c += ic;
            }
            ratio(s, c)
        })
        .collect();
    draws.sort_by(f64::total_cmp);
    let at = |p: f64| {
        let idx = ((draws.len() as f64 - 1.0) * p).round() as usize;
        draws.get(idx).copied()
    };
    Estimate {
        value,
        lo: at(0.025),
        hi: at(0.975),
        n,
    }
}

/// One predictor's report on the paired rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PredictorReport {
    /// Heuristic id or model settings id.
    pub id: String,
    /// Share of scoreable rows it answered.
    pub answer_rate: Option<f64>,
    /// Mean truncated pinball loss, seconds.
    pub truncated_pinball: Estimate,
    /// Landed rows only: mean pinball loss, seconds.
    pub landed_pinball: Estimate,
    /// Landed rows only: mean `|actual − p50|`, seconds.
    pub landed_mae: Estimate,
    /// Landed rows only: mean `actual − p50`, seconds.
    pub landed_bias: Estimate,
    /// 25–75% coverage over decidable rows.
    pub coverage: Estimate,
}

/// One frozen model against the baseline on the reported rows both answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Comparison {
    /// The frozen settings.
    pub settings: Settings,
    /// Reported rows both the baseline and this model answered.
    pub paired_rows: usize,
    /// Distinct issues among them.
    pub paired_issues: usize,
    /// The baseline on those rows.
    pub baseline: PredictorReport,
    /// The model on those rows.
    pub model: PredictorReport,
    /// Paired `model − baseline` truncated pinball, seconds.
    pub delta_truncated_pinball: Estimate,
}

/// The reported folds' comparison.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OfflineReport {
    /// The protocol.
    pub protocol: Protocol,
    /// Every fold, with its row counts.
    pub folds: Vec<FoldSummary>,
    /// Selection score of every settings candidate (mean truncated pinball
    /// on the selection rows every candidate of its family answers).
    pub selection: Vec<(String, Option<f64>)>,
    /// The heuristic baseline id.
    pub baseline_id: String,
    /// Scoreable reported rows (abandoned excluded).
    pub scoreable_rows: usize,
    /// The baseline's answer rate over the scoreable rows.
    pub baseline_answer_rate: Option<f64>,
    /// One per family: its frozen settings against the baseline.
    pub comparisons: Vec<Comparison>,
}

/// A fold's row counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FoldSummary {
    /// The fold.
    pub fold: Fold,
    /// Training rows.
    pub train_rows: usize,
    /// Validation rows.
    pub validate_rows: usize,
}

/// What can go wrong running the protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    /// The protocol itself is invalid.
    Protocol(ProtocolError),
    /// A built row set leaked.
    Leak(LeakError),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::Protocol(e) => e.fmt(f),
            RunError::Leak(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for RunError {}

impl From<ProtocolError> for RunError {
    fn from(e: ProtocolError) -> Self {
        RunError::Protocol(e)
    }
}

impl From<LeakError> for RunError {
    fn from(e: LeakError) -> Self {
        RunError::Leak(e)
    }
}

/// Bootstrap seed: fixed, so a report is reproducible from its inputs.
pub const BOOTSTRAP_SEED: u64 = 0x10193;

#[derive(Default)]
struct Acc {
    answered: usize,
    truncated: IssueSums,
    landed_pinball: IssueSums,
    mae: IssueSums,
    bias: IssueSums,
    coverage: IssueSums,
}

fn add(sums: &mut IssueSums, story: &str, value: f64) {
    let e = sums.entry(story.to_string()).or_insert((0.0, 0));
    e.0 += value;
    e.1 += 1;
}

impl Acc {
    fn push(&mut self, story: &str, s: &RowScore) {
        add(&mut self.truncated, story, s.truncated_pinball);
        if let Some(err) = s.error {
            add(&mut self.landed_pinball, story, s.truncated_pinball);
            add(&mut self.mae, story, err.abs());
            add(&mut self.bias, story, err);
        }
        if let Some(c) = s.covered {
            add(&mut self.coverage, story, if c { 1.0 } else { 0.0 });
        }
    }

    fn report(&self, id: &str, scoreable: usize, resamples: usize) -> PredictorReport {
        let b = |s: &IssueSums| bootstrap(s, resamples, BOOTSTRAP_SEED);
        PredictorReport {
            id: id.to_string(),
            answer_rate: (scoreable > 0).then(|| self.answered as f64 / scoreable as f64),
            truncated_pinball: b(&self.truncated),
            landed_pinball: b(&self.landed_pinball),
            landed_mae: b(&self.mae),
            landed_bias: b(&self.bias),
            coverage: b(&self.coverage),
        }
    }
}

/// Mean truncated pinball of each candidate on the selection rows every
/// candidate **of its family** answers, and the best of each family (a tie
/// keeps the earlier; a family no row scores keeps its first settings).
fn select(
    folds: &[(FoldRows, Vec<Fitted>)],
    grid: &[Settings],
) -> (Vec<(String, Option<f64>)>, Vec<Settings>) {
    let mut families: Vec<&'static str> = Vec::new();
    for s in grid {
        if !families.contains(&s.family()) {
            families.push(s.family());
        }
    }
    let mut totals = vec![(0.0_f64, 0_usize); grid.len()];
    for family in &families {
        let members: Vec<usize> = (0..grid.len())
            .filter(|i| grid[*i].family() == *family)
            .collect();
        for (rows, models) in folds {
            for row in &rows.validate {
                let scores: Option<Vec<RowScore>> = members
                    .iter()
                    .map(|i| {
                        models[*i]
                            .predict(row)
                            .and_then(|q| score_row(q, &row.label))
                    })
                    .collect();
                for (i, s) in members.iter().zip(scores.unwrap_or_default()) {
                    totals[*i].0 += s.truncated_pinball;
                    totals[*i].1 += 1;
                }
            }
        }
    }
    let scored: Vec<(String, Option<f64>)> = grid
        .iter()
        .zip(&totals)
        .map(|(g, t)| (g.id(), (t.1 > 0).then(|| t.0 / t.1 as f64)))
        .collect();
    let frozen = families
        .iter()
        .map(|family| {
            let members = grid
                .iter()
                .zip(&scored)
                .filter(|(g, _)| g.family() == *family);
            let first = members.clone().map(|(g, _)| *g).next();
            members
                .filter_map(|(g, (_, v))| v.map(|v| (*g, v)))
                .fold(None::<(Settings, f64)>, |acc, (g, v)| match acc {
                    Some((_, best)) if best <= v => acc,
                    _ => Some((g, v)),
                })
                .map(|(g, _)| g)
                .or(first)
                .unwrap_or(grid[0])
        })
        .collect();
    (scored, frozen)
}

/// One frozen model's running comparison.
struct Pairing {
    settings: Settings,
    fitted: Option<Fitted>,
    base: Acc,
    model: Acc,
    delta: IssueSums,
    paired: usize,
}

/// Run the protocol: choose settings per family on the selection folds,
/// then score each family's frozen model against `baseline_id`'s logged
/// estimates on the identical reported rows. An empty `grid` is every
/// family's default grid.
pub fn run(
    logged: &Logged,
    protocol: &Protocol,
    baseline_id: &str,
    grid: &[Settings],
    resamples: usize,
) -> Result<(OfflineReport, Vec<FoldRows>), RunError> {
    let grid: Vec<Settings> = if grid.is_empty() {
        Settings::grid()
    } else {
        grid.to_vec()
    };
    let folds = plan(protocol)?;
    let mut all_rows = Vec::new();
    let mut selection = Vec::new();
    for fold in folds.iter().filter(|f| f.role == FoldRole::Selection) {
        let rows = fold_rows(logged, fold)?;
        let models = grid
            .iter()
            .map(|s| s.fit(&rows.train, fold.cutoff))
            .collect::<Result<Vec<_>, _>>()?;
        selection.push((rows, models));
    }
    let (scored, frozen) = select(&selection, &grid);
    all_rows.extend(selection.into_iter().map(|(rows, _)| rows));

    let mut pairings: Vec<Pairing> = frozen
        .iter()
        .map(|s| Pairing {
            settings: *s,
            fitted: None,
            base: Acc::default(),
            model: Acc::default(),
            delta: IssueSums::new(),
            paired: 0,
        })
        .collect();
    let (mut scoreable, mut baseline_answered) = (0_usize, 0_usize);
    for fold in folds.iter().filter(|f| f.role == FoldRole::Reported) {
        let rows = fold_rows(logged, fold)?;
        // Walk-forward: each reported day is scored by the model fitted at
        // that day's own cutoff, never by one fitted later.
        for p in &mut pairings {
            p.fitted = Some(p.settings.fit(&rows.train, fold.cutoff)?);
        }
        for row in &rows.validate {
            if matches!(row.label, Label::Abandoned { .. }) {
                continue;
            }
            scoreable += 1;
            let story = row.snapshot.story.as_str();
            let b = row
                .snapshot
                .heuristics
                .get(baseline_id)
                .and_then(|a| a.quantiles());
            baseline_answered += usize::from(b.is_some());
            let bs = b.and_then(|q| score_row(q, &row.label));
            for p in &mut pairings {
                let m = p.fitted.as_ref().and_then(|f| f.predict(row));
                p.base.answered += usize::from(b.is_some());
                p.model.answered += usize::from(m.is_some());
                let (Some(bs), Some(ms)) = (bs, m.and_then(|q| score_row(q, &row.label))) else {
                    continue;
                };
                p.paired += 1;
                p.base.push(story, &bs);
                p.model.push(story, &ms);
                add(&mut p.delta, story, ms.truncated_pinball - bs.truncated_pinball);
            }
        }
        all_rows.push(rows);
    }
    let report = OfflineReport {
        protocol: *protocol,
        folds: all_rows
            .iter()
            .map(|r| FoldSummary {
                fold: r.fold,
                train_rows: r.train.len(),
                validate_rows: r.validate.len(),
            })
            .collect(),
        selection: scored,
        baseline_id: baseline_id.to_string(),
        scoreable_rows: scoreable,
        baseline_answer_rate: (scoreable > 0).then(|| baseline_answered as f64 / scoreable as f64),
        comparisons: pairings
            .iter()
            .map(|p| Comparison {
                settings: p.settings,
                paired_rows: p.paired,
                paired_issues: p.delta.len(),
                baseline: p.base.report(baseline_id, scoreable, resamples),
                model: p.model.report(&p.settings.id(), scoreable, resamples),
                delta_truncated_pinball: bootstrap(&p.delta, resamples, BOOTSTRAP_SEED),
            })
            .collect(),
    };
    Ok((report, all_rows))
}
