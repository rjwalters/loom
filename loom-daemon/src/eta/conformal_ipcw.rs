//! IPCW split-conformal calibration over a short, recent window (#10524):
//! the calibrator behind `land-2026-10-06-quick-tern`.
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
//!   `n` the effective sample size. If the level cannot be reached (mass
//!   lost to the weight cap), the shift clamps to the largest resolved
//!   score: never below any landing seen.
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
//! # Deferred (#10524 / #10528)
//!
//! - Interval inflation when the #10528 drift flag is set. No drift signal
//!   exists yet.
//! - A drift- or regime-driven adaptive half-life.
//! - History-aware conditioning.
//!
//! Pure: no clock, no file, no forge.

use super::conformal::{apply, round6, Calibration, CalibrationWindow, Q4, TAUS};
use super::explanation::Explanation;
use super::recalibrate::CalibrationObservation;
use super::Stage;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::f64::consts::LN_2;

/// Recorded in [`Calibration::method`].
pub const METHOD: &str = "ipcw_split_conformal_log";

/// Recorded in [`IpcwRecord::censoring`]: the censoring survival is the
/// product-limit estimate over fully observed (administrative) censoring
/// times `t − as_of`.
pub const CENSORING_MODEL: &str = "km_administrative";

/// Hard cutoff of the calibration window, days.
pub const WINDOW_DAYS: i64 = 7;

/// The window at a half-life is this many half-lives (capped at
/// [`WINDOW_DAYS`]): an older row would weigh under 0.4%, and it would only
/// carry the old regime's tail into the conservative clamp.
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
    /// Quantiles whose level the weighted CDF could not reach, clamped to
    /// the largest resolved score.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unresolved: Vec<String>,
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
                // Mass lost to the weight cap: clamp to the largest resolved
                // score, never below any landing seen.
                unresolved.push(LABELS[k].to_string());
                let c = round6(points.last().map_or(0.0, |p| p.0));
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

/// The fit at the shortest half-life on the doubling ladder whose events'
/// effective N reaches [`MIN_EFFECTIVE_EVENTS`], or `None`.
fn fit_ladder(members: &[&Member]) -> Option<Fit> {
    let mut half_life = HALF_LIFE_SEC;
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
    mut explanation: Explanation,
    observations: &[CalibrationObservation],
    base: &str,
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
    let mut chosen = None;
    for level in ["stage", "pooled"] {
        let cell: Vec<&Member> = members
            .iter()
            .filter(|m| level == "pooled" || m.stage == stage)
            .collect();
        if let Some(fit) = fit_ladder(&cell) {
            chosen = Some((level, fit));
            break;
        }
    }
    let Some((level, fit)) = chosen else {
        return explanation;
    };
    let shift = Q4::from_array(fit.shift);
    let (p25, p50, p75, p90) = apply(base_q, &shift);
    let record = Calibration {
        method: METHOD.to_string(),
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
