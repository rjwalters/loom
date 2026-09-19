//! Limit calibration (issue #8063, part of the #8052 tracking issue): a
//! daily `$-equivalent cost per weekly-limit point` signal, plus step-change
//! detection against a trailing 3-day baseline.
//!
//! # The incident this exists to catch
//!
//! On 2026-09-17 a hand-scraped 28-day analysis found the fleet's
//! $-equivalent cost per weekly-limit point dropped sharply — 5.7-7.4 (Aug
//! 24-Sep 4) -> 3.2-3.5 (Sep 6-8) -> 2.0-3.0 (Sep 11-16) — while raw usage
//! counters (messages/day, tokens, Claude Code version) stayed flat. Every
//! account in the pool doubled its points/day on the same day: an upstream
//! metering change, not a workload change. Nothing in Loom noticed — the
//! operator only found out when the pool started exhausting mid-week. See
//! #8052 for the full incident writeup.
//!
//! # Data sources: why claude-monitor's `usage_history`, not a new probe
//!
//! #8063's acceptance criteria name two acceptable sources for the daily
//! weekly-point signal: claude-monitor's own `usage_history` table, or a new
//! `loom-daemon tokens check`-sample history this issue would have to start
//! collecting from scratch. This module uses the **former**:
//!
//! - claude-monitor (`~/.claude-monitor/usage.db`) already records a
//!   `usage_history` row (`account_id`, `timestamp`, `weekly_all_percent`,
//!   ...) on every probe it makes, independently of Loom, at a cadence far
//!   finer than daily — a real install observed during implementation carried
//!   ~2000-2900 rows/day across 15-21 accounts for the two weeks preceding
//!   this issue landing. That is real, already-accruing history with no
//!   cold-start problem.
//! - The `tokens check`-sample fallback would have needed net-new persistent
//!   state (nothing in `loom-daemon` today stores more than the single most
//!   recent probe — `.ranking` is overwritten in place, and it does not even
//!   carry the 7-day/weekly utilization axis, only the 5-hour one — see
//!   [`crate::tokens_pool::check`]'s module doc). Every plausible place to
//!   add that (`loom-daemon/src/cli/tokens.rs`,
//!   `loom-daemon/src/tokens_pool/check.rs`) is at or within a few lines of
//!   this repo's file-size ratchet (`.loom/docs/file-size-policy.md`), and it
//!   would still need days to accrue a baseline before it could detect
//!   anything — a strictly worse starting position than a data source that
//!   already has months of history.
//! - `LOOM_CLAUDE_MONITOR_DIR` (honored via
//!   [`crate::tokens_pool::monitor::claude_monitor_dir`]) already relocates
//!   this exact directory for [`crate::tokens_pool::monitor_db`]'s live
//!   credential import, so this module inherits the same test/override story
//!   for free.
//!
//! Per the issue's own scope note, this fallback is loaded lazily and
//! degrades to [`CalibrationStatus::Unavailable`] — never a hard failure —
//! when claude-monitor is not installed on this host at all, so a Loom
//! install with no claude-monitor companion is unaffected.
//!
//! # Read-only, soft dependency
//!
//! Exactly like [`crate::tokens_pool::monitor_db`], claude-monitor's
//! `usage.db` is opened `file:...?mode=ro` (never written) with a bounded
//! busy timeout, and every failure (file absent, schema mismatch, lock held
//! by claude-monitor) degrades to [`CalibrationStatus::Unavailable`] rather
//! than propagating an error to the caller.
//!
//! # Units: one "weekly-limit point" is one percentage point
//!
//! `usage_history.weekly_all_percent` is claude-monitor's own 0-100 reading
//! of an account's rolling 7-day ("weekly") rate-limit utilization — the same
//! quantity `check-usage.sh` calls `weekly_all_percent`
//! ([`crate::script_helpers::usage::transform_api_response`]) and the native
//! probe calls `s7d_utilization` as a 0-1 fraction
//! ([`crate::tokens_pool::check::AccountResult::s7d_utilization`]). "One
//! weekly-limit point" here means one percentage point of that reading,
//! summed across every account in the pool — matching the units #8052's
//! hand-scraped table used.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;

use crate::activity::{ActivityDb, UsageReportGroupBy};

/// A step change fires when the current day's $/point ratio differs from its
/// trailing baseline by more than this factor, in either direction — matches
/// #8052's own threshold (a >2x drop is exactly the shape of the incident
/// that motivated this issue).
pub const STEP_CHANGE_RATIO_THRESHOLD: f64 = 1.5;

/// The number of prior calibration days averaged into the baseline a new
/// day's ratio is compared against.
pub const BASELINE_WINDOW_DAYS: usize = 3;

/// Default lookback window for both queries: long enough to build a baseline
/// and see a handful of days past it (and to eyeball a real fleet's history
/// per the issue's manual-verification test plan step), short enough that
/// every `loom-daemon health` invocation stays a bounded, indexed query
/// rather than a full-history scan.
pub const DEFAULT_LOOKBACK_DAYS: i64 = 14;

const MONITOR_DB_NAME: &str = "usage.db";
/// A lock held by claude-monitor surfaces as an error after this timeout
/// rather than hanging the `health` command indefinitely — mirrors
/// [`crate::tokens_pool::monitor_db`]'s own `SQLITE_TIMEOUT`.
const SQLITE_TIMEOUT: Duration = Duration::from_secs(5);

// ============================================================================
// Types
// ============================================================================

/// One day's cost-equivalent, from the `#8062` usage-report aggregation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DailyCost {
    pub date: NaiveDate,
    pub cost_usd: f64,
}

/// One day's fleet-wide weekly-limit-point consumption (the sum, across every
/// account with a same-day-adjacent prior sample, of that account's clamped
/// day-over-day `weekly_all_percent` delta).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DailyWeeklyPoints {
    pub date: NaiveDate,
    pub points_delta: f64,
}

/// One joined calibration day: a cost and a weekly-point delta that both
/// exist for the same date, plus the derived ratio.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CalibrationDay {
    pub date: NaiveDate,
    pub cost_usd: f64,
    pub weekly_points_delta: f64,
    pub usd_per_weekly_point: f64,
}

/// A detected step change: the most recent calibration day's ratio moved more
/// than [`STEP_CHANGE_RATIO_THRESHOLD`] away from its trailing
/// [`BASELINE_WINDOW_DAYS`]-day baseline.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StepChangeWarning {
    pub date: NaiveDate,
    pub baseline_usd_per_point: f64,
    pub current_usd_per_point: f64,
    /// Always >= [`STEP_CHANGE_RATIO_THRESHOLD`]; the larger of
    /// `current/baseline` and `baseline/current`, so it reads as a magnitude
    /// regardless of whether the ratio moved up or down.
    pub ratio: f64,
}

/// The outcome of [`compute`] — every failure mode is a variant here, never a
/// propagated `Err`, so a caller (`loom-daemon health`) can always render
/// something.
#[derive(Debug, Clone, PartialEq)]
pub enum CalibrationStatus {
    /// A joined daily series was computed. `warning` is `Some` exactly when
    /// [`detect_step_change`] fired against the most recent day.
    Ready {
        series: Vec<CalibrationDay>,
        warning: Option<StepChangeWarning>,
    },
    /// Fewer than `BASELINE_WINDOW_DAYS + 1` joined days of history — nothing
    /// to compare yet. Never a fault: a fresh install (or a host that has not
    /// accrued enough `usage_history`/`resource_usage` overlap) is expected
    /// to sit here for a while, and AC explicitly requires this not to
    /// false-positive.
    InsufficientData { joined_days: usize },
    /// The data could not be read at all (claude-monitor not installed on
    /// this host, activity.db absent, or a query failure against either).
    /// Carries the reason for a `--json` consumer; degrades a `health`
    /// section to `Unknown`, never `Degraded` — an unreadable *signal* is not
    /// evidence of an unhealthy fleet.
    Unavailable(String),
}

// ============================================================================
// Pure logic: join + step-change detection
// ============================================================================

/// Join a daily cost series against a daily weekly-points-delta series by
/// date. A day with a non-positive points delta (no accounts probed that day,
/// or every account's delta clamped to zero) is skipped — dividing by zero or
/// treating "no signal" as "zero cost per point" would both be misleading.
///
/// Returned sorted by date, ascending.
#[must_use]
pub fn join_daily_series(costs: &[DailyCost], points: &[DailyWeeklyPoints]) -> Vec<CalibrationDay> {
    let cost_by_date: BTreeMap<NaiveDate, f64> =
        costs.iter().map(|c| (c.date, c.cost_usd)).collect();

    let mut out: Vec<CalibrationDay> = points
        .iter()
        .filter(|p| p.points_delta > 0.0)
        .filter_map(|p| {
            cost_by_date.get(&p.date).map(|&cost_usd| CalibrationDay {
                date: p.date,
                cost_usd,
                weekly_points_delta: p.points_delta,
                usd_per_weekly_point: cost_usd / p.points_delta,
            })
        })
        .collect();

    out.sort_by_key(|d| d.date);
    out
}

/// Detect a step change in the most recent day of `series` (which must
/// already be sorted by date — [`join_daily_series`]'s contract) against the
/// mean of the [`BASELINE_WINDOW_DAYS`] calibration days immediately
/// preceding it.
///
/// Returns `None` (never a false positive) when:
/// - `series` has fewer than `BASELINE_WINDOW_DAYS + 1` entries (AC: "fewer
///   than 3 days of history should not false-positive").
/// - the baseline mean is non-positive (degenerate data).
/// - the ratio between the current day and the baseline does not exceed
///   [`STEP_CHANGE_RATIO_THRESHOLD`].
///
/// A gap in the underlying data (a day [`join_daily_series`] dropped because
/// it had no cost or no positive points delta) is never misread as a step
/// change: the baseline is the mean of the `BASELINE_WINDOW_DAYS` *joined*
/// days immediately preceding the current one, not a fixed calendar window,
/// so a missing calendar day simply is not one of the days averaged — it
/// never contributes a phantom zero.
#[must_use]
pub fn detect_step_change(series: &[CalibrationDay]) -> Option<StepChangeWarning> {
    if series.len() < BASELINE_WINDOW_DAYS + 1 {
        return None;
    }
    let n = series.len();
    let current = series[n - 1];
    let baseline_slice = &series[n - 1 - BASELINE_WINDOW_DAYS..n - 1];
    #[allow(clippy::cast_precision_loss)]
    let baseline = baseline_slice
        .iter()
        .map(|d| d.usd_per_weekly_point)
        .sum::<f64>()
        / BASELINE_WINDOW_DAYS as f64;
    if baseline <= 0.0 || current.usd_per_weekly_point <= 0.0 {
        return None;
    }

    let ratio = if current.usd_per_weekly_point >= baseline {
        current.usd_per_weekly_point / baseline
    } else {
        baseline / current.usd_per_weekly_point
    };

    if ratio > STEP_CHANGE_RATIO_THRESHOLD {
        Some(StepChangeWarning {
            date: current.date,
            baseline_usd_per_point: baseline,
            current_usd_per_point: current.usd_per_weekly_point,
            ratio,
        })
    } else {
        None
    }
}

/// Pure aggregation: raw `(account_id, rfc3339 timestamp, weekly_all_percent)`
/// samples -> one fleet-wide daily weekly-points delta per day.
///
/// For each account independently: keep only the sample with the latest
/// timestamp on each UTC calendar day (the running 7-day utilization reading
/// is cumulative within the day, so the last sample is the most complete
/// one), then take the day-over-day delta between calendar-adjacent days
/// only — a gap (a day this account was not sampled at all) contributes
/// nothing rather than folding several days of consumption into one delta. A
/// negative per-account delta (the account's 7-day window rolled over —
/// #4874's "reset", not "gave points back") is clamped to zero before being
/// summed into the day's fleet-wide total.
#[must_use]
fn aggregate_weekly_points(samples: &[(String, DateTime<Utc>, f64)]) -> Vec<DailyWeeklyPoints> {
    let mut by_account: BTreeMap<&str, BTreeMap<NaiveDate, (DateTime<Utc>, f64)>> = BTreeMap::new();
    for (account, ts, value) in samples {
        let date = ts.date_naive();
        by_account
            .entry(account.as_str())
            .or_default()
            .entry(date)
            .and_modify(|(existing_ts, existing_val)| {
                if ts > existing_ts {
                    *existing_ts = *ts;
                    *existing_val = *value;
                }
            })
            .or_insert((*ts, *value));
    }

    let mut totals: BTreeMap<NaiveDate, f64> = BTreeMap::new();
    for daily in by_account.values() {
        let mut days: Vec<(NaiveDate, f64)> = daily.iter().map(|(d, (_, v))| (*d, *v)).collect();
        days.sort_by_key(|(d, _)| *d);
        for pair in days.windows(2) {
            let (prev_date, prev_val) = pair[0];
            let (curr_date, curr_val) = pair[1];
            if curr_date.signed_duration_since(prev_date).num_days() == 1 {
                let delta = (curr_val - prev_val).max(0.0);
                *totals.entry(curr_date).or_insert(0.0) += delta;
            }
        }
    }

    totals
        .into_iter()
        .map(|(date, points_delta)| DailyWeeklyPoints { date, points_delta })
        .collect()
}

// ============================================================================
// I/O: claude-monitor `usage_history` + activity.db `resource_usage`
// ============================================================================

/// `~/.claude-monitor/usage.db` (or `LOOM_CLAUDE_MONITOR_DIR` if set) — the
/// same resolution [`crate::tokens_pool::monitor_db`] uses for its live
/// credential import.
#[must_use]
pub fn default_monitor_db_path() -> PathBuf {
    crate::tokens_pool::monitor::claude_monitor_dir().join(MONITOR_DB_NAME)
}

/// `~/.loom/activity.db`, or `LOOM_ACTIVITY_DB` if set — matches
/// `loom-daemon usage-report`'s own resolution
/// (`crate::cli::usage_report_cli`).
#[must_use]
pub fn default_activity_db_path() -> PathBuf {
    std::env::var_os("LOOM_ACTIVITY_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".loom")
                .join("activity.db")
        })
}

/// Read claude-monitor's `usage_history` table read-only and return the
/// fleet-wide daily weekly-points-delta series since `since`.
///
/// Only `is_synthetic = 0` rows are read — claude-monitor's own backfill
/// rows carry `is_synthetic = 1` and are not a real probe observation.
///
/// # Errors
/// A `String` reason on: the database file missing, the connection failing
/// to open, or the query failing (e.g. a claude-monitor schema old enough not
/// to carry `usage_history` at all).
fn weekly_points_from_monitor_db(
    monitor_db_path: &Path,
    since: DateTime<Utc>,
) -> Result<Vec<DailyWeeklyPoints>, String> {
    if !monitor_db_path.is_file() {
        return Err(format!(
            "claude-monitor database not found at {} (is claude-monitor installed on this \
             host? set LOOM_CLAUDE_MONITOR_DIR to point elsewhere)",
            monitor_db_path.display()
        ));
    }

    let uri = crate::tokens_pool::monitor_db::read_only_uri(monitor_db_path);
    let conn = Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| format!("could not open {}: {e}", monitor_db_path.display()))?;
    conn.busy_timeout(SQLITE_TIMEOUT)
        .map_err(|e| format!("could not open {}: {e}", monitor_db_path.display()))?;

    let mut stmt = conn
        .prepare(
            "SELECT account_id, timestamp, weekly_all_percent FROM usage_history \
             WHERE timestamp >= ?1 AND is_synthetic = 0 AND weekly_all_percent IS NOT NULL \
             ORDER BY account_id, timestamp",
        )
        .map_err(|e| format!("usage_history query failed (unexpected schema?): {e}"))?;

    // `timestamp` is TEXT, so `>=` is a *lexicographic* comparison: the bound
    // parameter has to be written in the same shape claude-monitor stores
    // (`2026-09-19T09:28:36Z`), not chrono's default `to_rfc3339()`
    // (`...+00:00`, with fractional seconds), or the cutoff drifts by up to a
    // second at the boundary and stops being a plain string prefix comparison.
    let since_text = since.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let rows = stmt
        .query_map(rusqlite::params![since_text], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, f64>(2)?))
        })
        .map_err(|e| format!("usage_history query failed: {e}"))?;

    let mut samples = Vec::new();
    for r in rows {
        let (account, ts_raw, value) =
            r.map_err(|e| format!("usage_history row read failed: {e}"))?;
        // Tolerate an unparsable timestamp on one row rather than discarding
        // the whole query — this table is written by a tool this crate does
        // not control.
        if let Ok(ts) = DateTime::parse_from_rfc3339(&ts_raw) {
            samples.push((account, ts.with_timezone(&Utc), value));
        }
    }

    Ok(aggregate_weekly_points(&samples))
}

/// The daily cost-equivalent series from the activity DB's `resource_usage`
/// table, via the same aggregation `loom-daemon usage-report --by day`
/// (issue #8062) already exposes as
/// [`crate::activity::ActivityDb::get_usage_report`].
///
/// # Errors
/// Propagates a query failure. A group label that does not parse as
/// `YYYY-MM-DD` (should not happen for [`UsageReportGroupBy::Day`]) is
/// skipped rather than failing the whole query.
fn daily_cost_series_from_activity_db(
    db: &ActivityDb,
    since: DateTime<Utc>,
) -> anyhow::Result<Vec<DailyCost>> {
    let rows = db.get_usage_report(since, UsageReportGroupBy::Day)?;
    let mut out: Vec<DailyCost> = rows
        .into_iter()
        .filter_map(|row| {
            NaiveDate::parse_from_str(&row.group, "%Y-%m-%d")
                .ok()
                .map(|date| DailyCost {
                    date,
                    cost_usd: row.cost_usd,
                })
        })
        .collect();
    out.sort_by_key(|d| d.date);
    Ok(out)
}

// ============================================================================
// Orchestration
// ============================================================================

/// The single degrade-safe entry point: never panics, never returns `Err` —
/// every failure mode becomes a [`CalibrationStatus`] variant a caller (e.g.
/// `loom-daemon health`, issue #8063) can render directly.
#[must_use]
pub fn compute(
    monitor_db_path: &Path,
    activity_db_path: &Path,
    since: DateTime<Utc>,
) -> CalibrationStatus {
    if !activity_db_path.is_file() {
        return CalibrationStatus::Unavailable(format!(
            "no activity database at {} — nothing to source the daily cost-equivalent from",
            activity_db_path.display()
        ));
    }
    let db = match ActivityDb::new(activity_db_path.to_path_buf()) {
        Ok(db) => db,
        Err(e) => return CalibrationStatus::Unavailable(format!("activity.db open failed: {e}")),
    };
    let costs = match daily_cost_series_from_activity_db(&db, since) {
        Ok(c) => c,
        Err(e) => return CalibrationStatus::Unavailable(format!("usage-report query failed: {e}")),
    };

    let points = match weekly_points_from_monitor_db(monitor_db_path, since) {
        Ok(p) => p,
        Err(e) => {
            return CalibrationStatus::Unavailable(format!(
                "claude-monitor usage_history unreadable ({e})"
            ))
        }
    };

    let series = join_daily_series(&costs, &points);
    if series.len() < BASELINE_WINDOW_DAYS + 1 {
        return CalibrationStatus::InsufficientData {
            joined_days: series.len(),
        };
    }
    let warning = detect_step_change(&series);
    CalibrationStatus::Ready { series, warning }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn date(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn day(date_str: &str, cost: f64, points: f64) -> CalibrationDay {
        CalibrationDay {
            date: date(date_str),
            cost_usd: cost,
            weekly_points_delta: points,
            usd_per_weekly_point: cost / points,
        }
    }

    // -- join_daily_series ---------------------------------------------

    #[test]
    fn join_matches_by_date_and_computes_ratio() {
        let costs = vec![
            DailyCost {
                date: date("2026-09-10"),
                cost_usd: 10.0,
            },
            DailyCost {
                date: date("2026-09-11"),
                cost_usd: 20.0,
            },
        ];
        let points = vec![
            DailyWeeklyPoints {
                date: date("2026-09-10"),
                points_delta: 2.0,
            },
            DailyWeeklyPoints {
                date: date("2026-09-11"),
                points_delta: 4.0,
            },
        ];
        let joined = join_daily_series(&costs, &points);
        assert_eq!(joined.len(), 2);
        assert!((joined[0].usd_per_weekly_point - 5.0).abs() < 1e-9);
        assert!((joined[1].usd_per_weekly_point - 5.0).abs() < 1e-9);
    }

    #[test]
    fn join_skips_a_day_with_no_matching_cost() {
        let costs = vec![DailyCost {
            date: date("2026-09-10"),
            cost_usd: 10.0,
        }];
        let points = vec![
            DailyWeeklyPoints {
                date: date("2026-09-10"),
                points_delta: 2.0,
            },
            // No cost row for 09-11 (e.g. no resource_usage activity that day).
            DailyWeeklyPoints {
                date: date("2026-09-11"),
                points_delta: 4.0,
            },
        ];
        let joined = join_daily_series(&costs, &points);
        assert_eq!(joined.len(), 1);
        assert_eq!(joined[0].date, date("2026-09-10"));
    }

    #[test]
    fn join_skips_a_non_positive_points_delta() {
        let costs = vec![
            DailyCost {
                date: date("2026-09-10"),
                cost_usd: 10.0,
            },
            DailyCost {
                date: date("2026-09-11"),
                cost_usd: 10.0,
            },
        ];
        let points = vec![
            DailyWeeklyPoints {
                date: date("2026-09-10"),
                points_delta: 0.0,
            },
            DailyWeeklyPoints {
                date: date("2026-09-11"),
                points_delta: 2.0,
            },
        ];
        let joined = join_daily_series(&costs, &points);
        assert_eq!(joined.len(), 1);
        assert_eq!(joined[0].date, date("2026-09-11"));
    }

    // -- detect_step_change ----------------------------------------------

    #[test]
    fn fewer_than_three_baseline_days_never_false_positives() {
        // Only 3 total days (2 baseline + 1 current) — one short of the
        // BASELINE_WINDOW_DAYS + 1 floor.
        let series = vec![
            day("2026-09-10", 10.0, 2.0),
            day("2026-09-11", 10.0, 2.0),
            day("2026-09-12", 1.0, 2.0), // would be a huge jump if evaluated
        ];
        assert!(detect_step_change(&series).is_none());
    }

    #[test]
    fn exactly_three_baseline_days_can_detect() {
        let series = vec![
            day("2026-09-08", 10.0, 2.0), // baseline
            day("2026-09-09", 10.0, 2.0), // baseline
            day("2026-09-10", 10.0, 2.0), // baseline (mean $/point = 5.0)
            day("2026-09-11", 4.0, 2.0),  // current: $/point = 2.0, ratio 2.5x
        ];
        let warning = detect_step_change(&series).unwrap();
        assert_eq!(warning.date, date("2026-09-11"));
        assert!((warning.baseline_usd_per_point - 5.0).abs() < 1e-9);
        assert!((warning.current_usd_per_point - 2.0).abs() < 1e-9);
        assert!((warning.ratio - 2.5).abs() < 1e-9);
    }

    #[test]
    fn matches_the_8052_incident_shape_a_sharp_drop() {
        // Mirrors the hand-scraped numbers: ~6.5 baseline -> ~2.5 current,
        // a >1.5x drop the same direction as the real incident.
        let series = vec![
            day("2026-09-14", 65.0, 10.0), // 6.5 $/pt
            day("2026-09-15", 68.0, 10.0), // 6.8 $/pt
            day("2026-09-16", 63.0, 10.0), // 6.3 $/pt (baseline mean 6.53..)
            day("2026-09-17", 25.0, 10.0), // 2.5 $/pt
        ];
        let warning = detect_step_change(&series).unwrap();
        assert!(warning.ratio > STEP_CHANGE_RATIO_THRESHOLD);
    }

    #[test]
    fn ordinary_noise_does_not_fire() {
        // +/- 10% day to day — well under the 1.5x threshold.
        let series = vec![
            day("2026-09-10", 50.0, 10.0), // 5.0
            day("2026-09-11", 55.0, 10.0), // 5.5
            day("2026-09-12", 48.0, 10.0), // 4.8
            day("2026-09-13", 52.0, 10.0), // 5.2 vs baseline mean ~5.1
        ];
        assert!(detect_step_change(&series).is_none());
    }

    #[test]
    fn a_gap_day_is_not_misread_as_a_step_change() {
        // 09-12 is absent from the joined series entirely (e.g. dropped by
        // join_daily_series for lacking a same-day cost or points row) —
        // the baseline must be the three joined days before current, not a
        // fixed calendar window that would otherwise average in a phantom
        // zero for the missing day.
        let series = vec![
            day("2026-09-08", 50.0, 10.0), // 5.0
            day("2026-09-09", 52.0, 10.0), // 5.2
            day("2026-09-10", 48.0, 10.0), // 4.8
            // 09-11 missing (gap)
            day("2026-09-12", 51.0, 10.0), // 5.1 — ordinary, should not fire
        ];
        assert!(detect_step_change(&series).is_none());
    }

    #[test]
    fn step_change_ratio_reads_as_a_magnitude_for_an_increase_too() {
        let series = vec![
            day("2026-09-08", 10.0, 5.0), // 2.0
            day("2026-09-09", 10.0, 5.0), // 2.0
            day("2026-09-10", 10.0, 5.0), // 2.0
            day("2026-09-11", 10.0, 1.0), // 10.0 -> 5x jump up
        ];
        let warning = detect_step_change(&series).unwrap();
        assert!((warning.ratio - 5.0).abs() < 1e-9);
    }

    // -- aggregate_weekly_points -------------------------------------------

    #[test]
    fn aggregate_takes_the_last_sample_per_account_per_day() {
        let samples = vec![
            ("acct-a".to_string(), ts("2026-09-10T01:00:00Z"), 10.0),
            ("acct-a".to_string(), ts("2026-09-10T20:00:00Z"), 15.0), // latest wins
            ("acct-a".to_string(), ts("2026-09-11T12:00:00Z"), 40.0),
        ];
        let out = aggregate_weekly_points(&samples);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].date, date("2026-09-11"));
        // 40 - 15 = 25, not 40 - 10.
        assert!((out[0].points_delta - 25.0).abs() < 1e-9);
    }

    #[test]
    fn aggregate_sums_across_accounts() {
        let samples = vec![
            ("acct-a".to_string(), ts("2026-09-10T00:00:00Z"), 10.0),
            ("acct-a".to_string(), ts("2026-09-11T00:00:00Z"), 30.0), // +20
            ("acct-b".to_string(), ts("2026-09-10T00:00:00Z"), 5.0),
            ("acct-b".to_string(), ts("2026-09-11T00:00:00Z"), 15.0), // +10
        ];
        let out = aggregate_weekly_points(&samples);
        assert_eq!(out.len(), 1);
        assert!((out[0].points_delta - 30.0).abs() < 1e-9);
    }

    #[test]
    fn aggregate_clamps_a_reset_to_zero_rather_than_going_negative() {
        let samples = vec![
            ("acct-a".to_string(), ts("2026-09-10T00:00:00Z"), 90.0),
            // 7d window rolled over — percent drops even though the account
            // kept consuming.
            ("acct-a".to_string(), ts("2026-09-11T00:00:00Z"), 10.0),
        ];
        let out = aggregate_weekly_points(&samples);
        assert_eq!(out.len(), 1);
        assert!((out[0].points_delta - 0.0).abs() < 1e-9);
    }

    #[test]
    fn aggregate_skips_a_non_adjacent_gap_day_for_that_account() {
        let samples = vec![
            ("acct-a".to_string(), ts("2026-09-10T00:00:00Z"), 10.0),
            // 09-11 missing entirely for this account.
            ("acct-a".to_string(), ts("2026-09-12T00:00:00Z"), 50.0),
        ];
        let out = aggregate_weekly_points(&samples);
        // No calendar-adjacent pair exists, so no delta is produced at all —
        // in particular the 40-point jump is never folded into 09-12.
        assert!(out.is_empty());
    }

    // -- compute: end-to-end degrade-safety --------------------------------

    #[test]
    fn compute_reports_unavailable_when_activity_db_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let status = compute(
            &dir.path().join("usage.db"),
            &dir.path().join("does-not-exist.db"),
            Utc::now() - chrono::Duration::days(DEFAULT_LOOKBACK_DAYS),
        );
        assert!(matches!(status, CalibrationStatus::Unavailable(_)));
    }

    #[test]
    fn compute_reports_unavailable_when_monitor_db_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let activity_db_path = dir.path().join("activity.db");
        // Touch an empty, schema-initialized activity DB (no resource_usage
        // rows) — `ActivityDb::new` creates the schema on first open.
        ActivityDb::new(activity_db_path.clone()).unwrap();

        let status = compute(
            &dir.path().join("usage.db"), // does not exist
            &activity_db_path,
            Utc::now() - chrono::Duration::days(DEFAULT_LOOKBACK_DAYS),
        );
        // No claude-monitor DB at all is Unavailable (the fallback data
        // source could not be read), not a fabricated "insufficient data".
        assert!(matches!(status, CalibrationStatus::Unavailable(_)));
    }

    /// Create a minimal claude-monitor-shaped `usage.db` at `path`, with just
    /// the `usage_history` table this module reads (the real database also
    /// carries `accounts`/`oauth_credentials`/etc., none of which this query
    /// touches).
    fn seed_monitor_db(path: &std::path::Path, rows: &[(&str, &str, f64)]) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE usage_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_id TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                weekly_all_percent REAL,
                is_synthetic INTEGER DEFAULT 0
             );",
        )
        .unwrap();
        for (account, timestamp, percent) in rows {
            conn.execute(
                "INSERT INTO usage_history (account_id, timestamp, weekly_all_percent, is_synthetic) \
                 VALUES (?1, ?2, ?3, 0)",
                rusqlite::params![account, timestamp, percent],
            )
            .unwrap();
        }
    }

    fn seed_activity_cost(db: &ActivityDb, timestamp: DateTime<Utc>, cost_usd: f64) {
        use crate::activity::resource_usage::ResourceUsage;
        db.record_resource_usage(&ResourceUsage {
            input_id: None,
            model: "claude-sonnet-5".to_string(),
            tokens_input: 100,
            tokens_output: 100,
            tokens_cache_read: None,
            tokens_cache_write: None,
            cost_usd,
            duration_ms: Some(1000),
            provider: "anthropic".to_string(),
            timestamp,
        })
        .unwrap();
    }

    #[test]
    fn compute_reports_insufficient_data_with_too_little_joined_history() {
        let dir = tempfile::tempdir().unwrap();
        let monitor_db_path = dir.path().join("usage.db");
        let activity_db_path = dir.path().join("activity.db");

        // Only two days of overlapping history — one short of
        // BASELINE_WINDOW_DAYS + 1.
        seed_monitor_db(
            &monitor_db_path,
            &[
                ("acct-a", "2026-09-10T00:00:00Z", 10.0),
                ("acct-a", "2026-09-11T00:00:00Z", 30.0),
            ],
        );
        let activity_db = ActivityDb::new(activity_db_path.clone()).unwrap();
        seed_activity_cost(&activity_db, ts("2026-09-10T12:00:00Z"), 5.0);
        seed_activity_cost(&activity_db, ts("2026-09-11T12:00:00Z"), 5.0);
        drop(activity_db);

        let status = compute(&monitor_db_path, &activity_db_path, ts("2026-09-01T00:00:00Z"));
        assert!(matches!(status, CalibrationStatus::InsufficientData { joined_days: 1 }));
    }

    /// `usage_history.timestamp` is TEXT, so the `>= ?1` cutoff is a
    /// lexicographic comparison against claude-monitor's own
    /// `2026-09-19T09:28:36Z` spelling. A `since` carrying fractional seconds
    /// (which `Utc::now()` always does) must still exclude rows genuinely
    /// before it — `chrono`'s default `to_rfc3339()` (`...+00:00`) does not
    /// compare correctly against a `Z`-suffixed column.
    #[test]
    fn monitor_query_cutoff_compares_correctly_against_z_suffixed_timestamps() {
        let dir = tempfile::tempdir().unwrap();
        let monitor_db_path = dir.path().join("usage.db");
        seed_monitor_db(
            &monitor_db_path,
            &[
                // Before the cutoff — must be excluded, so no adjacent pair
                // with 09-10 exists and no delta is produced for it.
                ("acct-a", "2026-09-09T23:00:00Z", 10.0),
                ("acct-a", "2026-09-10T12:00:00Z", 30.0),
                ("acct-a", "2026-09-11T12:00:00Z", 45.0),
            ],
        );
        let since = ts("2026-09-10T00:00:00Z") + chrono::Duration::nanoseconds(123_456_789);
        let out = weekly_points_from_monitor_db(&monitor_db_path, since).unwrap();
        assert_eq!(out.len(), 1, "only the 09-10 -> 09-11 pair survives the cutoff");
        assert_eq!(out[0].date, date("2026-09-11"));
        assert!((out[0].points_delta - 15.0).abs() < 1e-9);
    }

    #[test]
    fn compute_end_to_end_detects_a_step_change_from_real_sqlite_sources() {
        let dir = tempfile::tempdir().unwrap();
        let monitor_db_path = dir.path().join("usage.db");
        let activity_db_path = dir.path().join("activity.db");

        // Baseline: ~10 pts/day at $50/day (5.0 $/pt) for three days, then a
        // day where the same $50 buys only 5 pts (10.0 $/pt) — a 2x jump.
        seed_monitor_db(
            &monitor_db_path,
            &[
                ("acct-a", "2026-09-07T00:00:00Z", 0.0),
                ("acct-a", "2026-09-08T00:00:00Z", 10.0),
                ("acct-a", "2026-09-09T00:00:00Z", 20.0),
                ("acct-a", "2026-09-10T00:00:00Z", 30.0),
                ("acct-a", "2026-09-11T00:00:00Z", 35.0),
            ],
        );
        let activity_db = ActivityDb::new(activity_db_path.clone()).unwrap();
        for day in ["08", "09", "10", "11"] {
            seed_activity_cost(&activity_db, ts(&format!("2026-09-{day}T12:00:00Z")), 50.0);
        }
        drop(activity_db);

        let status = compute(&monitor_db_path, &activity_db_path, ts("2026-09-01T00:00:00Z"));
        match status {
            CalibrationStatus::Ready { series, warning } => {
                assert_eq!(series.len(), 4);
                let w = warning.expect("a >1.5x step change must be detected");
                assert_eq!(w.date, date("2026-09-11"));
                assert!(w.ratio > STEP_CHANGE_RATIO_THRESHOLD);
            }
            other => panic!("expected Ready with a warning, got {other:?}"),
        }
    }
}
