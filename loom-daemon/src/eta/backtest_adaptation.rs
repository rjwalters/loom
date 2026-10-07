//! Regime-layer adaptation times on a heuristic's own replayed residuals
//! (`eta backtest --adaptation`, #10528).
//!
//! The adaptation-time harness of #10563 (`eta/tests/regime.rs`) measures
//! `t_p50`, `t_cov` and `t_alarm` on a synthetic Gaussian stream. This module
//! measures the same three figures on **real** noise: the scored `land`
//! (or `start` / `finish`) cases a backtest replayed for one heuristic.
//!
//! # What is measured
//!
//! 1. Each answered, landed case becomes a row: its stage, the instant its
//!    outcome became known (`actual_at`), `ln(actual / p50)`, and the
//!    p25/p75 band as log-ratios about the p50.
//! 2. The busiest stage with at least [`MIN_SAMPLES`] rows on each side of
//!    its median `known_at` is chosen, and that median is the shift
//!    instant `T`.
//! 3. A synthetic, unmodelled shift is injected: every outcome known at or
//!    after `T` is [`SHIFT_FACTOR`] times longer (its log-ratio moves by
//!    `ln 2`). Nothing else changes, as in the fixture of #10563.
//! 4. Hour by hour for up to [`HORIZON_H`] hours after `T`, the drift-gated
//!    regime layer ([`regime::gated`], what `land-2026-10-06-brisk-petrel`
//!    serves) and the drift check ([`regime::drift`]) are evaluated on the
//!    shifted stream, and on the unshifted one for comparison.
//!
//! - `t_p50`: first hour the served (drift-gated) factor is within 25% of
//!   the new truth: `|f_served / f_old - 2| <= 0.5`, where `f_old` is the
//!   ungated [`regime::adjust`] factor of the unshifted stream (`1.0` unless
//!   the heuristic carries a significant bias of its own), so a biased
//!   heuristic is measured against its own old truth.
//! - `t_cov`: first hour from which the trailing-6 h p25-p75 coverage of the
//!   post-shift outcomes (each scored against the factor served just before
//!   it became known) is back in the 40-60% band. Only post-shift outcomes
//!   count: a window that still holds pre-shift outcomes (in band by
//!   construction) would flatter the figure. A window with fewer than
//!   [`MIN_SAMPLES`] outcomes is skipped.
//! - `t_alarm`: first hour the drift check trips on the shifted stream.
//! - `false_alarm_h`: first hour it trips on the **unshifted** stream; on a
//!   calm history this is `None`.
//!
//! # What this is not
//!
//! It is the adaptation of the shared regime layer *given this heuristic's
//! residual noise*, not the heuristic's own adaptation: replaying a
//! heuristic over shifted history (so its own learning reacts too) is a
//! follow-up on #10528. So these figures must **not** feed
//! [`super::super::shadow_stats::AdaptationTimes`] or the promotion gate.
//!
//! Pure and deterministic: no clock, no RNG. Leak-free: every factor and
//! verdict at an instant reads only rows known strictly before it
//! ([`regime`]'s own contract).

use super::super::regime::{self, Residual};
use super::super::score::OutcomeKind;
use super::super::shadow::{COVERAGE_MAX, COVERAGE_MIN};
use super::super::{Stage, MIN_SAMPLES};
use super::paired::Replayed;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The injected shift: every outcome known at or after `T` takes this many
/// times longer (the "all review times doubled" fixture of #10528).
pub const SHIFT_FACTOR: f64 = 2.0;
/// Hours after `T` the replay follows at most.
pub const HORIZON_H: i64 = 24;
/// The trailing coverage window, hours.
pub const COVERAGE_WINDOW_H: i64 = 6;
/// "Within 25% of the new truth".
pub const P50_TOLERANCE: f64 = 0.25;
/// Operator targets (#10528), hours: `t_p50`, `t_cov`, `t_alarm`.
pub const TARGET_T_P50_H: i64 = 6;
/// See [`TARGET_T_P50_H`].
pub const TARGET_T_COV_H: i64 = 12;
/// See [`TARGET_T_P50_H`].
pub const TARGET_T_ALARM_H: i64 = 3;

/// One scored outcome on a stage track.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScoredRow {
    /// The stage the estimate was made in.
    pub stage: Stage,
    /// When the outcome became known.
    pub known_at: DateTime<Utc>,
    /// `ln(actual_remaining / p50)`.
    pub log_ratio: f64,
    /// `ln(p25 / p50)` and `ln(p75 / p50)`, when the estimate had both.
    pub band: Option<(f64, f64)>,
}

/// The regime layer's adaptation times on one heuristic's residuals.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegimeAdaptation {
    /// The stage track measured.
    pub stage: Stage,
    /// The injected shift instant `T`.
    pub shift_at: DateTime<Utc>,
    /// The injected multiplier on durations ([`SHIFT_FACTOR`]).
    pub shift_factor: f64,
    /// Rows on the track known before `T`.
    pub n_before: usize,
    /// Rows known in `[T, T + horizon_h)`.
    pub n_after: usize,
    /// Hours after `T` actually followed.
    pub horizon_h: i64,
    /// Hours until the served p50 is within 25% of the new truth.
    pub t_p50_h: Option<i64>,
    /// Hours until post-shift p25-p75 coverage is back in the 40-60% band.
    pub t_cov_h: Option<i64>,
    /// Hours until the drift check trips.
    pub t_alarm_h: Option<i64>,
    /// First hour the drift check trips on the unshifted stream.
    pub false_alarm_h: Option<i64>,
    /// Unadjusted p25-p75 coverage of the rows before `T`.
    pub pre_coverage: Option<f64>,
}

impl RegimeAdaptation {
    /// Whether every figure met its operator target and nothing false-alarmed.
    #[must_use]
    pub fn meets_targets(&self) -> bool {
        let within = |t: Option<i64>, target: i64| t.is_some_and(|h| h <= target);
        within(self.t_p50_h, TARGET_T_P50_H)
            && within(self.t_cov_h, TARGET_T_COV_H)
            && within(self.t_alarm_h, TARGET_T_ALARM_H)
            && self.false_alarm_h.is_none()
    }
}

/// The rows of every replayed case that answered and landed (or started /
/// finished, for those kinds), in `known_at` order.
pub(super) fn rows_of(replayed: &[Replayed]) -> Vec<ScoredRow> {
    let mut rows: Vec<ScoredRow> = replayed
        .iter()
        .filter(|r| {
            matches!(
                r.score.outcome,
                OutcomeKind::Landed | OutcomeKind::Started | OutcomeKind::Finished
            )
        })
        .filter_map(|r| {
            let s = &r.summary;
            let stage = s.stage?;
            let p50 = s.p50_sec.filter(|p| *p > 0)?;
            let actual = (r.score.actual_at - s.as_of).num_seconds();
            if actual <= 0 {
                return None;
            }
            let ln = |sec: i64| (sec as f64 / p50 as f64).ln();
            let band = match (s.p25_sec, s.p75_sec) {
                (Some(lo), Some(hi)) if lo > 0 && hi >= lo => Some((ln(lo), ln(hi))),
                _ => None,
            };
            Some(ScoredRow {
                stage,
                known_at: r.score.actual_at,
                log_ratio: ln(actual),
                band,
            })
        })
        .collect();
    rows.sort_by_key(|r| r.known_at);
    rows
}

/// Measure on the busiest stage whose median `known_at` has at least
/// [`MIN_SAMPLES`] rows before it and at least [`MIN_SAMPLES`] in the
/// horizon after it. `None` when no stage qualifies.
#[must_use]
pub fn measure(rows: &[ScoredRow]) -> Option<RegimeAdaptation> {
    let mut by_stage: BTreeMap<Stage, Vec<DateTime<Utc>>> = BTreeMap::new();
    for r in rows {
        by_stage.entry(r.stage).or_default().push(r.known_at);
    }
    let mut ranked: Vec<(Stage, Vec<DateTime<Utc>>)> = by_stage.into_iter().collect();
    // Busiest first; the stable sort keeps `Stage` order among equals.
    ranked.sort_by_key(|(_, at)| std::cmp::Reverse(at.len()));
    ranked.into_iter().find_map(|(stage, mut at)| {
        at.sort();
        measure_at(rows, stage, *at.get(at.len() / 2)?)
    })
}

/// Measure `stage` with the shift injected at `shift_at`. `None` when fewer
/// than [`MIN_SAMPLES`] rows precede it or fall inside the horizon.
#[must_use]
pub fn measure_at(
    rows: &[ScoredRow],
    stage: Stage,
    shift_at: DateTime<Utc>,
) -> Option<RegimeAdaptation> {
    let track: Vec<&ScoredRow> = rows.iter().filter(|r| r.stage == stage).collect();
    let n_before = track.iter().filter(|r| r.known_at < shift_at).count();
    let last = track.iter().map(|r| r.known_at).max()?;
    // Follow the stream until its last outcome is past, at most HORIZON_H.
    let horizon_h = ((last - shift_at).num_seconds() / 3_600 + 1).clamp(0, HORIZON_H);
    let end = shift_at + Duration::hours(horizon_h);
    let after: Vec<&ScoredRow> = track
        .iter()
        .copied()
        .filter(|r| r.known_at >= shift_at && r.known_at < end)
        .collect();
    if n_before < MIN_SAMPLES || after.len() < MIN_SAMPLES {
        return None;
    }
    let shift = SHIFT_FACTOR.ln();
    let residual = |r: &ScoredRow, shifted: bool| Residual {
        stage,
        known_at: r.known_at,
        log_ratio: r.log_ratio
            + if shifted && r.known_at >= shift_at {
                shift
            } else {
                0.0
            },
    };
    let base: Vec<Residual> = track.iter().map(|r| residual(r, false)).collect();
    let moved: Vec<Residual> = track.iter().map(|r| residual(r, true)).collect();

    // Each post-shift outcome against the factor served just before it
    // became known: its log-ratio to the *served* p50.
    let served: Vec<ScoredRow> = after
        .iter()
        .map(|r| ScoredRow {
            log_ratio: r.log_ratio + shift - regime::gated(&moved, stage, r.known_at).factor.ln(),
            ..**r
        })
        .collect();

    let mut out = RegimeAdaptation {
        stage,
        shift_at,
        shift_factor: SHIFT_FACTOR,
        n_before,
        n_after: after.len(),
        horizon_h,
        t_p50_h: None,
        t_cov_h: None,
        t_alarm_h: None,
        false_alarm_h: None,
        pre_coverage: coverage(
            track
                .iter()
                .filter(|r| r.known_at < shift_at)
                .map(|r| (r.log_ratio, r.band)),
        )
        .map(|(c, _)| c),
    };
    for h in 1..=horizon_h {
        let as_of = shift_at + Duration::hours(h);
        let ratio =
            regime::gated(&moved, stage, as_of).factor / regime::adjust(&base, stage, as_of).factor;
        if out.t_p50_h.is_none() && (ratio - SHIFT_FACTOR).abs() <= P50_TOLERANCE * SHIFT_FACTOR {
            out.t_p50_h = Some(h);
        }
        if out.t_alarm_h.is_none() && regime::drift(&moved, stage, as_of).drifted {
            out.t_alarm_h = Some(h);
        }
        if out.false_alarm_h.is_none() && regime::drift(&base, stage, as_of).drifted {
            out.false_alarm_h = Some(h);
        }
        let from = as_of - Duration::hours(COVERAGE_WINDOW_H);
        let window = served
            .iter()
            .filter(|r| r.known_at >= from && r.known_at < as_of)
            .map(|r| (r.log_ratio, r.band));
        if out.t_cov_h.is_none()
            && coverage(window).is_some_and(|(c, _)| (COVERAGE_MIN..=COVERAGE_MAX).contains(&c))
        {
            out.t_cov_h = Some(h);
        }
    }
    Some(out)
}

/// `(coverage, n)` of the p25-p75 band over rows that carry one; `None`
/// below [`MIN_SAMPLES`] such rows.
fn coverage(rows: impl Iterator<Item = (f64, Option<(f64, f64)>)>) -> Option<(f64, usize)> {
    let (mut hit, mut n) = (0_usize, 0_usize);
    for (lr, band) in rows {
        let Some((lo, hi)) = band else { continue };
        n += 1;
        if (lo..=hi).contains(&lr) {
            hit += 1;
        }
    }
    (n >= MIN_SAMPLES).then(|| ((hit as f64 / n as f64 * 1_000.0).round() / 1_000.0, n))
}
