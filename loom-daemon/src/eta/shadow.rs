//! Shadow mode and the promotion switch (#9328, Phase 5 of #9289).
//!
//! # Shadow mode
//!
//! The tracker computes an estimate for **every** registered heuristic of a
//! kind ([`super::Registry::for_kind`]), not just `current`. Each one is
//! logged as its own `eta.estimate`; only `current`'s is
//! [`super::tracker::Emission::primary`], so nothing downstream changes
//! behaviour. Because every heuristic of a kind is estimated at the same
//! `as_of` for the same subject, and [`super::tracker::Tracker::resolve`]
//! scores every pending estimate of that `(repo, issue, kind)` series against
//! one outcome, the candidate's score and `current`'s score arrive together —
//! already paired, with no join to invent.
//!
//! [`ShadowLedger`] accumulates those pairs. It is a running-sum ledger, not a
//! sample store: a pair contributes to four counters and is forgotten.
//!
//! # The promotion rule (operator decision 2 on #9289)
//!
//! Two gates, **in this order**, both required:
//!
//! 1. **Backtest.** The candidate must be the better of the two in the
//!    phase-2 backtest for that kind ([`super::backtest::compare`]), over the
//!    identical replay set — judged on the **union** of cases counting
//!    refusals (#10233): lower paired mean `pinball4_loss_sec` on the cases
//!    both answered, with no answer-rate or late-surprise regression — and
//!    the win must hold across the walk-forward daily folds: at least
//!    [`MIN_FOLDS`] decided days, the 95% Wilson lower bound of the per-day
//!    win rate above 50%. Failing this, the live gate is not even consulted
//!    — a heuristic that cannot win on history it can be re-run against has no
//!    business being judged on a live sample nobody can replay.
//! 2. **Live** (#10233), every figure on the **common decidable subset** —
//!    the pairs where both sides are decidable for that figure:
//!    - at least [`MIN_LIVE_PAIRS`] pairs carrying a p90 on both sides, and
//!      the candidate's paired mean `pinball4_loss_sec` (q = .25, .5, .75,
//!      .9 — the deciding loss) no worse than `current`'s;
//!    - no late-surprise regression: the candidate's `actual > p90` rate at
//!      most [`LATE_SURPRISE_SLACK`] above `current`'s, over at least
//!      [`MIN_LIVE_PAIRS`] pairs where both are decided (censored outcomes
//!      included — see `tracker_censor.rs`);
//!    - no answer-rate regression: the candidate's answer rate, counted once
//!      per tracker pass rather than per emitted row, at most
//!      [`ANSWER_RATE_SLACK`] below `current`'s;
//!    - the candidate's p25–p75 coverage inside
//!      `[`[`COVERAGE_MIN`]`, `[`COVERAGE_MAX`]`]`;
//!    - the win holds day by day: over at least [`MIN_FOLDS`] decided UTC
//!      days of `as_of`, the 95% Wilson lower bound of the candidate's
//!      per-day win rate on the deciding loss is above 50%.
//!
//! Only a `candidate`-tier heuristic is ever promoted (#10525,
//! [`super::shadow_fleet`]): a `baseline` or `retired` one is refused whatever
//! its numbers, and the decision records the tier.
//!
//! Either gate failing leaves `current` exactly as it was. Every evaluation —
//! promoting or not — produces a [`PromotionDecision`] carrying the numbers
//! that decided it, so an operator can answer "why did this flip?" (or "why
//! has it not?") from the record rather than by re-deriving it.
//!
//! # Where a flip is persisted
//!
//! [`super::config::promote`] writes `autonomous.eta.current.<kind>` into the
//! **host-local** config tier (`.loom-local/local.json`, the highest-precedence
//! tier and a gitignored one). Host-local is the honest scope: the evidence
//! behind the flip is this host's own history and this host's own live pairs
//! (#9343), and a daemon must never dirty a tracked file under a fleet of
//! worktrees. Rolling a promotion fleet-wide is an operator action — copy the
//! key into the committed config — not something a single host may decide for
//! everyone.

use super::backtest::Comparison;
use super::score::Score;
use super::tracker::{PassAnswers, Resolved};
use super::{Kind, Tier};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Fewest live paired observations before the live gate may pass.
pub const MIN_LIVE_PAIRS: usize = 50;

/// Lowest candidate p25–p75 coverage the live gate accepts.
pub const COVERAGE_MIN: f64 = 0.40;

/// Highest candidate p25–p75 coverage the live gate accepts.
pub const COVERAGE_MAX: f64 = 0.60;

/// Fewest decided days (folds) the per-day win rate is judged over (#10233).
pub const MIN_FOLDS: usize = 7;

/// How many points the candidate's late-surprise rate (`actual > p90`) may
/// exceed `current`'s before it counts as a regression.
pub const LATE_SURPRISE_SLACK: f64 = 0.02;

/// How many points the candidate's answer rate may fall below `current`'s
/// before it counts as a regression.
pub const ANSWER_RATE_SLACK: f64 = 0.01;

/// Schema tag of one promotion-decision record. Every field added since v1
/// (#10233) is additive and defaults on read, so the tag is unchanged.
pub const DECISION_SCHEMA: &str = "eta-promotion-decision/v1";

/// Which two heuristics, for which kind, a run of pairs is between.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PairKey {
    /// What both predict.
    pub kind: Kind,
    /// The `current` heuristic's id at the time the pairs were taken.
    pub current: String,
    /// The candidate's id.
    pub candidate: String,
}

/// Running sums for one [`PairKey`]. Serialised as-is: the ledger persists.
///
/// Every field defaults (#10233): a `shadow.json` written before a field
/// existed must still parse **with its counts intact**. Without the default a
/// new field made the old file unparseable, and the load fell back to an
/// empty ledger — silently restarting every host's 50-pair count.
///
/// Each metric has its own pair count, because each is counted only on the
/// pairs where **both** sides are decidable for it — the common decidable
/// subset. A side that refused, or that recorded no p90, never makes the
/// other side's figure look better or worse by being left out of one half.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PairSums {
    /// Paired observations (both sides scored against the same outcome).
    pub pairs: usize,
    /// Σ of `current`'s pinball loss over those pairs, seconds.
    pub current_loss_sec: f64,
    /// Σ of the candidate's pinball loss over those pairs, seconds.
    pub candidate_loss_sec: f64,
    /// How many pairs `current` covered (`p25 ≤ actual ≤ p75`).
    pub current_covered: usize,
    /// How many pairs the candidate covered.
    pub candidate_covered: usize,
    /// Pairs where both sides carry a four-quantile pinball loss (both
    /// recorded a p90) — the deciding loss's population.
    pub loss4_pairs: usize,
    /// Σ of `current`'s `pinball4_loss_sec` over those pairs.
    pub current_loss4_sec: f64,
    /// Σ of the candidate's `pinball4_loss_sec` over those pairs.
    pub candidate_loss4_sec: f64,
    /// Pairs where both sides' late surprise (`actual > p90`) is decided,
    /// including censored ones ([`super::score::OutcomeKind::Censored`]).
    pub late_pairs: usize,
    /// How many of those `current` was late on.
    pub current_late: usize,
    /// How many the candidate was late on.
    pub candidate_late: usize,
    /// Tracker passes where both sides had an emitted state for the same
    /// live subject ([`super::tracker::PassAnswers`]).
    pub answer_pairs: usize,
    /// How many of those `current` was answering in.
    pub current_answered: usize,
    /// How many the candidate was answering in.
    pub candidate_answered: usize,
}

/// One UTC day's deciding-loss sums for one [`PairKey`] — a fold of the
/// per-day win rate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DaySums {
    /// Pairs (by `as_of` day) where both sides carry `pinball4_loss_sec`.
    pub pairs: usize,
    /// Σ of `current`'s four-quantile loss that day.
    pub current_loss4_sec: f64,
    /// Σ of the candidate's.
    pub candidate_loss4_sec: f64,
}

/// The per-day win rate of the candidate and its confidence interval.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DayWins {
    /// Days on which one side had the lower mean deciding loss (ties are
    /// left out, as a sign test leaves them out).
    pub days: usize,
    /// Of those, the days the candidate won.
    pub wins: usize,
    /// Days the two tied exactly.
    pub ties: usize,
    /// `wins / days`.
    pub win_rate: Option<f64>,
    /// Lower bound of the 95% Wilson score interval of the win rate.
    pub ci_low: Option<f64>,
    /// Upper bound.
    pub ci_high: Option<f64>,
}

impl DayWins {
    /// Count the candidate's winning days in `days`.
    #[must_use]
    pub fn of(days: &BTreeMap<String, DaySums>) -> Self {
        let mut out = DayWins::default();
        for day in days.values().filter(|d| d.pairs > 0) {
            let n = day.pairs as f64;
            let (current, candidate) = (day.current_loss4_sec / n, day.candidate_loss4_sec / n);
            if candidate < current {
                out.days += 1;
                out.wins += 1;
            } else if current < candidate {
                out.days += 1;
            } else {
                out.ties += 1;
            }
        }
        out.with_interval()
    }

    /// The other side's view of the same days: its wins are this side's
    /// losses (#10233 — a backtest pairs in argument order, not by role).
    #[must_use]
    pub fn swapped(self) -> Self {
        DayWins {
            days: self.days,
            wins: self.days - self.wins,
            ties: self.ties,
            ..DayWins::default()
        }
        .with_interval()
    }

    fn with_interval(self) -> Self {
        let mut out = self;
        if out.days > 0 {
            // Four decimals: the record is logged as JSON and read back, and
            // a 17-digit float does not survive that round trip exactly.
            let round = |x: f64| (x * 1e4).round() / 1e4;
            let (low, high) = wilson(out.wins, out.days);
            out.win_rate = Some(round(out.wins as f64 / out.days as f64));
            out.ci_low = Some(round(low));
            out.ci_high = Some(round(high));
        }
        out
    }
}

/// The 95% Wilson score interval of `wins` successes in `n` trials. `n > 0`.
#[must_use]
pub fn wilson(wins: usize, n: usize) -> (f64, f64) {
    const Z: f64 = 1.959_963_984_540_054;
    let n = n as f64;
    let p = wins as f64 / n;
    let z2 = Z * Z;
    let denominator = 1.0 + z2 / n;
    let centre = (p + z2 / (2.0 * n)) / denominator;
    let margin = Z * (p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt() / denominator;
    ((centre - margin).max(0.0), (centre + margin).min(1.0))
}

/// A readable view of one [`PairKey`]'s accumulated evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairedStats {
    /// Which comparison.
    pub key: PairKey,
    /// Paired observations.
    pub pairs: usize,
    /// `current`'s paired mean pinball loss, seconds. `None` with no pairs.
    pub current_mean_pinball_loss_sec: Option<f64>,
    /// The candidate's paired mean pinball loss, seconds.
    pub candidate_mean_pinball_loss_sec: Option<f64>,
    /// `current`'s p25–p75 coverage over the pairs.
    pub current_coverage: Option<f64>,
    /// The candidate's p25–p75 coverage over the pairs — the gated one.
    pub candidate_coverage: Option<f64>,
    /// Pairs carrying the four-quantile loss on both sides (#10233).
    #[serde(default)]
    pub loss4_pairs: usize,
    /// `current`'s paired mean `pinball4_loss_sec` — the deciding loss.
    #[serde(default)]
    pub current_mean_pinball4_loss_sec: Option<f64>,
    /// The candidate's.
    #[serde(default)]
    pub candidate_mean_pinball4_loss_sec: Option<f64>,
    /// Pairs whose late surprise is decided on both sides.
    #[serde(default)]
    pub late_pairs: usize,
    /// `current`'s late-surprise rate (`actual > p90`) over them.
    #[serde(default)]
    pub current_late_rate: Option<f64>,
    /// The candidate's.
    #[serde(default)]
    pub candidate_late_rate: Option<f64>,
    /// Tracker passes with an emitted state on both sides.
    #[serde(default)]
    pub answer_pairs: usize,
    /// `current`'s answer rate over them.
    #[serde(default)]
    pub current_answer_rate: Option<f64>,
    /// The candidate's.
    #[serde(default)]
    pub candidate_answer_rate: Option<f64>,
    /// The candidate's per-day win rate on the deciding loss.
    #[serde(default)]
    pub day_wins: DayWins,
}

impl PairedStats {
    /// The stats of `sums` under `key`, with no per-day folds.
    #[must_use]
    pub fn of(key: PairKey, sums: PairSums) -> Self {
        let n = sums.pairs;
        let mean = |total: f64, n: usize| (n > 0).then(|| total / n as f64);
        let share = |count: usize, n: usize| (n > 0).then(|| count as f64 / n as f64);
        PairedStats {
            key,
            pairs: n,
            current_mean_pinball_loss_sec: mean(sums.current_loss_sec, n),
            candidate_mean_pinball_loss_sec: mean(sums.candidate_loss_sec, n),
            current_coverage: share(sums.current_covered, n),
            candidate_coverage: share(sums.candidate_covered, n),
            loss4_pairs: sums.loss4_pairs,
            current_mean_pinball4_loss_sec: mean(sums.current_loss4_sec, sums.loss4_pairs),
            candidate_mean_pinball4_loss_sec: mean(sums.candidate_loss4_sec, sums.loss4_pairs),
            late_pairs: sums.late_pairs,
            current_late_rate: share(sums.current_late, sums.late_pairs),
            candidate_late_rate: share(sums.candidate_late, sums.late_pairs),
            answer_pairs: sums.answer_pairs,
            current_answer_rate: share(sums.current_answered, sums.answer_pairs),
            candidate_answer_rate: share(sums.candidate_answered, sums.answer_pairs),
            day_wins: DayWins::default(),
        }
    }

    /// These stats with the per-day win rate over `days`.
    #[must_use]
    pub fn with_days(mut self, days: &BTreeMap<String, DaySums>) -> Self {
        self.day_wins = DayWins::of(days);
        self
    }
}

/// The live paired-scoring accumulator.
///
/// Persisted across restarts as one JSON document (`.loom/state/eta/
/// shadow.json`), like the pending-estimate file beside it: a promotion gate
/// that needs 50 pairs cannot afford to restart its count on every daemon
/// roll.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShadowLedger {
    /// Running sums per comparison.
    pub pairs: BTreeMap<String, PairSums>,
    /// Keys, alongside their encoded form, so the map stays JSON-object
    /// shaped while [`PairKey`] stays structured.
    pub keys: BTreeMap<String, PairKey>,
    /// Per comparison, the deciding-loss sums of each UTC day of `as_of`
    /// (`YYYY-MM-DD`), newest [`MAX_DAYS`] kept (#10233).
    pub days: BTreeMap<String, BTreeMap<String, DaySums>>,
}

/// Most per-day folds kept per comparison; the oldest go first.
pub const MAX_DAYS: usize = 120;

/// `kind|current|candidate`, the map key for a [`PairKey`].
fn encode(key: &PairKey) -> String {
    format!("{}|{}|{}", key.kind.as_str(), key.current, key.candidate)
}

/// One scored estimate, reduced to what pairing needs.
struct Scored<'a> {
    heuristic: &'a str,
    score: &'a Score,
}

impl ShadowLedger {
    /// Record every pair `resolved` contains.
    ///
    /// Two resolved estimates pair when they share a subject (`repo`,
    /// `issue`), a [`Kind`] **and an `as_of`** — which is exactly the shape
    /// shadow mode produces, since one pass estimates every heuristic of a
    /// kind at one instant. Each metric counts a pair only when **both** sides
    /// are decidable for it (#10233): the three-quantile loss and coverage
    /// when both scored, the four-quantile loss when both carry it, the late
    /// surprise when both have one (a censored outcome has only that). An
    /// `abandoned` outcome and a refusal score nothing, and neither may be
    /// counted as a win for whichever side happened to answer.
    ///
    /// `current` names the heuristic that was primary; every other heuristic
    /// in the group is a candidate paired against it. A group with no
    /// `current` side records nothing: there is no baseline to compare to.
    pub fn record(&mut self, current: &dyn Fn(Kind) -> String, resolved: &[Resolved]) {
        let mut groups: BTreeMap<(String, u32, Kind, DateTime<Utc>), Vec<Scored<'_>>> =
            BTreeMap::new();
        for r in resolved {
            let key = (
                r.estimate.repo.to_ascii_lowercase(),
                r.estimate.issue,
                r.estimate.kind,
                r.estimate.as_of,
            );
            groups.entry(key).or_default().push(Scored {
                heuristic: r.estimate.heuristic.as_str(),
                score: &r.score,
            });
        }

        for ((_, _, kind, as_of), group) in groups {
            let current_id = current(kind);
            let Some(base) = group.iter().find(|s| s.heuristic == current_id) else {
                continue;
            };
            for other in group.iter().filter(|s| s.heuristic != current_id) {
                let (b, o) = (base.score, other.score);
                let loss = b.pinball_loss_sec.zip(o.pinball_loss_sec);
                let loss4 = b.pinball4_loss_sec.zip(o.pinball4_loss_sec);
                let late = b.above_p90.zip(o.above_p90);
                if loss.is_none() && loss4.is_none() && late.is_none() {
                    continue;
                }
                let encoded = self.key_for(kind, &current_id, other.heuristic);
                let sums = self.pairs.entry(encoded.clone()).or_default();
                if let Some((current_loss, candidate_loss)) = loss {
                    sums.pairs += 1;
                    sums.current_loss_sec += current_loss;
                    sums.candidate_loss_sec += candidate_loss;
                    sums.current_covered += usize::from(b.covered == Some(true));
                    sums.candidate_covered += usize::from(o.covered == Some(true));
                }
                if let Some((current_late, candidate_late)) = late {
                    sums.late_pairs += 1;
                    sums.current_late += usize::from(current_late);
                    sums.candidate_late += usize::from(candidate_late);
                }
                if let Some((current_loss4, candidate_loss4)) = loss4 {
                    sums.loss4_pairs += 1;
                    sums.current_loss4_sec += current_loss4;
                    sums.candidate_loss4_sec += candidate_loss4;
                    let days = self.days.entry(encoded).or_default();
                    let day = days
                        .entry(as_of.format("%Y-%m-%d").to_string())
                        .or_default();
                    day.pairs += 1;
                    day.current_loss4_sec += current_loss4;
                    day.candidate_loss4_sec += candidate_loss4;
                    while days.len() > MAX_DAYS {
                        days.pop_first();
                    }
                }
            }
        }
    }

    /// Record one tracker pass's answer states (#10233): for each live
    /// subject, `current`'s state paired with every candidate's. A candidate
    /// with no emitted state pairs with nothing — it is not counted as a
    /// refusal, and `current` is not credited for answering against it.
    pub fn record_answers(&mut self, current: &dyn Fn(Kind) -> String, passes: &[PassAnswers]) {
        for pass in passes {
            let current_id = current(pass.kind);
            let Some((_, base)) = pass.states.iter().find(|(h, _)| *h == current_id) else {
                continue;
            };
            for (candidate, answered) in pass.states.iter().filter(|(h, _)| *h != current_id) {
                let encoded = self.key_for(pass.kind, &current_id, candidate);
                let sums = self.pairs.entry(encoded).or_default();
                sums.answer_pairs += 1;
                sums.current_answered += usize::from(*base);
                sums.candidate_answered += usize::from(*answered);
            }
        }
    }

    /// The encoded key of `(kind, current, candidate)`, registered.
    fn key_for(&mut self, kind: Kind, current: &str, candidate: &str) -> String {
        let key = PairKey {
            kind,
            current: current.to_string(),
            candidate: candidate.to_string(),
        };
        let encoded = encode(&key);
        self.keys.entry(encoded.clone()).or_insert(key);
        encoded
    }

    /// The accumulated evidence for one comparison (all-zero when there is
    /// none — "no pairs yet" is an answer, never an absence).
    #[must_use]
    pub fn stats(&self, kind: Kind, current: &str, candidate: &str) -> PairedStats {
        let key = PairKey {
            kind,
            current: current.to_string(),
            candidate: candidate.to_string(),
        };
        self.stats_of(&encode(&key), key)
    }

    fn stats_of(&self, encoded: &str, key: PairKey) -> PairedStats {
        let sums = self.pairs.get(encoded).copied().unwrap_or_default();
        let stats = PairedStats::of(key, sums);
        match self.days.get(encoded) {
            Some(days) => stats.with_days(days),
            None => stats,
        }
    }

    /// Every comparison the ledger holds evidence for.
    #[must_use]
    pub fn all(&self) -> Vec<PairedStats> {
        self.keys
            .iter()
            .map(|(encoded, key)| self.stats_of(encoded, key.clone()))
            .collect()
    }

    /// Forget every pair recorded against `key` — what a completed promotion
    /// does, so the new `current`'s own comparisons start from zero rather
    /// than inheriting the pairs that justified the flip.
    pub fn clear(&mut self, key: &PairKey) {
        let encoded = encode(key);
        self.pairs.remove(&encoded);
        self.days.remove(&encoded);
        self.keys.remove(&encoded);
    }
}

/// How one gate came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateStatus {
    /// The gate passed.
    Passed,
    /// The gate failed; `current` stands.
    Failed,
    /// The gate was never reached — an earlier gate in the order failed, so
    /// this one was deliberately not consulted.
    NotReached,
}

impl GateStatus {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            GateStatus::Passed => "passed",
            GateStatus::Failed => "failed",
            GateStatus::NotReached => "not_reached",
        }
    }
}

/// The backtest gate's verdict and the numbers behind it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BacktestGate {
    /// Outcome.
    pub status: GateStatus,
    /// Why, in one line.
    pub detail: String,
    /// Cases the backtest actually scored for `current`.
    pub current_scored: usize,
    /// Cases it scored for the candidate (the same replay set).
    pub candidate_scored: usize,
    /// `current`'s mean pinball loss over the cases **both** scored
    /// (#10233; before it, over the cases each scored on its own), seconds.
    pub current_mean_pinball_loss_sec: Option<f64>,
    /// The candidate's.
    pub candidate_mean_pinball_loss_sec: Option<f64>,
    /// The union of replayed cases (#10233; additive, an older record reads 0).
    #[serde(default)]
    pub cases: usize,
    /// `current`'s answer rate over the union — refusals count against it.
    #[serde(default)]
    pub current_answer_rate: Option<f64>,
    /// The candidate's.
    #[serde(default)]
    pub candidate_answer_rate: Option<f64>,
    /// Cases both scored with a p90: the deciding loss's population.
    #[serde(default)]
    pub loss4_pairs: usize,
    /// `current`'s mean `pinball4_loss_sec` over them — the deciding loss.
    #[serde(default)]
    pub current_mean_pinball4_loss_sec: Option<f64>,
    /// The candidate's.
    #[serde(default)]
    pub candidate_mean_pinball4_loss_sec: Option<f64>,
    /// The candidate's per-day win rate over the walk-forward daily folds.
    #[serde(default)]
    pub day_wins: DayWins,
    /// Decided days the per-day win rate required.
    #[serde(default)]
    pub min_folds: usize,
}

/// The live gate's verdict and the numbers behind it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveGate {
    /// Outcome.
    pub status: GateStatus,
    /// Why, in one line.
    pub detail: String,
    /// The paired evidence, as accumulated.
    pub stats: PairedStats,
    /// Pairs the gate required.
    pub min_pairs: usize,
    /// Coverage band the gate required.
    pub coverage_band: (f64, f64),
    /// Decided days the per-day win rate required (#10233; additive, so a
    /// decision logged before it still parses, reading 0).
    #[serde(default)]
    pub min_folds: usize,
    /// How far the candidate's late-surprise rate may exceed `current`'s.
    #[serde(default)]
    pub late_surprise_slack: f64,
    /// How far the candidate's answer rate may fall below `current`'s.
    #[serde(default)]
    pub answer_rate_slack: f64,
}

/// One promotion evaluation, whatever it decided.
///
/// This is the auditable record the operator reads: which candidate, against
/// which `current`, what each gate saw, and whether the config was flipped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromotionDecision {
    /// Always [`DECISION_SCHEMA`].
    pub schema: String,
    /// When the evaluation ran.
    pub at: DateTime<Utc>,
    /// What is being promoted for.
    pub kind: Kind,
    /// The incumbent.
    pub current: String,
    /// The challenger.
    pub candidate: String,
    /// The challenger's tier (#10525); only a `candidate` is ever promoted.
    /// `None` for an id this build never shipped, and in a record logged
    /// before tiers existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_tier: Option<Tier>,
    /// Gate 1.
    pub backtest: BacktestGate,
    /// Gate 2 — `NotReached` when gate 1 failed.
    pub live: LiveGate,
    /// Whether both gates passed, i.e. whether `current` may be flipped.
    pub promote: bool,
    /// One-line summary of the deciding reason.
    pub reason: String,
    /// The config file the flip was written to, when one was.
    pub config_path: Option<String>,
}

/// Evaluate the promotion rule for `candidate` against `current`.
///
/// `comparison` is the phase-2 backtest of the two on the identical replay set
/// (`None` when no backtest could be run at all, which is a gate **failure**,
/// never a pass: an unrun gate has not been cleared). `stats` is the live
/// paired evidence from a [`ShadowLedger`].
///
/// Pure: it decides, it does not write. [`promote_if_ready`] is what acts on
/// the decision.
#[must_use]
pub fn evaluate(
    kind: Kind,
    current: &str,
    candidate: &str,
    comparison: Option<&Comparison>,
    stats: &PairedStats,
    at: DateTime<Utc>,
) -> PromotionDecision {
    let backtest = backtest_gate(current, candidate, comparison);
    let live = if backtest.status == GateStatus::Passed {
        live_gate(stats)
    } else {
        LiveGate {
            status: GateStatus::NotReached,
            detail: "backtest gate failed; the live gate is not consulted".to_string(),
            stats: stats.clone(),
            min_pairs: MIN_LIVE_PAIRS,
            coverage_band: (COVERAGE_MIN, COVERAGE_MAX),
            min_folds: MIN_FOLDS,
            late_surprise_slack: LATE_SURPRISE_SLACK,
            answer_rate_slack: ANSWER_RATE_SLACK,
        }
    };
    // #10525: the gate considers only `candidate` heuristics. An id this
    // build never shipped (a test double) is judged on the numbers alone; the
    // CLI refuses an unknown id before it gets here.
    let candidate_tier = super::shadow_fleet::builtin_tier(candidate);
    let promotable = candidate_tier.is_none_or(|tier| tier == Tier::Candidate);
    let promote =
        promotable && backtest.status == GateStatus::Passed && live.status == GateStatus::Passed;
    let reason = if let Some(tier) = candidate_tier.filter(|_| !promotable) {
        format!(
            "{candidate} is a {tier} heuristic and is never promoted: \
             the gate considers only candidate heuristics (#10525)"
        )
    } else if promote {
        format!("both gates passed: {} then {}", backtest.detail, live.detail)
    } else if backtest.status == GateStatus::Passed {
        format!("live gate failed: {}", live.detail)
    } else {
        format!("backtest gate failed: {}", backtest.detail)
    };
    PromotionDecision {
        schema: DECISION_SCHEMA.to_string(),
        at,
        kind,
        current: current.to_string(),
        candidate: candidate.to_string(),
        candidate_tier,
        backtest,
        live,
        promote,
        reason,
        config_path: None,
    }
}

/// Which report in `comparison` belongs to `id`.
fn report_for<'a>(
    comparison: &'a Comparison,
    id: &str,
) -> Option<&'a super::backtest::BacktestReport> {
    if comparison.a.heuristic == id {
        Some(&comparison.a)
    } else if comparison.b.heuristic == id {
        Some(&comparison.b)
    } else {
        None
    }
}

/// The backtest half of the promotion gate for `candidate` against `current`
/// on `comparison` — the one implementation `eta promote` ([`evaluate`]) and
/// the nightly folds' `gate_ready` (#10492) both call, so the two cannot
/// disagree on the same data.
#[must_use]
pub fn backtest_gate(
    current: &str,
    candidate: &str,
    comparison: Option<&Comparison>,
) -> BacktestGate {
    let fail = |detail: String| BacktestGate {
        status: GateStatus::Failed,
        detail,
        current_scored: 0,
        candidate_scored: 0,
        current_mean_pinball_loss_sec: None,
        candidate_mean_pinball_loss_sec: None,
        cases: 0,
        current_answer_rate: None,
        candidate_answer_rate: None,
        loss4_pairs: 0,
        current_mean_pinball4_loss_sec: None,
        candidate_mean_pinball4_loss_sec: None,
        day_wins: DayWins::default(),
        min_folds: MIN_FOLDS,
    };
    let Some(comparison) = comparison else {
        return fail("no backtest was run".to_string());
    };
    let (Some(a), Some(b)) = (report_for(comparison, current), report_for(comparison, candidate))
    else {
        return fail(format!("the comparison is not between {current} and {candidate}"));
    };
    // `paired` names the two sides `a` and `b` in `compare`'s argument
    // order; read it from `current`'s and the candidate's side. The per-day
    // wins are `b`'s, so they are the candidate's only when it is `b`.
    let p = &comparison.paired;
    let current_is_a = comparison.a.heuristic == current;
    let side = |a_value: Option<f64>, b_value: Option<f64>, of_current: bool| {
        if of_current == current_is_a {
            a_value
        } else {
            b_value
        }
    };
    let day_wins = if current_is_a {
        p.day_wins
    } else {
        p.day_wins.swapped()
    };
    let current_loss4 = side(p.a_mean_pinball4_loss_sec, p.b_mean_pinball4_loss_sec, true);
    let candidate_loss4 = side(p.a_mean_pinball4_loss_sec, p.b_mean_pinball4_loss_sec, false);
    let current_answers = side(p.a_answer_rate, p.b_answer_rate, true);
    let candidate_answers = side(p.a_answer_rate, p.b_answer_rate, false);
    let gate = |status: GateStatus, detail: String| BacktestGate {
        status,
        detail,
        current_scored: a.overall.scored,
        candidate_scored: b.overall.scored,
        current_mean_pinball_loss_sec: side(
            p.a_mean_pinball_loss_sec,
            p.b_mean_pinball_loss_sec,
            true,
        ),
        candidate_mean_pinball_loss_sec: side(
            p.a_mean_pinball_loss_sec,
            p.b_mean_pinball_loss_sec,
            false,
        ),
        cases: p.cases,
        current_answer_rate: current_answers,
        candidate_answer_rate: candidate_answers,
        loss4_pairs: p.loss4_pairs,
        current_mean_pinball4_loss_sec: current_loss4,
        candidate_mean_pinball4_loss_sec: candidate_loss4,
        day_wins,
        min_folds: MIN_FOLDS,
    };
    let pct = |rate: Option<f64>| rate.unwrap_or(0.0) * 100.0;
    if p.loss4_pairs == 0 {
        // #9579: the `land` kind derives zero replay cases on this fleet
        // today. Nothing to rule on is a refusal to promote, not a pass.
        return gate(
            GateStatus::Failed,
            format!(
                "the replay set scored {} case(s) for {current} and {} for {candidate}, \
                 {} with a p90 on both: nothing to compare",
                a.overall.scored, b.overall.scored, p.loss4_pairs
            ),
        );
    }
    let figures = format!(
        "{candidate} paired mean pinball4 {:.1}s vs {current}'s {:.1}s over {} common case(s), \
         answer rate {:.1}% vs {:.1}% over {} case(s)",
        candidate_loss4.unwrap_or(f64::NAN),
        current_loss4.unwrap_or(f64::NAN),
        p.loss4_pairs,
        pct(candidate_answers),
        pct(current_answers),
        p.cases
    );
    if comparison.better.as_deref() != Some(candidate) {
        return gate(GateStatus::Failed, format!("{figures}: {candidate} does not win"));
    }
    // The win is not one lucky day: walk-forward daily folds, the same rule
    // as the live gate's.
    if day_wins.days < MIN_FOLDS {
        return gate(
            GateStatus::Failed,
            format!("{figures}, but {} decided day(s), {MIN_FOLDS} required", day_wins.days),
        );
    }
    let low = day_wins.ci_low.unwrap_or(0.0);
    if low <= 0.5 {
        return gate(
            GateStatus::Failed,
            format!(
                "{figures}, but the per-day win rate {}/{} has 95% lower bound {:.1}%, \
                 not above 50%",
                day_wins.wins,
                day_wins.days,
                low * 100.0
            ),
        );
    }
    gate(
        GateStatus::Passed,
        format!(
            "{figures}, won {}/{} day(s) (95% lower bound {:.1}%)",
            day_wins.wins,
            day_wins.days,
            low * 100.0
        ),
    )
}

fn live_gate(stats: &PairedStats) -> LiveGate {
    let gate = |status: GateStatus, detail: String| LiveGate {
        status,
        detail,
        stats: stats.clone(),
        min_pairs: MIN_LIVE_PAIRS,
        coverage_band: (COVERAGE_MIN, COVERAGE_MAX),
        min_folds: MIN_FOLDS,
        late_surprise_slack: LATE_SURPRISE_SLACK,
        answer_rate_slack: ANSWER_RATE_SLACK,
    };
    let fail = |detail: String| gate(GateStatus::Failed, detail);
    let pct = |rate: f64| rate * 100.0;

    // 1. The deciding loss: four-quantile pinball, on the pairs where both
    //    sides carry a p90.
    if stats.loss4_pairs < MIN_LIVE_PAIRS {
        return fail(format!(
            "{} live pair(s) with a p90 on both sides, {MIN_LIVE_PAIRS} required",
            stats.loss4_pairs
        ));
    }
    let (Some(current_loss), Some(candidate_loss)) =
        (stats.current_mean_pinball4_loss_sec, stats.candidate_mean_pinball4_loss_sec)
    else {
        return fail("no paired losses to compare".to_string());
    };
    if candidate_loss > current_loss {
        return fail(format!(
            "paired mean pinball4 {candidate_loss:.1}s is worse than {current_loss:.1}s"
        ));
    }

    // 2. No late-surprise regression, on the pairs where both are decided.
    if stats.late_pairs < MIN_LIVE_PAIRS {
        return fail(format!(
            "{} pair(s) with a decided late surprise on both sides, {MIN_LIVE_PAIRS} required",
            stats.late_pairs
        ));
    }
    let (Some(current_late), Some(candidate_late)) =
        (stats.current_late_rate, stats.candidate_late_rate)
    else {
        return fail("no paired late-surprise evidence".to_string());
    };
    if candidate_late > current_late + LATE_SURPRISE_SLACK {
        return fail(format!(
            "late-surprise rate {:.1}% regresses on {:.1}% (slack {:.0} points)",
            pct(candidate_late),
            pct(current_late),
            pct(LATE_SURPRISE_SLACK)
        ));
    }

    // 3. No answer-rate regression, counted once per tracker pass.
    if stats.answer_pairs < MIN_LIVE_PAIRS {
        return fail(format!(
            "{} paired tracker pass(es) of answer state, {MIN_LIVE_PAIRS} required",
            stats.answer_pairs
        ));
    }
    let (Some(current_answers), Some(candidate_answers)) =
        (stats.current_answer_rate, stats.candidate_answer_rate)
    else {
        return fail("no paired answer-rate evidence".to_string());
    };
    if candidate_answers < current_answers - ANSWER_RATE_SLACK {
        return fail(format!(
            "answer rate {:.1}% regresses on {:.1}% (slack {:.0} point(s))",
            pct(candidate_answers),
            pct(current_answers),
            pct(ANSWER_RATE_SLACK)
        ));
    }

    // 4. Calibration: the candidate's p25-p75 coverage inside the band.
    let Some(coverage) = stats.candidate_coverage else {
        return fail("no coverage to check".to_string());
    };
    if !(COVERAGE_MIN..=COVERAGE_MAX).contains(&coverage) {
        return fail(format!(
            "coverage {:.1}% is outside [{:.0}%, {:.0}%]",
            pct(coverage),
            pct(COVERAGE_MIN),
            pct(COVERAGE_MAX)
        ));
    }

    // 5. The win is not one lucky day: a per-day win rate over at least
    //    MIN_FOLDS decided days, its 95% lower bound above a coin flip.
    let wins = stats.day_wins;
    if wins.days < MIN_FOLDS {
        return fail(format!(
            "{} decided day(s) of paired evidence, {MIN_FOLDS} required",
            wins.days
        ));
    }
    let low = wins.ci_low.unwrap_or(0.0);
    if low <= 0.5 {
        return fail(format!(
            "per-day win rate {}/{} has 95% lower bound {:.1}%, not above 50%",
            wins.wins,
            wins.days,
            pct(low)
        ));
    }
    gate(
        GateStatus::Passed,
        format!(
            "{} pair(s), paired mean pinball4 {candidate_loss:.1}s vs {current_loss:.1}s, \
             late surprise {:.1}% vs {:.1}%, answer rate {:.1}% vs {:.1}%, coverage {:.1}%, \
             won {}/{} day(s) (95% lower bound {:.1}%)",
            stats.loss4_pairs,
            pct(candidate_late),
            pct(current_late),
            pct(candidate_answers),
            pct(current_answers),
            pct(coverage),
            wins.wins,
            wins.days,
            pct(low)
        ),
    )
}

/// Evaluate, and **on a pass only**, flip `autonomous.eta.current.<kind>` in
/// `config_path` and clear the comparison's accumulated pairs.
///
/// The decision is returned either way, with `config_path` filled in exactly
/// when a flip was written. A failed gate touches nothing.
///
/// # Errors
///
/// The config file could not be read or rewritten. The decision is lost with
/// it — deliberately: a flip that was not persisted must not be recorded as
/// one.
pub fn promote_if_ready(
    ledger: &mut ShadowLedger,
    kind: Kind,
    current: &str,
    candidate: &str,
    comparison: Option<&Comparison>,
    config_path: &std::path::Path,
    at: DateTime<Utc>,
) -> std::io::Result<PromotionDecision> {
    let stats = ledger.stats(kind, current, candidate);
    let mut decision = evaluate(kind, current, candidate, comparison, &stats, at);
    if decision.promote {
        super::config::promote(config_path, kind, candidate)?;
        decision.config_path = Some(config_path.display().to_string());
        ledger.clear(&stats.key);
    }
    Ok(decision)
}

/// File name of the promotion-decision log under `<workspace>/.loom/logs/`.
pub const DECISION_LOG_FILENAME: &str = "eta-promotions.jsonl";

/// Where the shadow ledger persists, and where decisions are logged.
#[must_use]
pub fn ledger_path(workspace_root: &std::path::Path) -> std::path::PathBuf {
    workspace_root
        .join(".loom")
        .join("state")
        .join("eta")
        .join("shadow.json")
}

/// The promotion-decision log for `workspace_root`.
#[must_use]
pub fn decision_log_path(workspace_root: &std::path::Path) -> std::path::PathBuf {
    workspace_root
        .join(".loom")
        .join("logs")
        .join(DECISION_LOG_FILENAME)
}

/// Append `decision` to the log at `path`, one JSON line.
///
/// # Errors
///
/// The directory could not be created, or the append failed.
pub fn append_decision(
    path: &std::path::Path,
    decision: &PromotionDecision,
) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let line = serde_json::to_string(decision).map_err(std::io::Error::other)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")
}

/// Read the ledger at `path`. An absent file is an empty ledger (nothing has
/// been paired yet).
///
/// # Errors
///
/// The file exists but could not be read or parsed. This used to read as an
/// empty ledger too, which made a parse failure **silently** restart the
/// 50-pair count — and the next persist then overwrote the only copy
/// (#10233). Use [`load_ledger`] where a daemon must keep running regardless.
pub fn read_ledger(path: &std::path::Path) -> std::io::Result<ShadowLedger> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ShadowLedger::default());
        }
        Err(error) => return Err(error),
    };
    serde_json::from_str(&text).map_err(|error| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{}: {error}", path.display()))
    })
}

/// The daemon's load: [`read_ledger`], and on a failure set the unreadable
/// file aside as `shadow.json.unreadable-<UTC stamp>` — never overwritten by
/// the next persist — and start empty, loudly. Returns the ledger and, on a
/// failure, a one-line account of what happened for the caller to log.
#[must_use]
pub fn load_ledger(path: &std::path::Path, now: DateTime<Utc>) -> (ShadowLedger, Option<String>) {
    match read_ledger(path) {
        Ok(ledger) => (ledger, None),
        Err(error) => {
            let aside =
                path.with_extension(format!("json.unreadable-{}", now.format("%Y%m%dT%H%M%SZ")));
            let kept = match std::fs::rename(path, &aside) {
                Ok(()) => format!("kept as {}", aside.display()),
                Err(rename) => format!("and could not be set aside ({rename})"),
            };
            let note = format!(
                "the shadow ledger could not be loaded ({error}); its pair counts are NOT \
                 carried into this run — the file was {kept}; starting from an empty ledger"
            );
            (ShadowLedger::default(), Some(note))
        }
    }
}

/// Persist `ledger` to `path` atomically (write-then-rename).
///
/// # Errors
///
/// The directory could not be created, or the write/rename failed.
pub fn write_ledger(path: &std::path::Path, ledger: &ShadowLedger) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string(ledger).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}
