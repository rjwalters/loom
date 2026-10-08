//! In-flight cases, scored as censored (#9970, Slice 2).
//!
//! # Why
//!
//! A replay case normally exists only once its landing is known. In a short
//! evaluation window the fast landings are in and the slow in-flight items are
//! out, so a gate over resolved cases alone rewards optimistic heuristics:
//! the bias `land-v3` exists to fix. [`censor_at`] builds the walk-forward
//! view of a case set at a cutoff instant, in which an item not yet landed at
//! the cutoff is a [`OutcomeKind::Censored`] case rather than a missing one.
//!
//! # What a censored case decides
//!
//! Its actual lead is only a lower bound (`cutoff - as_of`). `score_censored`
//! ([`super::super::score::score_censored`]) sets what that bound decides — an
//! elapsed time past the p75 is a known coverage miss, past the p90 a known
//! late surprise — and never a loss or an error. So censored cases enter the
//! late-surprise figures and the answer rate, not the pinball means.
//!
//! # Rate and interval
//!
//! [`LateSurprise`] counts an in-flight case whose p90 is not yet behind the
//! cutoff as **not late**, so its rate is a *lower bound* on the true late
//! rate, never an overstatement. The interval resamples whole issues
//! ([`bootstrap`]): the many stage entries of one issue are one draw. The
//! resolved-only rate is carried beside it so the optimism of dropping
//! in-flight items is visible rather than asserted.

use super::super::offline::evaluate::{bootstrap, Estimate, IssueSums, BOOTSTRAP_SEED};
use super::super::score::OutcomeKind;
use super::paired::Replayed;
use super::ReplayCase;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Issue-bootstrap draws behind [`LateSurprise::rate`].
pub const LATE_BOOTSTRAP_RESAMPLES: usize = 1_000;

/// The walk-forward view of `cases` as observed at `cutoff`.
///
/// A case whose `as_of` is at or after `cutoff` was not yet askable and is
/// dropped. A case asked before `cutoff` that had already resolved by then
/// (`actual_at <= cutoff`) is kept as is. A case asked before `cutoff` whose
/// landing came later is not known at `cutoff`: it becomes
/// [`OutcomeKind::Censored`] with `actual_at = cutoff`, its real outcome
/// discarded. Case order is preserved.
#[must_use]
pub fn censor_at(cases: &[ReplayCase], cutoff: DateTime<Utc>) -> Vec<ReplayCase> {
    cases
        .iter()
        .filter(|c| c.as_of < cutoff)
        .map(|c| {
            let mut c = c.clone();
            if c.actual_at > cutoff {
                c.outcome = OutcomeKind::Censored;
                c.actual_at = cutoff;
            }
            c
        })
        .collect()
}

/// A report's late-surprise rate over resolved and in-flight cases.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LateSurprise {
    /// In-flight (censored) cases replayed.
    pub censored: usize,
    /// Of those, ones the heuristic answered with a p90 (so decidable).
    pub censored_answered: usize,
    /// Of those, decided late surprises (elapsed already past the p90).
    pub censored_late: usize,
    /// Resolved cases scored with a p90.
    pub resolved_answered: usize,
    /// Resolved cases whose actual exceeded the p90.
    pub resolved_late: usize,
    /// Resolved-only late rate: what the gate would read had in-flight items
    /// been dropped.
    pub resolved_only_rate: Option<f64>,
    /// Lower-bound late rate over resolved and in-flight cases, with its 95%
    /// issue-bootstrap interval (`n` counts cases, the draw is by issue).
    pub rate: Estimate,
    /// Distinct issues behind [`Self::rate`].
    pub items: usize,
}

/// The decidable late flag of one replayed case: `above_p90` for a resolved
/// case, and for a censored one the lower bound (`false` while undecided).
/// `None` when the case has no p90 to decide against.
pub(super) fn late_flag(r: &Replayed) -> Option<bool> {
    if r.case.outcome == OutcomeKind::Censored {
        r.summary.quantiles()?;
        r.summary.p90_sec?;
        Some(r.score.above_p90 == Some(true))
    } else {
        r.score.above_p90
    }
}

/// [`LateSurprise`] of `replayed`; `None` when no case is censored.
pub(super) fn late_surprise_of(replayed: &[Replayed]) -> Option<LateSurprise> {
    let censored = replayed
        .iter()
        .filter(|r| r.case.outcome == OutcomeKind::Censored)
        .count();
    if censored == 0 {
        return None;
    }
    let mut out = LateSurprise {
        censored,
        censored_answered: 0,
        censored_late: 0,
        resolved_answered: 0,
        resolved_late: 0,
        resolved_only_rate: None,
        rate: bootstrap(&IssueSums::new(), 0, BOOTSTRAP_SEED),
        items: 0,
    };
    let mut sums = IssueSums::new();
    for r in replayed {
        let Some(late) = late_flag(r) else {
            continue;
        };
        let is_censored = r.case.outcome == OutcomeKind::Censored;
        if is_censored {
            out.censored_answered += 1;
            out.censored_late += usize::from(late);
        } else {
            out.resolved_answered += 1;
            out.resolved_late += usize::from(late);
        }
        let key = format!("{}#{}", r.case.subject.repo, r.case.subject.issue);
        let e = sums.entry(key).or_insert((0.0, 0));
        e.0 += f64::from(u8::from(late));
        e.1 += 1;
    }
    out.resolved_only_rate = (out.resolved_answered > 0)
        .then(|| out.resolved_late as f64 / out.resolved_answered as f64);
    out.items = sums.len();
    out.rate = bootstrap(&sums, LATE_BOOTSTRAP_RESAMPLES, BOOTSTRAP_SEED);
    Some(out)
}
