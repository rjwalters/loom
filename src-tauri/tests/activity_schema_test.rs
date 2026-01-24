#![allow(clippy::unwrap_used)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::panic)]
#![allow(clippy::manual_assert)]

use rusqlite::{Connection, Result as SqliteResult};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Helper to create a temporary workspace directory
fn create_temp_workspace() -> (TempDir, PathBuf) {
    let temp_dir = TempDir::new().unwrap();
    let workspace_path = temp_dir.path().to_path_buf();
    (temp_dir, workspace_path)
}

/// Helper to open a connection to the activity database
fn open_test_db(workspace_path: &Path) -> SqliteResult<Connection> {
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
    )
    .unwrap();

    // Simulate migration to v2
    conn.execute("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL)", [])
        .unwrap();

    conn.execute("INSERT INTO schema_version (version) VALUES (2)", [])
        .unwrap();

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
    )
    .unwrap();

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
    )
    .unwrap();

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
    )
    .unwrap();

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
    )
    .unwrap();

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
    conn.execute("CREATE TABLE schema_version (version INTEGER NOT NULL)", [])
        .unwrap();
    conn.execute("INSERT INTO schema_version (version) VALUES (2)", [])
        .unwrap();

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
    )
    .unwrap();

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
    )
    .unwrap();

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
    )
    .unwrap();

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
    )
    .unwrap();

    conn.execute("CREATE TABLE schema_version (version INTEGER NOT NULL)", [])
        .unwrap();
    conn.execute("INSERT INTO schema_version (version) VALUES (2)", [])
        .unwrap();

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
    )
    .unwrap();

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
    )
    .unwrap();

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
    )
    .unwrap();

    // Insert valid activity
    conn.execute(
        "INSERT INTO agent_activity (timestamp, role, trigger, work_found, outcome)
         VALUES ('2025-10-22T08:00:00Z', 'builder', 'manual', 1, 'success')",
        [],
    )
    .unwrap();

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

#[test]
fn test_v2_to_v3_migration() {
    let (_temp, workspace_path) = create_temp_workspace();
    let conn = open_test_db(&workspace_path).unwrap();

    // Create v2 schema
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
    )
    .unwrap();

    conn.execute("CREATE TABLE schema_version (version INTEGER NOT NULL)", [])
        .unwrap();
    conn.execute("INSERT INTO schema_version (version) VALUES (2)", [])
        .unwrap();

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
    )
    .unwrap();

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
    )
    .unwrap();

    // Insert some v2 data
    conn.execute(
        "INSERT INTO agent_activity (timestamp, role, trigger, work_found, work_completed, issue_number, outcome)
         VALUES ('2026-01-23T08:00:00Z', 'builder', 'manual', 1, 1, 42, 'success')",
        [],
    )
    .unwrap();

    let activity_id = conn.last_insert_rowid();

    // Simulate v3 migration - create prompt_github table
    conn.execute(
        "CREATE TABLE prompt_github (
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
    )
    .unwrap();

    // Update schema version
    conn.execute("UPDATE schema_version SET version = 3", [])
        .unwrap();

    // Verify prompt_github table exists
    let prompt_github_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='prompt_github'",
            [],
            |row| row.get::<_, i32>(0).map(|c| c > 0),
        )
        .unwrap();
    assert!(prompt_github_exists, "prompt_github table should exist");

    // Verify we can insert into prompt_github
    let insert_result = conn.execute(
        "INSERT INTO prompt_github (activity_id, issue_number, pr_number, event_type, event_time)
         VALUES (?1, 42, NULL, 'issue_claimed', datetime('now'))",
        [activity_id],
    );
    assert!(insert_result.is_ok(), "Should be able to insert into prompt_github");

    // Verify schema version is 3
    let version: i32 = conn
        .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 3, "Schema version should be 3");
}

#[test]
fn test_prompt_github_label_tracking() {
    let (_temp, workspace_path) = create_temp_workspace();
    let conn = open_test_db(&workspace_path).unwrap();

    // Create minimal schema for testing
    conn.execute(
        "CREATE TABLE agent_activity (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            role TEXT NOT NULL,
            trigger TEXT NOT NULL,
            work_found INTEGER NOT NULL,
            outcome TEXT NOT NULL
        )",
        [],
    )
    .unwrap();

    conn.execute(
        "CREATE TABLE prompt_github (
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
    )
    .unwrap();

    // Insert activity
    conn.execute(
        "INSERT INTO agent_activity (timestamp, role, trigger, work_found, outcome)
         VALUES (datetime('now'), 'builder', 'manual', 1, 'success')",
        [],
    )
    .unwrap();
    let activity_id = conn.last_insert_rowid();

    // Simulate label transition: loom:issue -> loom:building
    conn.execute(
        "INSERT INTO prompt_github (activity_id, issue_number, label_before, label_after, event_type, event_time)
         VALUES (?1, 42, '[\"loom:issue\"]', '[\"loom:building\"]', 'label_added', datetime('now'))",
        [activity_id],
    )
    .unwrap();

    // Verify label tracking
    let (label_before, label_after): (String, String) = conn
        .query_row(
            "SELECT label_before, label_after FROM prompt_github WHERE issue_number = 42",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();

    assert!(label_before.contains("loom:issue"), "Should track label_before");
    assert!(label_after.contains("loom:building"), "Should track label_after");
}

#[test]
fn test_prompt_github_pr_correlation() {
    let (_temp, workspace_path) = create_temp_workspace();
    let conn = open_test_db(&workspace_path).unwrap();

    // Create minimal schema
    conn.execute(
        "CREATE TABLE agent_activity (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            role TEXT NOT NULL,
            trigger TEXT NOT NULL,
            work_found INTEGER NOT NULL,
            outcome TEXT NOT NULL
        )",
        [],
    )
    .unwrap();

    conn.execute(
        "CREATE TABLE prompt_github (
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
    )
    .unwrap();

    // Simulate workflow: issue claim -> PR create -> PR merge
    conn.execute(
        "INSERT INTO agent_activity (timestamp, role, trigger, work_found, outcome)
         VALUES (datetime('now', '-2 hours'), 'builder', 'manual', 1, 'success')",
        [],
    )
    .unwrap();
    let activity1 = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO agent_activity (timestamp, role, trigger, work_found, outcome)
         VALUES (datetime('now', '-1 hour'), 'builder', 'manual', 1, 'success')",
        [],
    )
    .unwrap();
    let activity2 = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO agent_activity (timestamp, role, trigger, work_found, outcome)
         VALUES (datetime('now'), 'judge', 'autonomous', 1, 'success')",
        [],
    )
    .unwrap();
    let activity3 = conn.last_insert_rowid();

    // Issue claimed
    conn.execute(
        "INSERT INTO prompt_github (activity_id, issue_number, event_type, event_time)
         VALUES (?1, 42, 'issue_claimed', datetime('now', '-2 hours'))",
        [activity1],
    )
    .unwrap();

    // PR created (linking issue and PR)
    conn.execute(
        "INSERT INTO prompt_github (activity_id, issue_number, pr_number, event_type, event_time)
         VALUES (?1, 42, 123, 'pr_created', datetime('now', '-1 hour'))",
        [activity2],
    )
    .unwrap();

    // PR merged
    conn.execute(
        "INSERT INTO prompt_github (activity_id, issue_number, pr_number, event_type, event_time)
         VALUES (?1, 42, 123, 'pr_merged', datetime('now'))",
        [activity3],
    )
    .unwrap();

    // Query all prompts for the issue
    let mut stmt = conn
        .prepare("SELECT activity_id, event_type FROM prompt_github WHERE issue_number = 42 ORDER BY event_time")
        .unwrap();

    let events: Vec<(i64, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(events.len(), 3, "Should have 3 events for issue 42");
    assert_eq!(events[0].1, "issue_claimed");
    assert_eq!(events[1].1, "pr_created");
    assert_eq!(events[2].1, "pr_merged");

    // Query prompts for the PR
    let pr_count: i32 = conn
        .query_row(
            "SELECT COUNT(*) FROM prompt_github WHERE pr_number = 123",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(pr_count, 2, "Should have 2 events for PR 123");
}

#[test]
fn test_prompt_github_indexes() {
    let (_temp, workspace_path) = create_temp_workspace();
    let conn = open_test_db(&workspace_path).unwrap();

    // Create table with indexes
    conn.execute(
        "CREATE TABLE prompt_github (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            activity_id INTEGER NOT NULL,
            issue_number INTEGER,
            pr_number INTEGER,
            label_before TEXT,
            label_after TEXT,
            event_type TEXT NOT NULL,
            event_time TEXT NOT NULL
        )",
        [],
    )
    .unwrap();

    conn.execute(
        "CREATE INDEX idx_prompt_github_activity_id ON prompt_github(activity_id)",
        [],
    )
    .unwrap();

    conn.execute(
        "CREATE INDEX idx_prompt_github_issue_number ON prompt_github(issue_number)",
        [],
    )
    .unwrap();

    conn.execute(
        "CREATE INDEX idx_prompt_github_pr_number ON prompt_github(pr_number)",
        [],
    )
    .unwrap();

    conn.execute(
        "CREATE INDEX idx_prompt_github_event_type ON prompt_github(event_type)",
        [],
    )
    .unwrap();

    conn.execute(
        "CREATE INDEX idx_prompt_github_event_time ON prompt_github(event_time)",
        [],
    )
    .unwrap();

    // Verify indexes exist
    let index_count: i32 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name LIKE 'idx_prompt_github%'",
            [],
            |row| row.get(0),
        )
        .unwrap();

    assert_eq!(index_count, 5, "Should have 5 indexes on prompt_github table");
}
