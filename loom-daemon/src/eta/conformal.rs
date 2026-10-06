//! Censoring-aware split-conformal calibration of a base heuristic's
//! quantiles (#10489).
//!
//! # Why
//!
//! `land-v2`'s p25–p75 range holds ~10% of landings (target 50%) and ~71% of
//! landings run past its p90 (target 10%). [`super::recalibrate`] (the
//! `amber-heron` recipe) rescales only the *spread* of the base estimate
//! around its *median*, from the distribution of `ln(actual / p50)`, and it
//! did worse live. This module calibrates **each reported quantile against
//! its own hit rate**, so p25/p50/p75/p90 each aim at 25/50/75/90%, and it
//! makes the evidence — landed and still-open estimates alike — explicit.
//!
//! # The method
//!
//! For a base estimate with quantile `q_τ` (remaining seconds) the
//! conformity score of a past estimate is `s = ln(actual_remaining / q_τ)`:
//! `s ≤ 0` exactly when the base's `τ` quantile was not exceeded. Split
//! conformal takes the (finite-sample corrected, level `(n+1)τ / n`)
//! quantile `c_τ` of the scores on a calibration set, and the adjusted
//! quantile is `q_τ · exp(c_τ)`, which would have held `τ` of the calibration
//! outcomes. The calibration set is the base's own track record in a trailing
//! window ([`WINDOW_DAYS`]), per **(stage, age bucket)** where that cell has
//! [`MIN_CELL_EVENTS`] landings, else per stage, else pooled, else nothing
//! (the base estimate is returned untouched and carries no `calibration`).
//!
//! # Censoring
//!
//! A still-open estimate is not a missing score, it is a **lower bound**:
//! `s ≥ ln(elapsed / q_τ)`. These are exactly the slow items, so dropping
//! them biases every shift down (an optimistic range). Scores and bounds go
//! through one product-limit (Kaplan–Meier) estimate on the score axis —
//! a bound stays at risk up to its value and contributes no event, censoring
//! follows events on a tie, and an unresolvable upper tail clamps to the
//! largest bound seen (which is conservative: the shift is never below the
//! bound that could not be resolved).
//!
//! # Rate limit
//!
//! The shift at `t` is the raw fit at `t`, clamped to within
//! [`MAX_DAILY_STEP`] (log units) of the shift at `t − 1 day`, itself
//! clamped likewise back [`STEPS`] days. It is stateless — recomputed from
//! the same evidence — so one bad day moves the range by at most one step.
//!
//! # Point-in-time
//!
//! Everything is a function of the estimate's own `as_of` and the
//! observations: an estimate made at or after `t`, or an outcome *known* at
//! or after `t`, is not used (an outcome landed but not yet known is a
//! censored bound at `t`, exactly as in [`super::recalibrate`]). Perturbing
//! any post-`as_of` outcome leaves the output bit-identical (pinned by a
//! test).
//!
//! Pure: no clock, no file, no forge.

use super::explanation::Explanation;
use super::recalibrate::CalibrationObservation;
use super::Stage;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// Recorded in [`Calibration::method`].
pub const METHOD: &str = "split_conformal_km_log";

/// The trailing window of base estimates calibrated against, days.
pub const WINDOW_DAYS: i64 = 14;

/// Fewest landings a cell needs for its own shifts.
pub const MIN_CELL_EVENTS: usize = 20;

/// Largest change of any quantile's shift (ln units) between two
/// evaluations one day apart: `ln 1.2 ≈ 0.18` is a 20% move of the range.
pub const MAX_DAILY_STEP: f64 = 0.18;

/// Days of clamped history the rate limit is anchored over.
pub const STEPS: i64 = 7;

/// The four quantile levels, in [`Q4`] order.
const TAUS: [f64; 4] = [0.25, 0.50, 0.75, 0.90];

/// Age-bucket upper edges, seconds; the last bucket is open.
const AGE_EDGES: [i64; 3] = [3_600, 4 * 3_600, 24 * 3_600];

/// Age-bucket labels, indexed by [`age_bucket`].
const AGE_LABELS: [&str; 4] = ["lt_1h", "1h_4h", "4h_24h", "ge_24h"];

/// One value per reported quantile.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Q4<T> {
    /// 25th percentile.
    pub p25: T,
    /// Median.
    pub p50: T,
    /// 75th percentile.
    pub p75: T,
    /// 90th percentile.
    pub p90: T,
}

impl<T: Copy> Q4<T> {
    fn from_array(a: [T; 4]) -> Self {
        Q4 {
            p25: a[0],
            p50: a[1],
            p75: a[2],
            p90: a[3],
        }
    }

    fn to_array(self) -> [T; 4] {
        [self.p25, self.p50, self.p75, self.p90]
    }
}

/// The trailing window the shifts were fitted over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalibrationWindow {
    /// Oldest base estimate used (inclusive).
    pub from: DateTime<Utc>,
    /// The fit instant: nothing at or after it is used.
    pub to: DateTime<Utc>,
    /// Window length, days.
    pub days: i64,
}

/// What the conformal wrapper did, recorded in the explanation so "why this
/// range?" is answerable and the result recomputes ([`apply`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Calibration {
    /// Always [`METHOD`].
    pub method: String,
    /// The heuristic whose track record was calibrated against.
    pub base: String,
    /// The trailing window.
    pub window: CalibrationWindow,
    /// `stage_age` (its stage and age bucket), `stage`, or `pooled`.
    pub level: String,
    /// The current stage.
    pub stage: Stage,
    /// The age bucket, when the level is `stage_age`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_bucket: Option<String>,
    /// The shift applied per quantile, ln units: `q' = q · exp(shift)`
    /// (after the rate limit).
    pub shift: Q4<f64>,
    /// The shift before the rate limit.
    pub raw_shift: Q4<f64>,
    /// Landings behind the cell.
    pub n_events: usize,
    /// Still-open (right-censored) base estimates behind it.
    pub n_censored: usize,
    /// The largest one-day move allowed ([`MAX_DAILY_STEP`]).
    pub max_daily_step: f64,
    /// The base estimate's quantiles, before calibration, seconds.
    pub base_quantiles_sec: Q4<i64>,
}

fn round6(x: f64) -> f64 {
    (x * 1_000_000.0).round() / 1_000_000.0
}

/// The age bucket of an item `age_sec` into its stage.
fn age_bucket(age_sec: i64) -> usize {
    AGE_EDGES.iter().take_while(|&&e| age_sec >= e).count()
}

/// One usable observation at fit instant `t`: its four base quantiles and
/// its state, or `None` when it postdates `t`, left the window, or has no
/// full quantile set.
struct Seen {
    stage: Stage,
    bucket: Option<usize>,
    /// Base quantiles (seconds, floored at 1) and the observed-or-elapsed
    /// remaining seconds, with whether it is an event.
    q: [f64; 4],
    remaining: f64,
    event: bool,
}

fn see(o: &CalibrationObservation, base: &str, t: DateTime<Utc>) -> Option<Seen> {
    if o.heuristic != base || o.as_of >= t || o.as_of < t - Duration::days(WINDOW_DAYS) {
        return None;
    }
    let q = [o.p25_sec?, o.p50_sec, o.p75_sec?, o.p90_sec?].map(|v| v.max(1) as f64);
    let (remaining, event) = match (o.actual_at, o.resolved_at) {
        (Some(actual), Some(known)) if known.max(actual) < t => {
            ((actual - o.as_of).num_seconds(), true)
        }
        _ => ((t - o.as_of).num_seconds(), false),
    };
    Some(Seen {
        stage: o.stage,
        bucket: o.age_sec.map(|a| age_bucket(a.max(0))),
        q,
        remaining: remaining.max(1) as f64,
        event,
    })
}

/// The `level` quantile of the product-limit estimate over `points`
/// (`(score, is_event)`), clamped to the largest point when unresolvable.
fn km_quantile(points: &mut [(f64, bool)], level: f64) -> f64 {
    // Ascending; events before bounds on a tie ("censoring follows events").
    points.sort_by(|a, b| a.0.total_cmp(&b.0).then(b.1.cmp(&a.1)));
    let horizon = points.last().map_or(0.0, |p| p.0);
    let target = 1.0 - level;
    let mut at_risk = points.len() as f64;
    let mut survival = 1.0_f64;
    let mut i = 0;
    while i < points.len() {
        let x = points[i].0;
        let (mut deaths, mut here) = (0.0, 0.0);
        while i < points.len() && points[i].0 == x {
            here += 1.0;
            if points[i].1 {
                deaths += 1.0;
            }
            i += 1;
        }
        if deaths > 0.0 && at_risk > 0.0 {
            survival *= 1.0 - deaths / at_risk;
            if survival <= target + 1e-12 {
                return x;
            }
        }
        at_risk -= here;
    }
    horizon
}

/// One fitted cell.
struct Fit {
    level: &'static str,
    bucket: Option<usize>,
    shift: [f64; 4],
    n_events: usize,
    n_censored: usize,
}

/// The raw (un-rate-limited) fit at `t`: the narrowest cell with
/// [`MIN_CELL_EVENTS`] landings, or `None`.
fn raw_fit(
    observations: &[CalibrationObservation],
    base: &str,
    t: DateTime<Utc>,
    stage: Stage,
    bucket: usize,
) -> Option<Fit> {
    let seen: Vec<Seen> = observations
        .iter()
        .filter_map(|o| see(o, base, t))
        .collect();
    for (level, bucket) in [
        ("stage_age", Some(bucket)),
        ("stage", None),
        ("pooled", None),
    ] {
        let keep = |s: &Seen| match level {
            "stage_age" => s.stage == stage && s.bucket == bucket,
            "stage" => s.stage == stage,
            _ => true,
        };
        let members: Vec<&Seen> = seen.iter().filter(|s| keep(s)).collect();
        let n_events = members.iter().filter(|s| s.event).count();
        if n_events < MIN_CELL_EVENTS {
            continue;
        }
        let n = members.len() as f64;
        let mut shift = [0.0; 4];
        for (k, tau) in TAUS.iter().enumerate() {
            let mut points: Vec<(f64, bool)> = members
                .iter()
                .map(|s| ((s.remaining / s.q[k]).ln(), s.event))
                .collect();
            // Finite-sample corrected conformal level.
            let lvl = ((n + 1.0) * tau / n).min(1.0);
            shift[k] = round6(km_quantile(&mut points, lvl));
        }
        return Some(Fit {
            level,
            bucket,
            shift,
            n_events,
            n_censored: members.len() - n_events,
        });
    }
    None
}

/// The fit at `t` and its rate-limited shift. `None` when `t` itself has no
/// cell with enough landings.
fn limited_fit(
    observations: &[CalibrationObservation],
    base: &str,
    t: DateTime<Utc>,
    stage: Stage,
    bucket: usize,
) -> Option<(Fit, [f64; 4])> {
    let mut previous: Option<[f64; 4]> = None;
    let mut last: Option<Fit> = None;
    for k in (0..=STEPS).rev() {
        let at = t - Duration::days(k);
        let raw = raw_fit(observations, base, at, stage, bucket);
        if let Some(fit) = &raw {
            previous = Some(match previous {
                None => fit.shift,
                Some(p) => {
                    let mut next = fit.shift;
                    for i in 0..4 {
                        next[i] =
                            round6(next[i].clamp(p[i] - MAX_DAILY_STEP, p[i] + MAX_DAILY_STEP));
                    }
                    next
                }
            });
        }
        if k == 0 {
            last = raw;
        }
    }
    Some((last?, previous?))
}

/// `(p25, p50, p75, p90)` from the base's quantiles and the shifts.
/// Monotone by construction, floored at zero.
#[must_use]
pub fn apply(base: (i64, i64, i64, i64), shift: &Q4<f64>) -> (i64, i64, i64, i64) {
    let q = [base.0, base.1, base.2, base.3];
    let s = shift.to_array();
    let at = |i: usize| ((q[i].max(1) as f64) * s[i].exp()).round().max(0.0) as i64;
    let p25 = at(0);
    let p50 = at(1).max(p25);
    let p75 = at(2).max(p50);
    let p90 = at(3).max(p75);
    (p25, p50, p75, p90)
}

/// Calibrate `explanation` (a base heuristic's estimate) against the track
/// record of `base` in `observations`. Pure.
///
/// The identity — `explanation` returned untouched — for a refusal, an
/// estimate with no current stage or no p90, or no cell with
/// [`MIN_CELL_EVENTS`] landings.
#[must_use]
pub fn calibrate(
    mut explanation: Explanation,
    observations: &[CalibrationObservation],
    base: &str,
) -> Explanation {
    let Some(current) = explanation.current_stage.as_ref() else {
        return explanation;
    };
    let (stage, age) = (current.stage, current.age_sec.max(0));
    let Some(base_q) = explanation.quantiles_with_p90() else {
        return explanation;
    };
    let as_of = explanation.as_of;
    let bucket = age_bucket(age);
    let Some((fit, shift)) = limited_fit(observations, base, as_of, stage, bucket) else {
        return explanation;
    };
    let shift = Q4::from_array(shift);
    let (p25, p50, p75, p90) = apply(base_q, &shift);
    let record = Calibration {
        method: METHOD.to_string(),
        base: base.to_string(),
        window: CalibrationWindow {
            from: as_of - Duration::days(WINDOW_DAYS),
            to: as_of,
            days: WINDOW_DAYS,
        },
        level: fit.level.to_string(),
        stage,
        age_bucket: fit.bucket.map(|b| AGE_LABELS[b].to_string()),
        shift,
        raw_shift: Q4::from_array(fit.shift),
        n_events: fit.n_events,
        n_censored: fit.n_censored,
        max_daily_step: MAX_DAILY_STEP,
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
