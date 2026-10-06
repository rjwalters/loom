//! Latent-regime residual adjustment and drift check (#10528, slice 1).
//!
//! The fitted models learn from a 14-60 day window; when the system changes
//! (a planner deploy, a slower reviewer, a CI change — cause unknown) they
//! keep predicting the old behaviour until enough new outcomes arrive. This
//! module adapts to the **residuals**, not the cause, with no refit:
//!
//! - [`adjust`] keeps a per-stage exponentially weighted mean of the
//!   log-residual `ln(actual / predicted)` of recently *scored* outcomes and
//!   returns a multiplicative `factor` for the served stage duration. It is
//!   exactly `1.0` below the row floor ([`super::MIN_SAMPLES`]) and when the
//!   recent mean is not distinguishable from noise, and is clamped to
//!   [`MIN_FACTOR`]..[`MAX_FACTOR`] so it cannot run away.
//! - [`drift`] runs a two-sided CUSUM over the last [`DRIFT_WINDOW_SEC`] of
//!   scored outcomes, standardised against the older baseline residuals, and
//!   yields a `drifted` flag. [`Drift::inflation`] exposes a plain widening
//!   factor for a conformal layer (#10524) to consume later.
//!
//! Both are pure and deterministic: no clock, no RNG. Leak-free: an outcome
//! is used only when it was known strictly before `as_of`. Residuals are
//! always taken against the *unadjusted* prediction, so the factor is a
//! function of the recent window and not an accumulating state.

use super::recalibrate::CalibrationObservation;
use super::{Stage, MIN_SAMPLES};
use chrono::{DateTime, Duration, Utc};

/// Residual window the adjustment looks back over: 24 hours.
pub const ADJUST_WINDOW_SEC: i64 = 24 * 3_600;
/// Half-life of a residual's weight in the adjustment: 3 hours.
pub const ADJUST_HALF_LIFE_SEC: i64 = 3 * 3_600;
/// The adjustment is applied only when the weighted mean log-residual is at
/// least this many standard errors from zero.
pub const SIGNIFICANCE: f64 = 3.0;
/// Lowest / highest factor the adjustment may serve.
pub const MIN_FACTOR: f64 = 0.25;
/// See [`MIN_FACTOR`].
pub const MAX_FACTOR: f64 = 4.0;

/// The drift check's recent window: 6 hours.
pub const DRIFT_WINDOW_SEC: i64 = 6 * 3_600;
/// How far back the baseline residuals reach: 7 days.
pub const BASELINE_WINDOW_SEC: i64 = 7 * 86_400;
/// CUSUM slack, in baseline standard deviations.
pub const CUSUM_K: f64 = 0.5;
/// CUSUM decision threshold, in baseline standard deviations.
pub const CUSUM_H: f64 = 8.0;
/// Baseline spread used when there is too little baseline to estimate one.
pub const DEFAULT_SIGMA: f64 = 0.5;
/// Lowest baseline spread: a degenerate baseline must not make every
/// residual look huge.
pub const MIN_SIGMA: f64 = 0.1;
/// Served-interval inflation per unit of drift magnitude beyond the
/// threshold, capped at [`MAX_INFLATION`].
pub const MAX_INFLATION: f64 = 2.0;

/// One scored outcome: when it became known and `ln(actual / predicted)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Residual {
    /// The stage the estimate was made in.
    pub stage: Stage,
    /// When the outcome became known (the leak-free instant).
    pub known_at: DateTime<Utc>,
    /// `ln(actual_remaining / predicted_p50)`.
    pub log_ratio: f64,
}

/// The residuals of the resolved calibration observations. An observation
/// that is open, or has a non-positive prediction or duration, is skipped.
#[must_use]
pub fn residuals(observations: &[CalibrationObservation]) -> Vec<Residual> {
    let mut out: Vec<Residual> = observations
        .iter()
        .filter_map(|o| {
            let (actual_at, known_at) = (o.actual_at?, o.resolved_at?);
            let actual = (actual_at - o.as_of).num_seconds();
            if actual <= 0 || o.p50_sec <= 0 {
                return None;
            }
            Some(Residual {
                stage: o.stage,
                known_at,
                log_ratio: (actual as f64 / o.p50_sec as f64).ln(),
            })
        })
        .collect();
    out.sort_by(|a, b| a.known_at.cmp(&b.known_at));
    out
}

fn round6(x: f64) -> f64 {
    (x * 1_000_000.0).round() / 1_000_000.0
}

/// The served adjustment for one stage; recorded in the explanation.
#[derive(Debug, Clone, PartialEq)]
pub struct Adjustment {
    /// The stage.
    pub stage: Stage,
    /// Multiplier on the stage's served duration. `1.0` is identity.
    pub factor: f64,
    /// Residuals inside the window.
    pub n_recent: usize,
    /// The weight half-life used, seconds.
    pub half_life_sec: i64,
}

impl Adjustment {
    /// Whether the adjustment changes nothing.
    #[must_use]
    pub fn is_identity(&self) -> bool {
        self.factor == 1.0
    }
}

/// The adjustment for `stage` at `as_of` from `all` residuals.
#[must_use]
pub fn adjust(all: &[Residual], stage: Stage, as_of: DateTime<Utc>) -> Adjustment {
    let from = as_of - Duration::seconds(ADJUST_WINDOW_SEC);
    let recent: Vec<&Residual> = all
        .iter()
        .filter(|r| r.stage == stage && r.known_at >= from && r.known_at < as_of)
        .collect();
    let identity = Adjustment {
        stage,
        factor: 1.0,
        n_recent: recent.len(),
        half_life_sec: ADJUST_HALF_LIFE_SEC,
    };
    if recent.len() < MIN_SAMPLES {
        return identity;
    }
    let weights: Vec<f64> = recent
        .iter()
        .map(|r| {
            let age = (as_of - r.known_at).num_seconds() as f64;
            (-std::f64::consts::LN_2 * age / ADJUST_HALF_LIFE_SEC as f64).exp()
        })
        .collect();
    let sum: f64 = weights.iter().sum();
    let ess = super::recency::effective_n(weights.iter().copied());
    if sum <= 0.0 || ess < 2.0 {
        return identity;
    }
    let mean = recent
        .iter()
        .zip(&weights)
        .map(|(r, w)| r.log_ratio * w)
        .sum::<f64>()
        / sum;
    let var = recent
        .iter()
        .zip(&weights)
        .map(|(r, w)| w * (r.log_ratio - mean).powi(2))
        .sum::<f64>()
        / sum;
    let se = var.sqrt().max(MIN_SIGMA * 0.5) / ess.sqrt();
    if mean.abs() < SIGNIFICANCE * se {
        return identity;
    }
    Adjustment {
        factor: round6(mean.exp().clamp(MIN_FACTOR, MAX_FACTOR)),
        ..identity
    }
}

/// Whether the system looks stable, drifted, or unknown (too few outcomes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftState {
    /// Fewer than [`MIN_SAMPLES`] recent outcomes: nothing is known.
    Unknown,
    /// Recent outcomes agree with the baseline.
    Stable,
    /// The CUSUM tripped.
    Drifted,
}

/// The drift check's verdict for one stage.
#[derive(Debug, Clone, PartialEq)]
pub struct Drift {
    /// The stage.
    pub stage: Stage,
    /// Outcomes in the recent window.
    pub n_recent: usize,
    /// Outcomes in the baseline.
    pub n_baseline: usize,
    /// The largest CUSUM statistic reached, in baseline standard deviations.
    pub statistic: f64,
    /// Whether the statistic reached [`CUSUM_H`].
    pub drifted: bool,
}

impl Drift {
    /// The tri-state view.
    #[must_use]
    pub fn state(&self) -> DriftState {
        if self.n_recent < MIN_SAMPLES {
            DriftState::Unknown
        } else if self.drifted {
            DriftState::Drifted
        } else {
            DriftState::Stable
        }
    }

    /// A plain widening factor for a conformal layer: `1.0` when not
    /// drifted, rising with the statistic to [`MAX_INFLATION`].
    #[must_use]
    pub fn inflation(&self) -> f64 {
        if !self.drifted {
            return 1.0;
        }
        round6((1.0 + (self.statistic - CUSUM_H) / CUSUM_H).clamp(1.0, MAX_INFLATION))
    }
}

/// The drift verdict for `stage` at `as_of`.
#[must_use]
pub fn drift(all: &[Residual], stage: Stage, as_of: DateTime<Utc>) -> Drift {
    let recent_from = as_of - Duration::seconds(DRIFT_WINDOW_SEC);
    let base_from = as_of - Duration::seconds(BASELINE_WINDOW_SEC);
    let mut recent = Vec::new();
    let mut base = Vec::new();
    for r in all
        .iter()
        .filter(|r| r.stage == stage && r.known_at < as_of)
    {
        if r.known_at >= recent_from {
            recent.push(r);
        } else if r.known_at >= base_from {
            base.push(r.log_ratio);
        }
    }
    recent.sort_by(|a, b| a.known_at.cmp(&b.known_at));
    let (mu, sigma) = if base.len() >= MIN_SAMPLES {
        let n = base.len() as f64;
        let mu = base.iter().sum::<f64>() / n;
        let var = base.iter().map(|x| (x - mu).powi(2)).sum::<f64>() / n;
        (mu, var.sqrt().max(MIN_SIGMA))
    } else {
        (0.0, DEFAULT_SIGMA)
    };
    let (mut hi, mut lo, mut peak) = (0.0_f64, 0.0_f64, 0.0_f64);
    for r in &recent {
        let z = (r.log_ratio - mu) / sigma;
        hi = (hi + z - CUSUM_K).max(0.0);
        lo = (lo - z - CUSUM_K).max(0.0);
        peak = peak.max(hi).max(lo);
    }
    Drift {
        stage,
        n_recent: recent.len(),
        n_baseline: base.len(),
        statistic: round6(peak),
        drifted: recent.len() >= MIN_SAMPLES && peak >= CUSUM_H,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 6, 12, 0, 0).unwrap()
    }

    fn res(stage: Stage, mins_before: i64, log_ratio: f64) -> Residual {
        Residual {
            stage,
            known_at: t0() - Duration::minutes(mins_before),
            log_ratio,
        }
    }

    #[test]
    fn identity_on_empty_and_small_n() {
        let few: Vec<Residual> = (0..MIN_SAMPLES - 1)
            .map(|i| res(Stage::ReviewWait, 10 + i as i64, 1.0))
            .collect();
        for rs in [&[][..], &few[..]] {
            let a = adjust(rs, Stage::ReviewWait, t0());
            assert!(a.is_identity());
        }
        assert_eq!(adjust(&few, Stage::ReviewWait, t0()).n_recent, MIN_SAMPLES - 1);
    }

    #[test]
    fn constant_shift_is_followed_and_clamped() {
        let rs: Vec<Residual> = (0..20)
            .map(|i| res(Stage::Doctor, 5 + i * 3, 0.5))
            .collect();
        let a = adjust(&rs, Stage::Doctor, t0());
        assert!((a.factor - 0.5_f64.exp()).abs() < 1e-3, "{}", a.factor);
        let wild: Vec<Residual> = (0..20)
            .map(|i| res(Stage::Doctor, 5 + i * 3, 9.0))
            .collect();
        assert_eq!(adjust(&wild, Stage::Doctor, t0()).factor, MAX_FACTOR);
        let tiny: Vec<Residual> = (0..20)
            .map(|i| res(Stage::Doctor, 5 + i * 3, -9.0))
            .collect();
        assert_eq!(adjust(&tiny, Stage::Doctor, t0()).factor, MIN_FACTOR);
    }

    #[test]
    fn other_stages_and_the_future_are_ignored() {
        let mut rs: Vec<Residual> = (0..20)
            .map(|i| res(Stage::Doctor, 5 + i * 3, 0.5))
            .collect();
        rs.extend((0..20).map(|i| res(Stage::ReviewWait, 5 + i * 3, 0.5)));
        assert!(adjust(&rs, Stage::MergeWait, t0()).is_identity());
        // Everything at or after as_of is invisible.
        let future: Vec<Residual> = (0..20).map(|i| res(Stage::Doctor, -i, 2.0)).collect();
        let a = adjust(&future, Stage::Doctor, t0());
        assert!(a.is_identity() && a.n_recent == 0);
        assert_eq!(drift(&future, Stage::Doctor, t0()).n_recent, 0);
    }

    #[test]
    fn deterministic() {
        let rs: Vec<Residual> = (0..30)
            .map(|i| res(Stage::Doctor, 5 + i * 7, 0.3))
            .collect();
        assert_eq!(adjust(&rs, Stage::Doctor, t0()), adjust(&rs, Stage::Doctor, t0()));
        assert_eq!(drift(&rs, Stage::Doctor, t0()), drift(&rs, Stage::Doctor, t0()));
    }

    #[test]
    fn drift_trips_on_a_step_and_not_on_flat() {
        let mut flat: Vec<Residual> = (0..40)
            .map(|i| res(Stage::ReviewWait, 400 + i * 20, if i % 2 == 0 { 0.2 } else { -0.2 }))
            .collect();
        flat.extend(
            (0..12)
                .map(|i| res(Stage::ReviewWait, 10 + i * 20, if i % 2 == 0 { 0.2 } else { -0.2 })),
        );
        let d = drift(&flat, Stage::ReviewWait, t0());
        assert!(!d.drifted, "{d:?}");
        assert_eq!(d.state(), DriftState::Stable);
        assert!((d.inflation() - 1.0).abs() < f64::EPSILON);

        let mut shifted = flat.clone();
        shifted.retain(|r| r.known_at < t0() - Duration::hours(6));
        shifted.extend((0..12).map(|i| res(Stage::ReviewWait, 10 + i * 20, 0.9)));
        let d = drift(&shifted, Stage::ReviewWait, t0());
        assert!(d.drifted, "{d:?}");
        assert!(d.inflation() >= 1.0 && d.inflation() <= MAX_INFLATION);
    }

    #[test]
    fn drift_unknown_below_floor() {
        let rs: Vec<Residual> = (0..3).map(|i| res(Stage::Doctor, 5 + i, 3.0)).collect();
        assert_eq!(drift(&rs, Stage::Doctor, t0()).state(), DriftState::Unknown);
    }
}
