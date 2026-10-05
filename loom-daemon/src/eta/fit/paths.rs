//! Path statistics for the stages after the current one: per-stage dwell-time
//! Kaplan–Meier curves with delayed entry, downsampled to at most
//! [`KM_MAX_POINTS`], and next-step probabilities (#10218 assigns both here).
//!
//! - **At risk** (counting-process convention): an episode is at risk at `t`
//!   iff `entry_h < t ≤ dwell_h`. Episodes that are malformed (non-finite,
//!   negative entry, or `dwell_h ≤ entry_h`, so never at risk) are ignored.
//! - **Events:** `Next(_)` and `Merged` ends; `Closed` and `Censored` ends
//!   are censored. (Curator's choice on #10221, matching #10222's
//!   "conditioning on eventually merging".)
//! - **Curve:** `t = [0, distinct event times ascending]`, `s = [1, S after
//!   each]`, in hours.
//! - **`next`:** proportions over `Next(_)` and `Merged` ends only — closes
//!   are excluded, as in the #10223 fixture's tables.

use std::collections::BTreeMap;

use super::coeffs::{KmCurve, NextStep, PathStats};
use super::{DwellEnd, DwellRow, FitStage, KM_MAX_POINTS};

/// Whether an end is a Kaplan–Meier event.
fn is_event(end: DwellEnd) -> bool {
    matches!(end, DwellEnd::Next(_) | DwellEnd::Merged)
}

/// Whether an episode can ever be at risk.
fn is_wellformed(d: &DwellRow) -> bool {
    d.entry_h.is_finite() && d.dwell_h.is_finite() && d.entry_h >= 0.0 && d.dwell_h > d.entry_h
}

/// The full delayed-entry Kaplan–Meier curve of `dwells` (one stage's
/// episodes), as `(t, s)`; malformed episodes are ignored.
#[must_use]
pub fn kaplan_meier(dwells: &[DwellRow]) -> (Vec<f64>, Vec<f64>) {
    let ok: Vec<&DwellRow> = dwells.iter().filter(|d| is_wellformed(d)).collect();
    let mut entries: Vec<f64> = ok.iter().map(|d| d.entry_h).collect();
    let mut exits: Vec<f64> = ok.iter().map(|d| d.dwell_h).collect();
    let mut events: Vec<f64> = ok
        .iter()
        .filter(|d| is_event(d.end))
        .map(|d| d.dwell_h)
        .collect();
    entries.sort_by(f64::total_cmp);
    exits.sort_by(f64::total_cmp);
    events.sort_by(f64::total_cmp);

    let mut t = vec![0.0];
    let mut s = vec![1.0];
    let mut surv = 1.0;
    let mut i = 0;
    while i < events.len() {
        let tau = events[i];
        let ties = events[i..].iter().take_while(|e| **e == tau).count();
        i += ties;
        // At risk at tau: entered before it, and not yet gone before it.
        let at_risk = entries.partition_point(|e| *e < tau) - exits.partition_point(|x| *x < tau);
        if at_risk == 0 {
            continue;
        }
        surv *= 1.0 - ties as f64 / at_risk as f64;
        t.push(tau);
        s.push(surv);
    }
    (t, s)
}

/// Downsample a Kaplan–Meier curve (points `0..=n`, `(t₀, s₀) = (0, 1)`, `s`
/// non-increasing) to at most `max_points` points (at least 2), keeping
/// exact values:
///
/// 1. if `n + 1 ≤ max_points`, keep every point;
/// 2. otherwise, with `M = max_points` and `step = (1 − sₙ)/(M − 1)`, keep
///    index 0; for each `j = 1 ..= M − 2` the **smallest** `i` with
///    `s_i ≤ 1 − j·step`; and index `n`. Duplicates are dropped.
///
/// The stored right-continuous step function overstates the full curve by
/// less than `(1 − sₙ)/(M − 1)` everywhere.
#[must_use]
pub fn downsample_km(t: &[f64], s: &[f64], max_points: usize) -> (Vec<f64>, Vec<f64>) {
    let len = t.len().min(s.len());
    let m = max_points.max(2);
    if len <= m {
        return (t[..len].to_vec(), s[..len].to_vec());
    }
    let n = len - 1;
    let step = (1.0 - s[n]) / (m - 1) as f64;
    let mut keep = vec![0];
    for j in 1..=(m - 2) {
        let q = 1.0 - j as f64 * step;
        let i = s[..len].partition_point(|v| *v > q).min(n);
        if keep.last() != Some(&i) {
            keep.push(i);
        }
    }
    if keep.last() != Some(&n) {
        keep.push(n);
    }
    (keep.iter().map(|&i| t[i]).collect(), keep.iter().map(|&i| s[i]).collect())
}

/// The stored curve for one stage's episodes: [`kaplan_meier`] downsampled to
/// `max_points`, with full-data `episodes` and `events` (well-formed
/// episodes only).
#[must_use]
pub fn km_curve(dwells: &[DwellRow], max_points: usize) -> KmCurve {
    let (t, s) = kaplan_meier(dwells);
    let (t, s) = downsample_km(&t, &s, max_points);
    let ok = dwells.iter().filter(|d| is_wellformed(d));
    KmCurve {
        t,
        s,
        episodes: ok.clone().count(),
        events: ok.filter(|d| is_event(d.end)).count(),
    }
}

/// Next-step probabilities over one stage's `Next(_)` and `Merged` ends;
/// empty when it has none.
#[must_use]
pub fn next_steps(dwells: &[DwellRow]) -> BTreeMap<NextStep, f64> {
    let mut counts: BTreeMap<NextStep, usize> = BTreeMap::new();
    for d in dwells {
        let step = match d.end {
            DwellEnd::Next(stage) => NextStep::from(stage),
            DwellEnd::Merged => NextStep::Merged,
            DwellEnd::Closed | DwellEnd::Censored => continue,
        };
        *counts.entry(step).or_default() += 1;
    }
    let total: usize = counts.values().sum();
    counts
        .into_iter()
        .map(|(step, c)| (step, c as f64 / total as f64))
        .collect()
}

/// The path statistics of `dwells`, per [`FitStage`]: a stage with no
/// well-formed episode has no curve, and one with no `Next`/`Merged` end has
/// no `next` table.
#[must_use]
pub fn path_stats(dwells: &[DwellRow]) -> PathStats {
    let mut stats = PathStats::default();
    for stage in FitStage::ALL {
        let of_stage: Vec<DwellRow> = dwells
            .iter()
            .filter(|d| d.stage == stage)
            .copied()
            .collect();
        if of_stage.iter().any(is_wellformed) {
            stats.km.insert(stage, km_curve(&of_stage, KM_MAX_POINTS));
        }
        let next = next_steps(&of_stage);
        if !next.is_empty() {
            stats.next.insert(stage, next);
        }
    }
    stats
}
