//! The 21-point nearest-rank quantile grid a stage distribution is summarised
//! as, and the two maps the simulation needs from it: the inverse CDF (a
//! draw) and the CDF (where the current age sits).
//!
//! Every grid value is an observed duration (nearest rank, the
//! `pr_latency::stats` discipline). Between grid points the maps interpolate
//! linearly, so a draw can fall between two observed values.

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
