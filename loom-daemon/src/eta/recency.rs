//! Recency-weighted stage samples (#10209).
//!
//! [`super::history::StageSamples::select`] admits every sample in the flat
//! [`super::WINDOW_DAYS`] window with equal weight, so a stage distribution
//! mostly describes the planner, role cadence, token pool and CI as they were
//! weeks ago. This module weighs each admitted sample by
//! `w = exp(−age / half_life)`, `age = as_of − observed_at`, and summarises
//! the weighted samples with [`super::grid::weighted_grid_of`].
//!
//! # What does not change
//!
//! - **The window, the cap and the floor.** The samples weighed are exactly
//!   the ones [`super::history::StageSamples::select_at`] picks — the same
//!   window, the same repo-then-host level rule on the same raw-count floor,
//!   the same worked-only conditioning (#9420), the same [`super::MAX_SAMPLES`]
//!   most-recent cap — and the censored side is the one
//!   [`super::history::StageSamples::select_censored`] picks at that level,
//!   weighed the same way. Weighting only reshapes a distribution the flat
//!   rule would have built; it never admits a sample the flat rule refuses.
//! - **Leak-freedom.** A sample observed at or after `as_of` is refused by the
//!   window before it is weighed, so `age > 0` for everything here and no
//!   weight depends on anything later than `as_of`.
//!
//! # The effective-N fallback
//!
//! Down-weighting old samples shrinks how much evidence a distribution really
//! rests on. The effective sample size `(Σw)² / Σw²` over the observed
//! samples measures it: `n` when every weight is equal, near `1` when one
//! sample dominates. Below [`super::MIN_SAMPLES`] the half-life is **widened**
//! — doubled, at most [`MAX_DOUBLINGS`] times — rather than the stage being
//! refused, and when even the widest half-life is short the weights fall back
//! to flat, where the effective N is the raw count the floor already passed.
//! The search is a fixed, finite sequence, so it is deterministic and always
//! terminates; the half-life actually used (`None` when flat) and the
//! effective N are recorded in the stage's explanation.
//!
//! The experiment on #10193 found a hard 14-day cut did *not* beat all
//! history, because long-tailed stages need long follow-up. That is why the
//! fallback widens instead of refusing, and why the censored lower bounds
//! are weighted rather than dropped.

use super::history::{selection_of, SampleSource, Selection, StageSample, StageSamples};
use super::{Stage, MIN_SAMPLES};
use chrono::{DateTime, Utc};

/// The default half-life: two days (#10209).
pub const DEFAULT_HALF_LIFE_SEC: i64 = 2 * 86_400;

/// How many times the half-life may double before the weights fall back to
/// flat. From two days, six doublings reach 128 days — past the whole 60-day
/// window, where the oldest admissible sample still weighs `exp(−60/128) ≈
/// 0.63` and the weights are already close to flat.
pub const MAX_DOUBLINGS: u32 = 6;

/// `exp(−age / half_life)`; `1.0` when `half_life_sec` is `None` (flat).
#[must_use]
pub fn weight(age_sec: i64, half_life_sec: Option<i64>) -> f64 {
    match half_life_sec {
        Some(h) if h > 0 => (-(age_sec.max(0) as f64) / h as f64).exp(),
        _ => 1.0,
    }
}

/// `(Σw)² / Σw²` over `weights`; `0.0` for none.
#[must_use]
pub fn effective_n(weights: impl IntoIterator<Item = f64>) -> f64 {
    let (sum, sum_sq) = weights
        .into_iter()
        .fold((0.0, 0.0), |(s, q), w| (s + w, q + w * w));
    if sum_sq > 0.0 {
        sum * sum / sum_sq
    } else {
        0.0
    }
}

/// The half-life to weigh samples of these `ages_sec` by: `base_sec`, doubled
/// until the effective N reaches `floor` ([`MAX_DOUBLINGS`] at most), else
/// `None` (flat). A non-positive `base_sec` is flat.
#[must_use]
pub fn resolve_half_life(ages_sec: &[i64], base_sec: i64, floor: usize) -> Option<i64> {
    if base_sec <= 0 {
        return None;
    }
    let mut half_life = base_sec;
    for _ in 0..=MAX_DOUBLINGS {
        let ess = effective_n(ages_sec.iter().map(|&a| weight(a, Some(half_life))));
        if ess >= floor as f64 {
            return Some(half_life);
        }
        half_life = half_life.saturating_mul(2);
    }
    None
}

/// Round to six decimals, as every float in an explanation is stored.
fn round6(x: f64) -> f64 {
    (x * 1_000_000.0).round() / 1_000_000.0
}

/// A stage's recency-weighted samples.
#[derive(Debug, Clone, PartialEq)]
pub struct WeightedSelection {
    /// The flat selection over the same samples: level, counts, and the
    /// durations ascending (`selection.sorted`), unweighted.
    pub selection: Selection,
    /// `(duration, weight)` of each observed sample, ascending by duration
    /// (ties by weight, so the order is total).
    pub observed: Vec<(i64, f64)>,
    /// `(lower bound, weight)` of each censored sample at the same level,
    /// ascending likewise. Empty unless censored samples were asked for.
    pub censored: Vec<(i64, f64)>,
    /// The half-life the weights use; `None` when it fell back to flat.
    pub half_life_sec: Option<i64>,
    /// `(Σw)² / Σw²` over the observed weights, rounded to six decimals.
    pub effective_n: f64,
}

fn weighted(
    picked: &[&StageSample],
    as_of: DateTime<Utc>,
    half_life_sec: Option<i64>,
) -> Vec<(i64, f64)> {
    let mut out: Vec<(i64, f64)> = picked
        .iter()
        .map(|s| {
            let age = (as_of - s.observed_at).num_seconds();
            (s.duration_sec, weight(age, half_life_sec))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
    out
}

impl StageSamples {
    /// [`Self::select`], recency-weighted with half-life `half_life_sec` and
    /// the effective-N fallback (module doc). With `censored`, the stage's
    /// censored lower bounds at the resolved level are weighted alongside, at
    /// the same half-life. `None` exactly when [`Self::select`] is `None`.
    #[must_use]
    pub fn select_weighted(
        &self,
        repo: &str,
        stage: Stage,
        as_of: DateTime<Utc>,
        sources: &[SampleSource],
        half_life_sec: i64,
        censored: bool,
    ) -> Option<WeightedSelection> {
        let (level, picked, excluded_unworked) =
            self.pick_observed(repo, stage, as_of, sources, MIN_SAMPLES)?;
        let ages: Vec<i64> = picked
            .iter()
            .map(|s| (as_of - s.observed_at).num_seconds())
            .collect();
        let used = resolve_half_life(&ages, half_life_sec, MIN_SAMPLES);
        let observed = weighted(&picked, as_of, used);
        let effective = round6(effective_n(observed.iter().map(|o| o.1)));
        let censored = if censored {
            weighted(&self.pick_censored(repo, stage, as_of, sources, level), as_of, used)
        } else {
            Vec::new()
        };
        let sorted: Vec<i64> = observed.iter().map(|o| o.0).collect();
        Some(WeightedSelection {
            selection: selection_of(level, sorted, &picked, excluded_unworked),
            observed,
            censored,
            half_life_sec: used,
            effective_n: effective,
        })
    }
}
