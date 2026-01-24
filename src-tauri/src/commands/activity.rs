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

/// Migrate schema from v2 to v3
/// Adds prompt_github table for linking prompts to GitHub entities
fn migrate_v2_to_v3(conn: &Connection) -> SqliteResult<()> {
    // Create prompt_github table for correlating prompts with GitHub entities
    conn.execute(
        "CREATE TABLE IF NOT EXISTS prompt_github (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            activity_id INTEGER NOT NULL,
            issue_number INTEGER,
            pr_number INTEGER,
            label_before TEXT,
            label_after TEXT,
            event_type TEXT NOT NULL,
            event_time TEXT NOT NULL,
            FOREIGN KEY (activity_id) REFERENCES agent_activity(id)
        )",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_prompt_github_activity_id ON prompt_github(activity_id)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_prompt_github_issue_number ON prompt_github(issue_number)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_prompt_github_pr_number ON prompt_github(pr_number)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_prompt_github_event_type ON prompt_github(event_type)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_prompt_github_event_time ON prompt_github(event_time)",
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

    // Migrate to v3 if needed
    if current_version < 3 {
        migrate_v2_to_v3(conn)?;
        set_schema_version(conn, 3)?;
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

// ============================================================================
// Prompt-GitHub Correlation (Phase 2: Correlation & Context)
// ============================================================================

/// GitHub event types for prompt correlation
/// These map to specific GitHub CLI operations detected in terminal output
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PromptGitHubEventType {
    /// Issue was claimed (label changed to loom:building)
    IssueClaimed,
    /// New PR was created
    PrCreated,
    /// PR was merged
    PrMerged,
    /// PR was closed without merge
    PrClosed,
    /// Label was added to issue or PR
    LabelAdded,
    /// Label was removed from issue or PR
    LabelRemoved,
    /// Issue was closed
    IssueClosed,
    /// Issue was reopened
    IssueReopened,
    /// PR review was submitted
    PrReviewed,
    /// PR changes were requested
    PrChangesRequested,
    /// PR was approved
    PrApproved,
}

impl std::fmt::Display for PromptGitHubEventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::IssueClaimed => "issue_claimed",
            Self::PrCreated => "pr_created",
            Self::PrMerged => "pr_merged",
            Self::PrClosed => "pr_closed",
            Self::LabelAdded => "label_added",
            Self::LabelRemoved => "label_removed",
            Self::IssueClosed => "issue_closed",
            Self::IssueReopened => "issue_reopened",
            Self::PrReviewed => "pr_reviewed",
            Self::PrChangesRequested => "pr_changes_requested",
            Self::PrApproved => "pr_approved",
        };
        write!(f, "{s}")
    }
}

/// Entry for prompt-GitHub correlation
#[derive(Debug, Serialize, Deserialize)]
pub struct PromptGitHubEntry {
    pub id: Option<i64>,
    pub activity_id: i64,
    pub issue_number: Option<i32>,
    pub pr_number: Option<i32>,
    pub label_before: Option<String>,
    pub label_after: Option<String>,
    pub event_type: String,
    pub event_time: String,
}

/// Log a prompt-GitHub correlation entry
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn log_prompt_github(
    workspace_path: String,
    activity_id: i64,
    event_type: String,
    issue_number: Option<i32>,
    pr_number: Option<i32>,
    label_before: Option<String>,
    label_after: Option<String>,
) -> Result<i64, String> {
    let conn =
        open_activity_db(&workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    conn.execute(
        "INSERT INTO prompt_github (
            activity_id, issue_number, pr_number, label_before, label_after, event_type, event_time
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'))",
        params![
            activity_id,
            issue_number,
            pr_number,
            label_before,
            label_after,
            event_type
        ],
    )
    .map_err(|e| format!("Failed to log prompt-GitHub correlation: {e}"))?;

    Ok(conn.last_insert_rowid())
}

/// Query prompt-GitHub correlations for a specific issue
#[tauri::command]
pub fn get_prompts_for_issue(
    workspace_path: &str,
    issue_number: i32,
) -> Result<Vec<PromptGitHubEntry>, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let mut stmt = conn
        .prepare(
            "SELECT id, activity_id, issue_number, pr_number, label_before, label_after, event_type, event_time
             FROM prompt_github
             WHERE issue_number = ?1
             ORDER BY event_time ASC",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let entries = stmt
        .query_map([issue_number], |row| {
            Ok(PromptGitHubEntry {
                id: row.get(0)?,
                activity_id: row.get(1)?,
                issue_number: row.get(2)?,
                pr_number: row.get(3)?,
                label_before: row.get(4)?,
                label_after: row.get(5)?,
                event_type: row.get(6)?,
                event_time: row.get(7)?,
            })
        })
        .map_err(|e| format!("Failed to query prompt-GitHub entries: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect entries: {e}"))?;

    Ok(entries)
}

/// Query prompt-GitHub correlations for a specific PR
#[tauri::command]
pub fn get_prompts_for_pr(
    workspace_path: &str,
    pr_number: i32,
) -> Result<Vec<PromptGitHubEntry>, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let mut stmt = conn
        .prepare(
            "SELECT id, activity_id, issue_number, pr_number, label_before, label_after, event_type, event_time
             FROM prompt_github
             WHERE pr_number = ?1
             ORDER BY event_time ASC",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let entries = stmt
        .query_map([pr_number], |row| {
            Ok(PromptGitHubEntry {
                id: row.get(0)?,
                activity_id: row.get(1)?,
                issue_number: row.get(2)?,
                pr_number: row.get(3)?,
                label_before: row.get(4)?,
                label_after: row.get(5)?,
                event_type: row.get(6)?,
                event_time: row.get(7)?,
            })
        })
        .map_err(|e| format!("Failed to query prompt-GitHub entries: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect entries: {e}"))?;

    Ok(entries)
}

/// Issue resolution cost summary
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
        .map(|id| id.to_string())
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
            .map(|(_, time, _)| time.clone())
            .unwrap_or_else(|| events[0].1.clone());

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

        let prompt_count = activity_ids.len() as i32;

        // Calculate total cost
        let total_cost = if activity_ids.is_empty() {
            0.0
        } else {
            let id_list = activity_ids
                .iter()
                .map(|id| id.to_string())
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

    let issues_resolved = resolved_issues.len() as i32;

    // Calculate totals across all resolved issues
    let issue_list = resolved_issues
        .iter()
        .map(|id| id.to_string())
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

    let total_prompts = activity_ids.len() as i32;

    let total_cost = if activity_ids.is_empty() {
        0.0
    } else {
        let id_list = activity_ids
            .iter()
            .map(|id| id.to_string())
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
