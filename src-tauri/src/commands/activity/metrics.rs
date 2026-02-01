use super::db::{calculate_cost, open_activity_db};
use serde::{Deserialize, Serialize};

/// Agent metrics summary for dashboard display
#[derive(Debug, Serialize, Deserialize)]
pub struct AgentMetrics {
    /// Total number of prompts/activities
    pub prompt_count: i32,
    /// Total tokens consumed
    pub total_tokens: i64,
    /// Estimated cost in USD (based on token pricing)
    pub total_cost: f64,
    /// Success rate (`work_completed` / total where `work_found`)
    pub success_rate: f64,
    /// Number of PRs created (from `github_events`)
    pub prs_created: i32,
    /// Number of issues closed (from `github_events`)
    pub issues_closed: i32,
}

/// Agent metrics broken down by role
#[derive(Debug, Serialize, Deserialize)]
pub struct RoleMetrics {
    pub role: String,
    pub prompt_count: i32,
    pub total_tokens: i64,
    pub total_cost: f64,
    pub success_rate: f64,
}

/// Get agent metrics for a time range
#[tauri::command]
pub fn get_agent_metrics(
    workspace_path: &str,
    time_range: &str, // "today", "week", "month", "all"
) -> Result<AgentMetrics, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let since_clause = match time_range {
        "today" => "datetime('now', 'start of day')",
        "week" => "datetime('now', '-7 days')",
        "month" => "datetime('now', '-30 days')",
        _ => "datetime('1970-01-01')", // "all" or fallback
    };

    // Get activity counts and success rate
    let activity_query = format!(
        "SELECT
            COUNT(*) as prompt_count,
            COALESCE(SUM(CASE WHEN work_found = 1 AND work_completed = 1 THEN 1 ELSE 0 END), 0) as completed,
            COALESCE(SUM(CASE WHEN work_found = 1 THEN 1 ELSE 0 END), 0) as with_work
         FROM agent_activity
         WHERE timestamp >= {since_clause}"
    );

    let (prompt_count, completed, with_work): (i32, i32, i32) = conn
        .query_row(&activity_query, [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .map_err(|e| format!("Failed to query activity metrics: {e}"))?;

    let success_rate = if with_work > 0 {
        f64::from(completed) / f64::from(with_work)
    } else {
        0.0
    };

    // Get token usage totals
    let token_query = format!(
        "SELECT
            COALESCE(SUM(t.prompt_tokens), 0) as total_prompt,
            COALESCE(SUM(t.completion_tokens), 0) as total_completion,
            COALESCE(SUM(t.total_tokens), 0) as total_tokens
         FROM agent_activity a
         JOIN token_usage t ON a.id = t.activity_id
         WHERE a.timestamp >= {since_clause}"
    );

    let (prompt_tokens, completion_tokens, total_tokens): (i64, i64, i64) = conn
        .query_row(&token_query, [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap_or((0, 0, 0));

    let total_cost = calculate_cost(prompt_tokens, completion_tokens);

    // Get GitHub events (PRs created, issues closed)
    let github_query = format!(
        "SELECT
            COALESCE(SUM(CASE WHEN event_type = 'pr_created' THEN 1 ELSE 0 END), 0) as prs_created,
            COALESCE(SUM(CASE WHEN event_type = 'issue_closed' THEN 1 ELSE 0 END), 0) as issues_closed
         FROM github_events
         WHERE event_time >= {since_clause}"
    );

    let (prs_created, issues_closed): (i32, i32) = conn
        .query_row(&github_query, [], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap_or((0, 0));

    Ok(AgentMetrics {
        prompt_count,
        total_tokens,
        total_cost,
        success_rate,
        prs_created,
        issues_closed,
    })
}

/// Get metrics broken down by role
#[tauri::command]
pub fn get_metrics_by_role(
    workspace_path: &str,
    time_range: &str,
) -> Result<Vec<RoleMetrics>, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let since_clause = match time_range {
        "today" => "datetime('now', 'start of day')",
        "week" => "datetime('now', '-7 days')",
        "month" => "datetime('now', '-30 days')",
        _ => "datetime('1970-01-01')",
    };

    let query = format!(
        "SELECT
            a.role,
            COUNT(*) as prompt_count,
            COALESCE(SUM(t.prompt_tokens), 0) as total_prompt,
            COALESCE(SUM(t.completion_tokens), 0) as total_completion,
            COALESCE(SUM(t.total_tokens), 0) as total_tokens,
            COALESCE(SUM(CASE WHEN a.work_found = 1 AND a.work_completed = 1 THEN 1 ELSE 0 END), 0) as completed,
            COALESCE(SUM(CASE WHEN a.work_found = 1 THEN 1 ELSE 0 END), 0) as with_work
         FROM agent_activity a
         LEFT JOIN token_usage t ON a.id = t.activity_id
         WHERE a.timestamp >= {since_clause}
         GROUP BY a.role
         ORDER BY total_tokens DESC"
    );

    let mut stmt = conn
        .prepare(&query)
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let metrics = stmt
        .query_map([], |row| {
            let role: String = row.get(0)?;
            let prompt_count: i32 = row.get(1)?;
            let prompt_tokens: i64 = row.get(2)?;
            let completion_tokens: i64 = row.get(3)?;
            let total_tokens: i64 = row.get(4)?;
            let completed: i32 = row.get(5)?;
            let with_work: i32 = row.get(6)?;

            let success_rate = if with_work > 0 {
                f64::from(completed) / f64::from(with_work)
            } else {
                0.0
            };

            Ok(RoleMetrics {
                role,
                prompt_count,
                total_tokens,
                total_cost: calculate_cost(prompt_tokens, completion_tokens),
                success_rate,
            })
        })
        .map_err(|e| format!("Failed to query role metrics: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect metrics: {e}"))?;

    Ok(metrics)
}
