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

use super::{Resolved, Tracker, MAX_PENDING, PENDING_MAX_AGE_DAYS};
use crate::eta::score::{score, EstimateSummary, OutcomeKind};
use chrono::{DateTime, Duration, Utc};

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
            let excess = self.pending.len() - MAX_PENDING;
            // The oldest are the long-horizon estimates scoring needs most;
            // the caller warns when this is non-zero. A decided late surprise
            // among them is still scored on the way out.
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
