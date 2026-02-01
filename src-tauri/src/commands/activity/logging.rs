use super::db::{open_activity_db, ActivityEntry};
use rusqlite::params;

/// Log activity entry to `SQLite` database
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn log_activity(workspace_path: String, entry: ActivityEntry) -> Result<(), String> {
    let conn =
        open_activity_db(&workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    conn.execute(
        "INSERT INTO agent_activity (
            timestamp, role, trigger, work_found, work_completed,
            issue_number, duration_ms, outcome, notes
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            &entry.timestamp,
            &entry.role,
            &entry.trigger,
            i32::from(entry.work_found),
            entry.work_completed.map(i32::from),
            entry.issue_number,
            entry.duration_ms,
            &entry.outcome,
            &entry.notes,
        ],
    )
    .map_err(|e| format!("Failed to insert activity: {e}"))?;

    let activity_id = conn.last_insert_rowid();

    // Insert token usage if present
    if let (Some(prompt_tokens), Some(completion_tokens), Some(total_tokens)) =
        (entry.prompt_tokens, entry.completion_tokens, entry.total_tokens)
    {
        conn.execute(
            "INSERT INTO token_usage (
                activity_id, prompt_tokens, completion_tokens, total_tokens, model
            ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                activity_id,
                prompt_tokens,
                completion_tokens,
                total_tokens,
                entry.model
            ],
        )
        .map_err(|e| format!("Failed to insert token usage: {e}"))?;
    }

    Ok(())
}

/// Read recent activity entries from `SQLite` database
#[tauri::command]
pub fn read_recent_activity(
    workspace_path: &str,
    limit: Option<i32>,
) -> Result<Vec<ActivityEntry>, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;
    let limit = limit.unwrap_or(100);

    let mut stmt = conn
        .prepare(
            "SELECT timestamp, role, trigger, work_found, work_completed,
                    issue_number, duration_ms, outcome, notes
             FROM agent_activity
             ORDER BY timestamp DESC
             LIMIT ?1",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let entries = stmt
        .query_map([limit], |row| {
            Ok(ActivityEntry {
                timestamp: row.get(0)?,
                role: row.get(1)?,
                trigger: row.get(2)?,
                work_found: row.get::<_, i32>(3)? != 0,
                work_completed: row.get::<_, Option<i32>>(4)?.map(|i| i != 0),
                issue_number: row.get(5)?,
                duration_ms: row.get(6)?,
                outcome: row.get(7)?,
                notes: row.get(8)?,
                prompt_tokens: None,
                completion_tokens: None,
                total_tokens: None,
                model: None,
            })
        })
        .map_err(|e| format!("Failed to query activities: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect activities: {e}"))?;

    Ok(entries)
}

/// Get activity entries filtered by role
#[tauri::command]
pub fn get_activity_by_role(
    workspace_path: &str,
    role: &str,
    limit: Option<i32>,
) -> Result<Vec<ActivityEntry>, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;
    let limit = limit.unwrap_or(100);

    let mut stmt = conn
        .prepare(
            "SELECT timestamp, role, trigger, work_found, work_completed,
                    issue_number, duration_ms, outcome, notes
             FROM agent_activity
             WHERE role = ?1
             ORDER BY timestamp DESC
             LIMIT ?2",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let entries = stmt
        .query_map(params![role, limit], |row| {
            Ok(ActivityEntry {
                timestamp: row.get(0)?,
                role: row.get(1)?,
                trigger: row.get(2)?,
                work_found: row.get::<_, i32>(3)? != 0,
                work_completed: row.get::<_, Option<i32>>(4)?.map(|i| i != 0),
                issue_number: row.get(5)?,
                duration_ms: row.get(6)?,
                outcome: row.get(7)?,
                notes: row.get(8)?,
                prompt_tokens: None,
                completion_tokens: None,
                total_tokens: None,
                model: None,
            })
        })
        .map_err(|e| format!("Failed to query activities: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect activities: {e}"))?;

    Ok(entries)
}
