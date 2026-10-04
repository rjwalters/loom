//! Online interval recalibration for `land` estimates (#10207).
//!
//! # Why
//!
//! `land-v2`'s median is roughly calibrated but its interval is far too
//! narrow: its p25–p75 range holds ~5–12% of landings against a 50% target
//! (#9970, experiment v0 on #10193). In v0 the candidate that only kept
//! `land-v2`'s median and stretched its interval per stage recovered most of
//! the available pinball gain. The defect is interval width, and fixing width
//! needs no new model — only the base heuristic's own track record.
//!
//! # What this module is
//!
//! Two pure functions and the data they read:
//!
//! - [`fit_table`] — from [`CalibrationObservation`]s (the base heuristic's
//!   past `land` estimates and, when known, when each landed), the per-stage
//!   distribution of `ln(actual_remaining / p50)` as of one instant.
//! - [`recalibrate`] — a base estimate plus a [`CalibrationTable`] gives the
//!   recalibrated estimate: `p_τ = p50 × exp(Q(τ) − Q(0.5))` (the default,
//!   [`Mode::SpreadOnly`], which keeps the base median and moves only the
//!   spread) or `p_τ = p50 × exp(Q(τ))` ([`Mode::Full`]).
//!
//! Neither reads anything but its arguments: no clock, no file, no forge.
//! Observations reach an estimate as data inside
//! [`super::history::StageSamples::calibration`], built outside the estimator
//! (the daemon's ETA pass, `eta backtest`), exactly like stage samples.
//!
//! # Point-in-time discipline
//!
//! [`fit_table`] at instant `T` sees only what was knowable before `T`:
//!
//! - an observation whose estimate was made at or after `T` is ignored;
//! - one whose landing became known (`resolved_at`) before `T` is an
//!   **event** at its true ratio;
//! - every other one — still open, or landed but not yet *known* to have
//!   landed at `T` — is a **right-censored lower bound** at
//!   `ln((T − as_of) / p50)`: at `T` all anyone knew was "not landed yet".
//!
//! So perturbing any outcome resolved at or after `T`, or adding any estimate
//! made at or after `T`, cannot move the table (pinned by a test). Because the
//! fit happens at the estimate's own `as_of`, a backtest replay over past
//! instants is leak-free by the same rule as [`super::history::StageSamples::select`].
//!
//! # Censoring
//!
//! Still-open estimates are not a random sample: they are the ones running
//! long. Dropping them would bias the ratio distribution short — the same
//! defect `land-v2` fixed for stage durations. They enter a weighted
//! Kaplan–Meier product-limit estimate on the log-ratio axis, the
//! [`super::grid::km_curve`] discipline (a censored point stays at risk up to
//! its bound and contributes no event; censoring follows events on ties; an
//! unresolvable upper tail clamps to the largest bound seen).
//!
//! # Recency weighting
//!
//! Each observation is weighted `exp(−age / half_life)` by the age of its
//! estimate at `T` ([`Weighting`], default [`DEFAULT_HALF_LIFE_SEC`]). When the
//! weighted events' effective sample size `(Σw)² / Σw²` falls below the
//! minimum, the fit falls back to flat weights rather than refusing — the
//! effective-N rule #10209 proposes for stage samples. The weighting is a
//! parameter so #10209 can swap it without touching the estimator.

use super::explanation::Explanation;
use super::score::{EstimateSummary, OutcomeKind, Score};
use super::{Kind, Stage, WINDOW_DAYS};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Schema tag of one persisted [`CalibrationObservation`].
pub const OBSERVATION_SCHEMA: &str = "eta-calibration-observation/v1";

/// Fewest **events** (resolved landings) a stage needs for its own ratio
/// distribution; below it the stage falls back to the pooled one.
pub const MIN_STAGE_EVENTS: usize = 20;

/// Fewest events the pooled distribution needs; below it the table is empty
/// and [`recalibrate`] returns the base estimate unchanged.
pub const MIN_POOLED_EVENTS: usize = 20;

/// Default recency half-life: seven days.
pub const DEFAULT_HALF_LIFE_SEC: i64 = 7 * 86_400;

/// The ratio's floor, in seconds, on both sides: a landing at the estimate's
/// own instant, or a zero p50, would otherwise be `ln(0)`.
pub const RATIO_FLOOR_SEC: i64 = 1;

/// One base-heuristic `land` estimate and, when known, its landing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalibrationObservation {
    /// Always [`OBSERVATION_SCHEMA`].
    pub schema: String,
    /// The estimate's derived id.
    pub estimate_id: String,
    /// The heuristic that made it ([`super::heuristics::LAND_V2`] live).
    pub heuristic: String,
    /// `owner/repo`.
    pub repo: String,
    /// Issue.
    pub issue: u32,
    /// The stage the item was in at the estimate.
    pub stage: Stage,
    /// When the estimate was made.
    pub as_of: DateTime<Utc>,
    /// Its median remaining seconds.
    pub p50_sec: i64,
    /// When the work landed. `None` while still open.
    pub actual_at: Option<DateTime<Utc>>,
    /// When the landing became known (a forge read can lag the merge).
    /// Never earlier than `actual_at`; `None` while still open.
    pub resolved_at: Option<DateTime<Utc>>,
}

impl CalibrationObservation {
    fn new(summary: &EstimateSummary) -> Option<Self> {
        if summary.kind != Kind::Land {
            return None;
        }
        Some(CalibrationObservation {
            schema: OBSERVATION_SCHEMA.to_string(),
            estimate_id: summary.estimate_id.clone(),
            heuristic: summary.heuristic.clone(),
            repo: summary.repo.clone(),
            issue: summary.issue,
            stage: summary.stage?,
            as_of: summary.as_of,
            p50_sec: summary.p50_sec?,
            actual_at: None,
            resolved_at: None,
        })
    }

    /// A scored estimate that **landed**, learned at `resolved_at`. `None`
    /// for a refusal, a non-`land` kind, or an abandoned outcome (which is
    /// never scored, so it is no evidence about remaining time).
    #[must_use]
    pub fn from_scored(
        summary: &EstimateSummary,
        score: &Score,
        resolved_at: DateTime<Utc>,
    ) -> Option<Self> {
        if score.outcome != OutcomeKind::Landed {
            return None;
        }
        let mut observation = Self::new(summary)?;
        observation.actual_at = Some(score.actual_at);
        observation.resolved_at = Some(resolved_at.max(score.actual_at));
        Some(observation)
    }

    /// A still-pending estimate: a lower bound at any later instant.
    #[must_use]
    pub fn from_pending(summary: &EstimateSummary) -> Option<Self> {
        Self::new(summary)
    }

    /// `(ln ratio, is_event)` as knowable at `t`, or `None` when the estimate
    /// postdates `t` or is outside the history window.
    fn state_at(&self, t: DateTime<Utc>) -> Option<(f64, bool)> {
        if self.as_of >= t || self.as_of < t - Duration::days(WINDOW_DAYS) {
            return None;
        }
        let p50 = self.p50_sec.max(RATIO_FLOOR_SEC) as f64;
        let resolved = match (self.actual_at, self.resolved_at) {
            (Some(actual), Some(known)) if known.max(actual) < t => Some(actual),
            _ => None,
        };
        let (remaining, event) = match resolved {
            Some(actual) => ((actual - self.as_of).num_seconds(), true),
            None => ((t - self.as_of).num_seconds(), false),
        };
        Some(((remaining.max(RATIO_FLOOR_SEC) as f64 / p50).ln(), event))
    }
}

/// How observations are weighted by age.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Weighting {
    /// `exp(−age / half_life)`; `None` weighs every observation equally.
    pub half_life_sec: Option<i64>,
}

impl Default for Weighting {
    fn default() -> Self {
        Weighting {
            half_life_sec: Some(DEFAULT_HALF_LIFE_SEC),
        }
    }
}

/// One ratio distribution: quantiles of `ln(actual_remaining / p50)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RatioQuantiles {
    /// Resolved landings it was fitted from.
    pub n_events: usize,
    /// Still-open (right-censored) estimates it was fitted from.
    pub n_censored: usize,
    /// `(Σw)² / Σw²` over the events, at the weighting actually used.
    pub effective_n: f64,
    /// The half-life used; `None` when it fell back to flat weights.
    pub half_life_sec: Option<i64>,
    /// `Q(0.25)`.
    pub q25: f64,
    /// `Q(0.50)`.
    pub q50: f64,
    /// `Q(0.75)`.
    pub q75: f64,
    /// `Q(0.90)`.
    pub q90: f64,
}

/// Per-stage ratio distributions, as of one instant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalibrationTable {
    /// The refit instant: only what was knowable strictly before it is used.
    pub fitted_as_of: DateTime<Utc>,
    /// Stages with at least [`MIN_STAGE_EVENTS`] events.
    pub per_stage: BTreeMap<Stage, RatioQuantiles>,
    /// Every stage together, when it has at least [`MIN_POOLED_EVENTS`].
    pub pooled: Option<RatioQuantiles>,
}

impl CalibrationTable {
    /// The table with nothing in it: [`recalibrate`] is then the identity.
    #[must_use]
    pub fn empty(fitted_as_of: DateTime<Utc>) -> Self {
        CalibrationTable {
            fitted_as_of,
            per_stage: BTreeMap::new(),
            pooled: None,
        }
    }

    /// The distribution for `stage` and the level it came from.
    #[must_use]
    pub fn lookup(&self, stage: Stage) -> Option<(&'static str, &RatioQuantiles)> {
        self.per_stage
            .get(&stage)
            .map(|q| ("stage", q))
            .or_else(|| self.pooled.as_ref().map(|q| ("pooled", q)))
    }
}

fn round6(x: f64) -> f64 {
    (x * 1_000_000.0).round() / 1_000_000.0
}

/// One point on the log-ratio axis.
#[derive(Debug, Clone, Copy)]
struct Point {
    x: f64,
    event: bool,
    age_sec: i64,
}

/// `(Σw)² / Σw²` over `weights`.
fn effective_n(weights: impl Iterator<Item = f64>) -> f64 {
    let (sum, sum_sq) = weights.fold((0.0, 0.0), |(s, q), w| (s + w, q + w * w));
    if sum_sq > 0.0 {
        sum * sum / sum_sq
    } else {
        0.0
    }
}

/// Fit one distribution over `points`, or `None` below `min_events`.
fn fit_points(points: &[Point], weighting: Weighting, min_events: usize) -> Option<RatioQuantiles> {
    let n_events = points.iter().filter(|p| p.event).count();
    if n_events < min_events {
        return None;
    }
    let weight_of = |half_life: Option<i64>, p: &Point| match half_life {
        Some(h) if h > 0 => (-(p.age_sec as f64) / h as f64).exp(),
        _ => 1.0,
    };
    // Effective-N fallback: too few effective events at the half-life → flat.
    let half_life = weighting.half_life_sec.filter(|&h| {
        effective_n(
            points
                .iter()
                .filter(|p| p.event)
                .map(|p| weight_of(Some(h), p)),
        ) >= min_events as f64
    });
    let mut weighted: Vec<(f64, bool, f64)> = points
        .iter()
        .map(|p| (p.x, p.event, weight_of(half_life, p)))
        .collect();
    // Ascending; events before censored points on a tie ("censoring follows
    // events"); then by weight, so equal multisets sum in one order.
    weighted.sort_by(|a, b| {
        a.0.total_cmp(&b.0)
            .then(b.1.cmp(&a.1))
            .then(a.2.total_cmp(&b.2))
    });
    let total: f64 = weighted.iter().map(|p| p.2).sum();
    let horizon = weighted.last().map_or(0.0, |p| p.0);

    // Weighted product-limit curve: (event x, S after it).
    let mut curve: Vec<(f64, f64)> = Vec::new();
    let mut survival = 1.0_f64;
    let mut before = 0.0_f64; // weight strictly below the current x
    let mut i = 0;
    while i < weighted.len() {
        let x = weighted[i].0;
        let mut deaths = 0.0;
        let mut here = 0.0;
        while i < weighted.len() && weighted[i].0 == x {
            here += weighted[i].2;
            if weighted[i].1 {
                deaths += weighted[i].2;
            }
            i += 1;
        }
        let at_risk = total - before;
        if deaths > 0.0 && at_risk > 0.0 {
            survival *= 1.0 - deaths / at_risk;
            curve.push((x, survival));
        }
        before += here;
    }
    let quantile = |tau: f64| {
        let target = 1.0 - tau;
        curve
            .iter()
            .find(|(_, s)| *s <= target + 1e-12)
            .map_or(horizon, |(x, _)| *x)
    };
    Some(RatioQuantiles {
        n_events,
        n_censored: points.len() - n_events,
        effective_n: round6(effective_n(
            points
                .iter()
                .filter(|p| p.event)
                .map(|p| weight_of(half_life, p)),
        )),
        half_life_sec: half_life,
        q25: round6(quantile(0.25)),
        q50: round6(quantile(0.50)),
        q75: round6(quantile(0.75)),
        q90: round6(quantile(0.90)),
    })
}

/// Fit the calibration table for `base_heuristic`'s observations as of
/// `as_of`. Pure, and point-in-time: see the module doc.
#[must_use]
pub fn fit_table(
    observations: &[CalibrationObservation],
    base_heuristic: &str,
    as_of: DateTime<Utc>,
    weighting: Weighting,
) -> CalibrationTable {
    let mut by_stage: BTreeMap<Stage, Vec<Point>> = BTreeMap::new();
    let mut pooled: Vec<Point> = Vec::new();
    for observation in observations {
        if observation.heuristic != base_heuristic {
            continue;
        }
        let Some((x, event)) = observation.state_at(as_of) else {
            continue;
        };
        let point = Point {
            x,
            event,
            age_sec: (as_of - observation.as_of).num_seconds().max(0),
        };
        by_stage.entry(observation.stage).or_default().push(point);
        pooled.push(point);
    }
    CalibrationTable {
        fitted_as_of: as_of,
        per_stage: by_stage
            .into_iter()
            .filter_map(|(stage, points)| {
                fit_points(&points, weighting, MIN_STAGE_EVENTS).map(|q| (stage, q))
            })
            .collect(),
        pooled: fit_points(&pooled, weighting, MIN_POOLED_EVENTS),
    }
}

/// Which quantiles [`recalibrate`] moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Keep the base median; `p_τ = p50 × exp(Q(τ) − Q(0.5))`.
    SpreadOnly,
    /// Move the median too; `p_τ = p50 × exp(Q(τ))`.
    Full,
}

/// What [`recalibrate`] did, recorded in the explanation so the result
/// recomputes exactly ([`apply`] over the simulated base quantiles).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recalibration {
    /// Always `km_log_ratio`.
    pub method: String,
    /// Which quantiles moved.
    pub mode: Mode,
    /// The heuristic whose track record the table was fitted from.
    pub base_heuristic: String,
    /// `stage` (the current stage's own distribution) or `pooled`.
    pub level: String,
    /// The table's refit instant (= the estimate's `as_of`).
    pub fitted_as_of: DateTime<Utc>,
    /// The base estimate's quartiles, before recalibration.
    pub base_p25_sec: i64,
    /// Base median.
    pub base_p50_sec: i64,
    /// Base upper quartile.
    pub base_p75_sec: i64,
    /// The ratio distribution applied.
    pub ratios: RatioQuantiles,
    /// The recalibrated p90 (the result itself carries p25/p50/p75).
    pub p90_sec: i64,
}

/// `(p25, p50, p75, p90)` from a base median and a ratio distribution.
/// Monotone by construction.
#[must_use]
pub fn apply(base_p50: i64, ratios: &RatioQuantiles, mode: Mode) -> (i64, i64, i64, i64) {
    let anchor = base_p50.max(RATIO_FLOOR_SEC) as f64;
    let shift = match mode {
        Mode::SpreadOnly => ratios.q50,
        Mode::Full => 0.0,
    };
    let at = |q: f64| (anchor * (q - shift).exp()).round() as i64;
    let p50 = match mode {
        Mode::SpreadOnly => base_p50,
        Mode::Full => at(ratios.q50),
    };
    let p25 = at(ratios.q25).min(p50);
    let p75 = at(ratios.q75).max(p50);
    let p90 = at(ratios.q90).max(p75);
    (p25, p50, p75, p90)
}

/// Recalibrate `base` with `table`. Pure.
///
/// The identity — `base` returned untouched — for a refusal, an estimate
/// with no current stage, or a table with nothing for the stage and no
/// pooled fallback (an empty table).
#[must_use]
pub fn recalibrate(
    mut base: Explanation,
    table: &CalibrationTable,
    base_heuristic: &str,
    mode: Mode,
) -> Explanation {
    let Some(stage) = base.current_stage.as_ref().map(|c| c.stage) else {
        return base;
    };
    let Some((level, ratios)) = table.lookup(stage) else {
        return base;
    };
    let as_of = base.as_of;
    let fitted_as_of = table.fitted_as_of;
    let record = match base.result.as_mut() {
        None => return base,
        Some(result) => {
            let (p25, p50, p75, p90) = apply(result.p50_sec, ratios, mode);
            let record = Recalibration {
                method: "km_log_ratio".to_string(),
                mode,
                base_heuristic: base_heuristic.to_string(),
                level: level.to_string(),
                fitted_as_of,
                base_p25_sec: result.p25_sec,
                base_p50_sec: result.p50_sec,
                base_p75_sec: result.p75_sec,
                ratios: ratios.clone(),
                p90_sec: p90,
            };
            result.p25_sec = p25;
            result.p50_sec = p50;
            result.p75_sec = p75;
            result.p90_sec = Some(p90);
            result.eta_p50_at = as_of + Duration::seconds(p50);
            record
        }
    };
    base.recalibration = Some(record);
    base.enforce_cap();
    base
}
