use super::db::{calculate_cost, open_activity_db};
use serde::{Deserialize, Serialize};

/// Timeline entry for activity playback visualization
#[derive(Debug, Serialize, Deserialize)]
pub struct TimelineEntry {
    pub id: i64,
    pub timestamp: String,
    pub role: String,
    pub action: String,
    pub duration_ms: Option<i32>,
    pub outcome: String,
    pub issue_number: Option<i32>,
    pub pr_number: Option<i32>,
    pub prompt_preview: Option<String>,
    pub output_preview: Option<String>,
    pub tokens: Option<i32>,
    pub cost: Option<f64>,
    pub event_type: Option<String>,
    pub label_before: Option<String>,
    pub label_after: Option<String>,
}

/// Get activity timeline for playback visualization
/// Supports filtering by issue, PR, role, and date range
#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
#[tauri::command]
pub fn get_activity_timeline(
    workspace_path: &str,
    issue_number: Option<i32>,
    pr_number: Option<i32>,
    role: Option<String>,
    date_from: Option<String>,
    date_to: Option<String>,
    limit: Option<i32>,
) -> Result<Vec<TimelineEntry>, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let limit_val = limit.unwrap_or(100);

    // Start with base query that joins activity with prompt_github for correlations
    // Use LEFT JOIN so we get activities even without GitHub correlations
    let base_query = "
        SELECT DISTINCT
            a.id,
            a.timestamp,
            a.role,
            a.outcome,
            a.duration_ms,
            a.notes,
            a.work_found,
            a.work_completed,
            a.issue_number,
            pg.pr_number,
            pg.event_type,
            pg.label_before,
            pg.label_after,
            t.total_tokens,
            t.prompt_tokens,
            t.completion_tokens
        FROM agent_activity a
        LEFT JOIN prompt_github pg ON a.id = pg.activity_id
        LEFT JOIN token_usage t ON a.id = t.activity_id
    ";

    // Rebuild with direct values (safe since we control all inputs)
    let mut where_parts: Vec<String> = Vec::new();

    if let Some(issue) = issue_number {
        where_parts.push(format!("(a.issue_number = {issue} OR pg.issue_number = {issue})"));
    }

    if let Some(pr) = pr_number {
        where_parts.push(format!("pg.pr_number = {pr}"));
    }

    if let Some(ref r) = role {
        // Escape single quotes for SQL safety
        let escaped_role = r.replace('\'', "''");
        where_parts.push(format!("LOWER(a.role) = LOWER('{escaped_role}')"));
    }

    if let Some(ref from) = date_from {
        let escaped_from = from.replace('\'', "''");
        where_parts.push(format!("DATE(a.timestamp) >= DATE('{escaped_from}')"));
    }

    if let Some(ref to) = date_to {
        let escaped_to = to.replace('\'', "''");
        where_parts.push(format!("DATE(a.timestamp) <= DATE('{escaped_to}')"));
    }

    let where_clause = if where_parts.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", where_parts.join(" AND "))
    };

    let query = format!("{base_query} {where_clause} ORDER BY a.timestamp DESC LIMIT {limit_val}");

    let mut stmt = conn
        .prepare(&query)
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let entries = stmt
        .query_map([], |row| {
            let id: i64 = row.get(0)?;
            let timestamp: String = row.get(1)?;
            let role: String = row.get(2)?;
            let outcome: String = row.get(3)?;
            let duration_ms: Option<i32> = row.get(4)?;
            let notes: Option<String> = row.get(5)?;
            let work_found: i32 = row.get(6)?;
            let work_completed: Option<i32> = row.get(7)?;
            let issue_number: Option<i32> = row.get(8)?;
            let pr_number: Option<i32> = row.get(9)?;
            let event_type: Option<String> = row.get(10)?;
            let label_before: Option<String> = row.get(11)?;
            let label_after: Option<String> = row.get(12)?;
            let total_tokens: Option<i32> = row.get(13)?;
            let prompt_tokens: Option<i32> = row.get(14)?;
            let completion_tokens: Option<i32> = row.get(15)?;

            // Determine action from outcome or event type
            let action = determine_action_from_outcome(&outcome, event_type.as_deref());

            // Determine outcome status
            let outcome_status = if work_completed == Some(1) {
                "success".to_string()
            } else if work_completed == Some(0) {
                "failure".to_string()
            } else if work_found == 0 {
                "pending".to_string()
            } else {
                "in_progress".to_string()
            };

            // Calculate cost if token data available
            let cost = match (prompt_tokens, completion_tokens) {
                (Some(p), Some(c)) => Some(calculate_cost(i64::from(p), i64::from(c))),
                _ => None,
            };

            Ok(TimelineEntry {
                id,
                timestamp,
                role,
                action,
                duration_ms,
                outcome: outcome_status,
                issue_number,
                pr_number,
                prompt_preview: notes,
                output_preview: None, // Could be populated from terminal_outputs if needed
                tokens: total_tokens,
                cost,
                event_type,
                label_before,
                label_after,
            })
        })
        .map_err(|e| format!("Failed to query timeline: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect timeline entries: {e}"))?;

    Ok(entries)
}

/// Helper function to determine action description from outcome and event type
fn determine_action_from_outcome(outcome: &str, event_type: Option<&str>) -> String {
    // First check event type for more specific actions
    if let Some(et) = event_type {
        match et {
            "pr_created" => return "Created PR".to_string(),
            "pr_merged" => return "Merged PR".to_string(),
            "pr_closed" => return "Closed PR".to_string(),
            "pr_approved" => return "Approved PR".to_string(),
            "pr_changes_requested" => return "Requested changes".to_string(),
            "pr_reviewed" => return "Reviewed PR".to_string(),
            "issue_claimed" => return "Claimed issue".to_string(),
            "issue_closed" => return "Closed issue".to_string(),
            "issue_reopened" => return "Reopened issue".to_string(),
            "label_added" => return "Added label".to_string(),
            "label_removed" => return "Removed label".to_string(),
            _ => {}
        }
    }

    // Fall back to outcome-based action
    match outcome {
        "pr_created" => "Created PR".to_string(),
        "issue_claimed" => "Claimed issue".to_string(),
        "review_complete" => "Completed review".to_string(),
        "changes_requested" => "Requested changes".to_string(),
        "approved" => "Approved".to_string(),
        "merged" => "Merged".to_string(),
        "no_work" => "No work found".to_string(),
        "success" => "Completed successfully".to_string(),
        "error" | "failed" => "Failed".to_string(),
        _ => {
            if outcome.is_empty() {
                "Activity".to_string()
            } else {
                // Capitalize first letter of outcome
                let mut chars = outcome.chars();
                match chars.next() {
                    None => "Activity".to_string(),
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                }
            }
        }
    }
}
