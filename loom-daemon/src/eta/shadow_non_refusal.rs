//! The live non-refusal check a primary flip must pass (#10949).
//!
//! A candidate that wins on pinball can still be unfit to serve: on
//! 2026-10-08 the fitted family (`land-2026-10-04-twin-otter-b` and its
//! wrappers) refused `no_model` on every PR stage on the authority, because
//! no fit had been published there yet. Promoting it would have replaced a
//! served ETA with a refusal on exactly the items users look at.
//!
//! So `eta promote` also asks: **of the tracker passes in the trailing
//! [`WINDOW_HOURS`] where `current` answered, in what share did the candidate
//! answer too?** It must be at least [`MIN_RATE`] over at least
//! [`MIN_PASSES`] such passes. This is conditional on `current` answering on
//! purpose: an item neither side can estimate is not the candidate's fault,
//! and the ledger's unconditional answer rate ([`super::shadow::LiveGate`])
//! counts it on both sides.
//!
//! # Why hours, and a separate map
//!
//! The ledger's answer-rate sums ([`super::shadow::PairSums`]) accumulate
//! from the day a comparison was first seen. A fit published this morning
//! cannot be read off them: weeks of `no_model` passes would drown it. The
//! window has to be recent, so the passes are bucketed by UTC hour
//! ([`AnswerHour`]) and the newest [`MAX_HOURS`] kept. A ledger written before
//! this existed reads empty, and the check refuses until a day accrues: the
//! comparison's oldest bucket must be at least [`WINDOW_HOURS`] old, so a
//! fresh ledger holding one burst of passes cannot pass.
//!
//! Pure apart from the ledger it reads: no clock (the caller passes `now`),
//! no file, no forge.

use super::shadow::{GateStatus, PairKey, ShadowLedger};
use super::tracker::PassAnswers;
use super::Kind;
use chrono::{DateTime, Duration, DurationRound as _, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The trailing window the check reads, in hours.
pub const WINDOW_HOURS: i64 = 24;

/// Lowest share of `current`'s answered passes the candidate must also answer.
pub const MIN_RATE: f64 = 0.95;

/// Fewest passes, in the window, where `current` answered.
pub const MIN_PASSES: usize = super::shadow::MIN_LIVE_PAIRS;

/// Most hourly buckets kept per comparison; the oldest go first.
pub const MAX_HOURS: usize = 72;

/// One UTC hour's paired answer states for one comparison.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AnswerHour {
    /// Passes where `current` answered.
    pub current_answered: usize,
    /// Of those, passes where the candidate answered too.
    pub both_answered: usize,
}

/// The check's verdict and the numbers behind it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NonRefusal {
    /// Outcome: `passed` or `failed` (never `not_reached`).
    pub status: GateStatus,
    /// Why, in one line.
    pub detail: String,
    /// Start of the window (inclusive, truncated to the hour).
    pub since: DateTime<Utc>,
    /// End of the window: the evaluation instant.
    pub until: DateTime<Utc>,
    /// Passes in the window where `current` answered.
    pub current_answered: usize,
    /// Of those, passes where the candidate answered too.
    pub candidate_answered: usize,
    /// `candidate_answered / current_answered`; `None` with no passes.
    pub rate: Option<f64>,
    /// The rate required.
    pub min_rate: f64,
    /// The passes required.
    pub min_passes: usize,
    /// Start of the hour the comparison was first observed (its oldest
    /// bucket); `None` for a ledger with no buckets for it.
    #[serde(default)]
    pub observed_since: Option<DateTime<Utc>>,
    /// Whole hours observed as of `until`; the check needs [`WINDOW_HOURS`].
    #[serde(default)]
    pub observed_hours: Option<i64>,
}

/// The start of the hour a bucket key names.
fn parse_hour_key(key: &str) -> Option<DateTime<Utc>> {
    chrono::NaiveDateTime::parse_from_str(&format!("{key}:00:00"), "%Y-%m-%dT%H:%M:%S")
        .ok()
        .map(|t| t.and_utc())
}

/// `YYYY-MM-DDTHH`, the bucket key of `at`.
fn hour_key(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H").to_string()
}

impl ShadowLedger {
    /// Record one batch of tracker passes, taken at `at`, into the hourly
    /// non-refusal buckets. Same pairing as
    /// [`ShadowLedger::record_answers`]: `current`'s state against every
    /// other heuristic's in the same pass; a pass without `current` records
    /// nothing, and one where `current` refused is not counted (the check is
    /// conditional on `current` answering).
    pub fn record_answer_hours(
        &mut self,
        current: &dyn Fn(Kind) -> String,
        passes: &[PassAnswers],
        at: DateTime<Utc>,
    ) {
        let hour = hour_key(at);
        for pass in passes {
            let current_id = current(pass.kind);
            let Some((_, true)) = pass.states.iter().find(|(h, _)| *h == current_id) else {
                continue;
            };
            for (candidate, answered) in pass.states.iter().filter(|(h, _)| *h != current_id) {
                let encoded = self.key_for(pass.kind, &current_id, candidate);
                let hours = self.answer_hours.entry(encoded).or_default();
                let bucket = hours.entry(hour.clone()).or_default();
                bucket.current_answered += 1;
                bucket.both_answered += usize::from(*answered);
                while hours.len() > MAX_HOURS {
                    hours.pop_first();
                }
            }
        }
    }

    /// The non-refusal check for `candidate` against `current` over the
    /// [`WINDOW_HOURS`] before `now` (from the start of that hour).
    #[must_use]
    pub fn non_refusal(
        &self,
        kind: Kind,
        current: &str,
        candidate: &str,
        now: DateTime<Utc>,
    ) -> NonRefusal {
        let since = now - Duration::hours(WINDOW_HOURS);
        let (from, to) = (hour_key(since), hour_key(now));
        let encoded = super::shadow::encode(&PairKey {
            kind,
            current: current.to_string(),
            candidate: candidate.to_string(),
        });
        // The oldest bucket is when this comparison was first observed: the
        // trailing window alone cannot tell a day of evidence from one burst.
        let observed_since = self
            .answer_hours
            .get(&encoded)
            .and_then(|hours| hours.keys().next())
            .and_then(|key| parse_hour_key(key));
        let observed_hours = observed_since.map(|t| (now - t).num_hours());
        let (mut current_answered, mut candidate_answered) = (0, 0);
        for (_, bucket) in self
            .answer_hours
            .get(&encoded)
            .into_iter()
            .flat_map(|hours| hours.range(from.clone()..=to.clone()))
        {
            current_answered += bucket.current_answered;
            candidate_answered += bucket.both_answered;
        }
        let rate =
            (current_answered > 0).then(|| candidate_answered as f64 / current_answered as f64);
        let pct = |r: f64| r * 100.0;
        let (status, detail) = match rate {
            _ if observed_hours.is_none_or(|h| h < WINDOW_HOURS) => (
                GateStatus::Failed,
                format!(
                    "{current} vs {candidate} observed for {} h, {WINDOW_HOURS} h required",
                    observed_hours.unwrap_or(0).max(0)
                ),
            ),
            _ if current_answered < MIN_PASSES => (
                GateStatus::Failed,
                format!(
                    "{current_answered} pass(es) in the last {WINDOW_HOURS} h where {current} \
                     answered, {MIN_PASSES} required"
                ),
            ),
            Some(r) if r >= MIN_RATE => (
                GateStatus::Passed,
                format!(
                    "{candidate} answered {:.1}% of the {current_answered} pass(es) {current} \
                     answered in the last {WINDOW_HOURS} h (>= {:.0}%)",
                    pct(r),
                    pct(MIN_RATE)
                ),
            ),
            _ => (
                GateStatus::Failed,
                format!(
                    "{candidate} answered {:.1}% of the {current_answered} pass(es) {current} \
                     answered in the last {WINDOW_HOURS} h, {:.0}% required",
                    pct(rate.unwrap_or(0.0)),
                    pct(MIN_RATE)
                ),
            ),
        };
        NonRefusal {
            status,
            detail,
            since: since.duration_trunc(Duration::hours(1)).unwrap_or(since),
            until: now,
            current_answered,
            candidate_answered,
            rate,
            min_rate: MIN_RATE,
            min_passes: MIN_PASSES,
            observed_since,
            observed_hours,
        }
    }
}

/// Fold `check` into `decision`: a failed check refuses the flip (#10949),
/// a passed one changes nothing. Recorded either way.
pub fn apply(decision: &mut super::shadow::PromotionDecision, check: NonRefusal) {
    if check.status != GateStatus::Passed && decision.promote {
        decision.promote = false;
        decision.reason = format!("live non-refusal check failed: {}", check.detail);
    }
    decision.non_refusal = Some(check);
}

/// The hourly buckets, keyed like the rest of the ledger.
pub type AnswerHours = BTreeMap<String, BTreeMap<String, AnswerHour>>;
