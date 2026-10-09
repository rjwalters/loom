//! IPCW split-conformal calibration over a short, recent window (#10524):
//! the calibrator behind the retired `land-2026-10-06-quick-tern` (#10949),
//! still available offline through [`super::conformal_wrap`].
//!
//! # Why a second calibrator
//!
//! [`super::conformal`] (calm-plover, #10489) calibrates each quantile over a
//! 14-day window, rate-limited to `ln 1.2` a day. When the fleet changes (a
//! planner deploy, a new review policy, #10528) that takes days to follow.
//! Its censoring correction is also on the wrong axis. It runs Kaplan–Meier
//! on the **score** `ln(remaining / q)`, but a still-open estimate is
//! censored in **time**, at `t − as_of`. Rows carry different `q`, so the
//! order on the score axis is not the order on the time axis.
//!
//! # The method
//!
//! - **Score.** The same as calm-plover: `s = ln(actual_remaining / q_τ)`. The
//!   adjusted quantile is `q_τ · exp(c_τ)`.
//! - **Censoring model.** Every calibration row has a known censoring time:
//!   `C_i = t − as_of_i`, the time from its estimate to the fit instant. A
//!   landing is an **event** iff it was known before `t`, at observed time
//!   `d_i = max(actual, known) − as_of_i < C_i`. The censoring survival
//!   `Ĝ(u) = P(C > u)` is the product-limit (Kaplan–Meier) estimate over the
//!   censoring times. Every `C_i` is observed (administrative censoring), so
//!   it is exact: the recency-weighted share of rows with `C > u`.
//! - **IPCW.** An event gets weight `w_i / max(Ĝ(d_i), G_FLOOR)`, which
//!   stands in for the rows like it that are still open only because they
//!   are recent. The weighted CDF of the scores, normalised by the weight of
//!   *all* rows, estimates the score distribution as if nothing were
//!   censored. `c_τ` is its smallest score reaching `τ (n+1)/n`, with
//!   `n` the effective sample size. If the level cannot be reached (the
//!   missing mass is rows that have not landed: still open past the
//!   window's resolution, or lost to the weight cap), the quantile is
//!   unidentified and is moved **conservatively**, never down: the shift is
//!   the largest of 0 (the base), any landing's score and any open row's
//!   elapsed-time bound `ln(C / q)`.
//! - **Recent window.** Rows are weighted `2^(−C/half_life)`, starting from
//!   [`HALF_LIFE_SEC`] (6 h), within [`WINDOW_HALF_LIVES`] half-lives
//!   (capped at [`WINDOW_DAYS`]). When the events' effective N falls below
//!   [`MIN_EFFECTIVE_EVENTS`], the half-life doubles, up to
//!   [`MAX_HALF_LIFE_SEC`]. The shortest window with enough landings
//!   answers.
//! - **No rate limit, but a noise floor.** A day-over-day limit would cap
//!   how fast the wrapper follows a regime shift (the `t_cov ≤ 12 h`
//!   acceptance of #10528). Instead, a raw shift within [`NOISE_Z`]
//!   standard errors of zero is sampling noise and is not applied. The
//!   standard error is `sqrt(τ(1−τ)/n_ipcw)` times the sparsity `dc/dτ`,
//!   measured from the same weighted CDF (Siddiqui–Bloch–Gastwirth), and is
//!   never below [`MIN_DEADBAND`]. A calibrated base is left exactly alone
//!   rather than jittered, and a thin, noisy cell moves only for a real
//!   miscalibration.
//! - **Stratified by stage**, else pooled, else the identity.
//!
//! # Point-in-time
//!
//! The rules are those of [`super::conformal`]. An estimate made at or after
//! `t`, or an outcome *known* at or after `t`, is not used as an event: a
//! landing known only later is a censored row at `t`. Perturbing any
//! post-`as_of` outcome leaves the output bit-identical (pinned by a test).
//!
//! # Drift-aware variant (#10524 slice 3)
//!
//! [`calibrate_drift_aware`] (the retired `land-2026-10-06-swift-tern`) adds the #10528
//! drift check: a drift-shortened half-life ladder. The interval inflation
//! the check would ask for is recorded but withheld. See its doc.
//!
//! # Deferred (#10524 / #10528)
//!
//! - Serving the drift inflation, behind a gate that live evidence supports.
//! - History-aware conditioning.
//!
//! Pure: no clock, no file, no forge.

use super::conformal::{apply, round6, Calibration, CalibrationWindow, Q4, TAUS};
use super::explanation::Explanation;
use super::recalibrate::CalibrationObservation;
use super::{recency, regime, Stage};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::f64::consts::LN_2;

/// Recorded in [`Calibration::method`].
pub const METHOD: &str = "ipcw_split_conformal_log";

/// Recorded in [`Calibration::method`] by [`calibrate_drift_aware`].
pub const METHOD_DRIFT: &str = "ipcw_split_conformal_log_drift";

/// Recorded in [`IpcwRecord::censoring`]: the censoring survival is the
/// product-limit estimate over fully observed (administrative) censoring
/// times `t − as_of`.
pub const CENSORING_MODEL: &str = "km_administrative";

/// Hard cutoff of the calibration window, days.
pub const WINDOW_DAYS: i64 = 7;

/// The window at a half-life is this many half-lives (capped at
/// [`WINDOW_DAYS`]): an older row would weigh under 0.4%, and it would only
/// carry the old regime's tail into the conservative bound of an
/// unidentified quantile.
pub const WINDOW_HALF_LIVES: i64 = 8;

/// The first (shortest) recency half-life tried, seconds.
pub const HALF_LIFE_SEC: i64 = 6 * 3_600;

/// The longest half-life the fallback doubles to, seconds.
pub const MAX_HALF_LIFE_SEC: i64 = 96 * 3_600;

/// Fewest **effective** events (`(Σw)² / Σw²` over the recency weights of
/// known landings) a cell needs at a half-life.
pub const MIN_EFFECTIVE_EVENTS: f64 = 20.0;

/// Floor of `Ĝ`: no event stands in for more than `1 / G_FLOOR` rows.
pub const G_FLOOR: f64 = 0.05;

/// The noise floor is this many standard errors of the shift estimate.
pub const NOISE_Z: f64 = 1.5;

/// The smallest noise floor, ln units, whatever the standard error.
pub const MIN_DEADBAND: f64 = 0.05;

/// Half-width of the level step the sparsity (`dc/dτ`) is measured over.
const SPACING: f64 = 0.05;

/// Quantile labels, in [`TAUS`] order.
const LABELS: [&str; 4] = ["p25", "p50", "p75", "p90"];

/// What the IPCW wrapper weighted with, recorded in
/// [`Calibration::ipcw`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpcwRecord {
    /// Always [`CENSORING_MODEL`].
    pub censoring: String,
    /// The recency half-life used, seconds (the shortest that answered).
    pub half_life_sec: i64,
    /// The window at that half-life, seconds.
    pub window_sec: i64,
    /// Effective number of events at that half-life (recency weights).
    pub n_effective: f64,
    /// [`G_FLOOR`].
    pub g_floor: f64,
    /// The largest inverse-censoring factor `1 / Ĝ(d)` applied.
    pub max_weight: f64,
    /// The noise floor per quantile, ln units: a raw shift smaller in
    /// magnitude is not applied. `0` for an unresolved quantile.
    pub deadband: Q4<f64>,
    /// Quantiles whose level the weighted CDF could not reach, moved
    /// conservatively: to the largest of the base, any landing and any open
    /// row's elapsed time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unresolved: Vec<String>,
    /// The drift check of [`calibrate_drift_aware`]; absent for
    /// [`calibrate`], for a pooled cell, and below the drift check's sample
    /// floor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drift: Option<DriftRecord>,
}

/// One row as knowable at the fit instant.
struct Member {
    stage: Stage,
    /// Censoring time `t − as_of`, seconds (≥ 1).
    censor: f64,
    /// Observed (known) time of the landing, seconds; meaningful for events.
    observed: f64,
    /// Actual remaining seconds (event) or elapsed (censored), ≥ 1.
    remaining: f64,
    /// The base quantiles, seconds (≥ 1).
    q: [f64; 4],
    event: bool,
}

fn member(o: &CalibrationObservation, base: &str, t: DateTime<Utc>) -> Option<Member> {
    if o.heuristic != base || o.as_of >= t || o.as_of < t - Duration::days(WINDOW_DAYS) {
        return None;
    }
    let q = [o.p25_sec?, o.p50_sec, o.p75_sec?, o.p90_sec?].map(|v| v.max(1) as f64);
    let censor = (t - o.as_of).num_seconds().max(1) as f64;
    let (remaining, observed, event) = match (o.actual_at, o.resolved_at) {
        (Some(actual), Some(known)) if known.max(actual) < t => (
            (actual - o.as_of).num_seconds().max(1) as f64,
            (known.max(actual) - o.as_of).num_seconds().max(0) as f64,
            true,
        ),
        _ => (censor, censor, false),
    };
    Some(Member {
        stage: o.stage,
        censor,
        observed,
        remaining,
        q,
        event,
    })
}

/// One cell's fit at one half-life.
struct Fit {
    raw_shift: [f64; 4],
    shift: [f64; 4],
    deadband: [f64; 4],
    half_life_sec: i64,
    window_sec: i64,
    n_effective: f64,
    n_events: usize,
    n_censored: usize,
    max_weight: f64,
    unresolved: Vec<String>,
}

/// `(Σw)² / Σw²`, or 0 for no weight.
fn effective(weights: impl Iterator<Item = f64>) -> f64 {
    let (sum, sq) = weights.fold((0.0, 0.0), |(s, q), w| (s + w, q + w * w));
    if sq > 0.0 {
        sum * sum / sq
    } else {
        0.0
    }
}

/// The smallest score whose weighted mass (`points` ascending, weights
/// already normalised) reaches `level`; `None` when the mass never does.
fn weighted_quantile(points: &[(f64, f64)], level: f64) -> Option<f64> {
    let mut mass = 0.0;
    for (score, weight) in points {
        mass += weight;
        if mass >= level - 1e-12 {
            return Some(*score);
        }
    }
    None
}

/// The shift of quantile `k` when the events' weighted CDF (`points`,
/// ascending, mass normalised by `total`) never reaches `level`.
///
/// The missing mass is rows that have not landed: still open past the last
/// landing, or lost to the weight cap. An open row is evidence of a *longer*
/// duration, so such a quantile never moves down (the result is at least 0,
/// the base). Within that, it is the **smallest value the data allow** (the
/// calm-plover tail rule, #10557): each open row whose elapsed-time bound
/// `ln(C / q)` lies past the last landing counts as a landing at that bound,
/// with its own recency weight, and the shift is the first such bound at
/// which the mass reaches `level`. One long-open outlier therefore cannot set
/// it: replaying the walk-forward folds, the earlier "largest open bound"
/// rule left p90 unresolved for 62% of estimates and moved it a median
/// `exp(3.0) ≈ 20×` (#10524). Only when even every bound leaves the level
/// unreached is it the largest bound or landing.
fn unresolved_tail(
    points: &[(f64, f64)],
    members: &[&Member],
    w: &[f64],
    total: f64,
    k: usize,
    level: f64,
) -> f64 {
    let last_landing = points.last().map_or(f64::NEG_INFINITY, |p| p.0);
    let mut tail: Vec<(f64, f64)> = members
        .iter()
        .zip(w)
        .filter(|(m, _)| !m.event)
        .map(|(m, wi)| ((m.censor / m.q[k]).ln(), wi / total))
        .filter(|(bound, _)| *bound >= last_landing)
        .collect();
    tail.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut mass: f64 = points.iter().map(|p| p.1).sum();
    let mut c = None;
    for (bound, weight) in &tail {
        mass += weight;
        if mass >= level - 1e-12 {
            c = Some(*bound);
            break;
        }
    }
    let c = c.unwrap_or_else(|| tail.last().map_or(last_landing, |p| p.0.max(last_landing)));
    if c.is_finite() {
        c.max(0.0)
    } else {
        0.0
    }
}

/// What the drift-aware variant ([`calibrate_drift_aware`]) checked and did,
/// recorded in [`IpcwRecord::drift`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriftRecord {
    /// Recent residuals (known in the last [`regime::DRIFT_WINDOW_SEC`]).
    pub n_recent: usize,
    /// Baseline residuals (known before the recent window, within
    /// [`regime::BASELINE_WINDOW_SEC`]).
    pub n_baseline: usize,
    /// The p50 shift the first check was centred on: the default-window
    /// fit's applied `shift.p50`.
    pub center: f64,
    /// The CUSUM statistic about [`Self::center`], in baseline standard
    /// deviations.
    pub statistic: f64,
    /// Whether that statistic reached [`regime::CUSUM_H`].
    pub drifted: bool,
    /// The first half-life the ladder tried: [`HALF_LIFE_SEC`], or
    /// `HALF_LIFE_SEC /` [`recency::DRIFTED_DIVISOR`] when drifted.
    pub half_life_start_sec: i64,
    /// The CUSUM statistic about the final fit's `shift.p50`: the drift the
    /// shorter window did not absorb. Equals [`Self::statistic`] when not
    /// drifted.
    pub residual_statistic: f64,
    /// The widening about p50 #10528 would apply for the residual drift
    /// ([`regime::Drift::inflation`] of the residual check), **not applied**
    /// (see [`calibrate_drift_aware`]). `1` is none.
    pub withheld_inflation: f64,
}

/// The IPCW conformal fit of `members` at `half_life_sec`, or `None` when
/// its events' effective N is below [`MIN_EFFECTIVE_EVENTS`].
fn fit_cell(members: &[&Member], half_life_sec: i64) -> Option<Fit> {
    let window_sec = (WINDOW_HALF_LIVES * half_life_sec).min(WINDOW_DAYS * 86_400);
    let members: Vec<&Member> = members
        .iter()
        .copied()
        .filter(|m| m.censor <= window_sec as f64)
        .collect();
    let hl = half_life_sec as f64;
    let w: Vec<f64> = members
        .iter()
        .map(|m| (-LN_2 * m.censor / hl).exp())
        .collect();
    let n_effective = effective(
        members
            .iter()
            .zip(&w)
            .filter(|(m, _)| m.event)
            .map(|(_, w)| *w),
    );
    if n_effective < MIN_EFFECTIVE_EVENTS {
        return None;
    }
    let total: f64 = w.iter().sum();
    let n_all = effective(w.iter().copied());
    // Ĝ(u) = Σ_{C_j > u} w_j / Σ w_j: censoring times ascending, suffix sums.
    let mut by_censor: Vec<(f64, f64)> = members.iter().map(|m| m.censor).zip(w.clone()).collect();
    by_censor.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut suffix = vec![0.0; by_censor.len() + 1];
    for i in (0..by_censor.len()).rev() {
        suffix[i] = suffix[i + 1] + by_censor[i].1;
    }
    let survival = |u: f64| suffix[by_censor.partition_point(|(c, _)| *c <= u)] / total;
    let mut max_weight = 1.0_f64;
    let events: Vec<(&Member, f64)> = members
        .iter()
        .zip(&w)
        .filter(|(m, _)| m.event)
        .map(|(m, wi)| {
            let factor = 1.0 / survival(m.observed).max(G_FLOOR);
            max_weight = max_weight.max(factor);
            (*m, wi * factor)
        })
        .collect();
    let n_ipcw = effective(events.iter().map(|(_, weight)| *weight));
    let mut raw_shift = [0.0; 4];
    let mut shift = [0.0; 4];
    let mut deadband = [0.0; 4];
    let mut unresolved = Vec::new();
    for (k, tau) in TAUS.iter().enumerate() {
        let mut points: Vec<(f64, f64)> = events
            .iter()
            .map(|(m, weight)| ((m.remaining / m.q[k]).ln(), weight / total))
            .collect();
        points.sort_by(|a, b| a.0.total_cmp(&b.0));
        // Finite-sample corrected conformal level.
        let level = (tau * (n_all + 1.0) / n_all).min(1.0);
        match weighted_quantile(&points, level) {
            Some(c) => {
                // Noise floor: NOISE_Z standard errors of the quantile,
                // sqrt(τ(1−τ)/n) times the sparsity dc/dτ measured below τ
                // (always resolvable when τ is).
                let below = weighted_quantile(&points, tau - SPACING).unwrap_or(c);
                let se = (c - below) / SPACING * (tau * (1.0 - tau) / n_ipcw).sqrt();
                let floor = round6((NOISE_Z * se).max(MIN_DEADBAND));
                raw_shift[k] = round6(c);
                deadband[k] = floor;
                shift[k] = if raw_shift[k].abs() < floor {
                    0.0
                } else {
                    raw_shift[k]
                };
            }
            None => {
                unresolved.push(LABELS[k].to_string());
                let c = round6(unresolved_tail(&points, &members, &w, total, k, level));
                raw_shift[k] = c;
                shift[k] = c;
            }
        }
    }
    let n_events = members.iter().filter(|m| m.event).count();
    Some(Fit {
        raw_shift,
        shift,
        deadband,
        half_life_sec,
        window_sec,
        n_effective: round6(n_effective),
        n_events,
        n_censored: members.len() - n_events,
        max_weight: round6(max_weight),
        unresolved,
    })
}

/// The fit at the shortest half-life on the doubling ladder from `start_sec`
/// whose events' effective N reaches [`MIN_EFFECTIVE_EVENTS`], or `None`.
fn fit_ladder(members: &[&Member], start_sec: i64) -> Option<Fit> {
    let mut half_life = start_sec;
    loop {
        if let Some(fit) = fit_cell(members, half_life) {
            return Some(fit);
        }
        if half_life >= MAX_HALF_LIFE_SEC {
            return None;
        }
        half_life = (half_life * 2).min(MAX_HALF_LIFE_SEC);
    }
}

/// Calibrate `explanation` (a base heuristic's estimate) against the track
/// record of `base` in `observations`. Pure.
///
/// The identity (`explanation` returned untouched) for a refusal, an
/// estimate with no current stage or no p90, or no cell with enough
/// effective events at any half-life.
#[must_use]
pub fn calibrate(
    explanation: Explanation,
    observations: &[CalibrationObservation],
    base: &str,
) -> Explanation {
    calibrate_with(explanation, observations, base, false)
}

/// [`calibrate`], made **drift-aware** with the #10528 drift check
/// ([`regime::drift_about`]); the calibrator behind the retired
/// `land-2026-10-06-swift-tern` (#10949). Pure.
///
/// For a stage cell, the residuals `ln(actual / p50)` of `base`'s landings
/// known before `as_of` are checked by CUSUM against the default fit's
/// served p50 shift (not the baseline's mean: a shift the calibrator has
/// already absorbed must not keep the flag up). When the check trips:
/// - **Adaptive half-life.** The ladder restarts from
///   `HALF_LIFE_SEC /` [`recency::DRIFTED_DIVISOR`] (1.5 h): the old regime
///   is forgotten faster. The effective-N floor still applies, so the
///   shorter window never rests on fewer than [`MIN_EFFECTIVE_EVENTS`].
/// - **Inflation is measured, not applied.** The check is re-run about the
///   shorter fit's p50 shift, and the widening #10528 would apply for the
///   drift left over ([`regime::Drift::inflation`]) is recorded as
///   [`DriftRecord::withheld_inflation`]. Applying it over-covers: right
///   after a shift the recent residuals are a mixture of both regimes, so
///   the check stays up after the shorter window has already caught up. On
///   the x0.1 fixture it held p25–p75 coverage at 0.83–0.89 from 1.5 h to
///   6 h after the shift, against 0.42–0.62 without it
///   (`eta::tests::conformal_ipcw_drift`). Serving it
///   needs a gate and live evidence first (as #10563 found for the
///   residual tracker).
///
/// When the check does not trip, or is below its sample floor, or the cell
/// is pooled, the answer is exactly [`calibrate`]'s (plus the `drift`
/// record when the check ran).
#[must_use]
pub fn calibrate_drift_aware(
    explanation: Explanation,
    observations: &[CalibrationObservation],
    base: &str,
) -> Explanation {
    calibrate_with(explanation, observations, base, true)
}

/// The cell fit (stage, else pooled) with the ladder starting at `start_sec`.
fn choose(members: &[Member], stage: Stage, start_sec: i64) -> Option<(&'static str, Fit)> {
    for level in ["stage", "pooled"] {
        let cell: Vec<&Member> = members
            .iter()
            .filter(|m| level == "pooled" || m.stage == stage)
            .collect();
        if let Some(fit) = fit_ladder(&cell, start_sec) {
            return Some((level, fit));
        }
    }
    None
}

/// The residuals of `base`'s landings known strictly before `t`, the same
/// point-in-time rule as [`member`].
fn residuals_before(
    observations: &[CalibrationObservation],
    base: &str,
    t: DateTime<Utc>,
) -> Vec<regime::Residual> {
    let known: Vec<CalibrationObservation> = observations
        .iter()
        .filter(|o| {
            o.heuristic == base
                && o.as_of < t
                && matches!((o.actual_at, o.resolved_at), (Some(a), Some(k)) if a.max(k) < t)
        })
        .cloned()
        .collect();
    regime::residuals(&known)
}

fn calibrate_with(
    mut explanation: Explanation,
    observations: &[CalibrationObservation],
    base: &str,
    drift_aware: bool,
) -> Explanation {
    let Some(stage) = explanation.current_stage.as_ref().map(|c| c.stage) else {
        return explanation;
    };
    let Some(base_q) = explanation.quantiles_with_p90() else {
        return explanation;
    };
    let as_of = explanation.as_of;
    let members: Vec<Member> = observations
        .iter()
        .filter_map(|o| member(o, base, as_of))
        .collect();
    let Some((level, mut fit)) = choose(&members, stage, HALF_LIFE_SEC) else {
        return explanation;
    };
    let mut drift_record = None;
    if drift_aware && level == "stage" {
        let residuals = residuals_before(observations, base, as_of);
        let first = regime::drift_about(&residuals, stage, as_of, fit.shift[1]);
        if first.state() != regime::DriftState::Unknown {
            let mut record = DriftRecord {
                n_recent: first.n_recent,
                n_baseline: first.n_baseline,
                center: fit.shift[1],
                statistic: first.statistic,
                drifted: first.drifted,
                half_life_start_sec: HALF_LIFE_SEC,
                residual_statistic: first.statistic,
                withheld_inflation: 1.0,
            };
            if first.drifted {
                let start = HALF_LIFE_SEC / recency::DRIFTED_DIVISOR;
                let cell: Vec<&Member> = members.iter().filter(|m| m.stage == stage).collect();
                if let Some(short) = fit_ladder(&cell, start) {
                    fit = short;
                }
                let residual = regime::drift_about(&residuals, stage, as_of, fit.shift[1]);
                record.half_life_start_sec = start;
                record.residual_statistic = residual.statistic;
                record.withheld_inflation = residual.inflation();
            }
            drift_record = Some(record);
        }
    }
    let shift = Q4::from_array(fit.shift);
    let (p25, p50, p75, p90) = apply(base_q, &shift);
    let record = Calibration {
        method: if drift_aware { METHOD_DRIFT } else { METHOD }.to_string(),
        base: base.to_string(),
        window: CalibrationWindow {
            from: as_of - Duration::seconds(fit.window_sec),
            to: as_of,
            // Whole days, rounded up; the exact span is `ipcw.window_sec`.
            days: (fit.window_sec + 86_399) / 86_400,
        },
        level: level.to_string(),
        stage,
        age_bucket: None,
        shift,
        raw_shift: Q4::from_array(fit.raw_shift),
        n_events: fit.n_events,
        n_censored: fit.n_censored,
        max_daily_step: None,
        replay_from: None,
        ipcw: Some(IpcwRecord {
            censoring: CENSORING_MODEL.to_string(),
            half_life_sec: fit.half_life_sec,
            window_sec: fit.window_sec,
            n_effective: fit.n_effective,
            g_floor: G_FLOOR,
            max_weight: fit.max_weight,
            deadband: Q4::from_array(fit.deadband),
            unresolved: fit.unresolved,
            drift: drift_record,
        }),
        base_quantiles_sec: Q4 {
            p25: base_q.0,
            p50: base_q.1,
            p75: base_q.2,
            p90: base_q.3,
        },
    };
    if let Some(result) = explanation.result.as_mut() {
        result.p25_sec = p25;
        result.p50_sec = p50;
        result.p75_sec = p75;
        result.p90_sec = Some(p90);
        result.eta_p50_at = as_of + Duration::seconds(p50);
    }
    explanation.calibration = Some(record);
    explanation.enforce_cap();
    explanation
}
