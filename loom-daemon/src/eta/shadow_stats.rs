//! Promotion statistics (#10525): the unit of independence, the primary
//! significance test, the day consistency check and the adaptation check.
//!
//! # Why the item, not the refresh, the hour or the day
//!
//! The tracker re-estimates every live item on every pass, so one landed PR
//! contributes dozens of paired observations, all scored against the **same**
//! outcome. Counting each observation (or each hourly block) as independent
//! multiplies `n` without adding evidence: the 2026-10-06 offline study saw
//! twin-otter's Wilson lower bound at 0.61-0.65 on 6-hour blocks but 0.34-0.44
//! on 2-3 day blocks, because the 6-hour figure counted the same days four
//! times. So:
//!
//! - **Primary test** ([`item_test`]): the paired four-quantile pinball
//!   difference (candidate minus `current`) over every scored observation,
//!   with a 95% **item-clustered** percentile bootstrap interval. Whole items
//!   (`repo#issue`) are resampled ([`super::offline::evaluate::bootstrap`],
//!   the estimator the offline evaluation and loom-experiments backtests
//!   share), so every refresh's prediction is used while each item counts once
//!   in the uncertainty. It passes only with at least [`MIN_DISTINCT_ITEMS`]
//!   distinct items and the interval's upper bound below 0.
//! - **Consistency check** ([`day_consistency`]): at least
//!   [`super::shadow::MIN_FOLDS`] decided UTC days and a strict majority of
//!   them won, so a win cannot come from one regime. Days align with the
//!   daily refit and the operators' cycle. The Wilson interval of the day win
//!   rate is still recorded, but it no longer gates.
//! - Hourly or nightly-fold scoreboards are monitoring only, never the
//!   significance unit.
//!
//! # Why 100 distinct items
//!
//! A percentile cluster bootstrap under-covers with few clusters (Cameron,
//! Gelbach & Miller 2008 report material over-rejection below roughly 30-50
//! clusters), so the floor sits at twice the top of that range. It does not
//! delay promotion on an active repository: `rjwalters/loom` merged 16-123
//! PRs a day over 2026-09-30..10-06 (median 53), so the seven decided days
//! the consistency check already needs carry several hundred distinct items.
//! On a quiet repository the floor is what binds, which is the point.
//!
//! # Adaptation time (#10528)
//!
//! [`adaptation_check`] compares `t_p50` and `t_cov` (how long after a shift
//! the median error, and the p25-p75 coverage, recover). The rule is **no
//! adaptation regression**: when both sides are measured, the candidate's
//! `t_p50` and `t_cov` must each be no longer than `current`'s, so a marginal
//! accuracy gain can never buy slower recovery. No build produces these
//! figures yet, so today every decision records `not_measured`, which does
//! not gate; the decision says so rather than implying the check passed.

use super::offline::evaluate::{bootstrap, Estimate, IssueSums, BOOTSTRAP_SEED};
use super::shadow::{DayWins, PromotionDecision, MIN_FOLDS};
use serde::{Deserialize, Serialize};

/// Distinct items the primary test needs (module docs justify the number).
pub const MIN_DISTINCT_ITEMS: usize = 100;

/// Bootstrap resamples behind [`item_test`].
pub const ITEM_BOOTSTRAP_RESAMPLES: usize = 1_000;

/// The independence unit, as recorded on every decision.
pub const INDEPENDENCE_UNIT: &str = "item (repo#issue)";

/// The sampling method, as recorded on every decision.
pub const SAMPLING_METHOD: &str = "item-clustered percentile bootstrap of the paired mean \
     pinball4 difference (candidate minus current), 1000 resamples, seed 0x10193, 95% interval";

/// Four decimals: the record is logged as JSON and read back, and a 17-digit
/// float does not survive that round trip exactly (as [`DayWins`]).
fn round(x: f64) -> f64 {
    (x * 1e4).round() / 1e4
}

/// The primary test's verdict and the numbers behind it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemTest {
    /// [`INDEPENDENCE_UNIT`].
    pub unit: String,
    /// [`SAMPLING_METHOD`].
    pub sampling: String,
    /// Distinct items with at least one paired observation.
    pub distinct_items: usize,
    /// Paired observations over them (refreshes included).
    pub observations: usize,
    /// Mean paired difference, candidate minus `current`, seconds; negative
    /// favours the candidate.
    pub mean_delta_pinball4_sec: Option<f64>,
    /// 95% interval of that mean, `(lower, upper)`.
    pub ci95: Option<(f64, f64)>,
    /// [`MIN_DISTINCT_ITEMS`] at the time.
    pub min_distinct_items: usize,
    /// Whether the test passed.
    pub passed: bool,
    /// Why, in one line.
    pub detail: String,
}

/// The primary test over per-item `(Σ candidate − current, count)` sums.
#[must_use]
pub fn item_test(sums: &IssueSums) -> ItemTest {
    let distinct = sums.values().filter(|(_, n)| *n > 0).count();
    judge(bootstrap(sums, ITEM_BOOTSTRAP_RESAMPLES, BOOTSTRAP_SEED), distinct)
}

/// The primary test from an already-computed item-bootstrap estimate (the
/// backtest's [`super::backtest_paired::Paired::delta_pinball4_loss_sec`]).
/// `estimate` is candidate minus `current`, or `current` minus candidate when
/// `flip` is set, in which case it is negated and its bounds swapped.
#[must_use]
pub fn item_test_of(estimate: Option<Estimate>, distinct_items: usize, flip: bool) -> ItemTest {
    let estimate = estimate.map(|e| {
        if flip {
            Estimate {
                value: e.value.map(|v| -v),
                lo: e.hi.map(|v| -v),
                hi: e.lo.map(|v| -v),
                n: e.n,
            }
        } else {
            e
        }
    });
    match estimate {
        Some(e) => judge(e, distinct_items),
        None => judge(
            Estimate {
                value: None,
                lo: None,
                hi: None,
                n: 0,
            },
            distinct_items,
        ),
    }
}

fn judge(e: Estimate, distinct: usize) -> ItemTest {
    let mean = e.value.filter(|v| v.is_finite()).map(round);
    let ci =
        e.lo.zip(e.hi)
            .filter(|(lo, hi)| lo.is_finite() && hi.is_finite())
            .map(|(lo, hi)| (round(lo), round(hi)));
    let (passed, detail) = match (mean, ci) {
        _ if distinct < MIN_DISTINCT_ITEMS => (
            false,
            format!(
                "{distinct} distinct item(s) of paired evidence ({} observation(s)), \
                 {MIN_DISTINCT_ITEMS} required",
                e.n
            ),
        ),
        (Some(m), Some((lo, hi))) if hi < 0.0 => (
            true,
            format!(
                "paired pinball4 difference {m:+.1}s (95% item-bootstrap CI {lo:+.1}s..{hi:+.1}s) \
                 over {distinct} item(s), {} observation(s)",
                e.n
            ),
        ),
        (Some(m), Some((lo, hi))) => (
            false,
            format!(
                "paired pinball4 difference {m:+.1}s has 95% item-bootstrap CI \
                 {lo:+.1}s..{hi:+.1}s over {distinct} item(s), which does not exclude 0 \
                 in the candidate's favour"
            ),
        ),
        _ => (false, "no finite paired difference to test".to_string()),
    };
    ItemTest {
        unit: INDEPENDENCE_UNIT.to_string(),
        sampling: SAMPLING_METHOD.to_string(),
        distinct_items: distinct,
        observations: e.n,
        mean_delta_pinball4_sec: mean,
        ci95: ci,
        min_distinct_items: MIN_DISTINCT_ITEMS,
        passed,
        detail,
    }
}

/// The day consistency check: at least [`MIN_FOLDS`] decided days and a
/// strict majority won. `Ok` carries the summary, `Err` the refusal.
///
/// # Errors
/// Too few decided days, or no majority.
pub fn day_consistency(w: &DayWins) -> Result<String, String> {
    if w.days < MIN_FOLDS {
        return Err(format!("{} decided day(s), {MIN_FOLDS} required", w.days));
    }
    if w.wins * 2 <= w.days {
        return Err(format!("per-day win rate {}/{} is not a majority", w.wins, w.days));
    }
    Ok(format!("won {}/{} day(s)", w.wins, w.days))
}

/// How long after a regime shift a heuristic recovers (#10528), seconds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AdaptationTimes {
    /// Until the median absolute error is back within its pre-shift band.
    pub t_p50_sec: f64,
    /// Until the p25-p75 coverage is back within its pre-shift band.
    pub t_cov_sec: f64,
}

/// Outcome of [`adaptation_check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdaptationStatus {
    /// Either side has no finite measurement; not gating, and said so.
    NotMeasured,
    /// The candidate adapts no slower on either figure.
    NoRegression,
    /// The candidate is slower on at least one figure; refuses promotion.
    Regressed,
}

/// The adaptation comparison recorded on every decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdaptationCheck {
    /// Outcome.
    pub status: AdaptationStatus,
    /// The rule applied, in one line.
    pub rule: String,
    /// Why, in one line.
    pub detail: String,
    /// `current`'s figures, when measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<AdaptationTimes>,
    /// The candidate's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<AdaptationTimes>,
}

/// The rule [`adaptation_check`] applies.
pub const ADAPTATION_RULE: &str = "no adaptation regression: the candidate's t_p50 and t_cov \
     must each be no longer than current's; not gating while either side is unmeasured (#10528)";

/// Compare `current`'s and the candidate's adaptation times.
#[must_use]
pub fn adaptation_check(
    current: Option<AdaptationTimes>,
    candidate: Option<AdaptationTimes>,
) -> AdaptationCheck {
    let finite = |t: Option<AdaptationTimes>| {
        t.filter(|t| t.t_p50_sec.is_finite() && t.t_cov_sec.is_finite())
    };
    let (status, detail) = match (finite(current), finite(candidate)) {
        (Some(c), Some(k)) => {
            let slower: Vec<String> = [
                ("t_p50", k.t_p50_sec, c.t_p50_sec),
                ("t_cov", k.t_cov_sec, c.t_cov_sec),
            ]
            .into_iter()
            .filter(|(_, mine, theirs)| mine > theirs)
            .map(|(name, mine, theirs)| format!("{name} {mine:.0}s vs {theirs:.0}s"))
            .collect();
            if slower.is_empty() {
                (
                    AdaptationStatus::NoRegression,
                    format!(
                        "t_p50 {:.0}s vs {:.0}s, t_cov {:.0}s vs {:.0}s",
                        k.t_p50_sec, c.t_p50_sec, k.t_cov_sec, c.t_cov_sec
                    ),
                )
            } else {
                (
                    AdaptationStatus::Regressed,
                    format!("the candidate adapts more slowly: {}", slower.join(", ")),
                )
            }
        }
        _ => (
            AdaptationStatus::NotMeasured,
            "no adaptation-time evidence (t_p50, t_cov) for both sides; this build does \
             not produce it yet (#10528), so the decision does not consider it"
                .to_string(),
        ),
    };
    AdaptationCheck {
        status,
        rule: ADAPTATION_RULE.to_string(),
        detail,
        current,
        candidate,
    }
}

/// Record `check` on `decision`; a [`AdaptationStatus::Regressed`] check
/// refuses the promotion whatever the gates said.
pub fn apply_adaptation(decision: &mut PromotionDecision, check: AdaptationCheck) {
    if check.status == AdaptationStatus::Regressed && decision.promote {
        decision.promote = false;
        decision.reason = format!("adaptation check failed: {}", check.detail);
    }
    decision.adaptation = Some(check);
}
