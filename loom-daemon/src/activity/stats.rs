//! Statistics and metrics views for agent effectiveness analysis.
//!
//! This module provides SQL views and query functions for aggregating
//! correlation data into actionable metrics:
//! - Agent effectiveness by role
//! - Cost efficiency comparisons
//! - Success rate trends
//! - Velocity metrics
//!
//! # Views
//!
//! The following SQL views are created:
//! - `agent_effectiveness` - Summary metrics per agent role
//! - `cost_per_issue` - Cost and effort breakdown per GitHub issue
//! - `daily_velocity` - Daily productivity metrics
//!
//! # Example
//!
//! ```ignore
//! use activity::stats::{AgentEffectiveness, CostPerIssue, DailyVelocity};
//!
//! let db = ActivityDb::new("activity.db".into())?;
//!
//! // Get agent effectiveness metrics
//! let effectiveness = db.get_agent_effectiveness(None)?;
//! for agent in effectiveness {
//!     println!("{}: {}% success rate", agent.agent_role, agent.success_rate);
//! }
//!
//! // Get cost breakdown for issue #42
//! let cost = db.get_cost_per_issue(Some(42))?;
//! ```

use chrono::{DateTime, NaiveDate, Utc};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

/// Agent effectiveness metrics aggregated by role.
///
/// Provides summary statistics for each agent role including
/// success rates, average costs, and average duration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEffectiveness {
    /// Agent role (e.g., "builder", "judge", "curator")
    pub agent_role: String,
    /// Total number of prompts/tasks for this role
    pub total_prompts: i64,
    /// Number of prompts that resulted in successful outcomes
    pub successful_prompts: i64,
    /// Success rate as a percentage (0.0 - 100.0)
    pub success_rate: f64,
    /// Average cost per prompt in USD
    pub avg_cost: f64,
    /// Average duration per prompt in seconds
    pub avg_duration_sec: f64,
}

/// Cost and effort breakdown per GitHub issue.
///
/// Aggregates all prompts associated with a specific issue
/// to track total cost, token usage, and time spent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostPerIssue {
    /// GitHub issue number
    pub issue_number: i32,
    /// Number of prompts used for this issue
    pub prompt_count: i64,
    /// Total cost in USD
    pub total_cost: f64,
    /// Total tokens used (input + output)
    pub total_tokens: i64,
    /// Timestamp when work started on this issue
    pub started: Option<DateTime<Utc>>,
    /// Timestamp when work was completed
    pub completed: Option<DateTime<Utc>>,
}

/// Daily velocity metrics for tracking productivity trends.
///
/// Aggregates activity by day to show development velocity,
/// including prompt counts, issues touched, and PRs created.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct DailyVelocity {
    /// Date of the activity
    pub date: NaiveDate,
    /// Number of prompts on this day
    pub prompts: i64,
    /// Number of unique issues touched
    pub issues_touched: i64,
    /// Number of PRs created
    pub prs_created: i64,
    /// Total cost for the day in USD
    pub daily_cost: f64,
}

/// Weekly velocity summary for trend analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeeklyVelocity {
    /// Week identifier (YYYY-WNN format)
    pub week: String,
    /// Total prompts for the week
    pub prompts: i64,
    /// Total cost for the week in USD
    pub cost: f64,
}

/// Overall stats summary for quick dashboard display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsSummary {
    /// Total number of prompts recorded
    pub total_prompts: i64,
    /// Total cost in USD across all activity
    pub total_cost: f64,
    /// Total tokens used
    pub total_tokens: i64,
    /// Number of unique issues worked on
    pub issues_count: i64,
    /// Number of unique PRs created
    pub prs_count: i64,
    /// Average success rate across all roles
    pub avg_success_rate: f64,
}

/// Create the SQL views for agent effectiveness metrics.
///
/// This function creates the following views:
/// - `agent_effectiveness` - Aggregates metrics by agent role
/// - `cost_per_issue` - Aggregates costs by GitHub issue
/// - `daily_velocity` - Aggregates activity by day
///
/// Views use LEFT JOINs to handle cases where correlation tables
/// may not have data for all prompts (e.g., prompts without GitHub links).
pub fn create_stats_views(conn: &Connection) -> rusqlite::Result<()> {
    // Agent effectiveness view
    // Joins agent_inputs with quality_metrics and resource_usage to compute
    // success rates, average costs, and durations per agent role.
    conn.execute_batch(
        r"
        CREATE VIEW IF NOT EXISTS agent_effectiveness AS
        SELECT
            COALESCE(i.agent_role, 'unknown') as agent_role,
            COUNT(*) as total_prompts,
            SUM(CASE WHEN q.tests_passed > 0 AND (q.tests_failed IS NULL OR q.tests_failed = 0) THEN 1 ELSE 0 END) as successful_prompts,
            ROUND(100.0 * SUM(CASE WHEN q.tests_passed > 0 AND (q.tests_failed IS NULL OR q.tests_failed = 0) THEN 1 ELSE 0 END) / NULLIF(COUNT(*), 0), 1) as success_rate,
            ROUND(COALESCE(AVG(r.cost_usd), 0.0), 4) as avg_cost,
            ROUND(COALESCE(AVG(r.duration_ms / 1000.0), 0.0), 1) as avg_duration_sec
        FROM agent_inputs i
        LEFT JOIN quality_metrics q ON i.id = q.input_id
        LEFT JOIN resource_usage r ON i.id = r.input_id
        GROUP BY COALESCE(i.agent_role, 'unknown');
        ",
    )?;

    // Cost per issue view
    // Aggregates all prompts linked to GitHub issues via prompt_github table
    // to calculate total cost, tokens, and time span per issue.
    conn.execute_batch(
        r"
        CREATE VIEW IF NOT EXISTS cost_per_issue AS
        SELECT
            pg.issue_number,
            COUNT(DISTINCT i.id) as prompt_count,
            COALESCE(SUM(r.cost_usd), 0.0) as total_cost,
            COALESCE(SUM(r.tokens_input + r.tokens_output), 0) as total_tokens,
            MIN(i.timestamp) as started,
            MAX(i.timestamp) as completed
        FROM prompt_github pg
        JOIN agent_inputs i ON pg.input_id = i.id
        LEFT JOIN resource_usage r ON i.id = r.input_id
        WHERE pg.issue_number IS NOT NULL
        GROUP BY pg.issue_number;
        ",
    )?;

    // Daily velocity view
    // Aggregates activity by day for trend analysis including
    // prompt counts, issues touched, PRs created, and daily cost.
    conn.execute_batch(
        r"
        CREATE VIEW IF NOT EXISTS daily_velocity AS
        SELECT
            DATE(i.timestamp) as date,
            COUNT(*) as prompts,
            COUNT(DISTINCT pg.issue_number) as issues_touched,
            COUNT(DISTINCT CASE WHEN pg.event_type = 'pr_created' THEN pg.pr_number END) as prs_created,
            COALESCE(SUM(r.cost_usd), 0.0) as daily_cost
        FROM agent_inputs i
        LEFT JOIN prompt_github pg ON i.id = pg.input_id
        LEFT JOIN resource_usage r ON i.id = r.input_id
        GROUP BY DATE(i.timestamp);
        ",
    )?;

    Ok(())
}

/// Query methods for stats views.
///
/// These methods are implemented on `ActivityDb` via extension trait
/// to keep the stats functionality modular.
pub trait StatsQueries {
    /// Get agent effectiveness metrics, optionally filtered by role.
    ///
    /// # Arguments
    /// * `role` - Optional role filter (e.g., "builder", "judge")
    ///
    /// # Returns
    /// Vector of `AgentEffectiveness` records ordered by success rate descending.
    fn get_agent_effectiveness(
        &self,
        role: Option<&str>,
    ) -> rusqlite::Result<Vec<AgentEffectiveness>>;

    /// Get cost breakdown per issue, optionally filtered by issue number.
    ///
    /// # Arguments
    /// * `issue_number` - Optional issue number filter
    ///
    /// # Returns
    /// Vector of `CostPerIssue` records ordered by total cost descending.
    fn get_cost_per_issue(&self, issue_number: Option<i32>) -> rusqlite::Result<Vec<CostPerIssue>>;

    /// Get daily velocity metrics for a date range.
    ///
    /// # Arguments
    /// * `start_date` - Optional start date filter (inclusive)
    /// * `end_date` - Optional end date filter (inclusive)
    ///
    /// # Returns
    /// Vector of `DailyVelocity` records ordered by date ascending.
    #[allow(dead_code)]
    fn get_daily_velocity(
        &self,
        start_date: Option<NaiveDate>,
        end_date: Option<NaiveDate>,
    ) -> rusqlite::Result<Vec<DailyVelocity>>;

    /// Get weekly velocity summary for trend analysis.
    ///
    /// # Returns
    /// Vector of `WeeklyVelocity` records ordered by week ascending.
    fn get_weekly_velocity(&self) -> rusqlite::Result<Vec<WeeklyVelocity>>;

    /// Get overall stats summary.
    ///
    /// # Returns
    /// A `StatsSummary` with aggregate metrics.
    fn get_stats_summary(&self) -> rusqlite::Result<StatsSummary>;
}

/// Implementation of stats queries for any type that provides a Connection reference.
pub fn query_agent_effectiveness(
    conn: &Connection,
    role: Option<&str>,
) -> rusqlite::Result<Vec<AgentEffectiveness>> {
    // Ensure views exist
    create_stats_views(conn)?;

    let sql = if role.is_some() {
        r"SELECT agent_role, total_prompts, successful_prompts, success_rate, avg_cost, avg_duration_sec
          FROM agent_effectiveness
          WHERE agent_role = ?1
          ORDER BY success_rate DESC"
    } else {
        r"SELECT agent_role, total_prompts, successful_prompts, success_rate, avg_cost, avg_duration_sec
          FROM agent_effectiveness
          ORDER BY success_rate DESC"
    };

    let mut stmt = conn.prepare(sql)?;

    let rows = if let Some(r) = role {
        stmt.query_map([r], map_agent_effectiveness)?
    } else {
        stmt.query_map([], map_agent_effectiveness)?
    };

    rows.collect()
}

fn map_agent_effectiveness(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentEffectiveness> {
    Ok(AgentEffectiveness {
        agent_role: row.get(0)?,
        total_prompts: row.get(1)?,
        successful_prompts: row.get(2)?,
        success_rate: row.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
        avg_cost: row.get::<_, Option<f64>>(4)?.unwrap_or(0.0),
        avg_duration_sec: row.get::<_, Option<f64>>(5)?.unwrap_or(0.0),
    })
}

pub fn query_cost_per_issue(
    conn: &Connection,
    issue_number: Option<i32>,
) -> rusqlite::Result<Vec<CostPerIssue>> {
    // Ensure views exist
    create_stats_views(conn)?;

    let sql = if issue_number.is_some() {
        r"SELECT issue_number, prompt_count, total_cost, total_tokens, started, completed
          FROM cost_per_issue
          WHERE issue_number = ?1
          ORDER BY total_cost DESC"
    } else {
        r"SELECT issue_number, prompt_count, total_cost, total_tokens, started, completed
          FROM cost_per_issue
          ORDER BY total_cost DESC
          LIMIT 100"
    };

    let mut stmt = conn.prepare(sql)?;

    let rows = if let Some(num) = issue_number {
        stmt.query_map([num], map_cost_per_issue)?
    } else {
        stmt.query_map([], map_cost_per_issue)?
    };

    rows.collect()
}

fn map_cost_per_issue(row: &rusqlite::Row<'_>) -> rusqlite::Result<CostPerIssue> {
    let started_str: Option<String> = row.get(4)?;
    let completed_str: Option<String> = row.get(5)?;

    let started = started_str.and_then(|s| {
        DateTime::parse_from_rfc3339(&s)
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
    });

    let completed = completed_str.and_then(|s| {
        DateTime::parse_from_rfc3339(&s)
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
    });

    Ok(CostPerIssue {
        issue_number: row.get(0)?,
        prompt_count: row.get(1)?,
        total_cost: row.get::<_, Option<f64>>(2)?.unwrap_or(0.0),
        total_tokens: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
        started,
        completed,
    })
}

#[allow(dead_code)]
pub fn query_daily_velocity(
    conn: &Connection,
    start_date: Option<NaiveDate>,
    end_date: Option<NaiveDate>,
) -> rusqlite::Result<Vec<DailyVelocity>> {
    // Ensure views exist
    create_stats_views(conn)?;

    let (sql, params): (&str, Vec<String>) = match (start_date, end_date) {
        (Some(start), Some(end)) => (
            r"SELECT date, prompts, issues_touched, prs_created, daily_cost
              FROM daily_velocity
              WHERE date >= ?1 AND date <= ?2
              ORDER BY date ASC",
            vec![start.to_string(), end.to_string()],
        ),
        (Some(start), None) => (
            r"SELECT date, prompts, issues_touched, prs_created, daily_cost
              FROM daily_velocity
              WHERE date >= ?1
              ORDER BY date ASC",
            vec![start.to_string()],
        ),
        (None, Some(end)) => (
            r"SELECT date, prompts, issues_touched, prs_created, daily_cost
              FROM daily_velocity
              WHERE date <= ?1
              ORDER BY date ASC",
            vec![end.to_string()],
        ),
        (None, None) => (
            r"SELECT date, prompts, issues_touched, prs_created, daily_cost
              FROM daily_velocity
              ORDER BY date ASC
              LIMIT 90",
            vec![],
        ),
    };

    let mut stmt = conn.prepare(sql)?;

    let rows = match params.len() {
        0 => stmt.query_map([], map_daily_velocity)?,
        1 => stmt.query_map([&params[0]], map_daily_velocity)?,
        2 => stmt.query_map([&params[0], &params[1]], map_daily_velocity)?,
        _ => unreachable!(),
    };

    rows.collect()
}

fn map_daily_velocity(row: &rusqlite::Row<'_>) -> rusqlite::Result<DailyVelocity> {
    let date_str: String = row.get(0)?;
    let date = NaiveDate::parse_from_str(&date_str, "%Y-%m-%d")
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;

    Ok(DailyVelocity {
        date,
        prompts: row.get(1)?,
        issues_touched: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
        prs_created: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
        daily_cost: row.get::<_, Option<f64>>(4)?.unwrap_or(0.0),
    })
}

pub fn query_weekly_velocity(conn: &Connection) -> rusqlite::Result<Vec<WeeklyVelocity>> {
    // Ensure views exist
    create_stats_views(conn)?;

    let mut stmt = conn.prepare(
        r"SELECT
            strftime('%Y-W%W', date) as week,
            SUM(prompts) as prompts,
            SUM(daily_cost) as cost
          FROM daily_velocity
          GROUP BY week
          ORDER BY week ASC",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(WeeklyVelocity {
            week: row.get(0)?,
            prompts: row.get(1)?,
            cost: row.get::<_, Option<f64>>(2)?.unwrap_or(0.0),
        })
    })?;

    rows.collect()
}

pub fn query_stats_summary(conn: &Connection) -> rusqlite::Result<StatsSummary> {
    // Ensure views exist
    create_stats_views(conn)?;

    // Get total prompts and cost from agent_inputs + resource_usage
    let (total_prompts, total_cost, total_tokens): (i64, f64, i64) = conn.query_row(
        r"SELECT
            COUNT(*),
            COALESCE(SUM(r.cost_usd), 0.0),
            COALESCE(SUM(r.tokens_input + r.tokens_output), 0)
          FROM agent_inputs i
          LEFT JOIN resource_usage r ON i.id = r.input_id",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;

    // Get unique issues and PRs
    let (issues_count, prs_count): (i64, i64) = conn.query_row(
        r"SELECT
            COUNT(DISTINCT issue_number),
            COUNT(DISTINCT pr_number)
          FROM prompt_github
          WHERE issue_number IS NOT NULL OR pr_number IS NOT NULL",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    // Get average success rate from agent_effectiveness view
    let avg_success_rate: f64 = conn
        .query_row(r"SELECT COALESCE(AVG(success_rate), 0.0) FROM agent_effectiveness", [], |row| {
            row.get(0)
        })
        .unwrap_or(0.0);

    Ok(StatsSummary {
        total_prompts,
        total_cost,
        total_tokens,
        issues_count,
        prs_count,
        avg_success_rate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::schema::init_schema;
    use tempfile::tempdir;

    fn setup_test_db() -> rusqlite::Result<(Connection, tempfile::TempDir)> {
        let temp_dir = tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");
        let conn = Connection::open(&db_path)?;
        init_schema(&conn)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.to_string().into()))?;
        // Return both connection and tempdir to keep tempdir alive
        Ok((conn, temp_dir))
    }

    #[test]
    fn test_create_stats_views() -> rusqlite::Result<()> {
        let (conn, _temp_dir) = setup_test_db()?;
        create_stats_views(&conn)?;

        // Verify views exist by querying them
        let _: Vec<AgentEffectiveness> = query_agent_effectiveness(&conn, None)?;
        let _: Vec<CostPerIssue> = query_cost_per_issue(&conn, None)?;
        let _: Vec<DailyVelocity> = query_daily_velocity(&conn, None, None)?;

        Ok(())
    }

    #[test]
    fn test_empty_database_stats() -> rusqlite::Result<()> {
        let (conn, _temp_dir) = setup_test_db()?;

        let effectiveness = query_agent_effectiveness(&conn, None)?;
        assert!(effectiveness.is_empty());

        let cost_per_issue = query_cost_per_issue(&conn, None)?;
        assert!(cost_per_issue.is_empty());

        let daily_velocity = query_daily_velocity(&conn, None, None)?;
        assert!(daily_velocity.is_empty());

        let summary = query_stats_summary(&conn)?;
        assert_eq!(summary.total_prompts, 0);
        assert_eq!(summary.total_cost, 0.0);

        Ok(())
    }

    #[test]
    fn test_agent_effectiveness_with_data() -> rusqlite::Result<()> {
        let (conn, _temp_dir) = setup_test_db()?;

        // Insert test data
        conn.execute(
            r"INSERT INTO agent_inputs (terminal_id, timestamp, input_type, content, agent_role, context)
              VALUES ('t1', datetime('now'), 'manual', 'test', 'builder', '{}')",
            [],
        )?;

        let input_id: i64 = conn.last_insert_rowid();

        // Add quality metrics (successful)
        conn.execute(
            r"INSERT INTO quality_metrics (input_id, timestamp, tests_passed, tests_failed)
              VALUES (?1, datetime('now'), 10, 0)",
            [input_id],
        )?;

        // Add resource usage
        conn.execute(
            r"INSERT INTO resource_usage (input_id, timestamp, model, tokens_input, tokens_output, cost_usd, duration_ms, provider)
              VALUES (?1, datetime('now'), 'claude-sonnet', 1000, 500, 0.0105, 1500, 'anthropic')",
            [input_id],
        )?;

        let effectiveness = query_agent_effectiveness(&conn, Some("builder"))?;
        assert_eq!(effectiveness.len(), 1);
        assert_eq!(effectiveness[0].agent_role, "builder");
        assert_eq!(effectiveness[0].total_prompts, 1);
        assert_eq!(effectiveness[0].successful_prompts, 1);
        assert_eq!(effectiveness[0].success_rate, 100.0);

        Ok(())
    }

    #[test]
    fn test_cost_per_issue_with_data() -> rusqlite::Result<()> {
        let (conn, _temp_dir) = setup_test_db()?;

        // Insert test data
        conn.execute(
            r"INSERT INTO agent_inputs (terminal_id, timestamp, input_type, content, agent_role, context)
              VALUES ('t1', datetime('now'), 'manual', 'test', 'builder', '{}')",
            [],
        )?;

        let input_id: i64 = conn.last_insert_rowid();

        // Link to GitHub issue
        conn.execute(
            r"INSERT INTO prompt_github (input_id, issue_number, event_type)
              VALUES (?1, 42, 'label_added')",
            [input_id],
        )?;

        // Add resource usage
        conn.execute(
            r"INSERT INTO resource_usage (input_id, timestamp, model, tokens_input, tokens_output, cost_usd, provider)
              VALUES (?1, datetime('now'), 'claude-sonnet', 1000, 500, 0.0105, 'anthropic')",
            [input_id],
        )?;

        let cost = query_cost_per_issue(&conn, Some(42))?;
        assert_eq!(cost.len(), 1);
        assert_eq!(cost[0].issue_number, 42);
        assert_eq!(cost[0].prompt_count, 1);
        assert!((cost[0].total_cost - 0.0105).abs() < 0.0001);

        Ok(())
    }

    #[test]
    fn test_stats_summary() -> rusqlite::Result<()> {
        let (conn, _temp_dir) = setup_test_db()?;

        // Insert test data
        for i in 0..3 {
            conn.execute(
                r"INSERT INTO agent_inputs (terminal_id, timestamp, input_type, content, agent_role, context)
                  VALUES ('t1', datetime('now'), 'manual', 'test', 'builder', '{}')",
                [],
            )?;

            let input_id: i64 = conn.last_insert_rowid();

            conn.execute(
                r"INSERT INTO resource_usage (input_id, timestamp, model, tokens_input, tokens_output, cost_usd, provider)
                  VALUES (?1, datetime('now'), 'claude-sonnet', 1000, 500, 0.01, 'anthropic')",
                [input_id],
            )?;

            conn.execute(
                r"INSERT INTO prompt_github (input_id, issue_number, event_type)
                  VALUES (?1, ?2, 'label_added')",
                rusqlite::params![input_id, 100 + i],
            )?;
        }

        let summary = query_stats_summary(&conn)?;
        assert_eq!(summary.total_prompts, 3);
        assert!((summary.total_cost - 0.03).abs() < 0.001);
        assert_eq!(summary.total_tokens, 4500); // 3 * (1000 + 500)
        assert_eq!(summary.issues_count, 3);

        Ok(())
    }

    #[test]
    fn test_weekly_velocity() -> rusqlite::Result<()> {
        let (conn, _temp_dir) = setup_test_db()?;

        // Insert data for multiple days
        conn.execute(
            r"INSERT INTO agent_inputs (terminal_id, timestamp, input_type, content, agent_role, context)
              VALUES ('t1', '2026-01-20T10:00:00Z', 'manual', 'test', 'builder', '{}')",
            [],
        )?;

        conn.execute(
            r"INSERT INTO agent_inputs (terminal_id, timestamp, input_type, content, agent_role, context)
              VALUES ('t1', '2026-01-21T10:00:00Z', 'manual', 'test', 'builder', '{}')",
            [],
        )?;

        let weekly = query_weekly_velocity(&conn)?;
        assert!(!weekly.is_empty());

        Ok(())
    }
}
