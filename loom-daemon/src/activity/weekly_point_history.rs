//! Daily weekly-limit-point history (Issue #8347, part of #8063).
//!
//! #8063 wants to chart `$`-equivalent **per weekly-limit point** and warn on a
//! step change. That needs two daily series joined on the UTC day: the
//! cost-equivalent (already available from #8062's
//! [`super::usage_report`]) and the number of weekly-limit points consumed —
//! which nothing persisted before this module.
//!
//! # What a "point" is
//!
//! `loom-daemon tokens check` derives each bootstrapped account's
//! `7d_utilization` (0.0–1.0, the fraction of its rolling weekly rate limit
//! already spent) from Anthropic's rate-limit headers — see
//! [`crate::tokens_pool::check::AccountResult::s7d_utilization`]. One account's
//! **fully** consumed weekly window is **100 points**, so a pool's consumption
//! signal is the sum, across every account, of `7d_utilization * 100`.
//! Accounts whose probe returned no utilization at all (`error`, `skipped`,
//! `unsupported`) contribute nothing and are not counted — see
//! [`sum_account_points`].
//!
//! # Why the day's MAXIMUM, upserted
//!
//! Utilization is monotonic non-decreasing inside a rolling 7-day window until
//! that window resets, so "the highest reading seen today" is a stable summary
//! of the day that recomputing cannot corrupt. The table is therefore keyed by
//! day and **upserted with `MAX`**, never appended to:
//!
//! - A reading taken after a window reset is *lower* than an earlier one the
//!   same day; keeping the max reports the day's real consumption instead of
//!   whatever the last tick of the day happened to catch.
//! - A partial reading (some accounts failed to probe) is also lower, so a
//!   flaky probe can never erase a good sample.
//! - Re-running `tokens check` N times a day is idempotent past the maximum.
//!
//! # Never fatal
//!
//! Every entry point here is best-effort, mirroring the convention
//! [`crate::token_ranking_refresh`]'s module doc states for the refresh loop
//! that drives it: a missing/locked/corrupt activity DB is warned about on
//! stderr and skipped. Nothing in this module may change `tokens check`'s
//! `.ranking` output or its exit code — see
//! [`record_daily_sample_best_effort_in`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use rusqlite::{params, Connection};
use serde::Serialize;

/// Points contributed by a single account whose weekly window is fully spent.
/// `7d_utilization` is a 0.0–1.0 fraction; a point is one percent of one
/// account's weekly limit.
pub const POINTS_PER_ACCOUNT: f64 = 100.0;

/// `day`'s storage format — the same `YYYY-MM-DD` spelling
/// [`super::usage_report::UsageReportGroupBy::Day`] emits, so #8063 can join
/// the two series on the raw column without reformatting either side.
const DAY_FORMAT: &str = "%Y-%m-%d";

/// One UTC day's high-water mark of weekly-limit consumption.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WeeklyPointSample {
    /// UTC calendar day.
    pub day: NaiveDate,
    /// Summed `7d_utilization` across the pool, in percentage points (see
    /// [`POINTS_PER_ACCOUNT`]).
    pub points: f64,
    /// How many accounts contributed to [`Self::points`] when this maximum was
    /// recorded. #8063 needs it to tell a genuine cost-per-point step change
    /// apart from the pool simply gaining or losing an account.
    pub account_count: i64,
}

/// Collapse one probe run's per-account `7d_utilization` readings into the
/// pool's point total and the number of accounts that contributed.
///
/// - An account with no reading (`None` — `error`/`skipped`/`unsupported`)
///   contributes nothing and is not counted.
/// - A non-finite reading is discarded and a negative one clamped to zero, so
///   a malformed header can never poison the stored maximum.
/// - A name seen more than once (the `--all-pools` fan-out can surface the
///   same account from a repo-local and the shared pool) contributes **once**,
///   at its highest reading — the total is "per bootstrapped account", not
///   "per pool membership".
pub fn sum_account_points<'a, I>(accounts: I) -> (f64, i64)
where
    I: IntoIterator<Item = (&'a str, Option<f64>)>,
{
    let mut best: BTreeMap<&'a str, f64> = BTreeMap::new();
    for (name, utilization) in accounts {
        let Some(value) = utilization else { continue };
        if !value.is_finite() {
            continue;
        }
        let value = value.max(0.0);
        best.entry(name)
            .and_modify(|current| *current = current.max(value))
            .or_insert(value);
    }
    let points = best.values().sum::<f64>() * POINTS_PER_ACCOUNT;
    let count = i64::try_from(best.len()).unwrap_or(i64::MAX);
    (points, count)
}

/// Upsert `day`'s sample, keeping the larger of the stored and incoming
/// `points`. `account_count`/`updated_at` move with `points` so the row always
/// describes the reading that set the maximum, not a later smaller one.
///
/// # Errors
/// Propagates any SQLite failure — callers that must not fail go through
/// [`record_daily_sample_best_effort_in`].
pub(super) fn record_weekly_point_sample(
    conn: &Connection,
    day: NaiveDate,
    points: f64,
    account_count: i64,
    now: DateTime<Utc>,
) -> Result<()> {
    // Every RHS below reads the PRE-update row (standard SQL UPDATE
    // semantics), so the `points` assignment ordering is irrelevant: the two
    // CASE guards and MAX() all compare against the same stored value.
    conn.execute(
        r"
        INSERT INTO weekly_point_samples (day, points, account_count, updated_at)
        VALUES (?1, ?2, ?3, ?4)
        ON CONFLICT(day) DO UPDATE SET
            account_count = CASE
                WHEN excluded.points > weekly_point_samples.points
                THEN excluded.account_count
                ELSE weekly_point_samples.account_count
            END,
            updated_at = CASE
                WHEN excluded.points > weekly_point_samples.points
                THEN excluded.updated_at
                ELSE weekly_point_samples.updated_at
            END,
            points = MAX(weekly_point_samples.points, excluded.points)
        ",
        params![
            day.format(DAY_FORMAT).to_string(),
            points,
            account_count,
            now.to_rfc3339(),
        ],
    )?;
    Ok(())
}

/// The daily series from `since` (inclusive) onward, oldest day first.
///
/// Returns an empty vec when nothing has been sampled in the window — the
/// caller decides how to render that, exactly as
/// [`super::usage_report::get_usage_report`] does.
///
/// # Errors
/// Propagates any SQLite failure.
pub(super) fn get_weekly_point_series(
    conn: &Connection,
    since: NaiveDate,
) -> Result<Vec<WeeklyPointSample>> {
    let mut stmt = conn.prepare(
        r"
        SELECT day, points, account_count
        FROM weekly_point_samples
        WHERE day >= ?1
        ORDER BY day ASC
        ",
    )?;
    let rows = stmt.query_map([since.format(DAY_FORMAT).to_string()], |row| {
        let day: String = row.get("day")?;
        Ok((day, row.get::<_, f64>("points")?, row.get::<_, i64>("account_count")?))
    })?;

    let mut out = Vec::new();
    for row in rows {
        let (day, points, account_count) = row?;
        // A day that will not parse is a hand-edited row, not a series point:
        // skip it rather than failing the whole read.
        if let Ok(day) = NaiveDate::parse_from_str(&day, DAY_FORMAT) {
            out.push(WeeklyPointSample {
                day,
                points,
                account_count,
            });
        }
    }
    Ok(out)
}

/// `~/.loom/activity.db`, or `LOOM_ACTIVITY_DB` if set — the same resolution
/// `loom-daemon usage-report` and `loom-daemon stats agent-metrics` use, so
/// the point series lands in the database #8063 will join it against.
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

/// Best-effort write against an explicit database path: records `day`'s sample
/// and returns whether it landed. **Never returns an error and never panics**
/// — an unopenable, locked, or read-only database is warned about on stderr
/// and skipped, so a probe run's `.ranking` output and exit code are
/// unaffected.
pub fn record_daily_sample_best_effort_in(
    db_path: &Path,
    day: NaiveDate,
    points: f64,
    account_count: i64,
) -> bool {
    match super::db::ActivityDb::new(db_path.to_path_buf())
        .and_then(|db| db.record_weekly_point_sample(day, points, account_count))
    {
        Ok(()) => true,
        Err(e) => {
            eprintln!(
                "warning: skipped recording today's weekly-limit-point sample in {}: {e}",
                db_path.display()
            );
            false
        }
    }
}

/// [`record_daily_sample_best_effort_in`] against
/// [`default_activity_db_path`], for today's UTC day.
pub fn record_daily_sample_best_effort(points: f64, account_count: i64) -> bool {
    record_daily_sample_best_effort_in(
        &default_activity_db_path(),
        Utc::now().date_naive(),
        points,
        account_count,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::db::ActivityDb;
    use super::*;

    /// The returned `TempDir` must stay alive for as long as `ActivityDb` is
    /// used: dropping it deletes the on-disk file out from under the open
    /// SQLite connection, which then fails writes as "readonly".
    fn open_db() -> (tempfile::TempDir, ActivityDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = ActivityDb::new(dir.path().join("activity.db")).unwrap();
        (dir, db)
    }

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, DAY_FORMAT).unwrap()
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn sums_utilization_into_percentage_points() {
        let (points, count) =
            sum_account_points([("a", Some(0.5)), ("b", Some(0.25)), ("c", Some(1.0))]);
        assert!((points - 175.0).abs() < 1e-9, "0.5 + 0.25 + 1.0 == 1.75 windows == 175 points");
        assert_eq!(count, 3);
    }

    #[test]
    fn accounts_without_a_reading_are_neither_summed_nor_counted() {
        let (points, count) =
            sum_account_points([("a", Some(0.5)), ("unsupported", None), ("errored", None)]);
        assert!((points - 50.0).abs() < 1e-9);
        assert_eq!(count, 1);
    }

    #[test]
    fn a_duplicate_account_name_contributes_once_at_its_highest_reading() {
        // The `--all-pools` fan-out can surface the same account from a
        // repo-local pool and the shared pool.
        let (points, count) = sum_account_points([("a", Some(0.2)), ("a", Some(0.6))]);
        assert!((points - 60.0).abs() < 1e-9);
        assert_eq!(count, 1, "one bootstrapped account, seen twice");
    }

    #[test]
    fn malformed_readings_cannot_poison_the_total() {
        let (points, count) = sum_account_points([
            ("nan", Some(f64::NAN)),
            ("neg", Some(-0.5)),
            ("ok", Some(0.1)),
        ]);
        assert!((points - 10.0).abs() < 1e-9, "NaN dropped, negative clamped to zero");
        assert_eq!(count, 2, "the clamped account still probed successfully");
    }

    #[test]
    fn empty_pool_is_zero_points() {
        let (points, count) = sum_account_points([]);
        assert!((points - 0.0).abs() < 1e-9);
        assert_eq!(count, 0);
    }

    #[test]
    fn two_samples_on_the_same_day_keep_the_maximum_not_the_latest() {
        let (_tmp, db) = open_db();
        let d = day("2026-09-19");

        record_weekly_point_sample(&db.conn, d, 175.0, 3, at("2026-09-19T08:00:00Z")).unwrap();
        // A later, LOWER reading (a weekly window reset mid-day, or a partial
        // probe) must not lower the day's high-water mark.
        record_weekly_point_sample(&db.conn, d, 20.0, 1, at("2026-09-19T20:00:00Z")).unwrap();

        let series = get_weekly_point_series(&db.conn, day("2026-01-01")).unwrap();
        assert_eq!(series.len(), 1, "one row per calendar day, never appended to");
        assert!((series[0].points - 175.0).abs() < 1e-9);
        assert_eq!(series[0].account_count, 3, "the count moves with the maximum");
    }

    #[test]
    fn a_higher_later_sample_replaces_the_day_and_its_metadata() {
        let (_tmp, db) = open_db();
        let d = day("2026-09-19");

        record_weekly_point_sample(&db.conn, d, 100.0, 2, at("2026-09-19T08:00:00Z")).unwrap();
        record_weekly_point_sample(&db.conn, d, 150.0, 3, at("2026-09-19T20:00:00Z")).unwrap();

        let series = get_weekly_point_series(&db.conn, day("2026-01-01")).unwrap();
        assert_eq!(series.len(), 1);
        assert!((series[0].points - 150.0).abs() < 1e-9);
        assert_eq!(series[0].account_count, 3);

        let updated_at: String = db
            .conn
            .query_row("SELECT updated_at FROM weekly_point_samples", [], |row| row.get(0))
            .unwrap();
        assert!(updated_at.starts_with("2026-09-19T20:00:00"));
    }

    #[test]
    fn rewriting_an_identical_sample_is_idempotent() {
        let (_tmp, db) = open_db();
        let d = day("2026-09-19");
        for _ in 0..5 {
            record_weekly_point_sample(&db.conn, d, 42.5, 2, at("2026-09-19T08:00:00Z")).unwrap();
        }
        let series = get_weekly_point_series(&db.conn, day("2026-01-01")).unwrap();
        assert_eq!(series.len(), 1);
        assert!((series[0].points - 42.5).abs() < 1e-9);
    }

    #[test]
    fn a_database_with_no_samples_reads_back_empty() {
        let (_tmp, db) = open_db();
        let series = get_weekly_point_series(&db.conn, day("2026-01-01")).unwrap();
        assert!(series.is_empty(), "no samples must read as empty, not as a zeroed day");
    }

    #[test]
    fn the_lookback_window_returns_the_ordered_series_and_excludes_older_days() {
        let (_tmp, db) = open_db();
        // Inserted out of order on purpose — the read must order, not the write.
        for (d, points, count) in [
            ("2026-09-17", 120.0, 3),
            ("2026-09-15", 60.0, 2),
            ("2026-09-19", 200.0, 3),
            ("2026-08-01", 999.0, 9),
        ] {
            record_weekly_point_sample(&db.conn, day(d), points, count, at("2026-09-19T08:00:00Z"))
                .unwrap();
        }

        let series = get_weekly_point_series(&db.conn, day("2026-09-15")).unwrap();
        let days: Vec<String> = series.iter().map(|s| s.day.to_string()).collect();
        assert_eq!(days, vec!["2026-09-15", "2026-09-17", "2026-09-19"]);
        assert!((series[0].points - 60.0).abs() < 1e-9);
        assert!((series[2].points - 200.0).abs() < 1e-9);
        assert!(!days.contains(&"2026-08-01".to_string()), "outside the lookback window");
    }

    #[test]
    fn the_activity_db_wrapper_round_trips_a_sample() {
        let (_tmp, db) = open_db();
        db.record_weekly_point_sample(day("2026-09-19"), 175.0, 3)
            .unwrap();
        let series = db.get_weekly_point_series(day("2026-09-19")).unwrap();
        assert_eq!(series.len(), 1);
        assert!((series[0].points - 175.0).abs() < 1e-9);
    }

    #[test]
    fn a_db_open_failure_in_the_write_path_is_reported_but_never_propagated() {
        let dir = tempfile::tempdir().unwrap();
        // Parent directory does not exist -> `Connection::open` fails.
        let unopenable = dir.path().join("no-such-dir").join("activity.db");
        assert!(
            !record_daily_sample_best_effort_in(&unopenable, day("2026-09-19"), 175.0, 3),
            "an unopenable database must report failure, not panic or propagate"
        );
        assert!(!unopenable.exists());
    }

    #[test]
    fn the_best_effort_write_lands_when_the_database_is_usable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("activity.db");
        assert!(record_daily_sample_best_effort_in(&path, day("2026-09-19"), 175.0, 3));

        let db = ActivityDb::new(path).unwrap();
        let series = db.get_weekly_point_series(day("2026-09-19")).unwrap();
        assert_eq!(series.len(), 1);
        assert!((series[0].points - 175.0).abs() < 1e-9);
        assert_eq!(series[0].account_count, 3);
    }

    #[test]
    fn the_day_column_matches_the_usage_report_day_spelling() {
        // #8063 joins this series against `usage-report --by day`, whose group
        // label is SQLite `DATE()`'s `YYYY-MM-DD`.
        let (_tmp, db) = open_db();
        record_weekly_point_sample(&db.conn, day("2026-09-19"), 1.0, 1, at("2026-09-19T08:00:00Z"))
            .unwrap();
        let stored: String = db
            .conn
            .query_row("SELECT day FROM weekly_point_samples", [], |row| row.get(0))
            .unwrap();
        assert_eq!(stored, "2026-09-19");
    }
}
