//! The conditional `limit_calibration` health section (#8063, wired by
//! #8349): the fleet's `$`-equivalent cost per weekly-limit point, plus the
//! step-change warning when it moves too fast.
//!
//! Split into its own file because `health.rs` sits at its
//! `.loom/docs/file-size-policy.md` ratchet: assessment logic that grows goes
//! in a sibling module and the parent keeps only the `mod` line plus the
//! re-export, exactly as [`super::codesign`] and [`super::busy`] already do.
//! Until #8349 the section was appended by the CLI collector itself
//! (`cli/health.rs`, #8366); #8349 moves it behind
//! [`HealthInputs::limit_calibration`] so `assess` renders it like every
//! other optional signal, and adds the claude-monitor-independent
//! #8347/#8348 fallback the collector now feeds it with.
//!
//! # Verdict mapping
//!
//! - [`CalibrationStatus::Ready`] with a warning -> `Degraded`, summary names
//!   the date, both ratios, and the multiple. This is the step-change warning
//!   #8063's AC asks for, on the existing anomaly surface — a `Degraded`
//!   section, which `assess`'s roll-up promotes into `overall` (and thus the
//!   exit code) like any other.
//! - [`CalibrationStatus::Ready`] without one -> `Green`, summary carries the
//!   latest day's $-eq-per-weekly-point. This is the "surface the metric" AC:
//!   the number is printed on an ordinary healthy run, not only on an alarm.
//! - [`CalibrationStatus::InsufficientData`] -> `Green`. Not yet having four
//!   joined days is the expected state of a fresh install, not a fault; the
//!   line still prints so an operator can see the pipeline is alive and
//!   accruing.
//! - `None` (not collected) or [`CalibrationStatus::Unavailable`] -> no
//!   section at all. Neither claude-monitor nor the persisted-sample fallback
//!   being readable on this host means the signal is not configured here,
//!   not that the fleet is unhealthy — the same "no section rather than a
//!   permanent non-green line" rule [`super::assess_observability`] applies to
//!   a disabled exporter (#4830) and [`super::assess_codesign_identity`] to an
//!   unconfigured identity.

use super::{HealthInputs, HealthSection, Verdict};
use crate::limit_calibration::{CalibrationStatus, BASELINE_WINDOW_DAYS};

/// Assess the collected limit-calibration reading into the conditional
/// `limit_calibration` section. Pure: the collector (`cli/health.rs`) has
/// already done the I/O ([`crate::limit_calibration::compute_with_fallback`]),
/// so this mapping from calibration state to verdict/summary is unit-testable
/// without a claude-monitor install or an activity database.
#[must_use]
pub fn assess_limit_calibration(inputs: &HealthInputs) -> Option<HealthSection> {
    const KEY: &str = "limit_calibration";
    let section = match inputs.limit_calibration.as_ref()? {
        CalibrationStatus::Ready {
            series,
            warning: Some(w),
        } => HealthSection {
            key: KEY,
            verdict: Verdict::Degraded,
            summary: format!(
                "$-eq/weekly-point STEP CHANGE on {}: {:.2} -> {:.2} ({:.2}x trailing {}d \
                 baseline)",
                w.date,
                w.baseline_usd_per_point,
                w.current_usd_per_point,
                w.ratio,
                BASELINE_WINDOW_DAYS
            ),
            detail: serde_json::json!({ "series": series, "warning": w }),
        },
        CalibrationStatus::Ready {
            series,
            warning: None,
        } => {
            let summary = series.last().map_or_else(
                || "no calibration data in window".to_string(),
                |d| {
                    format!(
                        "$-eq/weekly-point {:.2} on {} (no step change vs trailing {}d baseline)",
                        d.usd_per_weekly_point, d.date, BASELINE_WINDOW_DAYS
                    )
                },
            );
            HealthSection {
                key: KEY,
                verdict: Verdict::Green,
                summary,
                detail: serde_json::json!({ "series": series }),
            }
        }
        CalibrationStatus::InsufficientData { joined_days } => HealthSection {
            key: KEY,
            verdict: Verdict::Green,
            summary: format!(
                "calibrating — {joined_days} joined day(s) of history, need {}",
                BASELINE_WINDOW_DAYS + 1
            ),
            detail: serde_json::json!({ "joinedDays": joined_days }),
        },
        CalibrationStatus::Unavailable(_) => return None,
    };
    Some(section)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::health::HealthInputs;
    use crate::limit_calibration::{CalibrationDay, StepChangeWarning};

    fn calibration_day(date: &str, usd_per_point: f64) -> CalibrationDay {
        CalibrationDay {
            date: chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap(),
            cost_usd: usd_per_point * 10.0,
            weekly_points_delta: 10.0,
            usd_per_weekly_point: usd_per_point,
        }
    }

    fn inputs_with(limit_calibration: Option<CalibrationStatus>) -> HealthInputs {
        HealthInputs {
            limit_calibration,
            ..Default::default()
        }
    }

    /// AC (#8349 test plan): no calibration data available — the collection
    /// step degraded to "not collected" — renders no section at all, so a
    /// host without either data source keeps a clean report.
    #[test]
    fn not_collected_renders_no_section() {
        assert!(assess_limit_calibration(&inputs_with(None)).is_none());
        // ...and `assess` itself emits no `limit_calibration` line for it.
        let report = crate::health::assess(&inputs_with(None));
        assert!(report.sections.iter().all(|s| s.key != "limit_calibration"));
    }

    /// AC (#8349 test plan): a healthy ratio is surfaced as a GREEN section
    /// carrying the metric — "the computed $-eq-per-weekly-point metric is
    /// surfaced in `loom-daemon health` output" on an ordinary healthy run,
    /// not only when something is wrong.
    #[test]
    fn a_healthy_ratio_renders_green_and_surfaces_the_metric() {
        let section = assess_limit_calibration(&inputs_with(Some(CalibrationStatus::Ready {
            series: vec![
                calibration_day("2026-09-10", 5.0),
                calibration_day("2026-09-11", 5.2),
            ],
            warning: None,
        })))
        .expect("a readable calibration series always renders a section");
        assert_eq!(section.key, "limit_calibration");
        assert_eq!(section.verdict, Verdict::Green);
        assert!(
            section.summary.contains("5.20") && section.summary.contains("2026-09-11"),
            "the latest day's ratio must be in the summary line: {}",
            section.summary
        );
        assert!(section.detail["series"].is_array());
    }

    /// AC (#8349 test plan / #8063 AC): a flagged step change is visible as a
    /// DEGRADED section naming the date, both ratios and the multiple — the
    /// file's existing anomaly-surface convention, not a new output channel.
    #[test]
    fn a_flagged_step_change_renders_degraded_and_names_it() {
        let section = assess_limit_calibration(&inputs_with(Some(CalibrationStatus::Ready {
            series: vec![calibration_day("2026-09-17", 2.5)],
            warning: Some(StepChangeWarning {
                date: chrono::NaiveDate::parse_from_str("2026-09-17", "%Y-%m-%d").unwrap(),
                baseline_usd_per_point: 6.5,
                current_usd_per_point: 2.5,
                ratio: 2.6,
            }),
        })))
        .expect("a step change always renders a section");
        assert_eq!(section.verdict, Verdict::Degraded);
        assert!(section.summary.contains("STEP CHANGE"), "{}", section.summary);
        assert!(section.summary.contains("6.50"), "{}", section.summary);
        assert!(section.summary.contains("2.50"), "{}", section.summary);
        assert!(section.summary.contains("2.60x"), "{}", section.summary);
        assert_eq!(section.detail["warning"]["ratio"], serde_json::json!(2.6));
    }

    /// Too little history is the expected state of a fresh install, not a
    /// fault: the line prints (so the pipeline is visibly alive) but stays
    /// `Green` so it cannot flip the command's exit code.
    #[test]
    fn insufficient_history_is_green_while_accruing() {
        let section =
            assess_limit_calibration(&inputs_with(Some(CalibrationStatus::InsufficientData {
                joined_days: 2,
            })))
            .expect("an accruing pipeline still renders a section");
        assert_eq!(section.verdict, Verdict::Green);
        assert!(section.summary.contains("calibrating"), "{}", section.summary);
        assert_eq!(section.detail["joinedDays"], serde_json::json!(2));
    }

    /// An unreadable signal is not evidence of an unhealthy fleet: neither
    /// source readable means no section, never a permanent non-green line.
    #[test]
    fn unavailable_renders_no_section() {
        assert!(assess_limit_calibration(&inputs_with(Some(CalibrationStatus::Unavailable(
            "claude-monitor database not found".to_string()
        ))))
        .is_none());
    }

    /// The #8349 wiring itself: `assess` renders the section from
    /// [`HealthInputs::limit_calibration`], and its roll-up promotes a
    /// step change into `overall` — the promotion `collect` used to do by
    /// hand before the section moved behind `assess`.
    #[test]
    fn assess_renders_the_section_and_promotes_overall_for_a_step_change() {
        let inputs = inputs_with(Some(CalibrationStatus::Ready {
            series: vec![calibration_day("2026-09-17", 2.5)],
            warning: Some(StepChangeWarning {
                date: chrono::NaiveDate::parse_from_str("2026-09-17", "%Y-%m-%d").unwrap(),
                baseline_usd_per_point: 6.5,
                current_usd_per_point: 2.5,
                ratio: 2.6,
            }),
        }));
        let report = crate::health::assess(&inputs);
        let section = report
            .sections
            .iter()
            .find(|s| s.key == "limit_calibration")
            .expect("assess must render the calibration section");
        assert_eq!(section.verdict, Verdict::Degraded);
        assert_eq!(report.overall, Verdict::Degraded);
    }

    /// The same roll-up must stay `Green`-compatible when the reading is
    /// healthy — a Green calibration section can never be the reason a
    /// report turns non-green.
    #[test]
    fn a_green_reading_leaves_the_roll_up_unpromoted() {
        let inputs = inputs_with(Some(CalibrationStatus::Ready {
            series: vec![
                calibration_day("2026-09-10", 5.0),
                calibration_day("2026-09-11", 5.2),
            ],
            warning: None,
        }));
        let report = crate::health::assess(&inputs);
        let section = report
            .sections
            .iter()
            .find(|s| s.key == "limit_calibration")
            .expect("assess must render the calibration section");
        assert_eq!(section.verdict, Verdict::Green);
        assert_ne!(report.overall, Verdict::Degraded);
    }
}
