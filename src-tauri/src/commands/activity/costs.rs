use super::db::{calculate_cost, open_activity_db};
use rusqlite::params;
use serde::{Deserialize, Serialize};

/// Issue resolution cost summary
#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
pub struct IssueCostSummary {
    pub issue_number: i32,
    pub prompt_count: i32,
    pub total_tokens: i64,
    pub total_cost: f64,
    pub first_activity: String,
    pub last_activity: String,
    pub duration_hours: f64,
    pub pr_number: Option<i32>,
    pub merged: bool,
}

/// Get cost and metrics for resolving a specific issue
#[allow(dead_code)]
#[tauri::command]
pub fn get_issue_resolution_cost(
    workspace_path: &str,
    issue_number: i32,
) -> Result<Option<IssueCostSummary>, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    // Get all activity IDs associated with this issue
    let activity_ids: Vec<i64> = conn
        .prepare("SELECT DISTINCT activity_id FROM prompt_github WHERE issue_number = ?1")
        .map_err(|e| format!("Failed to prepare query: {e}"))?
        .query_map([issue_number], |row| row.get(0))
        .map_err(|e| format!("Failed to query activity IDs: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect activity IDs: {e}"))?;

    if activity_ids.is_empty() {
        return Ok(None);
    }

    // Build comma-separated list for IN clause
    let id_list = activity_ids
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");

    // Get prompt count and timestamps
    let (prompt_count, first_activity, last_activity): (i32, String, String) = conn
        .query_row(
            &format!(
                "SELECT COUNT(*), MIN(timestamp), MAX(timestamp)
                 FROM agent_activity
                 WHERE id IN ({id_list})"
            ),
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|e| format!("Failed to query activity metrics: {e}"))?;

    // Get token usage
    let (prompt_tokens, completion_tokens, total_tokens): (i64, i64, i64) = conn
        .query_row(
            &format!(
                "SELECT COALESCE(SUM(prompt_tokens), 0), COALESCE(SUM(completion_tokens), 0), COALESCE(SUM(total_tokens), 0)
                 FROM token_usage
                 WHERE activity_id IN ({id_list})"
            ),
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap_or((0, 0, 0));

    // Calculate duration in hours
    let duration_hours: f64 = conn
        .query_row(
            &format!(
                "SELECT (julianday(MAX(timestamp)) - julianday(MIN(timestamp))) * 24
                 FROM agent_activity
                 WHERE id IN ({id_list})"
            ),
            [],
            |row| row.get(0),
        )
        .unwrap_or(0.0);

    // Check if there's an associated PR and if it was merged
    let pr_info: Option<(i32, bool)> = conn
        .query_row(
            "SELECT pr_number, event_type = 'pr_merged'
             FROM prompt_github
             WHERE issue_number = ?1 AND pr_number IS NOT NULL
             ORDER BY CASE WHEN event_type = 'pr_merged' THEN 0 ELSE 1 END
             LIMIT 1",
            [issue_number],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();

    let (pr_number, merged) = pr_info.map_or((None, false), |(pr, m)| (Some(pr), m));

    Ok(Some(IssueCostSummary {
        issue_number,
        prompt_count,
        total_tokens,
        total_cost: calculate_cost(prompt_tokens, completion_tokens),
        first_activity,
        last_activity,
        duration_hours,
        pr_number,
        merged,
    }))
}

/// Label transition record for tracking workflow state changes
#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
pub struct LabelTransition {
    pub timestamp: String,
    pub issue_number: Option<i32>,
    pub pr_number: Option<i32>,
    pub label_before: Option<String>,
    pub label_after: Option<String>,
    pub activity_id: i64,
    pub role: String,
}

/// Get label transition history for tracking workflow state changes
#[allow(dead_code)]
#[tauri::command]
pub fn get_label_transitions(
    workspace_path: &str,
    time_range: &str,
) -> Result<Vec<LabelTransition>, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let since_clause = match time_range {
        "today" => "datetime('now', 'start of day')",
        "week" => "datetime('now', '-7 days')",
        "month" => "datetime('now', '-30 days')",
        _ => "datetime('1970-01-01')",
    };

    let query = format!(
        "SELECT pg.event_time, pg.issue_number, pg.pr_number, pg.label_before, pg.label_after, pg.activity_id, a.role
         FROM prompt_github pg
         JOIN agent_activity a ON pg.activity_id = a.id
         WHERE pg.event_type IN ('label_added', 'label_removed')
           AND pg.event_time >= {since_clause}
         ORDER BY pg.event_time DESC"
    );

    let mut stmt = conn
        .prepare(&query)
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let transitions = stmt
        .query_map([], |row| {
            Ok(LabelTransition {
                timestamp: row.get(0)?,
                issue_number: row.get(1)?,
                pr_number: row.get(2)?,
                label_before: row.get(3)?,
                label_after: row.get(4)?,
                activity_id: row.get(5)?,
                role: row.get(6)?,
            })
        })
        .map_err(|e| format!("Failed to query label transitions: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect transitions: {e}"))?;

    Ok(transitions)
}

/// PR cycle time analysis
#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
pub struct PRCycleTime {
    pub pr_number: i32,
    pub issue_number: Option<i32>,
    pub created_at: String,
    pub merged_at: Option<String>,
    pub closed_at: Option<String>,
    pub review_requested_at: Option<String>,
    pub approved_at: Option<String>,
    pub cycle_time_hours: Option<f64>,
    pub review_time_hours: Option<f64>,
    pub prompt_count: i32,
    pub total_cost: f64,
}

/// Get PR cycle time analysis for recent PRs
#[allow(dead_code, clippy::too_many_lines)]
#[tauri::command]
pub fn get_pr_cycle_times(
    workspace_path: &str,
    limit: Option<i32>,
) -> Result<Vec<PRCycleTime>, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let limit = limit.unwrap_or(20);

    // Get distinct PRs with their first and last events
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT pr_number FROM prompt_github
             WHERE pr_number IS NOT NULL
             ORDER BY event_time DESC
             LIMIT ?1",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let pr_numbers: Vec<i32> = stmt
        .query_map([limit], |row| row.get(0))
        .map_err(|e| format!("Failed to query PR numbers: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect PR numbers: {e}"))?;

    let mut results = Vec::new();

    for pr_number in pr_numbers {
        // Get all events for this PR
        let mut events_stmt = conn
            .prepare(
                "SELECT event_type, event_time, issue_number
                 FROM prompt_github
                 WHERE pr_number = ?1
                 ORDER BY event_time ASC",
            )
            .map_err(|e| format!("Failed to prepare events query: {e}"))?;

        let events: Vec<(String, String, Option<i32>)> = events_stmt
            .query_map([pr_number], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .map_err(|e| format!("Failed to query events: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("Failed to collect events: {e}"))?;

        if events.is_empty() {
            continue;
        }

        let issue_number = events.iter().find_map(|(_, _, i)| *i);
        let created_at = events
            .iter()
            .find(|(t, _, _)| t == "pr_created")
            .map_or_else(|| events[0].1.clone(), |(_, time, _)| time.clone());

        let merged_at = events
            .iter()
            .find(|(t, _, _)| t == "pr_merged")
            .map(|(_, time, _)| time.clone());

        let closed_at = events
            .iter()
            .find(|(t, _, _)| t == "pr_closed")
            .map(|(_, time, _)| time.clone());

        let review_requested_at = events
            .iter()
            .find(|(t, _, _)| t == "label_added")
            .filter(|(_, _, _)| true) // Could check for loom:review-requested label
            .map(|(_, time, _)| time.clone());

        let approved_at = events
            .iter()
            .find(|(t, _, _)| t == "pr_approved")
            .map(|(_, time, _)| time.clone());

        // Calculate cycle time (creation to merge/close)
        let cycle_time_hours = merged_at
            .as_ref()
            .or(closed_at.as_ref())
            .and_then(|end_time| {
                conn.query_row(
                    "SELECT (julianday(?1) - julianday(?2)) * 24",
                    params![end_time, &created_at],
                    |row| row.get(0),
                )
                .ok()
            });

        // Calculate review time (review requested to approved)
        let review_time_hours = match (&review_requested_at, &approved_at) {
            (Some(start), Some(end)) => conn
                .query_row(
                    "SELECT (julianday(?1) - julianday(?2)) * 24",
                    params![end, start],
                    |row| row.get(0),
                )
                .ok(),
            _ => None,
        };

        // Get activity IDs for this PR
        let activity_ids: Vec<i64> = conn
            .prepare("SELECT DISTINCT activity_id FROM prompt_github WHERE pr_number = ?1")
            .map_err(|e| format!("Failed to prepare query: {e}"))?
            .query_map([pr_number], |row| row.get(0))
            .map_err(|e| format!("Failed to query activity IDs: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("Failed to collect activity IDs: {e}"))?;

        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let prompt_count = activity_ids.len() as i32;

        // Calculate total cost
        let total_cost = if activity_ids.is_empty() {
            0.0
        } else {
            let id_list = activity_ids
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>()
                .join(",");

            let (prompt_tokens, completion_tokens): (i64, i64) = conn
                .query_row(
                    &format!(
                        "SELECT COALESCE(SUM(prompt_tokens), 0), COALESCE(SUM(completion_tokens), 0)
                         FROM token_usage
                         WHERE activity_id IN ({id_list})"
                    ),
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap_or((0, 0));

            calculate_cost(prompt_tokens, completion_tokens)
        };

        results.push(PRCycleTime {
            pr_number,
            issue_number,
            created_at,
            merged_at,
            closed_at,
            review_requested_at,
            approved_at,
            cycle_time_hours,
            review_time_hours,
            prompt_count,
            total_cost,
        });
    }

    Ok(results)
}

/// Average cost per issue resolved
#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
pub struct AverageIssueCost {
    pub issues_resolved: i32,
    pub total_cost: f64,
    pub average_cost: f64,
    pub total_prompts: i32,
    pub average_prompts: f64,
    pub average_duration_hours: f64,
}

/// Get average cost to resolve issues over a time range
#[allow(dead_code)]
#[tauri::command]
pub fn get_average_issue_cost(
    workspace_path: &str,
    time_range: &str,
) -> Result<AverageIssueCost, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let since_clause = match time_range {
        "today" => "datetime('now', 'start of day')",
        "week" => "datetime('now', '-7 days')",
        "month" => "datetime('now', '-30 days')",
        _ => "datetime('1970-01-01')",
    };

    // Get all issues that were resolved (merged or closed) in the time range
    let query = format!(
        "SELECT DISTINCT issue_number
         FROM prompt_github
         WHERE issue_number IS NOT NULL
           AND event_type IN ('pr_merged', 'issue_closed')
           AND event_time >= {since_clause}"
    );

    let resolved_issues: Vec<i32> = conn
        .prepare(&query)
        .map_err(|e| format!("Failed to prepare query: {e}"))?
        .query_map([], |row| row.get(0))
        .map_err(|e| format!("Failed to query resolved issues: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect issues: {e}"))?;

    if resolved_issues.is_empty() {
        return Ok(AverageIssueCost {
            issues_resolved: 0,
            total_cost: 0.0,
            average_cost: 0.0,
            total_prompts: 0,
            average_prompts: 0.0,
            average_duration_hours: 0.0,
        });
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let issues_resolved = resolved_issues.len() as i32;

    // Calculate totals across all resolved issues
    let issue_list = resolved_issues
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");

    // Get all activity IDs for these issues
    let activity_ids: Vec<i64> = conn
        .prepare(&format!(
            "SELECT DISTINCT activity_id FROM prompt_github WHERE issue_number IN ({issue_list})"
        ))
        .map_err(|e| format!("Failed to prepare query: {e}"))?
        .query_map([], |row| row.get(0))
        .map_err(|e| format!("Failed to query activity IDs: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect activity IDs: {e}"))?;

    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let total_prompts = activity_ids.len() as i32;

    let total_cost = if activity_ids.is_empty() {
        0.0
    } else {
        let id_list = activity_ids
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");

        let (prompt_tokens, completion_tokens): (i64, i64) = conn
            .query_row(
                &format!(
                    "SELECT COALESCE(SUM(prompt_tokens), 0), COALESCE(SUM(completion_tokens), 0)
                     FROM token_usage
                     WHERE activity_id IN ({id_list})"
                ),
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap_or((0, 0));

        calculate_cost(prompt_tokens, completion_tokens)
    };

    // Calculate average duration per issue
    let total_duration: f64 = resolved_issues
        .iter()
        .filter_map(|&issue_num| {
            conn.query_row(
                "SELECT (julianday(MAX(event_time)) - julianday(MIN(event_time))) * 24
                 FROM prompt_github
                 WHERE issue_number = ?1",
                [issue_num],
                |row| row.get::<_, f64>(0),
            )
            .ok()
        })
        .sum();

    Ok(AverageIssueCost {
        issues_resolved,
        total_cost,
        average_cost: total_cost / f64::from(issues_resolved),
        total_prompts,
        average_prompts: f64::from(total_prompts) / f64::from(issues_resolved),
        average_duration_hours: total_duration / f64::from(issues_resolved),
    })
}
