use super::db::{calculate_cost, open_activity_db};
use rusqlite::params;
use serde::{Deserialize, Serialize};

/// Daily velocity snapshot
#[derive(Debug, Serialize, Deserialize)]
pub struct VelocitySnapshot {
    pub snapshot_date: String,
    pub issues_closed: i32,
    pub prs_merged: i32,
    pub avg_cycle_time_hours: Option<f64>,
    pub total_prompts: i32,
    pub total_cost_usd: f64,
}

/// Trend direction indicator
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TrendDirection {
    /// Improving (higher is better)
    Improving,
    /// Declining (lower than before)
    Declining,
    /// Stable (within 10% variance)
    Stable,
}

impl std::fmt::Display for TrendDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Improving => "improving",
            Self::Declining => "declining",
            Self::Stable => "stable",
        };
        write!(f, "{s}")
    }
}

/// Velocity summary with trends
#[derive(Debug, Serialize, Deserialize)]
pub struct VelocitySummary {
    /// Current period metrics
    pub issues_closed: i32,
    pub prs_merged: i32,
    pub avg_cycle_time_hours: Option<f64>,
    pub total_prompts: i32,
    pub total_cost_usd: f64,
    /// Previous period metrics (for comparison)
    pub prev_issues_closed: i32,
    pub prev_prs_merged: i32,
    pub prev_avg_cycle_time_hours: Option<f64>,
    /// Trend directions
    pub issues_trend: TrendDirection,
    pub prs_trend: TrendDirection,
    pub cycle_time_trend: TrendDirection,
}

/// Rolling average metrics
#[derive(Debug, Serialize, Deserialize)]
pub struct RollingAverage {
    pub period_days: i32,
    pub avg_issues_per_day: f64,
    pub avg_prs_per_day: f64,
    pub avg_cycle_time_hours: Option<f64>,
    pub avg_cost_per_day: f64,
}

/// Calculate trend direction based on percentage change
fn calculate_trend(current: f64, previous: f64, lower_is_better: bool) -> TrendDirection {
    if previous == 0.0 {
        if current > 0.0 {
            return if lower_is_better {
                TrendDirection::Declining
            } else {
                TrendDirection::Improving
            };
        }
        return TrendDirection::Stable;
    }

    let change_pct = (current - previous) / previous;

    // Within 10% is considered stable
    if change_pct.abs() < 0.1 {
        TrendDirection::Stable
    } else if lower_is_better {
        if change_pct < 0.0 {
            TrendDirection::Improving
        } else {
            TrendDirection::Declining
        }
    } else if change_pct > 0.0 {
        TrendDirection::Improving
    } else {
        TrendDirection::Declining
    }
}

/// Generate or update today's velocity snapshot
#[tauri::command]
pub fn generate_velocity_snapshot(workspace_path: &str) -> Result<VelocitySnapshot, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let today = chrono::Local::now().format("%Y-%m-%d").to_string();

    // Get issues closed today (from prompt_github events)
    let issues_closed: i32 = conn
        .query_row(
            "SELECT COUNT(DISTINCT issue_number)
             FROM prompt_github
             WHERE event_type = 'issue_closed'
               AND DATE(event_time) = ?1",
            [&today],
            |row| row.get(0),
        )
        .unwrap_or(0);

    // Get PRs merged today
    let prs_merged: i32 = conn
        .query_row(
            "SELECT COUNT(DISTINCT pr_number)
             FROM prompt_github
             WHERE event_type = 'pr_merged'
               AND DATE(event_time) = ?1",
            [&today],
            |row| row.get(0),
        )
        .unwrap_or(0);

    // Calculate average cycle time for PRs merged today
    let avg_cycle_time: Option<f64> = conn
        .query_row(
            "SELECT AVG(
                (julianday(merged.event_time) - julianday(created.event_time)) * 24
             )
             FROM prompt_github merged
             INNER JOIN prompt_github created ON merged.pr_number = created.pr_number
             WHERE merged.event_type = 'pr_merged'
               AND created.event_type = 'pr_created'
               AND DATE(merged.event_time) = ?1",
            [&today],
            |row| row.get(0),
        )
        .ok();

    // Get total prompts today
    let total_prompts: i32 = conn
        .query_row(
            "SELECT COUNT(*)
             FROM agent_activity
             WHERE DATE(timestamp) = ?1",
            [&today],
            |row| row.get(0),
        )
        .unwrap_or(0);

    // Get total cost today
    let (prompt_tokens, completion_tokens): (i64, i64) = conn
        .query_row(
            "SELECT COALESCE(SUM(t.prompt_tokens), 0), COALESCE(SUM(t.completion_tokens), 0)
             FROM agent_activity a
             JOIN token_usage t ON a.id = t.activity_id
             WHERE DATE(a.timestamp) = ?1",
            [&today],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap_or((0, 0));

    let total_cost_usd = calculate_cost(prompt_tokens, completion_tokens);

    // Insert or update today's snapshot
    conn.execute(
        "INSERT INTO velocity_snapshots (snapshot_date, issues_closed, prs_merged, avg_cycle_time_hours, total_prompts, total_cost_usd)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(snapshot_date) DO UPDATE SET
             issues_closed = excluded.issues_closed,
             prs_merged = excluded.prs_merged,
             avg_cycle_time_hours = excluded.avg_cycle_time_hours,
             total_prompts = excluded.total_prompts,
             total_cost_usd = excluded.total_cost_usd",
        params![
            &today,
            issues_closed,
            prs_merged,
            avg_cycle_time,
            total_prompts,
            total_cost_usd
        ],
    )
    .map_err(|e| format!("Failed to save velocity snapshot: {e}"))?;

    Ok(VelocitySnapshot {
        snapshot_date: today,
        issues_closed,
        prs_merged,
        avg_cycle_time_hours: avg_cycle_time,
        total_prompts,
        total_cost_usd,
    })
}

/// Get velocity snapshots for a date range
#[tauri::command]
pub fn get_velocity_snapshots(
    workspace_path: &str,
    days: Option<i32>,
) -> Result<Vec<VelocitySnapshot>, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let days_val = days.unwrap_or(30);

    let mut stmt = conn
        .prepare(
            "SELECT snapshot_date, issues_closed, prs_merged, avg_cycle_time_hours, total_prompts, total_cost_usd
             FROM velocity_snapshots
             WHERE snapshot_date >= DATE('now', '-' || ?1 || ' days')
             ORDER BY snapshot_date DESC",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let snapshots = stmt
        .query_map([days_val], |row| {
            Ok(VelocitySnapshot {
                snapshot_date: row.get(0)?,
                issues_closed: row.get(1)?,
                prs_merged: row.get(2)?,
                avg_cycle_time_hours: row.get(3)?,
                total_prompts: row.get(4)?,
                total_cost_usd: row.get(5)?,
            })
        })
        .map_err(|e| format!("Failed to query snapshots: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect snapshots: {e}"))?;

    Ok(snapshots)
}

/// Get velocity summary with week-over-week comparison
#[tauri::command]
pub fn get_velocity_summary(workspace_path: &str) -> Result<VelocitySummary, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    // Current week (last 7 days)
    let current_query = "SELECT
        COALESCE(SUM(issues_closed), 0) as issues,
        COALESCE(SUM(prs_merged), 0) as prs,
        AVG(avg_cycle_time_hours) as cycle_time,
        COALESCE(SUM(total_prompts), 0) as prompts,
        COALESCE(SUM(total_cost_usd), 0) as cost
     FROM velocity_snapshots
     WHERE snapshot_date >= DATE('now', '-7 days')";

    let (issues_closed, prs_merged, avg_cycle_time_hours, total_prompts, total_cost_usd): (
        i32,
        i32,
        Option<f64>,
        i32,
        f64,
    ) = conn
        .query_row(current_query, [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
        })
        .unwrap_or((0, 0, None, 0, 0.0));

    // Previous week (8-14 days ago)
    let prev_query = "SELECT
        COALESCE(SUM(issues_closed), 0) as issues,
        COALESCE(SUM(prs_merged), 0) as prs,
        AVG(avg_cycle_time_hours) as cycle_time
     FROM velocity_snapshots
     WHERE snapshot_date >= DATE('now', '-14 days')
       AND snapshot_date < DATE('now', '-7 days')";

    let (prev_issues_closed, prev_prs_merged, prev_avg_cycle_time_hours): (i32, i32, Option<f64>) =
        conn.query_row(prev_query, [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap_or((0, 0, None));

    // Calculate trends
    let issues_trend =
        calculate_trend(f64::from(issues_closed), f64::from(prev_issues_closed), false);
    let prs_trend = calculate_trend(f64::from(prs_merged), f64::from(prev_prs_merged), false);
    let cycle_time_trend = match (avg_cycle_time_hours, prev_avg_cycle_time_hours) {
        (Some(current), Some(prev)) => calculate_trend(current, prev, true),
        _ => TrendDirection::Stable,
    };

    Ok(VelocitySummary {
        issues_closed,
        prs_merged,
        avg_cycle_time_hours,
        total_prompts,
        total_cost_usd,
        prev_issues_closed,
        prev_prs_merged,
        prev_avg_cycle_time_hours,
        issues_trend,
        prs_trend,
        cycle_time_trend,
    })
}

/// Get rolling average metrics
#[tauri::command]
pub fn get_rolling_average(
    workspace_path: &str,
    period_days: Option<i32>,
) -> Result<RollingAverage, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let days = period_days.unwrap_or(7);

    let query = format!(
        "SELECT
            COALESCE(SUM(issues_closed), 0) as total_issues,
            COALESCE(SUM(prs_merged), 0) as total_prs,
            AVG(avg_cycle_time_hours) as avg_cycle,
            COALESCE(SUM(total_cost_usd), 0) as total_cost,
            COUNT(*) as days_with_data
         FROM velocity_snapshots
         WHERE snapshot_date >= DATE('now', '-{days} days')"
    );

    let (total_issues, total_prs, avg_cycle, total_cost, days_with_data): (
        i32,
        i32,
        Option<f64>,
        f64,
        i32,
    ) = conn
        .query_row(&query, [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
        })
        .unwrap_or((0, 0, None, 0.0, 0));

    // Calculate daily averages
    let divisor = if days_with_data > 0 {
        f64::from(days_with_data)
    } else {
        1.0
    };

    Ok(RollingAverage {
        period_days: days,
        avg_issues_per_day: f64::from(total_issues) / divisor,
        avg_prs_per_day: f64::from(total_prs) / divisor,
        avg_cycle_time_hours: avg_cycle,
        avg_cost_per_day: total_cost / divisor,
    })
}

/// Backfill historical velocity data from existing activity records
#[tauri::command]
pub fn backfill_velocity_history(workspace_path: &str, days: Option<i32>) -> Result<i32, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let days_val = days.unwrap_or(30);
    let mut snapshots_created = 0;

    // Get distinct dates from prompt_github events
    let dates: Vec<String> = conn
        .prepare(
            "SELECT DISTINCT DATE(event_time) as date
             FROM prompt_github
             WHERE DATE(event_time) >= DATE('now', '-' || ?1 || ' days')
             ORDER BY date",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?
        .query_map([days_val], |row| row.get(0))
        .map_err(|e| format!("Failed to query dates: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect dates: {e}"))?;

    for date in dates {
        // Get issues closed on this date
        let issues_closed: i32 = conn
            .query_row(
                "SELECT COUNT(DISTINCT issue_number)
                 FROM prompt_github
                 WHERE event_type = 'issue_closed'
                   AND DATE(event_time) = ?1",
                [&date],
                |row| row.get(0),
            )
            .unwrap_or(0);

        // Get PRs merged on this date
        let prs_merged: i32 = conn
            .query_row(
                "SELECT COUNT(DISTINCT pr_number)
                 FROM prompt_github
                 WHERE event_type = 'pr_merged'
                   AND DATE(event_time) = ?1",
                [&date],
                |row| row.get(0),
            )
            .unwrap_or(0);

        // Calculate average cycle time for PRs merged on this date
        let avg_cycle_time: Option<f64> = conn
            .query_row(
                "SELECT AVG(
                    (julianday(merged.event_time) - julianday(created.event_time)) * 24
                 )
                 FROM prompt_github merged
                 INNER JOIN prompt_github created ON merged.pr_number = created.pr_number
                 WHERE merged.event_type = 'pr_merged'
                   AND created.event_type = 'pr_created'
                   AND DATE(merged.event_time) = ?1",
                [&date],
                |row| row.get(0),
            )
            .ok();

        // Get total prompts on this date
        let total_prompts: i32 = conn
            .query_row(
                "SELECT COUNT(*)
                 FROM agent_activity
                 WHERE DATE(timestamp) = ?1",
                [&date],
                |row| row.get(0),
            )
            .unwrap_or(0);

        // Get total cost on this date
        let (prompt_tokens, completion_tokens): (i64, i64) = conn
            .query_row(
                "SELECT COALESCE(SUM(t.prompt_tokens), 0), COALESCE(SUM(t.completion_tokens), 0)
                 FROM agent_activity a
                 JOIN token_usage t ON a.id = t.activity_id
                 WHERE DATE(a.timestamp) = ?1",
                [&date],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap_or((0, 0));

        let total_cost_usd = calculate_cost(prompt_tokens, completion_tokens);

        // Insert snapshot if there's any activity
        if issues_closed > 0 || prs_merged > 0 || total_prompts > 0 {
            conn.execute(
                "INSERT INTO velocity_snapshots (snapshot_date, issues_closed, prs_merged, avg_cycle_time_hours, total_prompts, total_cost_usd)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(snapshot_date) DO UPDATE SET
                     issues_closed = excluded.issues_closed,
                     prs_merged = excluded.prs_merged,
                     avg_cycle_time_hours = excluded.avg_cycle_time_hours,
                     total_prompts = excluded.total_prompts,
                     total_cost_usd = excluded.total_cost_usd",
                params![
                    &date,
                    issues_closed,
                    prs_merged,
                    avg_cycle_time,
                    total_prompts,
                    total_cost_usd
                ],
            )
            .map_err(|e| format!("Failed to save velocity snapshot: {e}"))?;

            snapshots_created += 1;
        }
    }

    Ok(snapshots_created)
}

/// Velocity trend data point for charting
#[derive(Debug, Serialize, Deserialize)]
pub struct VelocityTrendPoint {
    pub date: String,
    pub issues_closed: i32,
    pub issues_closed_7day_avg: f64,
    pub prs_merged: i32,
    pub prs_merged_7day_avg: f64,
    pub cycle_time_hours: Option<f64>,
    pub cycle_time_7day_avg: Option<f64>,
}

/// Get velocity trend data with 7-day rolling averages
#[tauri::command]
pub fn get_velocity_trends(
    workspace_path: &str,
    days: Option<i32>,
) -> Result<Vec<VelocityTrendPoint>, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let days_val = days.unwrap_or(30);

    // Query using window functions for rolling averages
    let query = format!(
        "SELECT
            snapshot_date,
            issues_closed,
            AVG(issues_closed) OVER (ORDER BY snapshot_date ROWS BETWEEN 6 PRECEDING AND CURRENT ROW) as issues_7day_avg,
            prs_merged,
            AVG(prs_merged) OVER (ORDER BY snapshot_date ROWS BETWEEN 6 PRECEDING AND CURRENT ROW) as prs_7day_avg,
            avg_cycle_time_hours,
            AVG(avg_cycle_time_hours) OVER (ORDER BY snapshot_date ROWS BETWEEN 6 PRECEDING AND CURRENT ROW) as cycle_7day_avg
         FROM velocity_snapshots
         WHERE snapshot_date >= DATE('now', '-{days_val} days')
         ORDER BY snapshot_date DESC"
    );

    let mut stmt = conn
        .prepare(&query)
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let trends = stmt
        .query_map([], |row| {
            Ok(VelocityTrendPoint {
                date: row.get(0)?,
                issues_closed: row.get(1)?,
                issues_closed_7day_avg: row.get(2)?,
                prs_merged: row.get(3)?,
                prs_merged_7day_avg: row.get(4)?,
                cycle_time_hours: row.get(5)?,
                cycle_time_7day_avg: row.get(6)?,
            })
        })
        .map_err(|e| format!("Failed to query trends: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect trends: {e}"))?;

    Ok(trends)
}

/// Compare two time periods for velocity metrics
#[derive(Debug, Serialize, Deserialize)]
pub struct PeriodComparison {
    pub period1_label: String,
    pub period2_label: String,
    pub period1_issues: i32,
    pub period2_issues: i32,
    pub issues_change_pct: f64,
    pub period1_prs: i32,
    pub period2_prs: i32,
    pub prs_change_pct: f64,
    pub period1_cycle_time: Option<f64>,
    pub period2_cycle_time: Option<f64>,
    pub cycle_time_change_pct: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- calculate_trend tests ----

    #[test]
    fn test_trend_stable_within_10_percent() {
        assert_eq!(calculate_trend(105.0, 100.0, false), TrendDirection::Stable);
        assert_eq!(calculate_trend(95.0, 100.0, false), TrendDirection::Stable);
    }

    #[test]
    fn test_trend_improving_higher_is_better() {
        // current > previous by > 10% and higher is better -> Improving
        assert_eq!(calculate_trend(120.0, 100.0, false), TrendDirection::Improving);
    }

    #[test]
    fn test_trend_declining_higher_is_better() {
        // current < previous by > 10% and higher is better -> Declining
        assert_eq!(calculate_trend(80.0, 100.0, false), TrendDirection::Declining);
    }

    #[test]
    fn test_trend_improving_lower_is_better() {
        // current < previous by > 10% and lower is better -> Improving
        assert_eq!(calculate_trend(80.0, 100.0, true), TrendDirection::Improving);
    }

    #[test]
    fn test_trend_declining_lower_is_better() {
        // current > previous by > 10% and lower is better -> Declining
        assert_eq!(calculate_trend(120.0, 100.0, true), TrendDirection::Declining);
    }

    #[test]
    fn test_trend_previous_zero_current_positive_higher_is_better() {
        assert_eq!(calculate_trend(10.0, 0.0, false), TrendDirection::Improving);
    }

    #[test]
    fn test_trend_previous_zero_current_positive_lower_is_better() {
        assert_eq!(calculate_trend(10.0, 0.0, true), TrendDirection::Declining);
    }

    #[test]
    fn test_trend_both_zero() {
        assert_eq!(calculate_trend(0.0, 0.0, false), TrendDirection::Stable);
        assert_eq!(calculate_trend(0.0, 0.0, true), TrendDirection::Stable);
    }

    #[test]
    fn test_trend_boundary_exactly_10_percent() {
        // Exactly 10% change is NOT stable (implementation uses strict < 0.1)
        assert_eq!(calculate_trend(110.0, 100.0, false), TrendDirection::Improving);
        assert_eq!(calculate_trend(90.0, 100.0, false), TrendDirection::Declining);
    }

    #[test]
    fn test_trend_just_over_10_percent() {
        // Just over 10% should not be stable
        assert_eq!(calculate_trend(111.0, 100.0, false), TrendDirection::Improving);
        assert_eq!(calculate_trend(89.0, 100.0, false), TrendDirection::Declining);
    }

    // ---- TrendDirection Display ----

    #[test]
    fn test_trend_direction_display() {
        assert_eq!(format!("{}", TrendDirection::Improving), "improving");
        assert_eq!(format!("{}", TrendDirection::Declining), "declining");
        assert_eq!(format!("{}", TrendDirection::Stable), "stable");
    }
}

/// Compare velocity between two time periods
#[tauri::command]
pub fn compare_velocity_periods(
    workspace_path: &str,
    period1_start: &str,
    period1_end: &str,
    period2_start: &str,
    period2_end: &str,
) -> Result<PeriodComparison, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    // Period 1 metrics
    let (period1_issues, period1_prs, period1_cycle_time): (i32, i32, Option<f64>) = conn
        .query_row(
            "SELECT
                COALESCE(SUM(issues_closed), 0),
                COALESCE(SUM(prs_merged), 0),
                AVG(avg_cycle_time_hours)
             FROM velocity_snapshots
             WHERE snapshot_date >= ?1 AND snapshot_date <= ?2",
            params![period1_start, period1_end],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap_or((0, 0, None));

    // Period 2 metrics
    let (period2_issues, period2_prs, period2_cycle_time): (i32, i32, Option<f64>) = conn
        .query_row(
            "SELECT
                COALESCE(SUM(issues_closed), 0),
                COALESCE(SUM(prs_merged), 0),
                AVG(avg_cycle_time_hours)
             FROM velocity_snapshots
             WHERE snapshot_date >= ?1 AND snapshot_date <= ?2",
            params![period2_start, period2_end],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap_or((0, 0, None));

    // Calculate percentage changes
    let issues_change_pct = if period1_issues > 0 {
        ((f64::from(period2_issues) - f64::from(period1_issues)) / f64::from(period1_issues))
            * 100.0
    } else if period2_issues > 0 {
        100.0
    } else {
        0.0
    };

    let prs_change_pct = if period1_prs > 0 {
        ((f64::from(period2_prs) - f64::from(period1_prs)) / f64::from(period1_prs)) * 100.0
    } else if period2_prs > 0 {
        100.0
    } else {
        0.0
    };

    let cycle_time_change_pct = match (period1_cycle_time, period2_cycle_time) {
        (Some(p1), Some(p2)) if p1 > 0.0 => Some(((p2 - p1) / p1) * 100.0),
        _ => None,
    };

    Ok(PeriodComparison {
        period1_label: format!("{period1_start} to {period1_end}"),
        period2_label: format!("{period2_start} to {period2_end}"),
        period1_issues,
        period2_issues,
        issues_change_pct,
        period1_prs,
        period2_prs,
        prs_change_pct,
        period1_cycle_time,
        period2_cycle_time,
        cycle_time_change_pct,
    })
}
