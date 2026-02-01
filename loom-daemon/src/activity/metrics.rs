//! Task-level agent metrics and productivity tracking.
//!
//! Extracted from `db.rs` — provides task lifecycle management (start, complete),
//! task quality metrics updates, token usage recording, and productivity summaries.

use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};

use super::models::{AgentMetric, ProductivitySummary, TokenUsage};

// ========================================================================
// Task Lifecycle
// ========================================================================

/// Start tracking a new task.
pub(super) fn start_task(conn: &Connection, metric: &AgentMetric) -> Result<i64> {
    let context_json = metric.context.as_deref();

    conn.execute(
        r"
        INSERT INTO agent_metrics (
            terminal_id, agent_role, agent_system, task_type,
            github_issue, github_pr, started_at, status, context
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
        ",
        params![
            &metric.terminal_id,
            &metric.agent_role,
            &metric.agent_system,
            &metric.task_type,
            metric.github_issue,
            metric.github_pr,
            metric.started_at.to_rfc3339(),
            &metric.status,
            context_json,
        ],
    )?;

    Ok(conn.last_insert_rowid())
}

/// Complete a task and calculate wall time.
pub(super) fn complete_task(
    conn: &Connection,
    metric_id: i64,
    status: &str,
    outcome_type: Option<&str>,
) -> Result<()> {
    let now = Utc::now();

    // Get started_at from database
    let started_at: String = conn.query_row(
        "SELECT started_at FROM agent_metrics WHERE id = ?1",
        params![metric_id],
        |row| row.get(0),
    )?;

    let started = DateTime::parse_from_rfc3339(&started_at)?.with_timezone(&Utc);
    let wall_time_seconds = (now - started).num_seconds();

    conn.execute(
        r"
        UPDATE agent_metrics
        SET completed_at = ?1,
            wall_time_seconds = ?2,
            status = ?3,
            outcome_type = ?4
        WHERE id = ?5
        ",
        params![
            now.to_rfc3339(),
            wall_time_seconds,
            status,
            outcome_type,
            metric_id
        ],
    )?;

    Ok(())
}

/// Update task quality metrics (commits, CI failures, etc.).
pub(super) fn update_task_metrics(
    conn: &Connection,
    metric_id: i64,
    test_failures: Option<i32>,
    ci_failures: Option<i32>,
    commits_count: Option<i32>,
    lines_changed: Option<i32>,
) -> Result<()> {
    conn.execute(
        r"
        UPDATE agent_metrics
        SET test_failures = COALESCE(?1, test_failures),
            ci_failures = COALESCE(?2, ci_failures),
            commits_count = COALESCE(?3, commits_count),
            lines_changed = COALESCE(?4, lines_changed)
        WHERE id = ?5
        ",
        params![
            test_failures,
            ci_failures,
            commits_count,
            lines_changed,
            metric_id
        ],
    )?;

    Ok(())
}

// ========================================================================
// Task Queries
// ========================================================================

/// Get metrics for a specific task.
pub(super) fn get_task_metrics(conn: &Connection, metric_id: i64) -> Result<AgentMetric> {
    Ok(conn.query_row(
        r"
        SELECT id, terminal_id, agent_role, agent_system, task_type,
               github_issue, github_pr, started_at, completed_at,
               wall_time_seconds, active_time_seconds, input_tokens,
               output_tokens, total_tokens, estimated_cost_usd, status,
               outcome_type, test_failures, ci_failures, commits_count,
               lines_changed, context
        FROM agent_metrics
        WHERE id = ?1
        ",
        params![metric_id],
        |row| {
            let started_str: String = row.get(7)?;
            let completed_str: Option<String> = row.get(8)?;

            let started_at = DateTime::parse_from_rfc3339(&started_str)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?
                .with_timezone(&Utc);

            let completed_at = if let Some(completed) = completed_str {
                Some(
                    DateTime::parse_from_rfc3339(&completed)
                        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?
                        .with_timezone(&Utc),
                )
            } else {
                None
            };

            Ok(AgentMetric {
                id: Some(row.get(0)?),
                terminal_id: row.get(1)?,
                agent_role: row.get(2)?,
                agent_system: row.get(3)?,
                task_type: row.get(4)?,
                github_issue: row.get(5)?,
                github_pr: row.get(6)?,
                started_at,
                completed_at,
                wall_time_seconds: row.get(9)?,
                active_time_seconds: row.get(10)?,
                input_tokens: row.get(11)?,
                output_tokens: row.get(12)?,
                total_tokens: row.get(13)?,
                estimated_cost_usd: row.get(14)?,
                status: row.get(15)?,
                outcome_type: row.get(16)?,
                test_failures: row.get(17)?,
                ci_failures: row.get(18)?,
                commits_count: row.get(19)?,
                lines_changed: row.get(20)?,
                context: row.get(21)?,
            })
        },
    )?)
}

/// Get productivity summary grouped by agent system.
pub(super) fn get_productivity_summary(conn: &Connection) -> Result<ProductivitySummary> {
    let mut stmt = conn.prepare(
        r"
        SELECT
            agent_system,
            COUNT(*) as tasks_completed,
            AVG(wall_time_seconds / 60.0) as avg_minutes,
            AVG(total_tokens) as avg_tokens,
            SUM(estimated_cost_usd) as total_cost
        FROM agent_metrics
        WHERE status = 'success' AND completed_at IS NOT NULL
        GROUP BY agent_system
        ORDER BY tasks_completed DESC
        ",
    )?;

    let results = stmt.query_map([], |row| {
        Ok((
            row.get(0)?, // agent_system
            row.get(1)?, // tasks_completed
            row.get(2)?, // avg_minutes
            row.get(3)?, // avg_tokens
            row.get(4)?, // total_cost
        ))
    })?;

    results.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

// ========================================================================
// Token Usage
// ========================================================================

/// Record token usage for an API request.
///
/// Records comprehensive LLM resource usage including token counts, cache usage,
/// duration, and cost. If a `metric_id` is provided, also updates the aggregate
/// token counts in `agent_metrics`.
pub(super) fn record_token_usage(conn: &Connection, usage: &TokenUsage) -> Result<i64> {
    conn.execute(
        r"
        INSERT INTO token_usage (
            input_id, metric_id, timestamp, prompt_tokens,
            completion_tokens, total_tokens, model, estimated_cost_usd,
            tokens_cache_read, tokens_cache_write, duration_ms, provider
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
        ",
        params![
            usage.input_id,
            usage.metric_id,
            usage.timestamp.to_rfc3339(),
            usage.prompt_tokens,
            usage.completion_tokens,
            usage.total_tokens,
            &usage.model,
            usage.estimated_cost_usd,
            usage.tokens_cache_read,
            usage.tokens_cache_write,
            usage.duration_ms,
            &usage.provider,
        ],
    )?;

    // Update aggregate token counts in agent_metrics if metric_id is provided
    if let Some(metric_id) = usage.metric_id {
        conn.execute(
            r"
            UPDATE agent_metrics
            SET input_tokens = input_tokens + ?1,
                output_tokens = output_tokens + ?2,
                total_tokens = total_tokens + ?3,
                estimated_cost_usd = estimated_cost_usd + ?4
            WHERE id = ?5
            ",
            params![
                usage.prompt_tokens,
                usage.completion_tokens,
                usage.total_tokens,
                usage.estimated_cost_usd,
                metric_id
            ],
        )?;
    }

    Ok(conn.last_insert_rowid())
}
