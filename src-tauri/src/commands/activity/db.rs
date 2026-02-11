use rusqlite::{Connection, Result as SqliteResult};
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

/// Default token pricing (Claude 3.5 Sonnet pricing per 1K tokens)
pub const INPUT_TOKEN_PRICE_PER_1K: f64 = 0.003;
pub const OUTPUT_TOKEN_PRICE_PER_1K: f64 = 0.015;

/// Calculate estimated cost from token counts
#[allow(clippy::cast_precision_loss)]
pub fn calculate_cost(prompt_tokens: i64, completion_tokens: i64) -> f64 {
    let input_cost = (prompt_tokens as f64 / 1000.0) * INPUT_TOKEN_PRICE_PER_1K;
    let output_cost = (completion_tokens as f64 / 1000.0) * OUTPUT_TOKEN_PRICE_PER_1K;
    input_cost + output_cost
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
/// Adds `prompt_github` table for linking prompts to GitHub entities
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
/// Adds `velocity_snapshots` table for daily velocity tracking
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
/// Adds `prompt_patterns` and `pattern_matches` tables for pattern catalog
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
/// Adds recommendations and `recommendation_rules` tables for the recommendation engine
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
pub fn run_migrations(conn: &Connection) -> SqliteResult<()> {
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
pub fn open_activity_db(workspace_path: &str) -> SqliteResult<Connection> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_cost_zero_tokens() {
        assert!((calculate_cost(0, 0)).abs() < 1e-10);
    }

    #[test]
    fn test_calculate_cost_only_input() {
        // 1000 prompt tokens at $0.003/1K = $0.003
        let cost = calculate_cost(1000, 0);
        assert!((cost - 0.003).abs() < 1e-10, "cost = {cost}");
    }

    #[test]
    fn test_calculate_cost_only_output() {
        // 1000 completion tokens at $0.015/1K = $0.015
        let cost = calculate_cost(0, 1000);
        assert!((cost - 0.015).abs() < 1e-10, "cost = {cost}");
    }

    #[test]
    fn test_calculate_cost_combined() {
        // 1000 prompt + 1000 completion = $0.003 + $0.015 = $0.018
        let cost = calculate_cost(1000, 1000);
        assert!((cost - 0.018).abs() < 1e-10, "cost = {cost}");
    }

    #[test]
    fn test_calculate_cost_large_values() {
        // 100K prompt + 50K completion
        let cost = calculate_cost(100_000, 50_000);
        let expected = (100.0 * 0.003) + (50.0 * 0.015); // 0.3 + 0.75 = 1.05
        assert!((cost - expected).abs() < 1e-10, "cost = {cost}");
    }

    #[test]
    fn test_calculate_cost_proportional() {
        // Cost should scale linearly
        let cost_1k = calculate_cost(1000, 1000);
        let cost_2k = calculate_cost(2000, 2000);
        assert!((cost_2k - 2.0 * cost_1k).abs() < 1e-10);
    }

    #[test]
    fn test_open_activity_db_creates_directory() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let workspace = tmp_dir.path().to_str().unwrap();
        let result = open_activity_db(workspace);
        assert!(result.is_ok());
        assert!(tmp_dir.path().join(".loom").join("activity.db").exists());
    }
}
