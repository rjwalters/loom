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

/// Migrate schema from v3 to v4
/// Adds velocity_snapshots table for daily velocity tracking
fn migrate_v3_to_v4(conn: &Connection) -> SqliteResult<()> {
    // Create velocity_snapshots table for daily metrics
    conn.execute(
        "CREATE TABLE IF NOT EXISTS velocity_snapshots (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            snapshot_date DATE NOT NULL UNIQUE,
            issues_closed INTEGER NOT NULL DEFAULT 0,
            prs_merged INTEGER NOT NULL DEFAULT 0,
            avg_cycle_time_hours REAL,
            total_prompts INTEGER NOT NULL DEFAULT 0,
            total_cost_usd REAL NOT NULL DEFAULT 0,
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        )",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_velocity_snapshots_date ON velocity_snapshots(snapshot_date)",
        [],
    )?;

    Ok(())
}

/// Migrate schema from v4 to v5
/// Adds prompt_patterns and pattern_matches tables for pattern catalog
fn migrate_v4_to_v5(conn: &Connection) -> SqliteResult<()> {
    // Create prompt_patterns table for storing extracted patterns
    conn.execute(
        "CREATE TABLE IF NOT EXISTS prompt_patterns (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            pattern_text TEXT NOT NULL,
            category TEXT,
            times_used INTEGER DEFAULT 0,
            success_count INTEGER DEFAULT 0,
            failure_count INTEGER DEFAULT 0,
            success_rate REAL DEFAULT 0.0,
            avg_cost_usd REAL DEFAULT 0.0,
            avg_duration_seconds INTEGER DEFAULT 0,
            avg_tokens INTEGER DEFAULT 0,
            created_at TEXT DEFAULT (datetime('now')),
            last_used_at TEXT,
            UNIQUE(pattern_text)
        )",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_prompt_patterns_category ON prompt_patterns(category)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_prompt_patterns_success_rate ON prompt_patterns(success_rate DESC)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_prompt_patterns_times_used ON prompt_patterns(times_used DESC)",
        [],
    )?;

    // Create pattern_matches table for linking patterns to activities
    conn.execute(
        "CREATE TABLE IF NOT EXISTS pattern_matches (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            pattern_id INTEGER NOT NULL,
            activity_id INTEGER NOT NULL,
            similarity_score REAL DEFAULT 1.0,
            matched_at TEXT DEFAULT (datetime('now')),
            FOREIGN KEY (pattern_id) REFERENCES prompt_patterns(id),
            FOREIGN KEY (activity_id) REFERENCES agent_activity(id)
        )",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_pattern_matches_pattern_id ON pattern_matches(pattern_id)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_pattern_matches_activity_id ON pattern_matches(activity_id)",
        [],
    )?;

    Ok(())
}

/// Migrate schema from v5 to v6
/// Adds recommendations and recommendation_rules tables for the recommendation engine
fn migrate_v5_to_v6(conn: &Connection) -> SqliteResult<()> {
    // Create recommendations table for storing generated recommendations
    conn.execute(
        "CREATE TABLE IF NOT EXISTS recommendations (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            type TEXT NOT NULL,
            title TEXT NOT NULL,
            description TEXT,
            confidence REAL DEFAULT 0.0,
            evidence TEXT,
            context_role TEXT,
            context_task_type TEXT,
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
            dismissed_at TIMESTAMP,
            acted_on INTEGER DEFAULT 0
        )",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_recommendations_type ON recommendations(type)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_recommendations_created_at ON recommendations(created_at DESC)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_recommendations_dismissed ON recommendations(dismissed_at)",
        [],
    )?;

    // Create recommendation_rules table for configurable rule definitions
    conn.execute(
        "CREATE TABLE IF NOT EXISTS recommendation_rules (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL UNIQUE,
            rule_type TEXT NOT NULL,
            description TEXT,
            threshold_value REAL,
            threshold_count INTEGER,
            recommendation_template TEXT NOT NULL,
            priority INTEGER DEFAULT 5,
            enabled INTEGER DEFAULT 1,
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
            updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        )",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_recommendation_rules_type ON recommendation_rules(rule_type)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_recommendation_rules_enabled ON recommendation_rules(enabled)",
        [],
    )?;

    // Insert default recommendation rules
    insert_default_recommendation_rules(conn)?;

    Ok(())
}

/// Insert default recommendation rules
fn insert_default_recommendation_rules(conn: &Connection) -> SqliteResult<()> {
    // Rule 1: Low success pattern warning
    conn.execute(
        "INSERT OR IGNORE INTO recommendation_rules (name, rule_type, description, threshold_value, threshold_count, recommendation_template, priority)
         VALUES ('low_success_pattern', 'warning', 'Warns about patterns with low success rate', 0.5, 5, 'Pattern \"{{pattern}}\" has only {{success_rate}}% success rate across {{uses}} uses. Consider revising this approach.', 1)",
        [],
    )?;

    // Rule 2: High cost feature alert
    conn.execute(
        "INSERT OR IGNORE INTO recommendation_rules (name, rule_type, description, threshold_value, threshold_count, recommendation_template, priority)
         VALUES ('high_cost_alert', 'cost', 'Alerts when a feature costs significantly more than average', 2.0, 3, 'This task type costs {{cost_multiplier}}x the average ({{actual_cost}} vs {{avg_cost}}). Consider optimizing the approach.', 2)",
        [],
    )?;

    // Rule 3: Optimal timing suggestion
    conn.execute(
        "INSERT OR IGNORE INTO recommendation_rules (name, rule_type, description, threshold_value, threshold_count, recommendation_template, priority)
         VALUES ('optimal_timing', 'timing', 'Suggests optimal times based on success correlation', 0.7, 10, 'Tasks of this type have {{success_rate}}% higher success rate between {{start_hour}} and {{end_hour}}.', 3)",
        [],
    )?;

    // Rule 4: Similar successful prompt suggestion
    conn.execute(
        "INSERT OR IGNORE INTO recommendation_rules (name, rule_type, description, threshold_value, threshold_count, recommendation_template, priority)
         VALUES ('similar_prompt', 'prompt', 'Suggests similar prompts that had higher success', 0.8, 3, 'Similar successful prompt: \"{{similar_prompt}}\" ({{success_rate}}% success rate, {{uses}} uses)', 4)",
        [],
    )?;

    // Rule 5: Role effectiveness suggestion
    conn.execute(
        "INSERT OR IGNORE INTO recommendation_rules (name, rule_type, description, threshold_value, threshold_count, recommendation_template, priority)
         VALUES ('role_effectiveness', 'role', 'Suggests which role is most effective for task types', 0.75, 5, '{{role}} has {{success_rate}}% success rate for this task type, vs {{current_rate}}% overall. Consider using {{role}} for similar tasks.', 5)",
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

    // Migrate to v4 if needed
    if current_version < 4 {
        migrate_v3_to_v4(conn)?;
        set_schema_version(conn, 4)?;
    }

    // Migrate to v5 if needed
    if current_version < 5 {
        migrate_v4_to_v5(conn)?;
        set_schema_version(conn, 5)?;
    }

    // Migrate to v6 if needed
    if current_version < 6 {
        migrate_v5_to_v6(conn)?;
        set_schema_version(conn, 6)?;
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

// ============================================================================
// Velocity Tracking and Trend Analysis (Phase 3: Intelligence & Learning)
// ============================================================================

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

// ============================================================================
// Prompt Pattern Catalog (Phase 3: Intelligence & Learning)
// ============================================================================

/// Prompt pattern categories based on agent roles and task types
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PatternCategory {
    /// Building features - issue implementation
    Build,
    /// Fixing bugs or addressing feedback
    Fix,
    /// Code refactoring and cleanup
    Refactor,
    /// Code review and quality checks
    Review,
    /// Issue curation and enhancement
    Curate,
    /// Architecture and design proposals
    Architect,
    /// Code simplification proposals
    Simplify,
    /// General/uncategorized patterns
    General,
}

impl std::fmt::Display for PatternCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Build => "build",
            Self::Fix => "fix",
            Self::Refactor => "refactor",
            Self::Review => "review",
            Self::Curate => "curate",
            Self::Architect => "architect",
            Self::Simplify => "simplify",
            Self::General => "general",
        };
        write!(f, "{s}")
    }
}

impl PatternCategory {
    /// Infer category from role name
    fn from_role(role: &str) -> Self {
        match role.to_lowercase().as_str() {
            "builder" => Self::Build,
            "doctor" => Self::Fix,
            "judge" => Self::Review,
            "curator" => Self::Curate,
            "architect" => Self::Architect,
            "hermit" => Self::Simplify,
            _ => Self::General,
        }
    }
}

/// A prompt pattern extracted from activity data
#[derive(Debug, Serialize, Deserialize)]
pub struct PromptPattern {
    pub id: Option<i64>,
    pub pattern_text: String,
    pub category: Option<String>,
    pub times_used: i32,
    pub success_count: i32,
    pub failure_count: i32,
    pub success_rate: f64,
    pub avg_cost_usd: f64,
    pub avg_duration_seconds: i32,
    pub avg_tokens: i32,
    pub created_at: Option<String>,
    pub last_used_at: Option<String>,
}

/// A match between a pattern and an activity
#[derive(Debug, Serialize, Deserialize)]
pub struct PatternMatch {
    pub id: Option<i64>,
    pub pattern_id: i64,
    pub activity_id: i64,
    pub similarity_score: f64,
    pub matched_at: Option<String>,
}

/// Summary of pattern extraction results
#[derive(Debug, Serialize, Deserialize)]
pub struct PatternExtractionResult {
    pub patterns_created: i32,
    pub patterns_updated: i32,
    pub activities_processed: i32,
    pub matches_created: i32,
}

/// Normalize a trigger/prompt text into a pattern
/// - Strips issue/PR numbers (e.g., #123 -> #N)
/// - Normalizes whitespace
/// - Lowercases for comparison
fn normalize_prompt_to_pattern(prompt: &str) -> String {
    // Replace issue/PR numbers with placeholder
    let pattern = regex::Regex::new(r"#\d+")
        .map(|re| re.replace_all(prompt, "#N").to_string())
        .unwrap_or_else(|_| prompt.to_string());

    // Normalize whitespace
    let pattern = regex::Regex::new(r"\s+")
        .map(|re| re.replace_all(&pattern, " ").to_string())
        .unwrap_or(pattern);

    // Trim and lowercase
    pattern.trim().to_lowercase()
}

/// Extract patterns from historical activity data
#[tauri::command]
pub fn extract_prompt_patterns(workspace_path: &str) -> Result<PatternExtractionResult, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let mut patterns_created = 0;
    let mut patterns_updated = 0;
    let mut activities_processed = 0;
    let mut matches_created = 0;

    // Get all activities with their outcomes and token usage
    let mut stmt = conn
        .prepare(
            "SELECT a.id, a.trigger, a.role, a.work_found, a.work_completed, a.duration_ms,
                    COALESCE(t.total_tokens, 0), COALESCE(t.prompt_tokens, 0), COALESCE(t.completion_tokens, 0)
             FROM agent_activity a
             LEFT JOIN token_usage t ON a.id = t.activity_id
             WHERE a.trigger IS NOT NULL AND a.trigger != ''
             ORDER BY a.timestamp ASC",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let activities: Vec<(i64, String, String, bool, Option<bool>, Option<i32>, i64, i64, i64)> =
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get::<_, i32>(3)? != 0,
                row.get::<_, Option<i32>>(4)?.map(|i| i != 0),
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
            ))
        })
        .map_err(|e| format!("Failed to query activities: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect activities: {e}"))?;

    for (
        activity_id,
        trigger,
        role,
        work_found,
        work_completed,
        duration_ms,
        total_tokens,
        prompt_tokens,
        completion_tokens,
    ) in activities
    {
        activities_processed += 1;

        // Normalize the prompt to a pattern
        let pattern_text = normalize_prompt_to_pattern(&trigger);
        if pattern_text.is_empty() {
            continue;
        }

        // Determine category from role
        let category = PatternCategory::from_role(&role).to_string();

        // Determine success (work_found AND work_completed)
        let is_success = work_found && work_completed.unwrap_or(false);

        // Calculate cost
        let cost = calculate_cost(prompt_tokens, completion_tokens);

        // Try to find existing pattern
        let existing_pattern: Option<(i64, i32, i32, i32, i64, i64, i64)> = conn
            .query_row(
                "SELECT id, times_used, success_count, failure_count,
                        CAST(avg_cost_usd * times_used * 100 AS INTEGER),
                        avg_duration_seconds * times_used,
                        avg_tokens * times_used
                 FROM prompt_patterns
                 WHERE pattern_text = ?1",
                [&pattern_text],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .ok();

        let pattern_id = if let Some((
            id,
            times_used,
            success_count,
            failure_count,
            total_cost_cents,
            total_duration,
            total_tok,
        )) = existing_pattern
        {
            // Update existing pattern
            let new_times_used = times_used + 1;
            let new_success_count = if is_success {
                success_count + 1
            } else {
                success_count
            };
            let new_failure_count = if !is_success {
                failure_count + 1
            } else {
                failure_count
            };
            let new_success_rate = f64::from(new_success_count) / f64::from(new_times_used);
            let new_total_cost_cents = total_cost_cents + (cost * 100.0) as i64;
            let new_avg_cost = (new_total_cost_cents as f64 / 100.0) / f64::from(new_times_used);
            let new_total_duration = total_duration + i64::from(duration_ms.unwrap_or(0) / 1000);
            let new_avg_duration = (new_total_duration / i64::from(new_times_used)) as i32;
            let new_total_tokens = total_tok + total_tokens;
            let new_avg_tokens = (new_total_tokens / i64::from(new_times_used)) as i32;

            conn.execute(
                "UPDATE prompt_patterns SET
                    times_used = ?1,
                    success_count = ?2,
                    failure_count = ?3,
                    success_rate = ?4,
                    avg_cost_usd = ?5,
                    avg_duration_seconds = ?6,
                    avg_tokens = ?7,
                    last_used_at = datetime('now')
                 WHERE id = ?8",
                params![
                    new_times_used,
                    new_success_count,
                    new_failure_count,
                    new_success_rate,
                    new_avg_cost,
                    new_avg_duration,
                    new_avg_tokens,
                    id
                ],
            )
            .map_err(|e| format!("Failed to update pattern: {e}"))?;

            patterns_updated += 1;
            id
        } else {
            // Create new pattern
            let success_count = if is_success { 1 } else { 0 };
            let failure_count = if !is_success { 1 } else { 0 };
            let success_rate = if is_success { 1.0 } else { 0.0 };

            conn.execute(
                "INSERT INTO prompt_patterns (
                    pattern_text, category, times_used, success_count, failure_count,
                    success_rate, avg_cost_usd, avg_duration_seconds, avg_tokens, last_used_at
                 ) VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, ?7, ?8, datetime('now'))",
                params![
                    &pattern_text,
                    &category,
                    success_count,
                    failure_count,
                    success_rate,
                    cost,
                    duration_ms.unwrap_or(0) / 1000,
                    total_tokens as i32
                ],
            )
            .map_err(|e| format!("Failed to insert pattern: {e}"))?;

            patterns_created += 1;
            conn.last_insert_rowid()
        };

        // Check if match already exists
        let match_exists: bool = conn
            .query_row(
                "SELECT 1 FROM pattern_matches WHERE pattern_id = ?1 AND activity_id = ?2",
                params![pattern_id, activity_id],
                |_| Ok(true),
            )
            .unwrap_or(false);

        if !match_exists {
            // Create pattern match
            conn.execute(
                "INSERT INTO pattern_matches (pattern_id, activity_id, similarity_score)
                 VALUES (?1, ?2, 1.0)",
                params![pattern_id, activity_id],
            )
            .map_err(|e| format!("Failed to insert pattern match: {e}"))?;

            matches_created += 1;
        }
    }

    Ok(PatternExtractionResult {
        patterns_created,
        patterns_updated,
        activities_processed,
        matches_created,
    })
}

/// Get patterns by category
#[tauri::command]
pub fn get_patterns_by_category(
    workspace_path: &str,
    category: &str,
    limit: Option<i32>,
) -> Result<Vec<PromptPattern>, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let limit = limit.unwrap_or(50);

    let mut stmt = conn
        .prepare(
            "SELECT id, pattern_text, category, times_used, success_count, failure_count,
                    success_rate, avg_cost_usd, avg_duration_seconds, avg_tokens, created_at, last_used_at
             FROM prompt_patterns
             WHERE category = ?1
             ORDER BY success_rate DESC, times_used DESC
             LIMIT ?2",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let patterns = stmt
        .query_map(params![category, limit], |row| {
            Ok(PromptPattern {
                id: row.get(0)?,
                pattern_text: row.get(1)?,
                category: row.get(2)?,
                times_used: row.get(3)?,
                success_count: row.get(4)?,
                failure_count: row.get(5)?,
                success_rate: row.get(6)?,
                avg_cost_usd: row.get(7)?,
                avg_duration_seconds: row.get(8)?,
                avg_tokens: row.get(9)?,
                created_at: row.get(10)?,
                last_used_at: row.get(11)?,
            })
        })
        .map_err(|e| format!("Failed to query patterns: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect patterns: {e}"))?;

    Ok(patterns)
}

/// Get top patterns sorted by a specific metric
#[tauri::command]
pub fn get_top_patterns(
    workspace_path: &str,
    sort_by: &str,
    limit: Option<i32>,
) -> Result<Vec<PromptPattern>, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let limit = limit.unwrap_or(20);

    let order_clause = match sort_by {
        "success_rate" => "success_rate DESC, times_used DESC",
        "times_used" => "times_used DESC, success_rate DESC",
        "cost" => "avg_cost_usd ASC, success_rate DESC",
        "tokens" => "avg_tokens ASC, success_rate DESC",
        "recent" => "last_used_at DESC",
        _ => "success_rate DESC, times_used DESC",
    };

    let query = format!(
        "SELECT id, pattern_text, category, times_used, success_count, failure_count,
                success_rate, avg_cost_usd, avg_duration_seconds, avg_tokens, created_at, last_used_at
         FROM prompt_patterns
         WHERE times_used >= 2
         ORDER BY {order_clause}
         LIMIT ?1"
    );

    let mut stmt = conn
        .prepare(&query)
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let patterns = stmt
        .query_map([limit], |row| {
            Ok(PromptPattern {
                id: row.get(0)?,
                pattern_text: row.get(1)?,
                category: row.get(2)?,
                times_used: row.get(3)?,
                success_count: row.get(4)?,
                failure_count: row.get(5)?,
                success_rate: row.get(6)?,
                avg_cost_usd: row.get(7)?,
                avg_duration_seconds: row.get(8)?,
                avg_tokens: row.get(9)?,
                created_at: row.get(10)?,
                last_used_at: row.get(11)?,
            })
        })
        .map_err(|e| format!("Failed to query patterns: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect patterns: {e}"))?;

    Ok(patterns)
}

/// Find patterns similar to a given prompt text
#[tauri::command]
pub fn find_similar_patterns(
    workspace_path: &str,
    prompt_text: &str,
    limit: Option<i32>,
) -> Result<Vec<PromptPattern>, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let limit = limit.unwrap_or(10);

    // Normalize the input prompt
    let normalized = normalize_prompt_to_pattern(prompt_text);

    // Extract key words for fuzzy matching (words with 3+ chars)
    let words: Vec<&str> = normalized
        .split_whitespace()
        .filter(|w| w.len() >= 3)
        .collect();

    if words.is_empty() {
        return Ok(Vec::new());
    }

    // Build LIKE conditions for each word
    let like_conditions: Vec<String> = words
        .iter()
        .map(|w| format!("pattern_text LIKE '%{w}%'"))
        .collect();

    let where_clause = like_conditions.join(" OR ");

    let query = format!(
        "SELECT id, pattern_text, category, times_used, success_count, failure_count,
                success_rate, avg_cost_usd, avg_duration_seconds, avg_tokens, created_at, last_used_at
         FROM prompt_patterns
         WHERE {where_clause}
         ORDER BY success_rate DESC, times_used DESC
         LIMIT ?1"
    );

    let mut stmt = conn
        .prepare(&query)
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let patterns = stmt
        .query_map([limit], |row| {
            Ok(PromptPattern {
                id: row.get(0)?,
                pattern_text: row.get(1)?,
                category: row.get(2)?,
                times_used: row.get(3)?,
                success_count: row.get(4)?,
                failure_count: row.get(5)?,
                success_rate: row.get(6)?,
                avg_cost_usd: row.get(7)?,
                avg_duration_seconds: row.get(8)?,
                avg_tokens: row.get(9)?,
                created_at: row.get(10)?,
                last_used_at: row.get(11)?,
            })
        })
        .map_err(|e| format!("Failed to query patterns: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect patterns: {e}"))?;

    Ok(patterns)
}

/// Get pattern catalog statistics
#[derive(Debug, Serialize, Deserialize)]
pub struct PatternCatalogStats {
    pub total_patterns: i32,
    pub patterns_by_category: Vec<CategoryCount>,
    pub avg_success_rate: f64,
    pub most_successful_category: Option<String>,
    pub most_used_pattern: Option<String>,
    pub total_activities_matched: i32,
}

/// Category count for statistics
#[derive(Debug, Serialize, Deserialize)]
pub struct CategoryCount {
    pub category: String,
    pub count: i32,
    pub avg_success_rate: f64,
}

/// Get statistics about the pattern catalog
#[tauri::command]
pub fn get_pattern_catalog_stats(workspace_path: &str) -> Result<PatternCatalogStats, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    // Total patterns
    let total_patterns: i32 = conn
        .query_row("SELECT COUNT(*) FROM prompt_patterns", [], |row| row.get(0))
        .unwrap_or(0);

    // Patterns by category with avg success rate
    let mut stmt = conn
        .prepare(
            "SELECT category, COUNT(*), AVG(success_rate)
             FROM prompt_patterns
             GROUP BY category
             ORDER BY COUNT(*) DESC",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let patterns_by_category: Vec<CategoryCount> = stmt
        .query_map([], |row| {
            Ok(CategoryCount {
                category: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                count: row.get(1)?,
                avg_success_rate: row.get(2)?,
            })
        })
        .map_err(|e| format!("Failed to query categories: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect categories: {e}"))?;

    // Average success rate across all patterns
    let avg_success_rate: f64 = conn
        .query_row(
            "SELECT AVG(success_rate) FROM prompt_patterns WHERE times_used >= 2",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0.0);

    // Most successful category (by avg success rate, min 5 patterns)
    let most_successful_category: Option<String> = conn
        .query_row(
            "SELECT category FROM prompt_patterns
             GROUP BY category
             HAVING COUNT(*) >= 5
             ORDER BY AVG(success_rate) DESC
             LIMIT 1",
            [],
            |row| row.get(0),
        )
        .ok();

    // Most used pattern
    let most_used_pattern: Option<String> = conn
        .query_row(
            "SELECT pattern_text FROM prompt_patterns ORDER BY times_used DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .ok();

    // Total activities matched
    let total_activities_matched: i32 = conn
        .query_row("SELECT COUNT(DISTINCT activity_id) FROM pattern_matches", [], |row| row.get(0))
        .unwrap_or(0);

    Ok(PatternCatalogStats {
        total_patterns,
        patterns_by_category,
        avg_success_rate,
        most_successful_category,
        most_used_pattern,
        total_activities_matched,
    })
}

/// Record that a pattern was used (for tracking when patterns are applied)
#[tauri::command]
pub fn record_pattern_usage(
    workspace_path: String,
    pattern_id: i64,
    activity_id: i64,
    was_successful: bool,
) -> Result<(), String> {
    let conn =
        open_activity_db(&workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    // Update pattern statistics
    if was_successful {
        conn.execute(
            "UPDATE prompt_patterns SET
                times_used = times_used + 1,
                success_count = success_count + 1,
                success_rate = CAST(success_count + 1 AS REAL) / (times_used + 1),
                last_used_at = datetime('now')
             WHERE id = ?1",
            [pattern_id],
        )
        .map_err(|e| format!("Failed to update pattern: {e}"))?;
    } else {
        conn.execute(
            "UPDATE prompt_patterns SET
                times_used = times_used + 1,
                failure_count = failure_count + 1,
                success_rate = CAST(success_count AS REAL) / (times_used + 1),
                last_used_at = datetime('now')
             WHERE id = ?1",
            [pattern_id],
        )
        .map_err(|e| format!("Failed to update pattern: {e}"))?;
    }

    // Create pattern match
    conn.execute(
        "INSERT INTO pattern_matches (pattern_id, activity_id, similarity_score)
         VALUES (?1, ?2, 1.0)",
        params![pattern_id, activity_id],
    )
    .map_err(|e| format!("Failed to insert pattern match: {e}"))?;

    Ok(())
}

// ============================================================================
// Recommendation Engine (Phase 3: Intelligence & Learning)
// ============================================================================

/// Recommendation types based on what kind of suggestion is being made
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RecommendationType {
    /// Prompt suggestions for better results
    Prompt,
    /// Optimal timing suggestions
    Timing,
    /// Role assignment suggestions
    Role,
    /// Anti-pattern warnings
    Warning,
    /// Cost optimization alerts
    Cost,
}

impl std::fmt::Display for RecommendationType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Prompt => "prompt",
            Self::Timing => "timing",
            Self::Role => "role",
            Self::Warning => "warning",
            Self::Cost => "cost",
        };
        write!(f, "{s}")
    }
}

/// A generated recommendation
#[derive(Debug, Serialize, Deserialize)]
pub struct Recommendation {
    pub id: Option<i64>,
    pub recommendation_type: String,
    pub title: String,
    pub description: Option<String>,
    pub confidence: f64,
    pub evidence: Option<String>,
    pub context_role: Option<String>,
    pub context_task_type: Option<String>,
    pub created_at: Option<String>,
    pub dismissed_at: Option<String>,
    pub acted_on: bool,
}

/// A recommendation rule configuration
#[derive(Debug, Serialize, Deserialize)]
pub struct RecommendationRule {
    pub id: Option<i64>,
    pub name: String,
    pub rule_type: String,
    pub description: Option<String>,
    pub threshold_value: Option<f64>,
    pub threshold_count: Option<i32>,
    pub recommendation_template: String,
    pub priority: i32,
    pub enabled: bool,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

/// Result of recommendation generation
#[derive(Debug, Serialize, Deserialize)]
pub struct RecommendationGenerationResult {
    pub recommendations_created: i32,
    pub rules_evaluated: i32,
    pub patterns_analyzed: i32,
}

/// Get all active (non-dismissed) recommendations
#[tauri::command]
pub fn get_active_recommendations(
    workspace_path: &str,
    limit: Option<i32>,
) -> Result<Vec<Recommendation>, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let limit = limit.unwrap_or(50);

    let mut stmt = conn
        .prepare(
            "SELECT id, type, title, description, confidence, evidence, context_role, context_task_type, created_at, dismissed_at, acted_on
             FROM recommendations
             WHERE dismissed_at IS NULL
             ORDER BY confidence DESC, created_at DESC
             LIMIT ?1",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let recommendations = stmt
        .query_map([limit], |row| {
            Ok(Recommendation {
                id: row.get(0)?,
                recommendation_type: row.get(1)?,
                title: row.get(2)?,
                description: row.get(3)?,
                confidence: row.get(4)?,
                evidence: row.get(5)?,
                context_role: row.get(6)?,
                context_task_type: row.get(7)?,
                created_at: row.get(8)?,
                dismissed_at: row.get(9)?,
                acted_on: row.get::<_, i32>(10)? != 0,
            })
        })
        .map_err(|e| format!("Failed to query recommendations: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect recommendations: {e}"))?;

    Ok(recommendations)
}

/// Get recommendations filtered by context (role and/or task type)
#[tauri::command]
pub fn get_recommendations_for_context(
    workspace_path: &str,
    role: Option<String>,
    task_type: Option<String>,
    limit: Option<i32>,
) -> Result<Vec<Recommendation>, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let limit_val = limit.unwrap_or(20);

    // Build query based on provided context
    let mut stmt = conn
        .prepare(
            "SELECT id, type, title, description, confidence, evidence, context_role, context_task_type, created_at, dismissed_at, acted_on
             FROM recommendations
             WHERE dismissed_at IS NULL
               AND (context_role IS NULL OR context_role = ?1 OR ?1 IS NULL)
               AND (context_task_type IS NULL OR context_task_type = ?2 OR ?2 IS NULL)
             ORDER BY confidence DESC, created_at DESC
             LIMIT ?3",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let recommendations = stmt
        .query_map(params![role, task_type, limit_val], |row| {
            Ok(Recommendation {
                id: row.get(0)?,
                recommendation_type: row.get(1)?,
                title: row.get(2)?,
                description: row.get(3)?,
                confidence: row.get(4)?,
                evidence: row.get(5)?,
                context_role: row.get(6)?,
                context_task_type: row.get(7)?,
                created_at: row.get(8)?,
                dismissed_at: row.get(9)?,
                acted_on: row.get::<_, i32>(10)? != 0,
            })
        })
        .map_err(|e| format!("Failed to query recommendations: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect recommendations: {e}"))?;

    Ok(recommendations)
}

/// Dismiss a recommendation
#[tauri::command]
pub fn dismiss_recommendation(
    workspace_path: String,
    recommendation_id: i64,
) -> Result<(), String> {
    let conn =
        open_activity_db(&workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    conn.execute(
        "UPDATE recommendations SET dismissed_at = datetime('now') WHERE id = ?1",
        [recommendation_id],
    )
    .map_err(|e| format!("Failed to dismiss recommendation: {e}"))?;

    Ok(())
}

/// Mark a recommendation as acted upon
#[tauri::command]
pub fn mark_recommendation_acted_on(
    workspace_path: String,
    recommendation_id: i64,
) -> Result<(), String> {
    let conn =
        open_activity_db(&workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    conn.execute("UPDATE recommendations SET acted_on = 1 WHERE id = ?1", [recommendation_id])
        .map_err(|e| format!("Failed to mark recommendation as acted on: {e}"))?;

    Ok(())
}

/// Get all recommendation rules
#[tauri::command]
pub fn get_recommendation_rules(workspace_path: &str) -> Result<Vec<RecommendationRule>, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let mut stmt = conn
        .prepare(
            "SELECT id, name, rule_type, description, threshold_value, threshold_count, recommendation_template, priority, enabled, created_at, updated_at
             FROM recommendation_rules
             ORDER BY priority ASC",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let rules = stmt
        .query_map([], |row| {
            Ok(RecommendationRule {
                id: row.get(0)?,
                name: row.get(1)?,
                rule_type: row.get(2)?,
                description: row.get(3)?,
                threshold_value: row.get(4)?,
                threshold_count: row.get(5)?,
                recommendation_template: row.get(6)?,
                priority: row.get(7)?,
                enabled: row.get::<_, i32>(8)? != 0,
                created_at: row.get(9)?,
                updated_at: row.get(10)?,
            })
        })
        .map_err(|e| format!("Failed to query rules: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect rules: {e}"))?;

    Ok(rules)
}

/// Update a recommendation rule
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn update_recommendation_rule(
    workspace_path: String,
    rule_id: i64,
    threshold_value: Option<f64>,
    threshold_count: Option<i32>,
    priority: Option<i32>,
    enabled: Option<bool>,
) -> Result<(), String> {
    let conn =
        open_activity_db(&workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    // Build dynamic update based on provided values
    if let Some(tv) = threshold_value {
        conn.execute(
            "UPDATE recommendation_rules SET threshold_value = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![tv, rule_id],
        )
        .map_err(|e| format!("Failed to update threshold_value: {e}"))?;
    }

    if let Some(tc) = threshold_count {
        conn.execute(
            "UPDATE recommendation_rules SET threshold_count = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![tc, rule_id],
        )
        .map_err(|e| format!("Failed to update threshold_count: {e}"))?;
    }

    if let Some(p) = priority {
        conn.execute(
            "UPDATE recommendation_rules SET priority = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![p, rule_id],
        )
        .map_err(|e| format!("Failed to update priority: {e}"))?;
    }

    if let Some(e) = enabled {
        conn.execute(
            "UPDATE recommendation_rules SET enabled = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![if e { 1 } else { 0 }, rule_id],
        )
        .map_err(|e| format!("Failed to update enabled: {e}"))?;
    }

    Ok(())
}

/// Generate recommendations from analytics data
/// Evaluates all enabled rules against current data and creates new recommendations
#[tauri::command]
pub fn generate_recommendations(
    workspace_path: &str,
) -> Result<RecommendationGenerationResult, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let mut recommendations_created = 0;
    let mut rules_evaluated = 0;
    let mut patterns_analyzed = 0;

    // Get all enabled rules
    let rules: Vec<(i64, String, String, Option<f64>, Option<i32>, String)> = conn
        .prepare(
            "SELECT id, name, rule_type, threshold_value, threshold_count, recommendation_template
             FROM recommendation_rules
             WHERE enabled = 1
             ORDER BY priority ASC",
        )
        .map_err(|e| format!("Failed to prepare rules query: {e}"))?
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?))
        })
        .map_err(|e| format!("Failed to query rules: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect rules: {e}"))?;

    for (_rule_id, rule_name, rule_type, threshold_value, threshold_count, template) in rules {
        rules_evaluated += 1;

        match rule_type.as_str() {
            "warning" => {
                // Low success pattern warning
                let threshold = threshold_value.unwrap_or(0.5);
                let min_uses = threshold_count.unwrap_or(5);

                let low_success_patterns: Vec<(i64, String, f64, i32)> = conn
                    .prepare(
                        "SELECT id, pattern_text, success_rate, times_used
                         FROM prompt_patterns
                         WHERE success_rate < ?1 AND times_used >= ?2
                         ORDER BY success_rate ASC
                         LIMIT 10",
                    )
                    .map_err(|e| format!("Failed to prepare pattern query: {e}"))?
                    .query_map(params![threshold, min_uses], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                    })
                    .map_err(|e| format!("Failed to query patterns: {e}"))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| format!("Failed to collect patterns: {e}"))?;

                for (pattern_id, pattern_text, success_rate, times_used) in low_success_patterns {
                    patterns_analyzed += 1;

                    // Check if recommendation already exists for this pattern
                    let exists: bool = conn
                        .query_row(
                            "SELECT 1 FROM recommendations
                             WHERE type = 'warning' AND evidence LIKE ?1 AND dismissed_at IS NULL",
                            [format!("%\"pattern_id\":{pattern_id}%")],
                            |_| Ok(true),
                        )
                        .unwrap_or(false);

                    if exists {
                        continue;
                    }

                    let title = format!(
                        "Low success pattern: {}",
                        if pattern_text.len() > 50 {
                            format!("{}...", &pattern_text[..50])
                        } else {
                            pattern_text.clone()
                        }
                    );

                    let description = template
                        .replace("{{pattern}}", &pattern_text)
                        .replace("{{success_rate}}", &format!("{:.0}", success_rate * 100.0))
                        .replace("{{uses}}", &times_used.to_string());

                    let evidence = serde_json::json!({
                        "pattern_id": pattern_id,
                        "pattern_text": pattern_text,
                        "success_rate": success_rate,
                        "times_used": times_used,
                        "rule_name": rule_name
                    })
                    .to_string();

                    // Confidence based on number of uses (more data = higher confidence)
                    let confidence = (1.0 - success_rate) * (times_used as f64 / 20.0).min(1.0);

                    conn.execute(
                        "INSERT INTO recommendations (type, title, description, confidence, evidence, created_at)
                         VALUES ('warning', ?1, ?2, ?3, ?4, datetime('now'))",
                        params![title, description, confidence, evidence],
                    )
                    .map_err(|e| format!("Failed to insert recommendation: {e}"))?;

                    recommendations_created += 1;
                }
            }
            "cost" => {
                // High cost alert
                let cost_multiplier = threshold_value.unwrap_or(2.0);
                let min_occurrences = threshold_count.unwrap_or(3);

                // Get average cost per role
                let avg_costs: Vec<(String, f64, i32)> = conn
                    .prepare(
                        "SELECT a.role,
                                AVG((t.prompt_tokens * 0.003 + t.completion_tokens * 0.015) / 1000.0) as avg_cost,
                                COUNT(*) as count
                         FROM agent_activity a
                         JOIN token_usage t ON a.id = t.activity_id
                         GROUP BY a.role
                         HAVING COUNT(*) >= ?1",
                    )
                    .map_err(|e| format!("Failed to prepare cost query: {e}"))?
                    .query_map([min_occurrences], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                    .map_err(|e| format!("Failed to query costs: {e}"))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| format!("Failed to collect costs: {e}"))?;

                if avg_costs.is_empty() {
                    continue;
                }

                let overall_avg: f64 =
                    avg_costs.iter().map(|(_, c, _)| c).sum::<f64>() / avg_costs.len() as f64;

                for (role, avg_cost, count) in &avg_costs {
                    if *avg_cost > overall_avg * cost_multiplier {
                        let actual_multiplier = avg_cost / overall_avg;

                        // Check if recommendation already exists
                        let exists: bool = conn
                            .query_row(
                                "SELECT 1 FROM recommendations
                                 WHERE type = 'cost' AND context_role = ?1 AND dismissed_at IS NULL",
                                [role],
                                |_| Ok(true),
                            )
                            .unwrap_or(false);

                        if exists {
                            continue;
                        }

                        let title = format!("High cost alert: {role}");
                        let description = template
                            .replace("{{cost_multiplier}}", &format!("{actual_multiplier:.1}"))
                            .replace("{{actual_cost}}", &format!("${avg_cost:.4}"))
                            .replace("{{avg_cost}}", &format!("${overall_avg:.4}"));

                        let evidence = serde_json::json!({
                            "role": role,
                            "avg_cost": avg_cost,
                            "overall_avg": overall_avg,
                            "multiplier": actual_multiplier,
                            "sample_size": count,
                            "rule_name": rule_name
                        })
                        .to_string();

                        let confidence = ((actual_multiplier - 1.0) / cost_multiplier).min(1.0)
                            * (*count as f64 / 50.0).min(1.0);

                        conn.execute(
                            "INSERT INTO recommendations (type, title, description, confidence, evidence, context_role, created_at)
                             VALUES ('cost', ?1, ?2, ?3, ?4, ?5, datetime('now'))",
                            params![title, description, confidence, evidence, role],
                        )
                        .map_err(|e| format!("Failed to insert recommendation: {e}"))?;

                        recommendations_created += 1;
                    }
                }
            }
            "prompt" => {
                // Similar successful prompt suggestion
                let min_success_rate = threshold_value.unwrap_or(0.8);
                let min_uses = threshold_count.unwrap_or(3);

                // Find high-success patterns to suggest
                let successful_patterns: Vec<(i64, String, String, f64, i32)> = conn
                    .prepare(
                        "SELECT id, pattern_text, COALESCE(category, 'general'), success_rate, times_used
                         FROM prompt_patterns
                         WHERE success_rate >= ?1 AND times_used >= ?2
                         ORDER BY success_rate DESC, times_used DESC
                         LIMIT 20",
                    )
                    .map_err(|e| format!("Failed to prepare pattern query: {e}"))?
                    .query_map(params![min_success_rate, min_uses], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
                    })
                    .map_err(|e| format!("Failed to query patterns: {e}"))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| format!("Failed to collect patterns: {e}"))?;

                for (pattern_id, pattern_text, category, success_rate, times_used) in
                    successful_patterns
                {
                    patterns_analyzed += 1;

                    // Check if recommendation already exists
                    let exists: bool = conn
                        .query_row(
                            "SELECT 1 FROM recommendations
                             WHERE type = 'prompt' AND evidence LIKE ?1 AND dismissed_at IS NULL",
                            [format!("%\"pattern_id\":{pattern_id}%")],
                            |_| Ok(true),
                        )
                        .unwrap_or(false);

                    if exists {
                        continue;
                    }

                    let title = format!(
                        "Successful pattern: {}",
                        if pattern_text.len() > 40 {
                            format!("{}...", &pattern_text[..40])
                        } else {
                            pattern_text.clone()
                        }
                    );

                    let description = template
                        .replace("{{similar_prompt}}", &pattern_text)
                        .replace("{{success_rate}}", &format!("{:.0}", success_rate * 100.0))
                        .replace("{{uses}}", &times_used.to_string());

                    let evidence = serde_json::json!({
                        "pattern_id": pattern_id,
                        "pattern_text": pattern_text,
                        "category": category,
                        "success_rate": success_rate,
                        "times_used": times_used,
                        "rule_name": rule_name
                    })
                    .to_string();

                    let confidence = success_rate * (times_used as f64 / 10.0).min(1.0);

                    conn.execute(
                        "INSERT INTO recommendations (type, title, description, confidence, evidence, context_task_type, created_at)
                         VALUES ('prompt', ?1, ?2, ?3, ?4, ?5, datetime('now'))",
                        params![title, description, confidence, evidence, category],
                    )
                    .map_err(|e| format!("Failed to insert recommendation: {e}"))?;

                    recommendations_created += 1;
                }
            }
            "role" => {
                // Role effectiveness suggestion
                let min_success_rate = threshold_value.unwrap_or(0.75);
                let min_uses = threshold_count.unwrap_or(5);

                // Get role success rates
                let role_stats: Vec<(String, f64, i32)> = conn
                    .prepare(
                        "SELECT role,
                                CAST(SUM(CASE WHEN work_found = 1 AND work_completed = 1 THEN 1 ELSE 0 END) AS REAL) /
                                NULLIF(SUM(CASE WHEN work_found = 1 THEN 1 ELSE 0 END), 0) as success_rate,
                                COUNT(*) as count
                         FROM agent_activity
                         WHERE work_found = 1
                         GROUP BY role
                         HAVING COUNT(*) >= ?1 AND success_rate IS NOT NULL",
                    )
                    .map_err(|e| format!("Failed to prepare role query: {e}"))?
                    .query_map([min_uses], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                    .map_err(|e| format!("Failed to query roles: {e}"))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| format!("Failed to collect roles: {e}"))?;

                // Calculate overall success rate
                let (total_success, total_with_work): (i32, i32) = conn
                    .query_row(
                        "SELECT
                            COALESCE(SUM(CASE WHEN work_found = 1 AND work_completed = 1 THEN 1 ELSE 0 END), 0),
                            COALESCE(SUM(CASE WHEN work_found = 1 THEN 1 ELSE 0 END), 1)
                         FROM agent_activity",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .unwrap_or((0, 1));

                let overall_rate = if total_with_work > 0 {
                    total_success as f64 / total_with_work as f64
                } else {
                    0.0
                };

                for (role, success_rate, count) in role_stats {
                    if success_rate >= min_success_rate && success_rate > overall_rate * 1.1 {
                        // Check if recommendation already exists
                        let exists: bool = conn
                            .query_row(
                                "SELECT 1 FROM recommendations
                                 WHERE type = 'role' AND context_role = ?1 AND dismissed_at IS NULL",
                                [&role],
                                |_| Ok(true),
                            )
                            .unwrap_or(false);

                        if exists {
                            continue;
                        }

                        let title = format!("{role} excels at task completion");
                        let description = template
                            .replace("{{role}}", &role)
                            .replace("{{success_rate}}", &format!("{:.0}", success_rate * 100.0))
                            .replace("{{current_rate}}", &format!("{:.0}", overall_rate * 100.0));

                        let evidence = serde_json::json!({
                            "role": role,
                            "success_rate": success_rate,
                            "overall_rate": overall_rate,
                            "sample_size": count,
                            "rule_name": rule_name
                        })
                        .to_string();

                        let confidence = success_rate * (count as f64 / 20.0).min(1.0);

                        conn.execute(
                            "INSERT INTO recommendations (type, title, description, confidence, evidence, context_role, created_at)
                             VALUES ('role', ?1, ?2, ?3, ?4, ?5, datetime('now'))",
                            params![title, description, confidence, evidence, role],
                        )
                        .map_err(|e| format!("Failed to insert recommendation: {e}"))?;

                        recommendations_created += 1;
                    }
                }
            }
            "timing" => {
                // Optimal timing suggestions - analyze success by hour of day
                let min_success_rate = threshold_value.unwrap_or(0.7);
                let min_samples = threshold_count.unwrap_or(10);

                // Get success rate by hour
                let hourly_stats: Vec<(i32, f64, i32)> = conn
                    .prepare(
                        "SELECT
                            CAST(strftime('%H', timestamp) AS INTEGER) as hour,
                            CAST(SUM(CASE WHEN work_found = 1 AND work_completed = 1 THEN 1 ELSE 0 END) AS REAL) /
                            NULLIF(SUM(CASE WHEN work_found = 1 THEN 1 ELSE 0 END), 0) as success_rate,
                            COUNT(*) as count
                         FROM agent_activity
                         WHERE work_found = 1
                         GROUP BY hour
                         HAVING COUNT(*) >= ?1 AND success_rate IS NOT NULL
                         ORDER BY success_rate DESC",
                    )
                    .map_err(|e| format!("Failed to prepare timing query: {e}"))?
                    .query_map([min_samples], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                    .map_err(|e| format!("Failed to query timing: {e}"))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| format!("Failed to collect timing: {e}"))?;

                // Find peak performance hours
                let peak_hours: Vec<_> = hourly_stats
                    .iter()
                    .filter(|(_, sr, _)| *sr >= min_success_rate)
                    .collect();

                if peak_hours.len() >= 2 {
                    // Check if timing recommendation already exists
                    let exists: bool = conn
                        .query_row(
                            "SELECT 1 FROM recommendations
                             WHERE type = 'timing' AND dismissed_at IS NULL",
                            [],
                            |_| Ok(true),
                        )
                        .unwrap_or(false);

                    if !exists {
                        let start_hour = peak_hours.iter().map(|(h, _, _)| *h).min().unwrap_or(9);
                        let end_hour = peak_hours.iter().map(|(h, _, _)| *h).max().unwrap_or(17);
                        let avg_success: f64 = peak_hours.iter().map(|(_, sr, _)| *sr).sum::<f64>()
                            / peak_hours.len() as f64;

                        let title = "Optimal timing identified".to_string();
                        let description = template
                            .replace("{{success_rate}}", &format!("{:.0}", avg_success * 100.0))
                            .replace("{{start_hour}}", &format!("{start_hour}:00"))
                            .replace("{{end_hour}}", &format!("{end_hour}:00"));

                        let evidence = serde_json::json!({
                            "peak_hours": peak_hours.iter().map(|(h, sr, c)| {
                                serde_json::json!({"hour": h, "success_rate": sr, "count": c})
                            }).collect::<Vec<_>>(),
                            "start_hour": start_hour,
                            "end_hour": end_hour,
                            "avg_success_rate": avg_success,
                            "rule_name": rule_name
                        })
                        .to_string();

                        let confidence = avg_success * (peak_hours.len() as f64 / 8.0).min(1.0);

                        conn.execute(
                            "INSERT INTO recommendations (type, title, description, confidence, evidence, created_at)
                             VALUES ('timing', ?1, ?2, ?3, ?4, datetime('now'))",
                            params![title, description, confidence, evidence],
                        )
                        .map_err(|e| format!("Failed to insert recommendation: {e}"))?;

                        recommendations_created += 1;
                    }
                }
            }
            _ => {
                // Unknown rule type, skip
            }
        }
    }

    Ok(RecommendationGenerationResult {
        recommendations_created,
        rules_evaluated,
        patterns_analyzed,
    })
}

/// Get recommendation statistics
#[derive(Debug, Serialize, Deserialize)]
pub struct RecommendationStats {
    pub total_recommendations: i32,
    pub active_recommendations: i32,
    pub dismissed_recommendations: i32,
    pub acted_on_recommendations: i32,
    pub recommendations_by_type: Vec<TypeCount>,
    pub avg_confidence: f64,
}

/// Type count for statistics
#[derive(Debug, Serialize, Deserialize)]
pub struct TypeCount {
    pub recommendation_type: String,
    pub count: i32,
    pub avg_confidence: f64,
}

/// Get statistics about recommendations
#[tauri::command]
pub fn get_recommendation_stats(workspace_path: &str) -> Result<RecommendationStats, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    // Total recommendations
    let total_recommendations: i32 = conn
        .query_row("SELECT COUNT(*) FROM recommendations", [], |row| row.get(0))
        .unwrap_or(0);

    // Active (non-dismissed) recommendations
    let active_recommendations: i32 = conn
        .query_row("SELECT COUNT(*) FROM recommendations WHERE dismissed_at IS NULL", [], |row| {
            row.get(0)
        })
        .unwrap_or(0);

    // Dismissed recommendations
    let dismissed_recommendations: i32 = conn
        .query_row(
            "SELECT COUNT(*) FROM recommendations WHERE dismissed_at IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    // Acted on recommendations
    let acted_on_recommendations: i32 = conn
        .query_row("SELECT COUNT(*) FROM recommendations WHERE acted_on = 1", [], |row| row.get(0))
        .unwrap_or(0);

    // Recommendations by type
    let mut stmt = conn
        .prepare(
            "SELECT type, COUNT(*), AVG(confidence)
             FROM recommendations
             GROUP BY type
             ORDER BY COUNT(*) DESC",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let recommendations_by_type: Vec<TypeCount> = stmt
        .query_map([], |row| {
            Ok(TypeCount {
                recommendation_type: row.get(0)?,
                count: row.get(1)?,
                avg_confidence: row.get::<_, Option<f64>>(2)?.unwrap_or(0.0),
            })
        })
        .map_err(|e| format!("Failed to query types: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect types: {e}"))?;

    // Average confidence
    let avg_confidence: f64 = conn
        .query_row("SELECT COALESCE(AVG(confidence), 0.0) FROM recommendations", [], |row| {
            row.get(0)
        })
        .unwrap_or(0.0);

    Ok(RecommendationStats {
        total_recommendations,
        active_recommendations,
        dismissed_recommendations,
        acted_on_recommendations,
        recommendations_by_type,
        avg_confidence,
    })
}

// ============================================================================
// Budget Management (Phase 5: Intelligence - Issue #1110)
// ============================================================================

/// Budget configuration stored in workspace
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetConfig {
    /// Daily budget limit in USD
    pub daily_limit: Option<f64>,
    /// Weekly budget limit in USD
    pub weekly_limit: Option<f64>,
    /// Monthly budget limit in USD
    pub monthly_limit: Option<f64>,
    /// Alert thresholds as percentages (e.g., [50, 75, 90, 100])
    pub alert_thresholds: Vec<i32>,
    /// Whether budget alerts are enabled
    pub alerts_enabled: bool,
    /// Timestamps of last alerts sent (to prevent spam)
    pub last_alerts: BudgetLastAlerts,
}

/// Last alert timestamps by period
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BudgetLastAlerts {
    pub daily: std::collections::HashMap<i32, i64>,
    pub weekly: std::collections::HashMap<i32, i64>,
    pub monthly: std::collections::HashMap<i32, i64>,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            daily_limit: None,
            weekly_limit: None,
            monthly_limit: None,
            alert_thresholds: vec![50, 75, 90, 100],
            alerts_enabled: true,
            last_alerts: BudgetLastAlerts::default(),
        }
    }
}

/// Get the budget config file path
fn get_budget_config_path(workspace_path: &str) -> std::path::PathBuf {
    std::path::Path::new(workspace_path)
        .join(".loom")
        .join("budget-config.json")
}

/// Get the current budget configuration
#[tauri::command]
pub fn get_budget_config(workspace_path: &str) -> Result<BudgetConfig, String> {
    let config_path = get_budget_config_path(workspace_path);

    if !config_path.exists() {
        return Ok(BudgetConfig::default());
    }

    let content = std::fs::read_to_string(&config_path)
        .map_err(|e| format!("Failed to read budget config: {e}"))?;

    serde_json::from_str(&content).map_err(|e| format!("Failed to parse budget config: {e}"))
}

/// Save the budget configuration
#[tauri::command]
pub fn save_budget_config(workspace_path: &str, config: BudgetConfig) -> Result<(), String> {
    let config_path = get_budget_config_path(workspace_path);

    // Ensure .loom directory exists
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create .loom directory: {e}"))?;
    }

    let content = serde_json::to_string_pretty(&config)
        .map_err(|e| format!("Failed to serialize budget config: {e}"))?;

    std::fs::write(&config_path, content)
        .map_err(|e| format!("Failed to write budget config: {e}"))?;

    Ok(())
}

/// Cost breakdown by issue
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueCost {
    pub issue_number: i32,
    pub issue_title: String,
    pub total_cost: f64,
    pub total_tokens: i64,
    pub prompt_count: i32,
    pub last_activity: String,
}

/// Get cost breakdown by issue for a time range
#[tauri::command]
pub fn get_costs_by_issue(
    workspace_path: &str,
    time_range: &str,
) -> Result<Vec<IssueCost>, String> {
    let conn =
        open_activity_db(workspace_path).map_err(|e| format!("Failed to open database: {e}"))?;

    let since_clause = match time_range {
        "today" => "datetime('now', 'start of day')",
        "week" => "datetime('now', '-7 days')",
        "month" => "datetime('now', '-30 days')",
        _ => "datetime('1970-01-01')",
    };

    // Query costs grouped by issue, joining with prompt_github to get issue numbers
    let query = format!(
        "SELECT
            pg.issue_number,
            COALESCE(pg.issue_number, 0) as issue_num,
            COUNT(DISTINCT a.id) as prompt_count,
            COALESCE(SUM(t.prompt_tokens), 0) as total_prompt,
            COALESCE(SUM(t.completion_tokens), 0) as total_completion,
            COALESCE(SUM(t.total_tokens), 0) as total_tokens,
            MAX(a.timestamp) as last_activity
         FROM agent_activity a
         LEFT JOIN token_usage t ON a.id = t.activity_id
         LEFT JOIN prompt_github pg ON a.id = pg.activity_id
         WHERE a.timestamp >= {since_clause}
           AND pg.issue_number IS NOT NULL
         GROUP BY pg.issue_number
         ORDER BY total_tokens DESC
         LIMIT 50"
    );

    let mut stmt = conn
        .prepare(&query)
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let costs = stmt
        .query_map([], |row| {
            let issue_number: i32 = row.get(1)?;
            let prompt_count: i32 = row.get(2)?;
            let prompt_tokens: i64 = row.get(3)?;
            let completion_tokens: i64 = row.get(4)?;
            let total_tokens: i64 = row.get(5)?;
            let last_activity: String = row.get(6)?;

            Ok(IssueCost {
                issue_number,
                issue_title: format!("Issue #{issue_number}"), // Title would need GitHub API
                total_cost: calculate_cost(prompt_tokens, completion_tokens),
                total_tokens,
                prompt_count,
                last_activity,
            })
        })
        .map_err(|e| format!("Failed to query issue costs: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect costs: {e}"))?;

    Ok(costs)
}

// ============================================================================
// Activity Timeline for Playback (Phase 5: Feature 5 - Playback & Replay)
// ============================================================================

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
