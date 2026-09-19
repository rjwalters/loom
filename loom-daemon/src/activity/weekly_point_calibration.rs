//! `$-equivalent per weekly-limit-point` calibration (Issue #8348, part of
//! #8063): join a daily cost series against a daily weekly-limit-point series
//! and flag step changes in the resulting ratio.
//!
//! # Why this exists
//!
//! On 2026-09-17 a hand-scraped analysis found the fleet's $-equivalent cost
//! per weekly-limit point had dropped across three windows (≈5.7–7.4, then
//! ≈3.2–3.5, then ≈2.0–3.0) while every raw usage counter stayed flat — an
//! upstream metering change, not a workload change. **Nothing in Loom
//! noticed.** This module is the detector that would have.
//!
//! # Scope: pure logic, no I/O
//!
//! Deliberately split from its two data sources so the part with real
//! algorithmic content is reviewable and testable on its own:
//!
//! * the daily cost-equivalent series comes from [`super::usage_report`]
//!   (`loom-daemon usage-report --by day`, #8062);
//! * the daily weekly-limit-point series comes from #8347's samples;
//! * rendering the result in `loom-daemon health` is a separate sub-issue.
//!
//! Nothing here touches a database, a clock, or the filesystem: [`calibrate`]
//! is a total function of its two arguments, which is what lets the whole
//! contract below be pinned by unit tests against synthetic series.
//!
//! # The join
//!
//! Both inputs are `(day, value)` pairs. A day contributes a ratio only when
//! **both** series carry a usable sample for it; any other day is *skipped*,
//! never defaulted to zero. That distinction is the whole point of AC4 — a
//! missing sample coerced to `0.0` would manufacture either a 100% collapse
//! (`0 / points`) or a division by zero (`cost / 0`), i.e. exactly the step
//! change this module exists to report, out of thin air.
//!
//! "Usable" means strictly positive and finite on both axes. A zero on either
//! axis is treated as absent rather than real for the same reason: on a day
//! the fleet genuinely burned no points, `$/point` is undefined rather than
//! infinite, and a zero cost against non-zero points is far more likely to be
//! un-ingested cost data than free work.
//!
//! # The detector
//!
//! Each day's ratio is compared against the mean of the [`BASELINE_DAYS`]
//! **previously observed ratio days** — prior entries in this series, which
//! are not necessarily the prior three *calendar* days, because skipped days
//! leave no ratio behind. A day with fewer than `BASELINE_DAYS` predecessors
//! has no baseline at all ([`DayCalibration::baseline`] is `None`) and can
//! never warn, however extreme its ratio (AC3): with one sample there is no
//! such thing as a change.
//!
//! A warning fires when the fold change (`ratio / baseline`) leaves the band
//! `[1/`[`STEP_CHANGE_FOLD_THRESHOLD`]`, `[`STEP_CHANGE_FOLD_THRESHOLD`]`]` —
//! **in either direction**. The incident above was a drop, but a jump is an
//! equally strong metering-change signal, so direction is reported
//! ([`StepChangeDirection`]) rather than assumed.
//!
//! ## A sustained step may flag on more than one day
//!
//! This is a property of a trailing-mean baseline, not a bug: for up to
//! `BASELINE_DAYS` days after a real step, the baseline still mixes pre-step
//! values, so the ratio can stay outside the band while the window rolls
//! forward. The **first** flagged day is the transition; consumers that want
//! a single alert per step should de-duplicate on
//! [`CalibrationSeries::warnings`] rather than expecting exactly one entry.

use serde::Serialize;
use std::collections::BTreeMap;

/// How many previously observed ratio days form the trailing baseline.
///
/// Matches the ">1.5× over a trailing 3-day baseline" threshold specified by
/// #8063.
pub const BASELINE_DAYS: usize = 3;

/// The fold change (in either direction) a day's ratio must exceed relative
/// to its baseline to be flagged as a step change.
///
/// Strictly exceeded: a fold change of exactly `1.5` is *not* a warning, per
/// #8063's ">1.5×" wording.
pub const STEP_CHANGE_FOLD_THRESHOLD: f64 = 1.5;

/// One day's sample of either input series.
///
/// `day` is an opaque calendar-day key compared and ordered as a string, so
/// the `YYYY-MM-DD` spelling that
/// [`UsageReportGroupBy::Day`](super::UsageReportGroupBy::Day) already emits
/// sorts chronologically for free. Any stable spelling works as long as both
/// series use the same one.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyValue {
    /// Calendar-day key, e.g. `2026-09-11`.
    pub day: String,
    /// The day's total — dollars on the cost series, weekly-limit points on
    /// the points series.
    pub value: f64,
}

impl DailyValue {
    /// Convenience constructor for call sites (and tests) building a series
    /// from primitives.
    pub fn new(day: impl Into<String>, value: f64) -> Self {
        Self {
            day: day.into(),
            value,
        }
    }
}

/// Which way a flagged day moved relative to its baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum StepChangeDirection {
    /// The ratio rose by more than [`STEP_CHANGE_FOLD_THRESHOLD`]×: each
    /// weekly-limit point now costs materially *more* $-equivalent.
    Jump,
    /// The ratio fell to less than `1/`[`STEP_CHANGE_FOLD_THRESHOLD`]× — the
    /// shape of the incident behind #8063.
    Drop,
}

impl StepChangeDirection {
    /// The wire/display spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Jump => "jump",
            Self::Drop => "drop",
        }
    }
}

/// One day of the calibration series: the joined inputs, the ratio derived
/// from them, and everything the detector considered.
///
/// Carries the baseline and fold change even on days that did not warn so a
/// renderer can show the full series without recomputing anything (AC5).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DayCalibration {
    /// Calendar-day key, as supplied on both input series.
    pub day: String,
    /// The day's cost-equivalent total, in dollars.
    pub cost_usd: f64,
    /// The day's weekly-limit points consumed.
    pub weekly_points: f64,
    /// `cost_usd / weekly_points` — the calibration metric itself.
    pub usd_per_point: f64,
    /// Mean ratio over the [`BASELINE_DAYS`] previously observed days, or
    /// `None` when this day has fewer than that many predecessors.
    pub baseline: Option<f64>,
    /// `usd_per_point / baseline`. `None` exactly when `baseline` is `None`.
    pub fold_change: Option<f64>,
    /// `Some(direction)` when the fold change left the threshold band —
    /// i.e. this day is flagged.
    pub warning: Option<StepChangeDirection>,
}

impl DayCalibration {
    /// Whether this day fired a step-change warning.
    #[must_use]
    pub fn warned(&self) -> bool {
        self.warning.is_some()
    }
}

/// The full calibration series: one entry per day where both inputs were
/// usable, ordered by day ascending.
///
/// Days skipped by the join are simply absent — see the module doc; the
/// series never fabricates a zeroed row for a day it has no data for.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CalibrationSeries {
    /// Per-day results, ascending by [`DayCalibration::day`].
    pub days: Vec<DayCalibration>,
}

impl CalibrationSeries {
    /// Every flagged day, in series order.
    ///
    /// Note the "sustained step may flag on more than one day" caveat in the
    /// module doc: the first entry is the transition.
    #[must_use]
    pub fn warnings(&self) -> Vec<&DayCalibration> {
        self.days.iter().filter(|d| d.warned()).collect()
    }

    /// The most recent flagged day, if any — the form a health surface
    /// showing "is something wrong *now*" wants.
    #[must_use]
    pub fn latest_warning(&self) -> Option<&DayCalibration> {
        self.days.iter().rev().find(|d| d.warned())
    }
}

/// Join a daily cost-equivalent series against a daily weekly-limit-point
/// series and flag step changes in the resulting `$/point` ratio.
///
/// Neither input needs to be sorted, deduplicated, or the same length as the
/// other. Repeated entries for one day are summed (both series are per-day
/// totals, so two partial rows for a day are two parts of that day's total);
/// days present in only one series, or carrying a non-positive or non-finite
/// value in either, are skipped rather than defaulted (see the module doc).
///
/// Total and side-effect free: there is no error case and no I/O.
#[must_use]
pub fn calibrate(
    cost_usd_by_day: &[DailyValue],
    weekly_points_by_day: &[DailyValue],
) -> CalibrationSeries {
    let costs = totals_by_day(cost_usd_by_day);
    let points = totals_by_day(weekly_points_by_day);

    let mut days: Vec<DayCalibration> = Vec::new();
    // Observed ratios in series order; the tail is the trailing baseline
    // window. Skipped days contribute nothing, so this is a history of
    // *observations*, not of calendar days.
    let mut history: Vec<f64> = Vec::new();

    for (day, &cost_usd) in &costs {
        let Some(&weekly_points) = points.get(day) else {
            continue;
        };
        if !is_usable(cost_usd) || !is_usable(weekly_points) {
            continue;
        }
        let usd_per_point = cost_usd / weekly_points;
        if !is_usable(usd_per_point) {
            continue;
        }

        let baseline = trailing_baseline(&history);
        let fold_change = baseline.map(|b| usd_per_point / b);
        let warning = fold_change.and_then(classify_fold_change);

        days.push(DayCalibration {
            day: day.clone(),
            cost_usd,
            weekly_points,
            usd_per_point,
            baseline,
            fold_change,
            warning,
        });
        history.push(usd_per_point);
    }

    CalibrationSeries { days }
}

/// Whether a value can participate in a ratio: finite and strictly positive.
///
/// The strictness is deliberate — see the module doc on why a zero is treated
/// as an absent sample rather than a real one.
fn is_usable(value: f64) -> bool {
    value.is_finite() && value > 0.0
}

/// Fold one series into per-day totals, ordered by day.
fn totals_by_day(series: &[DailyValue]) -> BTreeMap<String, f64> {
    let mut totals: BTreeMap<String, f64> = BTreeMap::new();
    for sample in series {
        *totals.entry(sample.day.clone()).or_insert(0.0) += sample.value;
    }
    totals
}

/// Mean of the last [`BASELINE_DAYS`] observed ratios, or `None` when fewer
/// than that many exist (AC3: no baseline, so no possible warning).
fn trailing_baseline(history: &[f64]) -> Option<f64> {
    if history.len() < BASELINE_DAYS {
        return None;
    }
    let window = &history[history.len() - BASELINE_DAYS..];
    let mean = window.iter().sum::<f64>() / BASELINE_DAYS as f64;
    is_usable(mean).then_some(mean)
}

/// Classify a fold change against [`STEP_CHANGE_FOLD_THRESHOLD`], in either
/// direction. `None` inside the band (ordinary noise) or for a fold change
/// that is not a usable number.
fn classify_fold_change(fold_change: f64) -> Option<StepChangeDirection> {
    if !fold_change.is_finite() {
        return None;
    }
    if fold_change > STEP_CHANGE_FOLD_THRESHOLD {
        Some(StepChangeDirection::Jump)
    } else if fold_change < 1.0 / STEP_CHANGE_FOLD_THRESHOLD {
        Some(StepChangeDirection::Drop)
    } else {
        None
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Build a `YYYY-MM-DD` key for a 1-based day index in September 2026.
    fn day(n: usize) -> String {
        format!("2026-09-{n:02}")
    }

    /// Build both input series from a per-day `$/point` ratio, holding the
    /// points axis constant so each day's ratio is exactly `ratios[i]`.
    ///
    /// Returns `(cost_series, points_series)`.
    fn series_from_ratios(ratios: &[f64]) -> (Vec<DailyValue>, Vec<DailyValue>) {
        const POINTS_PER_DAY: f64 = 20.0;
        let costs = ratios
            .iter()
            .enumerate()
            .map(|(i, r)| DailyValue::new(day(i + 1), r * POINTS_PER_DAY))
            .collect();
        let points = (0..ratios.len())
            .map(|i| DailyValue::new(day(i + 1), POINTS_PER_DAY))
            .collect();
        (costs, points)
    }

    /// The days that fired, as `YYYY-MM-DD` keys.
    fn warned_days(series: &CalibrationSeries) -> Vec<String> {
        series
            .warnings()
            .iter()
            .map(|d| d.day.clone())
            .collect::<Vec<_>>()
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1e-9, "expected {expected}, got {actual}");
    }

    // ===============================================================
    // AC1 — the join produces a ratio per day with both inputs present
    // ===============================================================

    #[test]
    fn computes_usd_per_point_for_days_present_in_both_series() {
        let costs = vec![
            DailyValue::new(day(1), 120.0),
            DailyValue::new(day(2), 60.0),
        ];
        let points = vec![DailyValue::new(day(1), 20.0), DailyValue::new(day(2), 30.0)];

        let series = calibrate(&costs, &points);

        assert_eq!(series.days.len(), 2);
        assert_eq!(series.days[0].day, day(1));
        assert_close(series.days[0].usd_per_point, 6.0);
        assert_close(series.days[1].usd_per_point, 2.0);
        // Both days carry the raw inputs through for rendering (AC5).
        assert_close(series.days[0].cost_usd, 120.0);
        assert_close(series.days[0].weekly_points, 20.0);
    }

    #[test]
    fn unsorted_input_is_ordered_by_day_ascending() {
        let costs = vec![
            DailyValue::new(day(3), 30.0),
            DailyValue::new(day(1), 10.0),
            DailyValue::new(day(2), 20.0),
        ];
        let points = vec![
            DailyValue::new(day(2), 10.0),
            DailyValue::new(day(3), 10.0),
            DailyValue::new(day(1), 10.0),
        ];

        let series = calibrate(&costs, &points);

        let days: Vec<&str> = series.days.iter().map(|d| d.day.as_str()).collect();
        assert_eq!(days, vec![day(1), day(2), day(3)]);
    }

    #[test]
    fn repeated_entries_for_one_day_are_summed_as_partial_totals() {
        let costs = vec![DailyValue::new(day(1), 40.0), DailyValue::new(day(1), 80.0)];
        let points = vec![DailyValue::new(day(1), 5.0), DailyValue::new(day(1), 15.0)];

        let series = calibrate(&costs, &points);

        assert_eq!(series.days.len(), 1);
        assert_close(series.days[0].cost_usd, 120.0);
        assert_close(series.days[0].weekly_points, 20.0);
        assert_close(series.days[0].usd_per_point, 6.0);
    }

    #[test]
    fn empty_input_yields_an_empty_series_not_a_zero_row() {
        let series = calibrate(&[], &[]);
        assert!(series.days.is_empty());
        assert!(series.warnings().is_empty());
        assert!(series.latest_warning().is_none());
    }

    // ===============================================================
    // AC2 / Test Plan 1 — a known >1.5x jump fires exactly on its day
    // ===============================================================

    #[test]
    fn a_known_jump_fires_exactly_on_the_day_it_happens() {
        // Five flat days at 2.0, then a 1.7x jump to 3.4 that holds.
        let ratios = [2.0, 2.0, 2.0, 2.0, 2.0, 3.4, 3.4, 3.4, 3.4];
        let (costs, points) = series_from_ratios(&ratios);

        let series = calibrate(&costs, &points);

        assert_eq!(
            warned_days(&series),
            vec![day(6)],
            "the step is on day 6 and nowhere else: {:?}",
            series.days
        );
        let flagged = series.days.iter().find(|d| d.day == day(6)).unwrap();
        assert_eq!(flagged.warning, Some(StepChangeDirection::Jump));
        assert_close(flagged.baseline.unwrap(), 2.0);
        assert_close(flagged.fold_change.unwrap(), 1.7);
    }

    #[test]
    fn a_jump_of_exactly_the_threshold_does_not_fire() {
        // ">1.5x", not ">=": 3.0 against a flat 2.0 baseline is exactly 1.5.
        let ratios = [2.0, 2.0, 2.0, 3.0];
        let (costs, points) = series_from_ratios(&ratios);

        let series = calibrate(&costs, &points);

        assert!(warned_days(&series).is_empty());
        let last = series.days.last().unwrap();
        assert_close(last.fold_change.unwrap(), 1.5);
    }

    #[test]
    fn a_drop_is_flagged_as_well_as_a_jump() {
        // Direction must not be hard-coded: a >1.5x fall is equally a signal.
        let ratios = [6.0, 6.0, 6.0, 3.0];
        let (costs, points) = series_from_ratios(&ratios);

        let series = calibrate(&costs, &points);

        assert_eq!(warned_days(&series), vec![day(4)]);
        let flagged = series.days.last().unwrap();
        assert_eq!(flagged.warning, Some(StepChangeDirection::Drop));
        assert_close(flagged.baseline.unwrap(), 6.0);
        assert_close(flagged.fold_change.unwrap(), 0.5);
    }

    // ===============================================================
    // Test Plan 2 — ordinary noise never false-positives
    // ===============================================================

    #[test]
    fn ordinary_day_to_day_noise_never_fires() {
        // A realistic 20-day series wobbling within +/-20% of a 5.0 mean.
        let multipliers = [
            1.00, 1.12, 0.88, 1.05, 0.93, 1.18, 0.82, 1.09, 0.96, 1.15, 0.85, 1.02, 1.20, 0.80,
            1.07, 0.94, 1.11, 0.89, 1.03, 0.98,
        ];
        let ratios: Vec<f64> = multipliers.iter().map(|m| 5.0 * m).collect();
        let (costs, points) = series_from_ratios(&ratios);

        let series = calibrate(&costs, &points);

        assert_eq!(series.days.len(), 20);
        assert!(
            series.warnings().is_empty(),
            "+/-20% noise must not be reported as a step change: {:?}",
            warned_days(&series)
        );
        // Every day past the warm-up still reports a baseline to render.
        assert!(series.days[BASELINE_DAYS].baseline.is_some());
    }

    // ===============================================================
    // AC3 / Test Plan 3 — fewer than 3 days of history never fires
    // ===============================================================

    #[test]
    fn fewer_than_three_prior_days_never_fires_however_extreme() {
        // Day 3 is 100x day 2 and still cannot warn: with two predecessors
        // there is no baseline to have changed from.
        let ratios = [1.0, 1.0, 100.0];
        let (costs, points) = series_from_ratios(&ratios);

        let series = calibrate(&costs, &points);

        assert_eq!(series.days.len(), 3);
        assert!(series.warnings().is_empty());
        for d in &series.days {
            assert!(d.baseline.is_none(), "{} must have no baseline", d.day);
            assert!(d.fold_change.is_none());
        }
    }

    #[test]
    fn a_single_day_series_never_fires() {
        let (costs, points) = series_from_ratios(&[999.0]);
        let series = calibrate(&costs, &points);

        assert_eq!(series.days.len(), 1);
        assert!(series.days[0].baseline.is_none());
        assert!(series.warnings().is_empty());
    }

    #[test]
    fn the_baseline_appears_on_the_first_day_with_three_predecessors() {
        let ratios = [2.0, 4.0, 6.0, 4.0];
        let (costs, points) = series_from_ratios(&ratios);

        let series = calibrate(&costs, &points);

        assert!(series.days[2].baseline.is_none(), "only two predecessors");
        assert_close(series.days[3].baseline.unwrap(), 4.0);
        assert_close(series.days[3].fold_change.unwrap(), 1.0);
        assert!(!series.days[3].warned());
    }

    // ===============================================================
    // AC4 / Test Plan 4 — a gap is skipped, never read as zero
    // ===============================================================

    #[test]
    fn a_missing_weekly_point_sample_is_skipped_not_treated_as_zero() {
        // Perfectly flat 5.0 ratio; day 4 has no points sample at all.
        let ratios = [5.0, 5.0, 5.0, 5.0, 5.0, 5.0];
        let (costs, mut points) = series_from_ratios(&ratios);
        points.retain(|p| p.day != day(4));

        let series = calibrate(&costs, &points);

        let days: Vec<String> = series.days.iter().map(|d| d.day.clone()).collect();
        assert_eq!(days, vec![day(1), day(2), day(3), day(5), day(6)]);
        assert!(
            series.warnings().is_empty(),
            "a data gap is not a step change: {:?}",
            warned_days(&series)
        );
        // The day after the gap compares against the three observed days
        // before it — the gap neither zeroed nor reset the baseline.
        let after_gap = series.days.iter().find(|d| d.day == day(5)).unwrap();
        assert_close(after_gap.baseline.unwrap(), 5.0);
    }

    #[test]
    fn a_missing_cost_sample_is_skipped_not_treated_as_zero() {
        let ratios = [5.0, 5.0, 5.0, 5.0, 5.0, 5.0];
        let (mut costs, points) = series_from_ratios(&ratios);
        costs.retain(|c| c.day != day(4));

        let series = calibrate(&costs, &points);

        let days: Vec<String> = series.days.iter().map(|d| d.day.clone()).collect();
        assert_eq!(days, vec![day(1), day(2), day(3), day(5), day(6)]);
        assert!(series.warnings().is_empty());
    }

    #[test]
    fn an_explicit_zero_on_either_axis_is_treated_as_absent() {
        // A zeroed sample is the shape a naive "missing means zero" join
        // would produce; it must not divide by zero or collapse the ratio.
        let ratios = [5.0, 5.0, 5.0, 5.0, 5.0];
        let (mut costs, mut points) = series_from_ratios(&ratios);
        points[3] = DailyValue::new(day(4), 0.0);
        costs[4] = DailyValue::new(day(5), 0.0);

        let series = calibrate(&costs, &points);

        let days: Vec<String> = series.days.iter().map(|d| d.day.clone()).collect();
        assert_eq!(days, vec![day(1), day(2), day(3)]);
        assert!(series.warnings().is_empty());
        for d in &series.days {
            assert!(d.usd_per_point.is_finite());
        }
    }

    #[test]
    fn non_finite_and_negative_samples_are_skipped() {
        let ratios = [5.0, 5.0, 5.0, 5.0, 5.0, 5.0];
        let (mut costs, mut points) = series_from_ratios(&ratios);
        costs[3] = DailyValue::new(day(4), f64::NAN);
        points[4] = DailyValue::new(day(5), f64::INFINITY);
        costs[5] = DailyValue::new(day(6), -100.0);

        let series = calibrate(&costs, &points);

        let days: Vec<String> = series.days.iter().map(|d| d.day.clone()).collect();
        assert_eq!(days, vec![day(1), day(2), day(3)]);
        assert!(series.warnings().is_empty());
    }

    #[test]
    fn a_day_present_in_only_one_series_contributes_nothing() {
        let costs = vec![
            DailyValue::new(day(1), 100.0),
            DailyValue::new(day(2), 100.0),
        ];
        let points = vec![DailyValue::new(day(2), 20.0), DailyValue::new(day(3), 20.0)];

        let series = calibrate(&costs, &points);

        assert_eq!(series.days.len(), 1);
        assert_eq!(series.days[0].day, day(2));
    }

    // ===============================================================
    // Test Plan 5 — #8063's own three-window pattern
    // ===============================================================

    #[test]
    fn reproduces_the_incident_three_window_pattern_and_fires_at_the_first_transition() {
        // #8063's table, scaled to the same shape:
        //   window 1 (days 1-5):  5.7 - 7.4
        //   window 2 (days 6-8):  3.2 - 3.5
        //   window 3 (days 9-12): 2.0 - 3.0
        let ratios = [
            6.5, 7.0, 5.9, 7.4, 6.2, // window 1
            3.4, 3.2, 3.5, // window 2
            2.0, 2.8, 3.0, 2.4, // window 3
        ];
        let (costs, points) = series_from_ratios(&ratios);

        let series = calibrate(&costs, &points);
        let warned = warned_days(&series);

        // The first transition (day 6) is flagged, and it is the first flag:
        // nothing inside window 1's own spread fires.
        assert_eq!(
            warned.first().map(String::as_str),
            Some(day(6).as_str()),
            "the window-1 -> window-2 transition must be the first flag: {warned:?}"
        );
        let transition = series.days.iter().find(|d| d.day == day(6)).unwrap();
        assert_eq!(transition.warning, Some(StepChangeDirection::Drop));
        assert_close(transition.baseline.unwrap(), (5.9 + 7.4 + 6.2) / 3.0);
        assert!(transition.fold_change.unwrap() < 1.0 / STEP_CHANGE_FOLD_THRESHOLD);

        // The second transition (day 9) is a real step too, and is flagged.
        assert!(
            warned.contains(&day(9)),
            "the window-2 -> window-3 transition is also a step: {warned:?}"
        );

        // Days 1-5 — ordinary spread inside one window — never fire.
        for n in 1..=5 {
            assert!(!warned.contains(&day(n)), "day {n} is within-window spread");
        }

        // Exactly the documented behaviour: a sustained step keeps flagging
        // only while the trailing window still mixes pre-step values.
        assert_eq!(warned, vec![day(6), day(7), day(9)]);
        assert_eq!(series.latest_warning().unwrap().day, day(9));
    }

    // ===============================================================
    // AC5 — the returned structure renders without recomputation
    // ===============================================================

    #[test]
    fn every_day_carries_its_baseline_and_fold_change_for_rendering() {
        let ratios = [2.0, 2.0, 2.0, 3.4];
        let (costs, points) = series_from_ratios(&ratios);

        let series = calibrate(&costs, &points);
        let last = series.days.last().unwrap();

        assert_eq!(last.day, day(4));
        assert_close(last.cost_usd, 68.0);
        assert_close(last.weekly_points, 20.0);
        assert_close(last.usd_per_point, 3.4);
        assert_close(last.baseline.unwrap(), 2.0);
        assert_close(last.fold_change.unwrap(), 1.7);
        assert!(last.warned());
        assert_eq!(last.warning.unwrap().as_str(), "jump");
    }

    #[test]
    fn the_series_serializes_to_camel_case_json() {
        let ratios = [2.0, 2.0, 2.0, 3.4];
        let (costs, points) = series_from_ratios(&ratios);

        let json = serde_json::to_value(calibrate(&costs, &points)).unwrap();
        let last = &json["days"][3];

        assert_eq!(last["day"], day(4));
        assert_eq!(last["usdPerPoint"], 3.4);
        assert_eq!(last["warning"], "jump");
        assert!(last["foldChange"].is_number());
        assert_eq!(json["days"][0]["baseline"], serde_json::Value::Null);
    }
}
