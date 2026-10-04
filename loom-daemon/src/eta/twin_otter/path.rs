//! The stage-by-stage part's Monte Carlo. Each path draws its exit from the
//! current stage out of the survival curve, then walks the later stages
//! through the path statistics until `merged`. The draw order is set out in
//! the module docs of [`super`].

use super::{EvalConfig, EvalError, HOP_GUARD, TAU_PERCENT};
use crate::eta::fit::{FitStage, KmCurve, NextStep, PathStats};
use crate::eta::simulate::SplitMix64;
use std::collections::{BTreeMap, BTreeSet};

/// How far a next-stage table's total probability may be from 1.
const PROBABILITY_SUM_TOLERANCE: f64 = 1e-6;

/// The path quantiles at [`super::TAUS`], over `config.paths` path totals
/// from `start`. Each path has its own generator, seeded by one draw of the
/// master stream.
pub(super) fn path_quantiles(
    survival: &[f64],
    start: FitStage,
    stats: &PathStats,
    config: &EvalConfig,
) -> [f64; 4] {
    let mut master = SplitMix64::new(config.seed);
    let mut totals: Vec<f64> = (0..config.paths)
        .map(|_| {
            let mut rng = SplitMix64::new(master.next_u64());
            path_total(survival, start, stats, config, &mut rng)
        })
        .collect();
    totals.sort_by(f64::total_cmp);
    TAU_PERCENT.map(|pct| nearest_rank(&totals, pct))
}

/// One path's total hours, capped at `config.cap_h`.
fn path_total(
    survival: &[f64],
    start: FitStage,
    stats: &PathStats,
    config: &EvalConfig,
    rng: &mut SplitMix64,
) -> f64 {
    let cap = config.cap_h;
    let Some(mut total) = first_exit(survival, rng.next_f64(), config.step_h) else {
        return cap;
    };
    let mut stage = start;
    let mut hops = 0;
    while total < cap {
        if hops == HOP_GUARD {
            return cap;
        }
        // `check_paths` has vetted every table and curve a path can reach;
        // the `else` arms only keep this function total.
        let Some(table) = stats.next.get(&stage) else {
            return cap;
        };
        let Some(next) = next_stage(table, rng.next_f64()) else {
            break; // merged
        };
        let Some(curve) = stats.km.get(&next) else {
            return cap;
        };
        total += km_inverse(curve, rng.next_f64());
        stage = next;
        hops += 1;
    }
    total.min(cap)
}

/// Hours until the item leaves the current stage, for the uniform `u1`, or
/// `None` when it survives every step of the horizon.
///
/// `survival[i]` is `S_{i+1}`, and `S_0 = 1`. With
/// `k = #{k : S_k > u1}`, the exit falls in step `k`, at the fraction
/// `(S_k − u1) / (S_k − S_{k+1})` of it. Given `k`, that fraction is uniform
/// on `(0, 1]`, and it is continuous in both `u1` and the curve.
#[must_use]
pub fn first_exit(survival: &[f64], u1: f64, step_h: f64) -> Option<f64> {
    let k = survival.partition_point(|&s| s > u1);
    let below = *survival.get(k)?;
    let above = if k == 0 { 1.0 } else { survival[k - 1] };
    // above > u1 >= below, so the span is positive.
    let fraction = (above - u1) / (above - below);
    Some((k as f64 + fraction) * step_h)
}

/// The Kaplan–Meier dwell for the uniform `u`: the first `t_i` with
/// `s_i ≤ u`. This is a step-function inverse, never an interpolation. A
/// curve that stops above `u` gives its last time, the largest time the data
/// has evidence about (the `grid::km_quantile` convention).
#[must_use]
pub fn km_inverse(curve: &KmCurve, u: f64) -> f64 {
    curve
        .t
        .iter()
        .zip(&curve.s)
        .find(|(_, s)| **s <= u)
        .map_or_else(|| curve.t.last().copied().unwrap_or(0.0), |(t, _)| *t)
}

/// The next stage for the uniform `u`, accumulating `table` in key order
/// (the last entry absorbs rounding). `None` means `merged`.
fn next_stage(table: &BTreeMap<NextStep, f64>, u: f64) -> Option<FitStage> {
    let mut acc = 0.0;
    let mut pick = None;
    for (step, p) in table {
        acc += p;
        pick = Some(*step);
        if u < acc {
            break;
        }
    }
    pick.and_then(stage_of)
}

/// The stage a next step leads to; `None` for `merged`.
fn stage_of(step: NextStep) -> Option<FitStage> {
    match step {
        NextStep::ReviewWait => Some(FitStage::ReviewWait),
        NextStep::DoctorWait => Some(FitStage::DoctorWait),
        NextStep::MergeWait => Some(FitStage::MergeWait),
        NextStep::MergeHold => Some(FitStage::MergeHold),
        NextStep::Merged => None,
    }
}

/// Nearest rank over ascending, non-empty `sorted`: `rank = ⌈pct·n/100⌉`,
/// clamped to `[1, n]` (the `simulate` rule).
#[must_use]
pub fn nearest_rank(sorted: &[f64], pct: usize) -> f64 {
    let n = sorted.len();
    let rank = (pct * n).div_ceil(100).clamp(1, n);
    sorted[rank - 1]
}

/// Every stage a path from `start` can reach has a next-stage table that
/// sums to 1, and every target other than `merged` has a dwell curve.
pub(super) fn check_paths(stats: &PathStats, start: FitStage) -> Result<(), EvalError> {
    let mut seen = BTreeSet::from([start]);
    let mut queue = vec![start];
    while let Some(stage) = queue.pop() {
        let table = stats
            .next
            .get(&stage)
            .ok_or_else(|| EvalError::InvalidModel(format!("no next-stage table for `{stage}`")))?;
        check_table(stage, table)?;
        for target in table.keys().filter_map(|step| stage_of(*step)) {
            let curve = stats.km.get(&target).ok_or_else(|| {
                EvalError::InvalidModel(format!(
                    "next target `{target}` of `{stage}` has no dwell curve"
                ))
            })?;
            check_curve(target, curve)?;
            if seen.insert(target) {
                queue.push(target);
            }
        }
    }
    Ok(())
}

fn check_table(stage: FitStage, table: &BTreeMap<NextStep, f64>) -> Result<(), EvalError> {
    if table.values().any(|p| !(p.is_finite() && *p >= 0.0)) {
        return Err(EvalError::InvalidModel(format!(
            "next-stage probabilities of `{stage}` are not all finite and non-negative"
        )));
    }
    let sum: f64 = table.values().sum();
    if (sum - 1.0).abs() > PROBABILITY_SUM_TOLERANCE {
        return Err(EvalError::InvalidModel(format!(
            "next-stage probabilities of `{stage}` sum to {sum}"
        )));
    }
    Ok(())
}

fn check_curve(stage: FitStage, curve: &KmCurve) -> Result<(), EvalError> {
    if curve.t.is_empty() || curve.t.len() != curve.s.len() {
        return Err(EvalError::InvalidModel(format!(
            "the dwell curve of `{stage}` has {} times and {} survivals",
            curve.t.len(),
            curve.s.len()
        )));
    }
    let bad_t = curve.t.iter().any(|t| !(t.is_finite() && *t >= 0.0));
    if bad_t || curve.s.iter().any(|s| !s.is_finite()) {
        return Err(EvalError::InvalidModel(format!(
            "the dwell curve of `{stage}` has a negative or non-finite value"
        )));
    }
    Ok(())
}
