//! The multi-fold half of the backtest (#10233): the paired comparison on the
//! **union** of cases, its walk-forward daily folds, and the two diagnostic
//! views every report carries — stability of the predicted landing instant
//! and convergence of the interval.
//!
//! # Why the union, counting refusals
//!
//! A heuristic that refuses the slowest cases scores only the fast ones, so a
//! mean over *its own* scored cases looks better than a heuristic that
//! answered everything. The comparison therefore starts from the union of
//! cases (every case of the kind the filter keeps) and reads each figure on
//! the subset where **both** sides are decidable for it — the same common
//! decidable subset the live gate uses (`shadow.rs`). The refusals are not
//! dropped: they are counted in each side's answer rate over the union, and a
//! side whose answer rate falls more than [`ANSWER_RATE_SLACK`] below the
//! other's cannot be the better one.
//!
//! # Why the folds are walk-forward
//!
//! Every case is replayed against history strictly before its own `as_of`
//! (`backtest.rs` module docs), so grouping cases by the UTC day of their
//! `as_of` gives folds whose "training" is everything earlier and whose
//! "test" is that day — walk-forward by construction, with no fitting step
//! that could see a later fold. A day goes to whichever side had the lower
//! mean deciding loss on that day's common pairs (ties are left out), and the
//! per-day win rate carries the same deterministic 95% Wilson interval as the
//! live gate ([`DayWins`]).

use super::super::offline::evaluate::{bootstrap, Estimate, IssueSums, BOOTSTRAP_SEED};
use super::super::score::{bucket, EstimateSummary, Score};
use super::super::shadow::{DaySums, DayWins, ANSWER_RATE_SLACK, LATE_SURPRISE_SLACK};
use super::censored::late_flag;
use super::ReplayCase;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One replayed case: what was asked, what the heuristic answered, how it
/// scored.
#[derive(Debug, Clone)]
pub(super) struct Replayed {
    pub case: ReplayCase,
    pub summary: EstimateSummary,
    pub score: Score,
}

/// How far the predicted landing **instant** (`as_of + p50`) moves between
/// consecutive cases of one series (repo, issue, sweep) that both answered, in
/// seconds. A refusal between two answers breaks the pair; it is not skipped.
///
/// Measured on the instant, not on remaining seconds: a perfectly steady ETA
/// loses one second of remaining time per second, which a remaining-seconds
/// view would read as movement. In a replay, consecutive cases of a series are
/// consecutive stage entries, so this is how much the promise moves as the
/// work advances. Diagnostic; not a gate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Stability {
    /// Consecutive pairs measured where both cases answered.
    pub steps: usize,
    /// Median absolute shift of the predicted instant, seconds.
    pub median_shift_sec: Option<f64>,
    /// Largest absolute shift, seconds.
    pub max_shift_sec: Option<i64>,
}

/// Interval width against the time that actually remained, for one bucket of
/// the actual lead ([`bucket`] of `lead_sec`). A converging heuristic narrows
/// as the event nears. Diagnostic; not a gate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Convergence {
    /// Scored cases in the bucket.
    pub scored: usize,
    /// Median `p75 − p25`, seconds.
    pub median_p25_p75_sec: Option<f64>,
    /// Scored cases that carried a p90.
    pub with_p90: usize,
    /// Median `p90 − p25` over those, seconds.
    pub median_p25_p90_sec: Option<f64>,
}

fn median(mut values: Vec<i64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let mid = values.len() / 2;
    Some(if values.len().is_multiple_of(2) {
        (values[mid - 1] as f64 + values[mid] as f64) / 2.0
    } else {
        values[mid] as f64
    })
}

/// The [`Stability`] of `replayed`.
pub(super) fn stability_of(replayed: &[Replayed]) -> Stability {
    let mut series: BTreeMap<(String, u32, Option<String>), Vec<&Replayed>> = BTreeMap::new();
    for r in replayed {
        let s = &r.case.subject;
        series
            .entry((s.repo.clone(), s.issue, s.sweep_id.clone()))
            .or_default()
            .push(r);
    }
    let mut shifts = Vec::new();
    for mut cases in series.into_values() {
        cases.sort_by_key(|r| r.case.as_of);
        let landing = |r: &Replayed| r.summary.p50_sec.map(|p50| r.case.as_of.timestamp() + p50);
        for pair in cases.windows(2) {
            if let (Some(before), Some(after)) = (landing(pair[0]), landing(pair[1])) {
                shifts.push((after - before).abs());
            }
        }
    }
    Stability {
        steps: shifts.len(),
        max_shift_sec: shifts.iter().copied().max(),
        median_shift_sec: median(shifts),
    }
}

/// The [`Convergence`] of `replayed`, per bucket of the actual lead.
pub(super) fn convergence_of(replayed: &[Replayed]) -> BTreeMap<String, Convergence> {
    let mut widths: BTreeMap<&'static str, (Vec<i64>, Vec<i64>)> = BTreeMap::new();
    for r in replayed
        .iter()
        .filter(|r| r.score.pinball_loss_sec.is_some())
    {
        let Some((p25, _, p75)) = r.summary.quantiles() else {
            continue;
        };
        let entry = widths.entry(bucket(r.score.lead_sec)).or_default();
        entry.0.push(p75 - p25);
        if let Some(p90) = r.summary.p90_sec {
            entry.1.push(p90 - p25);
        }
    }
    widths
        .into_iter()
        .map(|(b, (iqr, wide))| {
            let c = Convergence {
                scored: iqr.len(),
                with_p90: wide.len(),
                median_p25_p75_sec: median(iqr),
                median_p25_p90_sec: median(wide),
            };
            (b.to_string(), c)
        })
        .collect()
}

/// One walk-forward fold: the cases whose `as_of` falls on one UTC day.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Fold {
    /// `YYYY-MM-DD` (UTC) of the cases' `as_of`.
    pub day: String,
    /// Union cases that day.
    pub cases: usize,
    /// Of those, the ones both sides scored with a p90 (the deciding loss's
    /// population).
    pub loss4_pairs: usize,
    /// `a`'s mean `pinball4_loss_sec` over them.
    pub a_mean_pinball4_loss_sec: Option<f64>,
    /// `b`'s.
    pub b_mean_pinball4_loss_sec: Option<f64>,
}

/// Both heuristics on the union of cases, every figure on its common
/// decidable subset (module docs).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Paired {
    /// The union: every case of the kind the filter kept.
    pub cases: usize,
    /// Cases `a` answered (scored).
    pub a_answered: usize,
    /// Cases `b` answered.
    pub b_answered: usize,
    /// `a_answered / cases` — refusals count against it.
    pub a_answer_rate: Option<f64>,
    /// `b_answered / cases`.
    pub b_answer_rate: Option<f64>,
    /// Cases both sides scored.
    pub common: usize,
    /// `a`'s mean three-quantile pinball loss over `common`.
    pub a_mean_pinball_loss_sec: Option<f64>,
    /// `b`'s.
    pub b_mean_pinball_loss_sec: Option<f64>,
    /// Cases both sides scored with a p90 — the deciding loss's population.
    pub loss4_pairs: usize,
    /// `a`'s mean `pinball4_loss_sec` over `loss4_pairs` — the deciding loss.
    pub a_mean_pinball4_loss_sec: Option<f64>,
    /// `b`'s.
    pub b_mean_pinball4_loss_sec: Option<f64>,
    /// Cases both sides answered with a p90, in-flight cases included
    /// (#9970 Slice 2) — the same population as [`Self::delta_late_rate`].
    pub late_pairs: usize,
    /// `a`'s late-surprise (`actual > p90`) rate over them. An in-flight case
    /// still inside its p90 counts as not late, so this is a lower bound,
    /// never an overstatement; with no in-flight cases it is the resolved rate.
    pub a_late_rate: Option<f64>,
    /// `b`'s.
    pub b_late_rate: Option<f64>,
    /// `b − a` mean three-quantile pinball loss over `common`, with its 95%
    /// issue-bootstrap interval (#10489): whole issues are resampled, so the
    /// many stage entries of one issue never inflate `n`. Negative favours
    /// `b`. `None` when nothing was common.
    pub delta_pinball_loss_sec: Option<Estimate>,
    /// `b − a` mean `pinball4_loss_sec` over `loss4_pairs`, likewise.
    pub delta_pinball4_loss_sec: Option<Estimate>,
    /// Distinct issues behind [`Self::delta_pinball4_loss_sec`]: the
    /// promotion gate's independence count (#10525).
    pub delta4_items: usize,
    /// `b − a` late-surprise rate over every case both sides answered with a
    /// p90, **in-flight cases included** (#9970 Slice 2), with its 95%
    /// issue-bootstrap interval. An in-flight case still inside the p90
    /// counts as not late, so each rate is a lower bound. Negative favours
    /// `b`. `None` when no case was decidable.
    pub delta_late_rate: Option<Estimate>,
    /// The walk-forward daily folds, oldest first.
    pub folds: Vec<Fold>,
    /// `b`'s per-day win rate over `a` on the deciding loss, with its 95%
    /// Wilson interval. `b` is the challenger: `eta promote` compares
    /// `(current, candidate)`, `eta backtest --heuristic A --compare B` reads
    /// `B` as the challenger.
    pub day_wins: DayWins,
}

/// Issue-bootstrap draws behind [`Paired::delta_pinball_loss_sec`] and
/// [`Paired::delta_pinball4_loss_sec`].
pub const PAIRED_BOOTSTRAP_RESAMPLES: usize = 1_000;

/// Pair `a` and `b`, replayed over the same cases in the same order.
pub(super) fn paired_of(a: &[Replayed], b: &[Replayed]) -> Paired {
    debug_assert_eq!(a.len(), b.len());
    let mean = |total: f64, n: usize| (n > 0).then(|| total / n as f64);
    let share = |count: usize, n: usize| (n > 0).then(|| count as f64 / n as f64);
    let mut out = Paired {
        cases: a.len(),
        ..Paired::default()
    };
    let (mut a_loss, mut b_loss, mut a_loss4, mut b_loss4) = (0.0, 0.0, 0.0, 0.0);
    let (mut a_late, mut b_late) = (0_usize, 0_usize);
    let mut days: BTreeMap<String, (usize, DaySums)> = BTreeMap::new();
    let (mut delta, mut delta4) = (IssueSums::new(), IssueSums::new());
    let mut delta_late = IssueSums::new();
    let add = |sums: &mut IssueSums, key: &str, d: f64| {
        let e = sums.entry(key.to_string()).or_insert((0.0, 0));
        e.0 += d;
        e.1 += 1;
    };
    for (ra, rb) in a.iter().zip(b) {
        let issue = format!("{}#{}", ra.case.subject.repo, ra.case.subject.issue);
        let (sa, sb) = (&ra.score, &rb.score);
        // Answered, not merely loss-scored: an in-flight case carries no loss.
        out.a_answered += usize::from(ra.summary.quantiles().is_some());
        out.b_answered += usize::from(rb.summary.quantiles().is_some());
        let day = days
            .entry(ra.case.as_of.format("%Y-%m-%d").to_string())
            .or_default();
        day.0 += 1;
        if let (Some(la), Some(lb)) = (sa.pinball_loss_sec, sb.pinball_loss_sec) {
            out.common += 1;
            a_loss += la;
            b_loss += lb;
            add(&mut delta, &issue, lb - la);
        }
        if let (Some(la), Some(lb)) = (sa.pinball4_loss_sec, sb.pinball4_loss_sec) {
            out.loss4_pairs += 1;
            a_loss4 += la;
            b_loss4 += lb;
            add(&mut delta4, &issue, lb - la);
            day.1.pairs += 1;
            day.1.current_loss4_sec += la;
            day.1.candidate_loss4_sec += lb;
        }
        // One lower-bound late flag feeds both the gate's rates and
        // `delta_late_rate`: keying on raw `above_p90` would admit only the
        // in-flight cases already past p90 and overstate lateness (#9970).
        if let (Some(la), Some(lb)) = (late_flag(ra), late_flag(rb)) {
            add(&mut delta_late, &issue, f64::from(u8::from(lb)) - f64::from(u8::from(la)));
            out.late_pairs += 1;
            a_late += usize::from(la);
            b_late += usize::from(lb);
        }
    }
    out.a_answer_rate = share(out.a_answered, out.cases);
    out.b_answer_rate = share(out.b_answered, out.cases);
    out.a_mean_pinball_loss_sec = mean(a_loss, out.common);
    out.b_mean_pinball_loss_sec = mean(b_loss, out.common);
    out.a_mean_pinball4_loss_sec = mean(a_loss4, out.loss4_pairs);
    out.b_mean_pinball4_loss_sec = mean(b_loss4, out.loss4_pairs);
    out.a_late_rate = share(a_late, out.late_pairs);
    out.b_late_rate = share(b_late, out.late_pairs);
    let ci = |sums: &IssueSums| {
        (!sums.is_empty()).then(|| bootstrap(sums, PAIRED_BOOTSTRAP_RESAMPLES, BOOTSTRAP_SEED))
    };
    out.delta_pinball_loss_sec = ci(&delta);
    out.delta_pinball4_loss_sec = ci(&delta4);
    out.delta4_items = delta4.len();
    out.delta_late_rate = ci(&delta_late);
    let sums: BTreeMap<String, DaySums> = days.iter().map(|(d, (_, s))| (d.clone(), *s)).collect();
    out.day_wins = DayWins::of(&sums);
    out.folds = days
        .into_iter()
        .map(|(day, (cases, s))| Fold {
            day,
            cases,
            loss4_pairs: s.pairs,
            a_mean_pinball4_loss_sec: mean(s.current_loss4_sec, s.pairs),
            b_mean_pinball4_loss_sec: mean(s.candidate_loss4_sec, s.pairs),
        })
        .collect();
    out
}

/// Which side of `p` is better: `Some(true)` for `a`, `Some(false)` for `b`,
/// `None` for neither.
///
/// A side may win only when it does not regress on the other's answer rate
/// over the union (by more than [`ANSWER_RATE_SLACK`]) nor on its late-surprise
/// rate over the common subset (by more than [`LATE_SURPRISE_SLACK`]). Among the
/// sides that may, the lower mean deciding loss (`pinball4`, common subset)
/// wins; an exact tie goes to the side that answered strictly more. So a
/// heuristic that refuses the slowest cases cannot win by refusing them: on
/// the common subset its loss is the other's, and over the union it answered
/// less.
pub(super) fn better_side(p: &Paired) -> Option<bool> {
    let (Some(a_loss), Some(b_loss)) = (p.a_mean_pinball4_loss_sec, p.b_mean_pinball4_loss_sec)
    else {
        return None;
    };
    let answers = (p.a_answer_rate.unwrap_or(0.0), p.b_answer_rate.unwrap_or(0.0));
    let late = (p.a_late_rate.unwrap_or(0.0), p.b_late_rate.unwrap_or(0.0));
    let may_win = |mine: usize| {
        let (my_answers, their_answers) = if mine == 0 {
            answers
        } else {
            (answers.1, answers.0)
        };
        let (my_late, their_late) = if mine == 0 { late } else { (late.1, late.0) };
        my_answers >= their_answers - ANSWER_RATE_SLACK
            && my_late <= their_late + LATE_SURPRISE_SLACK
    };
    let (a_may, b_may) = (may_win(0), may_win(1));
    if a_loss < b_loss {
        a_may.then_some(true)
    } else if b_loss < a_loss {
        b_may.then_some(false)
    } else if answers.0 > answers.1 {
        a_may.then_some(true)
    } else if answers.1 > answers.0 {
        b_may.then_some(false)
    } else {
        None
    }
}
