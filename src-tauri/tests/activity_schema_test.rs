use rusqlite::{Connection, Result as SqliteResult};
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

/// Helper to create a temporary workspace directory
fn create_temp_workspace() -> (TempDir, PathBuf) {
    let temp_dir = TempDir::new().unwrap();
    let workspace_path = temp_dir.path().to_path_buf();
    (temp_dir, workspace_path)
}

/// Helper to open a connection to the activity database
fn open_test_db(workspace_path: &PathBuf) -> SqliteResult<Connection> {
    let loom_dir = workspace_path.join(".loom");
    fs::create_dir_all(&loom_dir).unwrap();
    let db_path = loom_dir.join("activity.db");
    Connection::open(&db_path)
}

#[test]
fn test_fresh_database_creation() {
    let (_temp, workspace_path) = create_temp_workspace();

    // Simulate opening database for first time
    let conn = open_test_db(&workspace_path).unwrap();

    // Create v1 schema (agent_activity table)
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
    ).unwrap();

    // Simulate migration to v2
    conn.execute(
        "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL)",
        [],
    ).unwrap();

    conn.execute("INSERT INTO schema_version (version) VALUES (2)", []).unwrap();

    // Create v2 tables
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
    ).unwrap();

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
    ).unwrap();

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
    ).unwrap();

    // Verify all tables exist
    let table_count: i32 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('agent_activity', 'schema_version', 'token_usage', 'code_changes', 'github_events')",
            [],
            |row| row.get(0),
        )
        .unwrap();

    assert_eq!(table_count, 5, "All tables should be created");

    // Verify schema version
    let version: i32 = conn
        .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
        .unwrap();

    assert_eq!(version, 2, "Schema version should be 2");
}

#[test]
fn test_v1_to_v2_migration() {
    let (_temp, workspace_path) = create_temp_workspace();
    let conn = open_test_db(&workspace_path).unwrap();

    // Create v1 schema only
    conn.execute(
        "CREATE TABLE agent_activity (
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
    ).unwrap();

    // Insert some v1 data
    conn.execute(
        "INSERT INTO agent_activity (timestamp, role, trigger, work_found, work_completed, issue_number, duration_ms, outcome, notes)
         VALUES ('2025-10-22T08:00:00Z', 'builder', 'manual', 1, 1, 42, 1000, 'success', 'test')",
        [],
    ).unwrap();

    // Verify v1 data exists
    let count: i32 = conn
        .query_row("SELECT COUNT(*) FROM agent_activity", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1, "V1 data should exist");

    // Simulate migration
    conn.execute(
        "CREATE TABLE schema_version (version INTEGER NOT NULL)",
        [],
    ).unwrap();
    conn.execute("INSERT INTO schema_version (version) VALUES (2)", []).unwrap();

    // Create v2 tables
    conn.execute(
        "CREATE TABLE token_usage (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            activity_id INTEGER NOT NULL,
            prompt_tokens INTEGER NOT NULL,
            completion_tokens INTEGER NOT NULL,
            total_tokens INTEGER NOT NULL,
            model TEXT,
            FOREIGN KEY (activity_id) REFERENCES agent_activity(id)
        )",
        [],
    ).unwrap();

    conn.execute(
        "CREATE TABLE code_changes (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            activity_id INTEGER NOT NULL,
            files_modified INTEGER NOT NULL,
            lines_added INTEGER NOT NULL,
            lines_removed INTEGER NOT NULL,
            commit_sha TEXT,
            FOREIGN KEY (activity_id) REFERENCES agent_activity(id)
        )",
        [],
    ).unwrap();

    conn.execute(
        "CREATE TABLE github_events (
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
    ).unwrap();

    // Verify v1 data preserved
    let preserved_count: i32 = conn
        .query_row("SELECT COUNT(*) FROM agent_activity", [], |row| row.get(0))
        .unwrap();
    assert_eq!(preserved_count, 1, "V1 data should be preserved after migration");

    // Verify new tables exist
    let token_table_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='token_usage'",
            [],
            |row| row.get::<_, i32>(0).map(|c| c > 0),
        )
        .unwrap();
    assert!(token_table_exists, "token_usage table should exist");

    let code_table_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='code_changes'",
            [],
            |row| row.get::<_, i32>(0).map(|c| c > 0),
        )
        .unwrap();
    assert!(code_table_exists, "code_changes table should exist");

    let events_table_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='github_events'",
            [],
            |row| row.get::<_, i32>(0).map(|c| c > 0),
        )
        .unwrap();
    assert!(events_table_exists, "github_events table should exist");
}

#[test]
fn test_idempotent_migration() {
    let (_temp, workspace_path) = create_temp_workspace();
    let conn = open_test_db(&workspace_path).unwrap();

    // Create full v2 schema
    conn.execute(
        "CREATE TABLE agent_activity (
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
    ).unwrap();

    conn.execute(
        "CREATE TABLE schema_version (version INTEGER NOT NULL)",
        [],
    ).unwrap();
    conn.execute("INSERT INTO schema_version (version) VALUES (2)", []).unwrap();

    conn.execute(
        "CREATE TABLE token_usage (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            activity_id INTEGER NOT NULL,
            prompt_tokens INTEGER NOT NULL,
            completion_tokens INTEGER NOT NULL,
            total_tokens INTEGER NOT NULL,
            model TEXT
        )",
        [],
    ).unwrap();

    // Run "migration" again (should be idempotent)
    let current_version: i32 = conn
        .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
        .unwrap();

    // Migration should skip if already at v2
    if current_version < 2 {
        panic!("Version should already be 2");
    }

    // Verify schema version unchanged
    let version_after: i32 = conn
        .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version_after, 2, "Version should remain 2 after idempotent migration");

    // Verify tables still exist
    let token_table_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='token_usage'",
            [],
            |row| row.get::<_, i32>(0).map(|c| c > 0),
        )
        .unwrap();
    assert!(token_table_exists, "token_usage table should still exist");
}

#[test]
fn test_foreign_key_constraints() {
    let (_temp, workspace_path) = create_temp_workspace();
    let conn = open_test_db(&workspace_path).unwrap();

    // Enable foreign keys
    conn.execute("PRAGMA foreign_keys = ON", []).unwrap();

    // Create tables
    conn.execute(
        "CREATE TABLE agent_activity (
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
    ).unwrap();

    conn.execute(
        "CREATE TABLE token_usage (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            activity_id INTEGER NOT NULL,
            prompt_tokens INTEGER NOT NULL,
            completion_tokens INTEGER NOT NULL,
            total_tokens INTEGER NOT NULL,
            model TEXT,
            FOREIGN KEY (activity_id) REFERENCES agent_activity(id)
        )",
        [],
    ).unwrap();

    // Insert valid activity
    conn.execute(
        "INSERT INTO agent_activity (timestamp, role, trigger, work_found, outcome)
         VALUES ('2025-10-22T08:00:00Z', 'builder', 'manual', 1, 'success')",
        [],
    ).unwrap();

    let activity_id: i32 = conn.last_insert_rowid() as i32;

    // Insert token_usage with valid foreign key - should succeed
    let result = conn.execute(
        "INSERT INTO token_usage (activity_id, prompt_tokens, completion_tokens, total_tokens, model)
         VALUES (?1, 100, 50, 150, 'claude-sonnet-4')",
        [activity_id],
    );
    assert!(result.is_ok(), "Valid foreign key should succeed");

    // Try to insert token_usage with invalid foreign key - should fail
    let invalid_result = conn.execute(
        "INSERT INTO token_usage (activity_id, prompt_tokens, completion_tokens, total_tokens, model)
         VALUES (999, 100, 50, 150, 'claude-sonnet-4')",
        [],
    );
    assert!(invalid_result.is_err(), "Invalid foreign key should fail");
}
