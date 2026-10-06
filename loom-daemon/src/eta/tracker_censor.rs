//! Expiry with censoring (#10233): a pending estimate that ages out is scored
//! first when its outcome is already decided.
//!
//! Before this, [`Tracker::expire`] dropped every pending estimate older than
//! [`PENDING_MAX_AGE_DAYS`] unscored. The estimates that live that long are
//! exactly the ones whose subject has not landed for a month — the late
//! surprises — so dropping them biased every late-surprise rate downwards.
//!
//! The rule: an expiring estimate whose `as_of + p90` is already behind `now`
//! is a **decided** late surprise. Whenever the event eventually happens, it
//! happens after `now`, so `actual > p90` holds whatever the outcome turns out
//! to be. It is scored as [`OutcomeKind::Censored`]: `above_p90 = true`, and
//! nothing else, because the actual is only known as a lower bound — a
//! pinball loss or an error computed against `now` would understate it.
//!
//! An expiring estimate with no p90 (a refusal, or one persisted before
//! #10211), or whose p90 is not yet behind `now`, is undecided and is dropped
//! as before.

use super::Kind;
use super::{Resolved, Tracker, MAX_PENDING, PENDING_MAX_AGE_DAYS};
use crate::eta::score::{score, EstimateSummary, OutcomeKind};
use chrono::{DateTime, Duration, Utc};
use std::collections::{HashMap, HashSet};

/// The `outcome_source` of a censored outcome.
pub const CENSOR_SOURCE: &str = "pending_expiry";

/// What [`Tracker::expire`] did.
#[derive(Debug, Default)]
pub struct Expired {
    /// Pending estimates removed (scored or not), including the cap's.
    pub dropped: usize,
    /// The decided late surprises among them, scored before they went.
    pub censored: Vec<Resolved>,
}

/// The censored score of `estimate` at `now`, when its late surprise is
/// already decided; `None` when it is not.
#[must_use]
pub fn censor(estimate: &EstimateSummary, now: DateTime<Utc>) -> Option<Resolved> {
    let p90 = estimate.p90_sec?;
    estimate.quantiles()?;
    let lead = (now - estimate.as_of).num_seconds();
    if lead <= p90 {
        return None;
    }
    let mut scored = score(estimate, OutcomeKind::Censored, now, &[]);
    scored.above_p90 = Some(true);
    Some(Resolved {
        estimate: estimate.clone(),
        score: scored,
        outcome_source: CENSOR_SOURCE.to_string(),
        outcome_resolution_sec: None,
        result: None,
    })
}

/// Evict up to `excess` redundant estimates from `pending` (ordered oldest
/// first), keeping each series' earliest and latest. A series is
/// `(repo, issue, kind, heuristic)`. Each round removes every other middle
/// estimate of each series, oldest candidates first, so the survivors stay
/// spread across the series' whole lead range. Returns the number evicted.
fn thin_redundant(pending: &mut Vec<EstimateSummary>, mut excess: usize) -> usize {
    let mut evicted = 0;
    while excess > 0 {
        let mut series: HashMap<(&str, u32, Kind, &str), Vec<usize>> = HashMap::new();
        for (i, p) in pending.iter().enumerate() {
            series
                .entry((p.repo.as_str(), p.issue, p.kind, p.heuristic.as_str()))
                .or_default()
                .push(i);
        }
        let mut candidates: Vec<usize> = series
            .values()
            .filter(|ix| ix.len() >= 3)
            .flat_map(|ix| ix[1..ix.len() - 1].iter().copied().step_by(2))
            .collect();
        if candidates.is_empty() {
            break;
        }
        candidates.sort_unstable();
        candidates.truncate(excess);
        let doomed: HashSet<usize> = candidates.into_iter().collect();
        let mut i = 0;
        pending.retain(|_| {
            let keep = !doomed.contains(&i);
            i += 1;
            keep
        });
        evicted += doomed.len();
        excess -= doomed.len();
    }
    evicted
}

impl Tracker {
    /// Drop pending estimates older than [`PENDING_MAX_AGE_DAYS`] — scoring
    /// each decided late surprise among them first — and the oldest past
    /// [`MAX_PENDING`].
    pub fn expire(&mut self, now: DateTime<Utc>) -> Expired {
        let before = self.pending.len();
        let cutoff = now - Duration::days(PENDING_MAX_AGE_DAYS);
        let (expiring, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|p| p.as_of < cutoff);
        self.pending = keep;
        let mut censored: Vec<Resolved> = expiring.iter().filter_map(|p| censor(p, now)).collect();
        if self.pending.len() > MAX_PENDING {
            // Evict redundant refreshes per series first (#10496): the oldest
            // estimate of a series is its longest-lead one, which scoring
            // needs most, so a series keeps its earliest and latest.
            let excess = self.pending.len() - MAX_PENDING;
            self.cap_dropped += thin_redundant(&mut self.pending, excess);
        }
        if self.pending.len() > MAX_PENDING {
            // Only when even two per series do not fit: the oldest go, and a
            // decided late surprise among them is still scored on the way out.
            let excess = self.pending.len() - MAX_PENDING;
            let over: Vec<EstimateSummary> = self.pending.drain(..excess).collect();
            censored.extend(over.iter().filter_map(|p| censor(p, now)));
            self.cap_dropped += excess;
        }
        Expired {
            dropped: before - self.pending.len(),
            censored,
        }
    }
}
