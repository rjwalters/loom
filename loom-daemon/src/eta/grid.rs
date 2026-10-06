//! The 21-point nearest-rank quantile grid a stage distribution is summarised
//! as, and the two maps the simulation needs from it: the inverse CDF (a
//! draw) and the CDF (where the current age sits).
//!
//! Every grid value is an observed duration (nearest rank, the
//! `pr_latency::stats` discipline). Between grid points the maps interpolate
//! linearly, so a draw can fall between two observed values.
//!
//! # Censoring (#9328)
//!
//! The nearest-rank grid above reads only *completed* stage durations, and so
//! silently drops every sample that was still open when it was observed — the
//! "ignore right-censoring" simplification `land-v1` ships with. Dropping them
//! is not neutral: a stage's still-open samples are exactly its **slow** ones,
//! so the surviving set is biased short.
//!
//! [`km_grid_of`] is the censoring-aware alternative `land-v2` uses: the
//! Kaplan–Meier product-limit estimator of the survival function `S(t)`, read
//! back at the same 21 percentiles. A censored sample never contributes an
//! event, but it does stay *at risk* up to its lower bound, which is the whole
//! point — it says "this stage lasted at least this long" without pretending to
//! know how much longer.
//!
//! With no censored samples the product-limit estimator is the empirical CDF,
//! so [`km_grid_of`] returns exactly [`grid_of`] and `land-v2` reduces to
//! `land-v1`'s grid; that identity is pinned by a test.

use crate::pr_latency::stats::nearest_rank;

/// Number of grid points: `p0, p5, …, p100`.
pub const GRID_POINTS: usize = 21;

/// The grid's percentiles.
#[must_use]
pub fn grid_pct() -> Vec<u8> {
    (0..GRID_POINTS).map(|i| (i * 5) as u8).collect()
}

/// The grid over `sorted` (ascending, non-empty).
#[must_use]
pub fn grid_of(sorted: &[i64]) -> Vec<i64> {
    (0..GRID_POINTS)
        .map(|i| nearest_rank(sorted, i * 5))
        .collect()
}

/// The nearest-rank `pct`th percentile of `sorted` (ascending, non-empty).
#[must_use]
pub fn quantile(sorted: &[i64], pct: usize) -> i64 {
    nearest_rank(sorted, pct)
}

/// One step of the Kaplan–Meier product-limit curve: an event time and the
/// survival probability from that time until the next event.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KmStep {
    /// The event (completed-duration) time, seconds.
    pub t: i64,
    /// `S(t)` after the event(s) at `t`.
    pub survival: f64,
}

/// The Kaplan–Meier product-limit curve over `observed` (exact durations,
/// ascending) and `censored` (lower bounds, ascending).
///
/// `S(t) = Π_{t_i ≤ t} (1 − d_i / n_i)`, where `d_i` is how many observed
/// durations equal `t_i` and `n_i` is how many samples — observed **or**
/// censored — are still at risk at `t_i` (duration ≥ `t_i`). A sample censored
/// at exactly `t_i` counts as at risk there, the usual "censoring follows
/// events" tie rule.
#[must_use]
pub fn km_curve(observed: &[i64], censored: &[i64]) -> Vec<KmStep> {
    let mut curve = Vec::new();
    let mut survival = 1.0_f64;
    let mut i = 0;
    while i < observed.len() {
        let t = observed[i];
        let mut deaths = 0_usize;
        while i < observed.len() && observed[i] == t {
            deaths += 1;
            i += 1;
        }
        // Ascending inputs, so "at risk" is a suffix count on each side.
        let at_risk = observed.len() - observed.partition_point(|&d| d < t)
            + (censored.len() - censored.partition_point(|&c| c < t));
        if at_risk > 0 {
            survival *= 1.0 - deaths as f64 / at_risk as f64;
        }
        curve.push(KmStep { t, survival });
    }
    curve
}

/// The Kaplan–Meier `p`-quantile (`p ∈ [0, 1]`): the smallest event time whose
/// survival has fallen to `1 − p` or below.
///
/// When censoring keeps `S` above `1 − p` for every event — the tail the data
/// genuinely cannot resolve — the answer is `horizon`, the largest time any
/// sample gives evidence about. That is a deliberate, documented clamp: a
/// classical Kaplan–Meier quantile is *undefined* there, and a grid has to
/// return something monotone.
#[must_use]
pub fn km_quantile(curve: &[KmStep], p: f64, horizon: i64) -> i64 {
    let target = 1.0 - p.clamp(0.0, 1.0);
    curve
        .iter()
        .find(|step| step.survival <= target + 1e-12)
        .map_or(horizon, |step| step.t)
}

/// The 21-point grid over `observed` (exact, ascending, non-empty) with
/// `censored` (lower bounds, ascending) handled by Kaplan–Meier.
///
/// With no censored samples this is exactly [`grid_of`] — the product-limit
/// estimator degenerates to the empirical CDF, and returning the nearest-rank
/// grid verbatim keeps that identity exact rather than merely close.
#[must_use]
pub fn km_grid_of(observed: &[i64], censored: &[i64]) -> Vec<i64> {
    if censored.is_empty() || observed.is_empty() {
        return grid_of(observed);
    }
    let curve = km_curve(observed, censored);
    let horizon = observed
        .last()
        .copied()
        .unwrap_or(0)
        .max(censored.last().copied().unwrap_or(0));
    (0..GRID_POINTS)
        .map(|i| km_quantile(&curve, i as f64 * 5.0 / 100.0, horizon))
        .collect()
}

/// The 21-point grid over **weighted** samples (#10209): `observed` exact
/// durations and `censored` lower bounds, each `(duration, weight)` and
/// ascending by duration, every weight `> 0`.
///
/// The weighted twin of [`km_grid_of`]. With no censored samples it is the
/// weighted nearest rank — the smallest observed value whose cumulative
/// weight reaches `p` of the total ([`weighted_rank`]); otherwise it is the
/// weighted product-limit estimate ([`weighted_km_curve`]) read at the 21
/// percentiles, with the same horizon clamp. With every weight equal to `1`
/// both reduce exactly to the unweighted grid: the cumulative weights are
/// then exact integer counts, so every comparison and every hazard is the
/// same floating-point operation the unweighted code performs.
#[must_use]
pub fn weighted_grid_of(observed: &[(i64, f64)], censored: &[(i64, f64)]) -> Vec<i64> {
    if censored.is_empty() || observed.is_empty() {
        return (0..GRID_POINTS)
            .map(|i| weighted_rank(observed, i * 5))
            .collect();
    }
    let curve = weighted_km_curve(observed, censored);
    let horizon = observed
        .last()
        .map_or(0, |o| o.0)
        .max(censored.last().map_or(0, |c| c.0));
    (0..GRID_POINTS)
        .map(|i| km_quantile(&curve, i as f64 * 5.0 / 100.0, horizon))
        .collect()
}

/// The weighted nearest-rank `pct`th percentile of `sorted` (ascending by
/// duration, non-empty): the first value whose cumulative weight is at least
/// `pct / 100` of the total, and never before the first value. With unit
/// weights this is [`quantile`]: `ceil(pct·n / 100)` clamped into `1..=n`.
#[must_use]
pub fn weighted_rank(sorted: &[(i64, f64)], pct: usize) -> i64 {
    let total: f64 = sorted.iter().map(|s| s.1).sum();
    let target = pct as f64 * total / 100.0;
    let mut cumulative = 0.0_f64;
    for &(value, weight) in sorted {
        cumulative += weight;
        if cumulative >= target {
            return value;
        }
    }
    // Rounding can leave the running sum a hair under `total` at p100.
    sorted.last().map_or(0, |s| s.0)
}

/// The Kaplan–Meier curve over weighted samples: [`km_curve`] with each
/// sample counting its weight instead of `1`, so
/// `S(t) = Π (1 − d_w(t_i) / n_w(t_i))` with `d_w` the weight of the events
/// at `t_i` and `n_w` the weight of every sample (event or censored) whose
/// duration is `≥ t_i` — the same "censoring follows events" tie rule.
#[must_use]
pub fn weighted_km_curve(observed: &[(i64, f64)], censored: &[(i64, f64)]) -> Vec<KmStep> {
    // Suffix weight sums, so "at risk at t" is one lookup per side.
    let suffix = |xs: &[(i64, f64)]| {
        let mut acc = vec![0.0_f64; xs.len() + 1];
        for i in (0..xs.len()).rev() {
            acc[i] = acc[i + 1] + xs[i].1;
        }
        acc
    };
    let observed_tail = suffix(observed);
    let censored_tail = suffix(censored);
    let mut curve = Vec::new();
    let mut survival = 1.0_f64;
    let mut i = 0;
    while i < observed.len() {
        let t = observed[i].0;
        let mut deaths = 0.0_f64;
        while i < observed.len() && observed[i].0 == t {
            deaths += observed[i].1;
            i += 1;
        }
        let at_risk = observed_tail[observed.partition_point(|o| o.0 < t)]
            + censored_tail[censored.partition_point(|c| c.0 < t)];
        if at_risk > 0.0 {
            survival *= 1.0 - deaths / at_risk;
        }
        curve.push(KmStep { t, survival });
    }
    curve
}

/// The value at cumulative probability `u ∈ [0, 1]`, interpolating linearly
/// between grid points.
#[must_use]
pub fn inv_cdf(grid: &[i64], u: f64) -> f64 {
    let last = grid.len() - 1;
    let x = u.clamp(0.0, 1.0) * last as f64;
    let i = (x.floor() as usize).min(last - 1);
    let frac = x - i as f64;
    let lo = grid[i] as f64;
    let hi = grid[i + 1] as f64;
    lo + frac * (hi - lo)
}

/// The cumulative probability at `age` — the inverse of [`inv_cdf`], taking
/// the largest `u` on a flat run so a conditioned draw never lands below
/// `age`. `0` at or below the minimum, `1` at or above the maximum.
#[must_use]
pub fn cdf(grid: &[i64], age: i64) -> f64 {
    let last = grid.len() - 1;
    if age < grid[0] {
        return 0.0;
    }
    if age >= grid[last] {
        return 1.0;
    }
    // The last point at or below `age`; it is < last because age < grid[last].
    let i = grid.iter().rposition(|&g| g <= age).unwrap_or(0);
    let lo = grid[i] as f64;
    let hi = grid[i + 1] as f64;
    let frac = (age as f64 - lo) / (hi - lo);
    (i as f64 + frac) / last as f64
}
