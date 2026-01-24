use rusqlite::{params, Connection, Result as SqliteResult};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Activity log entry matching TypeScript interface
#[derive(Debug, Serialize, Deserialize)]
pub struct ActivityEntry {
    pub timestamp: String,
    pub role: String,
    pub trigger: String,
    pub work_found: bool,
    pub work_completed: Option<bool>,
    pub issue_number: Option<i32>,
    pub duration_ms: Option<i32>,
    pub outcome: String,
    pub notes: Option<String>,
    // Token usage tracking (optional)
    pub prompt_tokens: Option<i32>,
    pub completion_tokens: Option<i32>,
    pub total_tokens: Option<i32>,
    pub model: Option<String>,
}

/// Get current schema version from database
fn get_schema_version(conn: &Connection) -> SqliteResult<i32> {
    // Check if schema_version table exists
    let table_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_version'",
            [],
            |row| row.get(0),
        )
        .map(|count: i32| count > 0)?;

    if !table_exists {
        // No version table = v1 schema
        return Ok(1);
    }

    // Read version from table
    conn.query_row("SELECT version FROM schema_version LIMIT 1", [], |row| row.get(0))
        .or(Ok(1)) // Default to v1 if no row exists
}

/// Update schema version in database
fn set_schema_version(conn: &Connection, version: i32) -> SqliteResult<()> {
    // Create table if it doesn't exist
    conn.execute(
        "CREATE TABLE IF NOT EXISTS schema_version (
            version INTEGER NOT NULL
        )",
        [],
    )?;

    // Check if version row exists
    let row_exists: bool = conn
        .query_row("SELECT COUNT(*) FROM schema_version", [], |row| row.get(0))
        .map(|count: i32| count > 0)?;

    if row_exists {
        conn.execute("UPDATE schema_version SET version = ?1", [version])?;
    } else {
        conn.execute("INSERT INTO schema_version (version) VALUES (?1)", [version])?;
    }

    Ok(())
}

/// Migrate schema from v1 to v2
fn migrate_v1_to_v2(conn: &Connection) -> SqliteResult<()> {
    // Create token_usage table
    conn.execute(
        "CREATE TABLE IF NOT EXISTS token_usage (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            activity_id INTEGER NOT NULL,
            prompt_tokens INTEGER NOT NULL,
            completion_tokens INTEGER NOT NULL,
            total_tokens INTEGER NOT NULL,
            model TEXT,
            FOREIGN KEY (activity_id) REFERENCES agent_activity(id)
        )",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_token_usage_activity_id ON token_usage(activity_id)",
        [],
    )?;

    // Create code_changes table
    conn.execute(
        "CREATE TABLE IF NOT EXISTS code_changes (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            activity_id INTEGER NOT NULL,
            files_modified INTEGER NOT NULL,
            lines_added INTEGER NOT NULL,
            lines_removed INTEGER NOT NULL,
            commit_sha TEXT,
            FOREIGN KEY (activity_id) REFERENCES agent_activity(id)
        )",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_code_changes_activity_id ON code_changes(activity_id)",
        [],
    )?;

    // Create github_events table
    conn.execute(
        "CREATE TABLE IF NOT EXISTS github_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            activity_id INTEGER,
            event_type TEXT NOT NULL,
            event_time TEXT NOT NULL,
            pr_number INTEGER,
            issue_number INTEGER,
            commit_sha TEXT,
            author TEXT,
            FOREIGN KEY (activity_id) REFERENCES agent_activity(id)
        )",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_github_events_activity_id ON github_events(activity_id)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_github_events_event_type ON github_events(event_type)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_github_events_event_time ON github_events(event_time)",
        [],
    )?;

    Ok(())
}

/// Run all pending migrations
fn run_migrations(conn: &Connection) -> SqliteResult<()> {
    let current_version = get_schema_version(conn)?;

    // Migrate to v2 if needed
    if current_version < 2 {
        migrate_v1_to_v2(conn)?;
        set_schema_version(conn, 2)?;
    }

    Ok(())
}

/// Open `SQLite` connection to activity database
fn open_activity_db(workspace_path: &str) -> SqliteResult<Connection> {
    let loom_dir = Path::new(workspace_path).join(".loom");
    let db_path = loom_dir.join("activity.db");

    // Ensure .loom directory exists
    if !loom_dir.exists() {
        std::fs::create_dir_all(&loom_dir)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    }

    let conn = Connection::open(&db_path)?;

    // Create v1 tables if they don't exist (for new databases)
    conn.execute(
        "CREATE TABLE IF NOT EXISTS agent_activity (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            role TEXT NOT NULL,
            trigger TEXT NOT NULL,
            work_found INTEGER NOT NULL,
            work_completed INTEGER,
            issue_number INTEGER,
            duration_ms INTEGER,
            outcome TEXT NOT NULL,
            notes TEXT
        )",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_activity_timestamp ON agent_activity(timestamp)",
        [],
    )?;

    conn.execute("CREATE INDEX IF NOT EXISTS idx_activity_role ON agent_activity(role)", [])?;

    conn.execute("CREATE INDEX IF NOT EXISTS idx_activity_outcome ON agent_activity(outcome)", [])?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_activity_work_found ON agent_activity(work_found)",
        [],
    )?;

    // Run migrations to latest version
    run_migrations(&conn)?;

    Ok(conn)
}

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

/// Token usage summary by role
#[derive(Debug, Serialize, Deserialize)]
pub struct TokenUsageSummary {
    pub role: String,
    pub total_prompt_tokens: i64,
    pub total_completion_tokens: i64,
    pub total_tokens: i64,
    pub activity_count: i32,
    pub avg_tokens_per_activity: f64,
}

/// Query token usage statistics grouped by role
#[tauri::command]
pub fn query_token_usage_by_role(
    workspace_path: &str,
    since: Option<String>,
) -> Result<Vec<TokenUsageSummary>, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let query = if since.is_some() {
        "SELECT
            a.role,
            SUM(t.prompt_tokens) as total_prompt,
            SUM(t.completion_tokens) as total_completion,
            SUM(t.total_tokens) as total_tokens,
            COUNT(DISTINCT a.id) as activity_count,
            CAST(SUM(t.total_tokens) AS REAL) / COUNT(DISTINCT a.id) as avg_tokens
         FROM agent_activity a
         JOIN token_usage t ON a.id = t.activity_id
         WHERE a.timestamp > ?1
         GROUP BY a.role
         ORDER BY total_tokens DESC"
    } else {
        "SELECT
            a.role,
            SUM(t.prompt_tokens) as total_prompt,
            SUM(t.completion_tokens) as total_completion,
            SUM(t.total_tokens) as total_tokens,
            COUNT(DISTINCT a.id) as activity_count,
            CAST(SUM(t.total_tokens) AS REAL) / COUNT(DISTINCT a.id) as avg_tokens
         FROM agent_activity a
         JOIN token_usage t ON a.id = t.activity_id
         GROUP BY a.role
         ORDER BY total_tokens DESC"
    };

    let mut stmt = conn
        .prepare(query)
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let summaries = if let Some(since_time) = since {
        stmt.query_map([since_time], |row| {
            Ok(TokenUsageSummary {
                role: row.get(0)?,
                total_prompt_tokens: row.get(1)?,
                total_completion_tokens: row.get(2)?,
                total_tokens: row.get(3)?,
                activity_count: row.get(4)?,
                avg_tokens_per_activity: row.get(5)?,
            })
        })
        .map_err(|e| format!("Failed to query token usage: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect summaries: {e}"))?
    } else {
        stmt.query_map([], |row| {
            Ok(TokenUsageSummary {
                role: row.get(0)?,
                total_prompt_tokens: row.get(1)?,
                total_completion_tokens: row.get(2)?,
                total_tokens: row.get(3)?,
                activity_count: row.get(4)?,
                avg_tokens_per_activity: row.get(5)?,
            })
        })
        .map_err(|e| format!("Failed to query token usage: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect summaries: {e}"))?
    };

    Ok(summaries)
}

/// Token usage over time (daily aggregation)
#[derive(Debug, Serialize, Deserialize)]
pub struct DailyTokenUsage {
    pub date: String,
    pub role: String,
    pub total_tokens: i64,
}

/// Query token usage over time, grouped by date and role
#[tauri::command]
pub fn query_token_usage_timeline(
    workspace_path: &str,
    days: Option<i32>,
) -> Result<Vec<DailyTokenUsage>, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let days_val = days.unwrap_or(30);

    let mut stmt = conn
        .prepare(
            "SELECT
                DATE(a.timestamp) as date,
                a.role,
                SUM(t.total_tokens) as daily_tokens
             FROM agent_activity a
             JOIN token_usage t ON a.id = t.activity_id
             WHERE a.timestamp > datetime('now', '-' || ?1 || ' days')
             GROUP BY date, a.role
             ORDER BY date DESC, a.role",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let timeline = stmt
        .query_map([days_val], |row| {
            Ok(DailyTokenUsage {
                date: row.get(0)?,
                role: row.get(1)?,
                total_tokens: row.get(2)?,
            })
        })
        .map_err(|e| format!("Failed to query timeline: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect timeline: {e}"))?;

    Ok(timeline)
}

// ============================================================================
// Agent Effectiveness Metrics
// ============================================================================

/// Agent metrics summary for dashboard display
#[derive(Debug, Serialize, Deserialize)]
pub struct AgentMetrics {
    /// Total number of prompts/activities
    pub prompt_count: i32,
    /// Total tokens consumed
    pub total_tokens: i64,
    /// Estimated cost in USD (based on token pricing)
    pub total_cost: f64,
    /// Success rate (work_completed / total where work_found)
    pub success_rate: f64,
    /// Number of PRs created (from github_events)
    pub prs_created: i32,
    /// Number of issues closed (from github_events)
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

/// Default token pricing (Claude 3.5 Sonnet pricing per 1K tokens)
const INPUT_TOKEN_PRICE_PER_1K: f64 = 0.003;
const OUTPUT_TOKEN_PRICE_PER_1K: f64 = 0.015;

/// Calculate estimated cost from token counts
fn calculate_cost(prompt_tokens: i64, completion_tokens: i64) -> f64 {
    let input_cost = (prompt_tokens as f64 / 1000.0) * INPUT_TOKEN_PRICE_PER_1K;
    let output_cost = (completion_tokens as f64 / 1000.0) * OUTPUT_TOKEN_PRICE_PER_1K;
    input_cost + output_cost
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
        (completed as f64) / (with_work as f64)
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
                (completed as f64) / (with_work as f64)
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

/// Log a GitHub event (PR created, issue closed, etc.)
#[tauri::command]
pub fn log_github_event(
    workspace_path: String,
    event_type: String,
    pr_number: Option<i32>,
    issue_number: Option<i32>,
    commit_sha: Option<String>,
    author: Option<String>,
) -> Result<(), String> {
    let conn =
        open_activity_db(&workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    conn.execute(
        "INSERT INTO github_events (event_type, event_time, pr_number, issue_number, commit_sha, author)
         VALUES (?1, datetime('now'), ?2, ?3, ?4, ?5)",
        params![event_type, pr_number, issue_number, commit_sha, author],
    )
    .map_err(|e| format!("Failed to log GitHub event: {e}"))?;

    Ok(())
}
