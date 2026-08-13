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
    /// `true` when at least one `resource_usage` row is joined for this role
    /// — i.e. `avg_cost`/`avg_duration_sec` reflect a real (possibly-zero)
    /// measurement rather than "never recorded". `stats`'s text renderer
    /// prints `n/a` for those two fields when this is `false` (#6128), since
    /// a bare `0`/`0.0%` is indistinguishable from a genuine failure.
    #[serde(default = "default_true")]
    pub cost_data_available: bool,
    /// `true` when at least one `quality_metrics` row is joined for this
    /// role — gates `success_rate` the same way `cost_data_available` gates
    /// the cost/duration fields (#6128).
    #[serde(default = "default_true")]
    pub quality_data_available: bool,
}

/// `serde(default)` helper — see [`AgentEffectiveness::cost_data_available`].
/// Defaults new deserializers (e.g. a JSON fixture written before #6128) to
/// `true` so pre-existing callers that never see this field keep the old
/// "trust the number" behavior instead of silently flipping to `n/a`.
fn default_true() -> bool {
    true
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
    /// `true` when at least one `resource_usage` row exists anywhere in the
    /// database — gates `total_cost`/`total_tokens` (#6128): distinguishes
    /// "no cost telemetry has ever been recorded" from "recorded, and it
    /// happens to be zero".
    #[serde(default = "default_true")]
    pub cost_data_available: bool,
    /// `true` when at least one `prompt_github` row exists — gates
    /// `issues_count`/`prs_count` the same way `cost_data_available` gates
    /// the cost fields (#6128).
    #[serde(default = "default_true")]
    pub github_data_available: bool,
    /// `true` when at least one `quality_metrics` row exists — gates
    /// `avg_success_rate` the same way (#6128).
    #[serde(default = "default_true")]
    pub quality_data_available: bool,
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
            ROUND(COALESCE(AVG(r.duration_ms / 1000.0), 0.0), 1) as avg_duration_sec,
            SUM(CASE WHEN r.input_id IS NOT NULL THEN 1 ELSE 0 END) as resource_rows,
            SUM(CASE WHEN q.input_id IS NOT NULL THEN 1 ELSE 0 END) as quality_rows
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

// ---------------------------------------------------------------------------
// `agent-metrics` CLI parity (epic #4081 Phase 3 family 4, issue #4274).
//
// The types and queries below are the native port of
// `loom_tools.agent_metrics` — the CLI contract `agent-metrics.sh` and
// `mcp__loom__get_agent_metrics` invoke. They intentionally do NOT reuse the
// `AgentEffectiveness`/`CostPerIssue` views above: those back the original
// `loom-daemon stats` dashboard and have no period filter or per-model
// dimension. Querying the base tables directly (mirroring the Python SQL)
// keeps both surfaces independently stable.
// ---------------------------------------------------------------------------

/// Summary metrics for the `agent-metrics summary` command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummaryMetrics {
    pub total_prompts: i64,
    pub total_tokens: i64,
    pub total_cost: f64,
    pub issues_count: i64,
    pub prs_count: i64,
    pub success_rate: f64,
}

/// Effectiveness row for the `agent-metrics effectiveness` command,
/// optionally grouped by model (`--by-model`, #3482).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectivenessRow {
    pub role: String,
    pub total_prompts: i64,
    pub successful_prompts: i64,
    pub success_rate: f64,
    pub avg_cost: f64,
    pub avg_duration_sec: f64,
    /// Populated only when queried with `by_model = true`; NULL/absent model
    /// values in the DB render as `"default"`. Omitted from JSON output
    /// (rather than emitted as `null`) when absent, matching the pre-#3482
    /// JSON shape for existing consumers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Cost row for the `agent-metrics costs` command, optionally grouped by
/// model (`--by-model`, #3482). `model` semantics match [`EffectivenessRow`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostRow {
    pub issue_number: i64,
    pub prompt_count: i64,
    pub total_cost: f64,
    pub total_tokens: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Weekly velocity row for the `agent-metrics velocity` command (last 8
/// weeks / 56 days).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VelocityRow {
    pub week: String,
    pub prompts: i64,
    pub issues: i64,
    pub prs_merged: i64,
    pub cost: f64,
}

/// SQL WHERE-clause fragment for a `--period` value. Unknown values behave
/// like `"all"` (no filter) — the CLI layer is responsible for rejecting
/// invalid `--period` values before calling into these queries.
fn period_sql_filter(period: &str) -> &'static str {
    match period {
        "today" => "AND i.timestamp >= datetime('now', 'start of day')",
        "week" => "AND i.timestamp >= datetime('now', '-7 days')",
        "month" => "AND i.timestamp >= datetime('now', '-30 days')",
        _ => "",
    }
}

/// SQL WHERE-clause fragment for a `role` filter. Quotes are doubled
/// defensively; the CLI layer additionally validates `role` against a fixed
/// allow-list before this is ever reached.
fn role_sql_filter(role: Option<&str>) -> String {
    match role {
        Some(r) => format!("AND i.agent_role = '{}'", r.replace('\'', "''")),
        None => String::new(),
    }
}

const MODEL_EXPR: &str = "COALESCE(NULLIF(r.model, ''), 'default')";

/// Query summary metrics (`agent_metrics.get_summary` parity).
pub fn query_summary_metrics(
    conn: &Connection,
    role: Option<&str>,
    period: &str,
) -> rusqlite::Result<SummaryMetrics> {
    let role_filter = role_sql_filter(role);
    let period_filter = period_sql_filter(period);

    let (total_prompts, total_tokens, total_cost, issues_count, prs_count): (
        i64,
        i64,
        f64,
        i64,
        i64,
    ) = conn.query_row(
        &format!(
            r"SELECT
                COUNT(*),
                COALESCE(SUM(r.tokens_input + r.tokens_output), 0),
                ROUND(COALESCE(SUM(r.cost_usd), 0), 4),
                COUNT(DISTINCT pg.issue_number),
                COUNT(DISTINCT pg.pr_number)
              FROM agent_inputs i
              LEFT JOIN resource_usage r ON i.id = r.input_id
              LEFT JOIN prompt_github pg ON i.id = pg.input_id
              WHERE 1=1 {role_filter} {period_filter}"
        ),
        [],
        |row| {
            Ok((
                row.get(0)?,
                row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                row.get::<_, Option<f64>>(2)?.unwrap_or(0.0),
                row.get(3)?,
                row.get(4)?,
            ))
        },
    )?;

    let success_rate: f64 = conn
        .query_row(
            &format!(
                r"SELECT
                    ROUND(
                        100.0 * SUM(
                            CASE WHEN q.tests_passed > 0
                                 AND (q.tests_failed IS NULL OR q.tests_failed = 0)
                            THEN 1 ELSE 0 END
                        ) / NULLIF(COUNT(*), 0),
                        1
                    )
                  FROM agent_inputs i
                  LEFT JOIN quality_metrics q ON i.id = q.input_id
                  WHERE 1=1 {role_filter} {period_filter}"
            ),
            [],
            |row| row.get::<_, Option<f64>>(0),
        )?
        .unwrap_or(0.0);

    Ok(SummaryMetrics {
        total_prompts,
        total_tokens,
        total_cost,
        issues_count,
        prs_count,
        success_rate,
    })
}

/// Query effectiveness rows (`agent_metrics.get_effectiveness` parity).
pub fn query_effectiveness_rows(
    conn: &Connection,
    role: Option<&str>,
    period: &str,
    by_model: bool,
) -> rusqlite::Result<Vec<EffectivenessRow>> {
    let role_filter = role_sql_filter(role);
    let period_filter = period_sql_filter(period);
    let model_select = if by_model {
        format!("{MODEL_EXPR} as model,")
    } else {
        String::new()
    };
    let model_group = if by_model {
        format!(", {MODEL_EXPR}")
    } else {
        String::new()
    };

    let sql = format!(
        r"SELECT
            COALESCE(i.agent_role, 'unknown') as role,
            {model_select}
            COUNT(*) as total_prompts,
            SUM(CASE WHEN q.tests_passed > 0 AND (q.tests_failed IS NULL OR q.tests_failed = 0) THEN 1 ELSE 0 END) as successful_prompts,
            ROUND(100.0 * SUM(CASE WHEN q.tests_passed > 0 AND (q.tests_failed IS NULL OR q.tests_failed = 0) THEN 1 ELSE 0 END) / NULLIF(COUNT(*), 0), 1) as success_rate,
            ROUND(COALESCE(AVG(r.cost_usd), 0), 4) as avg_cost,
            ROUND(COALESCE(AVG(r.duration_ms / 1000.0), 0), 1) as avg_duration_sec
          FROM agent_inputs i
          LEFT JOIN quality_metrics q ON i.id = q.input_id
          LEFT JOIN resource_usage r ON i.id = r.input_id
          WHERE 1=1 {role_filter} {period_filter}
          GROUP BY COALESCE(i.agent_role, 'unknown'){model_group}
          ORDER BY success_rate DESC"
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        Ok(EffectivenessRow {
            role: row.get("role")?,
            total_prompts: row.get("total_prompts")?,
            successful_prompts: row
                .get::<_, Option<i64>>("successful_prompts")?
                .unwrap_or(0),
            success_rate: row.get::<_, Option<f64>>("success_rate")?.unwrap_or(0.0),
            avg_cost: row.get::<_, Option<f64>>("avg_cost")?.unwrap_or(0.0),
            avg_duration_sec: row
                .get::<_, Option<f64>>("avg_duration_sec")?
                .unwrap_or(0.0),
            model: if by_model {
                Some(
                    row.get::<_, Option<String>>("model")?
                        .unwrap_or_else(|| "default".to_string()),
                )
            } else {
                None
            },
        })
    })?;

    rows.collect()
}

/// Query cost rows (`agent_metrics.get_costs` parity).
pub fn query_cost_rows(
    conn: &Connection,
    issue_number: Option<i32>,
    by_model: bool,
) -> rusqlite::Result<Vec<CostRow>> {
    let issue_filter = match issue_number {
        Some(n) => format!("WHERE pg.issue_number = {n}"),
        None => String::new(),
    };
    let model_select = if by_model {
        format!("{MODEL_EXPR} as model,")
    } else {
        String::new()
    };
    let model_group = if by_model {
        format!(", {MODEL_EXPR}")
    } else {
        String::new()
    };

    let sql = format!(
        r"SELECT
            pg.issue_number,
            {model_select}
            COUNT(DISTINCT i.id) as prompt_count,
            ROUND(COALESCE(SUM(r.cost_usd), 0), 4) as total_cost,
            COALESCE(SUM(r.tokens_input + r.tokens_output), 0) as total_tokens
          FROM prompt_github pg
          JOIN agent_inputs i ON pg.input_id = i.id
          LEFT JOIN resource_usage r ON i.id = r.input_id
          {issue_filter}
          GROUP BY pg.issue_number{model_group}
          ORDER BY total_cost DESC
          LIMIT 20"
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        let issue_number: Option<i64> = row.get("issue_number")?;
        Ok((
            issue_number,
            CostRow {
                issue_number: issue_number.unwrap_or(0),
                prompt_count: row.get("prompt_count")?,
                total_cost: row.get::<_, Option<f64>>("total_cost")?.unwrap_or(0.0),
                total_tokens: row.get::<_, Option<i64>>("total_tokens")?.unwrap_or(0),
                model: if by_model {
                    Some(
                        row.get::<_, Option<String>>("model")?
                            .unwrap_or_else(|| "default".to_string()),
                    )
                } else {
                    None
                },
            },
        ))
    })?;

    let mut out = Vec::new();
    for r in rows {
        let (issue_number, row) = r?;
        if issue_number.is_some() {
            out.push(row);
        }
    }
    Ok(out)
}

/// Query velocity rows (`agent_metrics.get_velocity` parity): last 8 weeks
/// (56 days), including per-week issues touched and PRs merged.
pub fn query_velocity_rows(conn: &Connection) -> rusqlite::Result<Vec<VelocityRow>> {
    let mut stmt = conn.prepare(
        r"SELECT
            strftime('%Y-W%W', i.timestamp) as week,
            COUNT(*) as prompts,
            COUNT(DISTINCT pg.issue_number) as issues,
            COUNT(DISTINCT CASE WHEN pg.event_type = 'pr_merged' THEN pg.pr_number END) as prs_merged,
            ROUND(SUM(r.cost_usd), 2) as cost
          FROM agent_inputs i
          LEFT JOIN prompt_github pg ON i.id = pg.input_id
          LEFT JOIN resource_usage r ON i.id = r.input_id
          WHERE i.timestamp >= datetime('now', '-56 days')
          GROUP BY week
          ORDER BY week DESC
          LIMIT 8",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(VelocityRow {
            week: row.get("week")?,
            prompts: row.get("prompts")?,
            issues: row.get::<_, Option<i64>>("issues")?.unwrap_or(0),
            prs_merged: row.get::<_, Option<i64>>("prs_merged")?.unwrap_or(0),
            cost: row.get::<_, Option<f64>>("cost")?.unwrap_or(0.0),
        })
    })?;

    rows.collect()
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

    /// `agent-metrics summary` parity: role/period-filtered summary metrics
    /// (issue #4274).
    fn get_summary_metrics(
        &self,
        role: Option<&str>,
        period: &str,
    ) -> rusqlite::Result<SummaryMetrics>;

    /// `agent-metrics effectiveness` parity: role/period-filtered
    /// effectiveness rows, optionally grouped by model (issue #4274).
    fn get_effectiveness_rows(
        &self,
        role: Option<&str>,
        period: &str,
        by_model: bool,
    ) -> rusqlite::Result<Vec<EffectivenessRow>>;

    /// `agent-metrics costs` parity: cost rows, optionally filtered by issue
    /// and grouped by model (issue #4274).
    fn get_cost_rows(
        &self,
        issue_number: Option<i32>,
        by_model: bool,
    ) -> rusqlite::Result<Vec<CostRow>>;

    /// `agent-metrics velocity` parity: weekly velocity rows for the last 8
    /// weeks (issue #4274).
    fn get_velocity_rows(&self) -> rusqlite::Result<Vec<VelocityRow>>;
}

/// Implementation of stats queries for any type that provides a Connection reference.
pub fn query_agent_effectiveness(
    conn: &Connection,
    role: Option<&str>,
) -> rusqlite::Result<Vec<AgentEffectiveness>> {
    // Ensure views exist
    create_stats_views(conn)?;

    let sql = if role.is_some() {
        r"SELECT agent_role, total_prompts, successful_prompts, success_rate, avg_cost, avg_duration_sec, resource_rows, quality_rows
          FROM agent_effectiveness
          WHERE agent_role = ?1
          ORDER BY success_rate DESC"
    } else {
        r"SELECT agent_role, total_prompts, successful_prompts, success_rate, avg_cost, avg_duration_sec, resource_rows, quality_rows
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
    let resource_rows: i64 = row.get::<_, Option<i64>>(6)?.unwrap_or(0);
    let quality_rows: i64 = row.get::<_, Option<i64>>(7)?.unwrap_or(0);
    Ok(AgentEffectiveness {
        agent_role: row.get(0)?,
        total_prompts: row.get(1)?,
        successful_prompts: row.get(2)?,
        success_rate: row.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
        avg_cost: row.get::<_, Option<f64>>(4)?.unwrap_or(0.0),
        avg_duration_sec: row.get::<_, Option<f64>>(5)?.unwrap_or(0.0),
        cost_data_available: resource_rows > 0,
        quality_data_available: quality_rows > 0,
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

    // Availability gates (#6128): a bare 0/0.0% is indistinguishable from a
    // real "nothing happened" result, so callers need to know whether the
    // underlying table has ever recorded a row at all before trusting the
    // aggregate above as a real measurement rather than "never recorded".
    let cost_data_available: bool =
        conn.query_row("SELECT EXISTS(SELECT 1 FROM resource_usage)", [], |row| row.get(0))?;
    let github_data_available: bool =
        conn.query_row("SELECT EXISTS(SELECT 1 FROM prompt_github)", [], |row| row.get(0))?;
    let quality_data_available: bool =
        conn.query_row("SELECT EXISTS(SELECT 1 FROM quality_metrics)", [], |row| row.get(0))?;

    Ok(StatsSummary {
        total_prompts,
        total_cost,
        total_tokens,
        issues_count,
        prs_count,
        avg_success_rate,
        cost_data_available,
        github_data_available,
        quality_data_available,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::float_cmp)]
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
        // #6128: an empty database has recorded nothing, so every
        // availability gate must be false (renders `n/a`, not `0`).
        assert!(!summary.cost_data_available);
        assert!(!summary.github_data_available);
        assert!(!summary.quality_data_available);

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
        // #6128: both real quality_metrics and resource_usage rows exist for
        // this role, so the availability gates must be true.
        assert!(effectiveness[0].quality_data_available);
        assert!(effectiveness[0].cost_data_available);

        Ok(())
    }

    /// #6128: a role whose prompts were only ever recorded via `SendInput`
    /// (an `agent_inputs` row) and never correlated with a
    /// `GetTerminalOutput`-parsed `resource_usage`/`quality_metrics` row
    /// (the exact symptom from the issue report — 26 prompts, every other
    /// aggregate zero) must report its rate/cost/duration fields as
    /// unavailable, not as a real zero.
    #[test]
    fn test_agent_effectiveness_without_output_data_is_unavailable() -> rusqlite::Result<()> {
        let (conn, _temp_dir) = setup_test_db()?;

        conn.execute(
            r"INSERT INTO agent_inputs (terminal_id, timestamp, input_type, content, agent_role, context)
              VALUES ('t1', datetime('now'), 'manual', 'test', 'judge', '{}')",
            [],
        )?;

        let effectiveness = query_agent_effectiveness(&conn, Some("judge"))?;
        assert_eq!(effectiveness.len(), 1);
        assert_eq!(effectiveness[0].total_prompts, 1);
        assert!(!effectiveness[0].quality_data_available);
        assert!(!effectiveness[0].cost_data_available);
        // The underlying placeholder values are still 0.0 — callers must
        // gate on the availability flags above, not trust these directly.
        assert_eq!(effectiveness[0].success_rate, 0.0);
        assert_eq!(effectiveness[0].avg_cost, 0.0);

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
        // #6128: resource_usage and prompt_github rows were recorded above,
        // so those gates are true; no quality_metrics row was ever inserted
        // in this test, so that gate stays false.
        assert!(summary.cost_data_available);
        assert!(summary.github_data_available);
        assert!(!summary.quality_data_available);

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

    // -----------------------------------------------------------------
    // `agent-metrics` CLI parity tests (issue #4274)
    // -----------------------------------------------------------------

    fn insert_sample_input(
        conn: &Connection,
        role: &str,
        model: &str,
        cost: f64,
        tests_passed: i64,
        tests_failed: i64,
        issue_number: i64,
    ) -> rusqlite::Result<i64> {
        conn.execute(
            r"INSERT INTO agent_inputs (terminal_id, timestamp, input_type, content, agent_role, context)
              VALUES ('t1', datetime('now'), 'manual', 'test', ?1, '{}')",
            [role],
        )?;
        let input_id = conn.last_insert_rowid();

        conn.execute(
            r"INSERT INTO quality_metrics (input_id, timestamp, tests_passed, tests_failed)
              VALUES (?1, datetime('now'), ?2, ?3)",
            rusqlite::params![input_id, tests_passed, tests_failed],
        )?;

        conn.execute(
            r"INSERT INTO resource_usage (input_id, timestamp, model, tokens_input, tokens_output, cost_usd, duration_ms, provider)
              VALUES (?1, datetime('now'), ?2, 1000, 500, ?3, 1500, 'anthropic')",
            rusqlite::params![input_id, model, cost],
        )?;

        conn.execute(
            r"INSERT INTO prompt_github (input_id, issue_number, event_type)
              VALUES (?1, ?2, 'label_added')",
            rusqlite::params![input_id, issue_number],
        )?;

        Ok(input_id)
    }

    #[test]
    fn test_query_summary_metrics() -> rusqlite::Result<()> {
        let (conn, _temp_dir) = setup_test_db()?;
        insert_sample_input(&conn, "builder", "claude-sonnet", 0.05, 10, 0, 42)?;
        insert_sample_input(&conn, "judge", "claude-sonnet", 0.02, 0, 1, 43)?;

        let all = query_summary_metrics(&conn, None, "all")?;
        assert_eq!(all.total_prompts, 2);
        assert_eq!(all.issues_count, 2);
        assert!((all.total_cost - 0.07).abs() < 0.0001);
        assert!((all.success_rate - 50.0).abs() < 0.0001);

        let builder_only = query_summary_metrics(&conn, Some("builder"), "all")?;
        assert_eq!(builder_only.total_prompts, 1);
        assert!((builder_only.success_rate - 100.0).abs() < 0.0001);

        // "today" period filter should still include rows inserted "now".
        let today = query_summary_metrics(&conn, None, "today")?;
        assert_eq!(today.total_prompts, 2);

        Ok(())
    }

    #[test]
    fn test_query_effectiveness_rows_by_model() -> rusqlite::Result<()> {
        let (conn, _temp_dir) = setup_test_db()?;
        insert_sample_input(&conn, "builder", "claude-sonnet", 0.05, 10, 0, 42)?;
        insert_sample_input(&conn, "builder", "claude-opus", 0.20, 10, 0, 43)?;

        let unmodeled = query_effectiveness_rows(&conn, None, "all", false)?;
        assert_eq!(unmodeled.len(), 1);
        assert_eq!(unmodeled[0].total_prompts, 2);
        assert!(unmodeled[0].model.is_none());

        let by_model = query_effectiveness_rows(&conn, None, "all", true)?;
        assert_eq!(by_model.len(), 2);
        for row in &by_model {
            assert!(row.model.is_some());
        }

        Ok(())
    }

    #[test]
    fn test_query_cost_rows() -> rusqlite::Result<()> {
        let (conn, _temp_dir) = setup_test_db()?;
        insert_sample_input(&conn, "builder", "claude-sonnet", 0.05, 10, 0, 42)?;
        insert_sample_input(&conn, "builder", "claude-opus", 0.20, 10, 0, 42)?;
        insert_sample_input(&conn, "builder", "claude-sonnet", 0.01, 10, 0, 99)?;

        let all = query_cost_rows(&conn, None, false)?;
        assert_eq!(all.len(), 2);
        assert!(all[0].total_cost >= all[1].total_cost);

        let filtered = query_cost_rows(&conn, Some(42), false)?;
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].issue_number, 42);
        assert_eq!(filtered[0].prompt_count, 2);
        assert!((filtered[0].total_cost - 0.25).abs() < 0.0001);

        let by_model = query_cost_rows(&conn, Some(42), true)?;
        assert_eq!(by_model.len(), 2);

        Ok(())
    }

    #[test]
    fn test_query_velocity_rows() -> rusqlite::Result<()> {
        let (conn, _temp_dir) = setup_test_db()?;
        insert_sample_input(&conn, "builder", "claude-sonnet", 0.05, 10, 0, 42)?;

        let rows = query_velocity_rows(&conn)?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].prompts, 1);
        assert_eq!(rows[0].issues, 1);

        Ok(())
    }
}
