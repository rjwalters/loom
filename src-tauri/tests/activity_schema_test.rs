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

#[test]
fn test_v4_to_v5_migration() {
    let (_temp, workspace_path) = create_temp_workspace();
    let conn = open_test_db(&workspace_path).unwrap();

    // Create v4 schema (with velocity_snapshots from v4)
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
    conn.execute("INSERT INTO schema_version (version) VALUES (4)", [])
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

    conn.execute(
        "CREATE TABLE velocity_snapshots (
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
    )
    .unwrap();

    // Insert some v4 data
    conn.execute(
        "INSERT INTO agent_activity (timestamp, role, trigger, work_found, work_completed, outcome)
         VALUES ('2026-01-23T08:00:00Z', 'builder', 'Build issue #42', 1, 1, 'success')",
        [],
    )
    .unwrap();

    // Simulate v5 migration - create prompt_patterns and pattern_matches tables
    conn.execute(
        "CREATE TABLE prompt_patterns (
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
    )
    .unwrap();

    conn.execute(
        "CREATE TABLE pattern_matches (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            pattern_id INTEGER NOT NULL,
            activity_id INTEGER NOT NULL,
            similarity_score REAL DEFAULT 1.0,
            matched_at TEXT DEFAULT (datetime('now')),
            FOREIGN KEY (pattern_id) REFERENCES prompt_patterns(id),
            FOREIGN KEY (activity_id) REFERENCES agent_activity(id)
        )",
        [],
    )
    .unwrap();

    // Update schema version
    conn.execute("UPDATE schema_version SET version = 5", [])
        .unwrap();

    // Verify prompt_patterns table exists
    let patterns_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='prompt_patterns'",
            [],
            |row| row.get::<_, i32>(0).map(|c| c > 0),
        )
        .unwrap();
    assert!(patterns_exists, "prompt_patterns table should exist");

    // Verify pattern_matches table exists
    let matches_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='pattern_matches'",
            [],
            |row| row.get::<_, i32>(0).map(|c| c > 0),
        )
        .unwrap();
    assert!(matches_exists, "pattern_matches table should exist");

    // Verify velocity_snapshots still exists (from v4)
    let velocity_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='velocity_snapshots'",
            [],
            |row| row.get::<_, i32>(0).map(|c| c > 0),
        )
        .unwrap();
    assert!(velocity_exists, "velocity_snapshots table should still exist");

    // Verify schema version is 5
    let version: i32 = conn
        .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 5, "Schema version should be 5");
}

#[test]
fn test_prompt_patterns_crud() {
    let (_temp, workspace_path) = create_temp_workspace();
    let conn = open_test_db(&workspace_path).unwrap();

    // Create tables
    conn.execute(
        "CREATE TABLE prompt_patterns (
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
    )
    .unwrap();

    // Insert a pattern
    conn.execute(
        "INSERT INTO prompt_patterns (pattern_text, category, times_used, success_count, success_rate)
         VALUES ('build issue #n', 'build', 5, 4, 0.8)",
        [],
    )
    .unwrap();

    let pattern_id = conn.last_insert_rowid();

    // Verify pattern was inserted
    let (text, category, times_used, success_rate): (String, String, i32, f64) = conn
        .query_row(
            "SELECT pattern_text, category, times_used, success_rate FROM prompt_patterns WHERE id = ?1",
            [pattern_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();

    assert_eq!(text, "build issue #n");
    assert_eq!(category, "build");
    assert_eq!(times_used, 5);
    assert!((success_rate - 0.8).abs() < 0.01);

    // Update pattern
    conn.execute(
        "UPDATE prompt_patterns SET times_used = times_used + 1, success_count = success_count + 1,
         success_rate = CAST(success_count + 1 AS REAL) / (times_used + 1)
         WHERE id = ?1",
        [pattern_id],
    )
    .unwrap();

    // Verify update
    let (new_times, new_rate): (i32, f64) = conn
        .query_row(
            "SELECT times_used, success_rate FROM prompt_patterns WHERE id = ?1",
            [pattern_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();

    assert_eq!(new_times, 6);
    assert!((new_rate - 0.833).abs() < 0.01);
}

#[test]
fn test_pattern_matches_linking() {
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
            outcome TEXT NOT NULL
        )",
        [],
    )
    .unwrap();

    conn.execute(
        "CREATE TABLE prompt_patterns (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            pattern_text TEXT NOT NULL UNIQUE,
            category TEXT,
            times_used INTEGER DEFAULT 0
        )",
        [],
    )
    .unwrap();

    conn.execute(
        "CREATE TABLE pattern_matches (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            pattern_id INTEGER NOT NULL,
            activity_id INTEGER NOT NULL,
            similarity_score REAL DEFAULT 1.0,
            matched_at TEXT DEFAULT (datetime('now')),
            FOREIGN KEY (pattern_id) REFERENCES prompt_patterns(id),
            FOREIGN KEY (activity_id) REFERENCES agent_activity(id)
        )",
        [],
    )
    .unwrap();

    // Insert activity
    conn.execute(
        "INSERT INTO agent_activity (timestamp, role, trigger, work_found, outcome)
         VALUES (datetime('now'), 'builder', 'Build issue #42', 1, 'success')",
        [],
    )
    .unwrap();
    let activity_id = conn.last_insert_rowid();

    // Insert pattern
    conn.execute(
        "INSERT INTO prompt_patterns (pattern_text, category, times_used)
         VALUES ('build issue #n', 'build', 1)",
        [],
    )
    .unwrap();
    let pattern_id = conn.last_insert_rowid();

    // Link them via pattern_matches
    conn.execute(
        "INSERT INTO pattern_matches (pattern_id, activity_id, similarity_score)
         VALUES (?1, ?2, 1.0)",
        [pattern_id, activity_id],
    )
    .unwrap();

    // Verify we can query activities for a pattern
    let matched_activities: Vec<i64> = conn
        .prepare("SELECT activity_id FROM pattern_matches WHERE pattern_id = ?1")
        .unwrap()
        .query_map([pattern_id], |row| row.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(matched_activities.len(), 1);
    assert_eq!(matched_activities[0], activity_id);

    // Verify we can query patterns for an activity
    let matched_patterns: Vec<i64> = conn
        .prepare("SELECT pattern_id FROM pattern_matches WHERE activity_id = ?1")
        .unwrap()
        .query_map([activity_id], |row| row.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(matched_patterns.len(), 1);
    assert_eq!(matched_patterns[0], pattern_id);
}

#[test]
fn test_pattern_uniqueness() {
    let (_temp, workspace_path) = create_temp_workspace();
    let conn = open_test_db(&workspace_path).unwrap();

    conn.execute(
        "CREATE TABLE prompt_patterns (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            pattern_text TEXT NOT NULL UNIQUE,
            category TEXT
        )",
        [],
    )
    .unwrap();

    // Insert first pattern
    conn.execute(
        "INSERT INTO prompt_patterns (pattern_text, category) VALUES ('build issue #n', 'build')",
        [],
    )
    .unwrap();

    // Try to insert duplicate - should fail
    let result = conn.execute(
        "INSERT INTO prompt_patterns (pattern_text, category) VALUES ('build issue #n', 'build')",
        [],
    );
    assert!(result.is_err(), "Duplicate pattern_text should fail");

    // Insert different pattern - should succeed
    let result = conn.execute(
        "INSERT INTO prompt_patterns (pattern_text, category) VALUES ('fix bug #n', 'fix')",
        [],
    );
    assert!(result.is_ok(), "Different pattern should succeed");
}

#[test]
fn test_pattern_category_query() {
    let (_temp, workspace_path) = create_temp_workspace();
    let conn = open_test_db(&workspace_path).unwrap();

    conn.execute(
        "CREATE TABLE prompt_patterns (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            pattern_text TEXT NOT NULL UNIQUE,
            category TEXT,
            success_rate REAL DEFAULT 0.0
        )",
        [],
    )
    .unwrap();

    // Insert patterns in different categories
    conn.execute(
        "INSERT INTO prompt_patterns (pattern_text, category, success_rate) VALUES ('build feature #n', 'build', 0.9)",
        [],
    ).unwrap();
    conn.execute(
        "INSERT INTO prompt_patterns (pattern_text, category, success_rate) VALUES ('implement #n', 'build', 0.8)",
        [],
    ).unwrap();
    conn.execute(
        "INSERT INTO prompt_patterns (pattern_text, category, success_rate) VALUES ('fix bug #n', 'fix', 0.7)",
        [],
    ).unwrap();
    conn.execute(
        "INSERT INTO prompt_patterns (pattern_text, category, success_rate) VALUES ('review pr #n', 'review', 0.95)",
        [],
    ).unwrap();

    // Query by category
    let build_patterns: Vec<String> = conn
        .prepare("SELECT pattern_text FROM prompt_patterns WHERE category = 'build' ORDER BY success_rate DESC")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(build_patterns.len(), 2);
    assert_eq!(build_patterns[0], "build feature #n"); // Higher success rate first

    // Count by category
    let category_counts: Vec<(String, i32)> = conn
        .prepare("SELECT category, COUNT(*) FROM prompt_patterns GROUP BY category ORDER BY COUNT(*) DESC")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(category_counts.len(), 3);
    assert_eq!(category_counts[0].0, "build");
    assert_eq!(category_counts[0].1, 2);
}

// ============================================================================
// Recommendation Engine Tests (v6 schema)
// ============================================================================

#[test]
fn test_v5_to_v6_migration() {
    let (_temp, workspace_path) = create_temp_workspace();
    let conn = open_test_db(&workspace_path).unwrap();

    // Create v5 schema
    conn.execute(
        "CREATE TABLE agent_activity (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            role TEXT NOT NULL,
            trigger TEXT NOT NULL,
            work_found INTEGER NOT NULL,
            work_completed INTEGER,
            outcome TEXT NOT NULL
        )",
        [],
    )
    .unwrap();

    conn.execute("CREATE TABLE schema_version (version INTEGER NOT NULL)", [])
        .unwrap();
    conn.execute("INSERT INTO schema_version (version) VALUES (5)", [])
        .unwrap();

    conn.execute(
        "CREATE TABLE prompt_patterns (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            pattern_text TEXT NOT NULL UNIQUE,
            category TEXT,
            times_used INTEGER DEFAULT 0,
            success_count INTEGER DEFAULT 0,
            success_rate REAL DEFAULT 0.0
        )",
        [],
    )
    .unwrap();

    // Simulate v6 migration - create recommendations and recommendation_rules tables
    conn.execute(
        "CREATE TABLE recommendations (
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
    )
    .unwrap();

    conn.execute(
        "CREATE TABLE recommendation_rules (
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
    )
    .unwrap();

    // Update schema version
    conn.execute("UPDATE schema_version SET version = 6", [])
        .unwrap();

    // Verify recommendations table exists
    let recommendations_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='recommendations'",
            [],
            |row| row.get::<_, i32>(0).map(|c| c > 0),
        )
        .unwrap();
    assert!(recommendations_exists, "recommendations table should exist");

    // Verify recommendation_rules table exists
    let rules_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='recommendation_rules'",
            [],
            |row| row.get::<_, i32>(0).map(|c| c > 0),
        )
        .unwrap();
    assert!(rules_exists, "recommendation_rules table should exist");

    // Verify schema version is 6
    let version: i32 = conn
        .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 6, "Schema version should be 6");
}

#[test]
fn test_recommendations_crud() {
    let (_temp, workspace_path) = create_temp_workspace();
    let conn = open_test_db(&workspace_path).unwrap();

    // Create recommendations table
    conn.execute(
        "CREATE TABLE recommendations (
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
    )
    .unwrap();

    // Insert a recommendation
    conn.execute(
        "INSERT INTO recommendations (type, title, description, confidence, evidence, context_role)
         VALUES ('warning', 'Low success pattern', 'Pattern X has low success rate', 0.8, '{\"pattern_id\": 1}', 'builder')",
        [],
    )
    .unwrap();

    let rec_id = conn.last_insert_rowid();

    // Verify insertion
    let (rec_type, title, confidence): (String, String, f64) = conn
        .query_row(
            "SELECT type, title, confidence FROM recommendations WHERE id = ?1",
            [rec_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();

    assert_eq!(rec_type, "warning");
    assert_eq!(title, "Low success pattern");
    assert!((confidence - 0.8).abs() < 0.01);

    // Test dismiss
    conn.execute(
        "UPDATE recommendations SET dismissed_at = datetime('now') WHERE id = ?1",
        [rec_id],
    )
    .unwrap();

    let dismissed: bool = conn
        .query_row(
            "SELECT dismissed_at IS NOT NULL FROM recommendations WHERE id = ?1",
            [rec_id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(dismissed, "Recommendation should be dismissed");

    // Test mark acted on
    conn.execute(
        "UPDATE recommendations SET acted_on = 1 WHERE id = ?1",
        [rec_id],
    )
    .unwrap();

    let acted_on: bool = conn
        .query_row(
            "SELECT acted_on = 1 FROM recommendations WHERE id = ?1",
            [rec_id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(acted_on, "Recommendation should be marked as acted on");
}

#[test]
fn test_recommendation_rules_crud() {
    let (_temp, workspace_path) = create_temp_workspace();
    let conn = open_test_db(&workspace_path).unwrap();

    // Create recommendation_rules table
    conn.execute(
        "CREATE TABLE recommendation_rules (
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
    )
    .unwrap();

    // Insert a rule
    conn.execute(
        "INSERT INTO recommendation_rules (name, rule_type, description, threshold_value, threshold_count, recommendation_template, priority)
         VALUES ('low_success_pattern', 'warning', 'Warns about low success patterns', 0.5, 5, 'Pattern has {{success_rate}}% success', 1)",
        [],
    )
    .unwrap();

    let rule_id = conn.last_insert_rowid();

    // Verify insertion
    let (name, rule_type, threshold_value, priority): (String, String, f64, i32) = conn
        .query_row(
            "SELECT name, rule_type, threshold_value, priority FROM recommendation_rules WHERE id = ?1",
            [rule_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();

    assert_eq!(name, "low_success_pattern");
    assert_eq!(rule_type, "warning");
    assert!((threshold_value - 0.5).abs() < 0.01);
    assert_eq!(priority, 1);

    // Test update threshold
    conn.execute(
        "UPDATE recommendation_rules SET threshold_value = 0.4, updated_at = datetime('now') WHERE id = ?1",
        [rule_id],
    )
    .unwrap();

    let new_threshold: f64 = conn
        .query_row(
            "SELECT threshold_value FROM recommendation_rules WHERE id = ?1",
            [rule_id],
            |row| row.get(0),
        )
        .unwrap();
    assert!((new_threshold - 0.4).abs() < 0.01);

    // Test disable rule
    conn.execute(
        "UPDATE recommendation_rules SET enabled = 0 WHERE id = ?1",
        [rule_id],
    )
    .unwrap();

    let enabled: bool = conn
        .query_row(
            "SELECT enabled = 1 FROM recommendation_rules WHERE id = ?1",
            [rule_id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!enabled, "Rule should be disabled");
}

#[test]
fn test_recommendations_filtering() {
    let (_temp, workspace_path) = create_temp_workspace();
    let conn = open_test_db(&workspace_path).unwrap();

    // Create recommendations table
    conn.execute(
        "CREATE TABLE recommendations (
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
    )
    .unwrap();

    // Insert various recommendations
    conn.execute(
        "INSERT INTO recommendations (type, title, confidence, context_role, context_task_type)
         VALUES ('warning', 'Warning 1', 0.9, 'builder', 'build')",
        [],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO recommendations (type, title, confidence, context_role, context_task_type)
         VALUES ('prompt', 'Prompt 1', 0.8, 'builder', 'build')",
        [],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO recommendations (type, title, confidence, context_role, context_task_type)
         VALUES ('cost', 'Cost 1', 0.7, 'judge', 'review')",
        [],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO recommendations (type, title, confidence, context_role, dismissed_at)
         VALUES ('warning', 'Dismissed Warning', 0.6, 'builder', datetime('now'))",
        [],
    )
    .unwrap();

    // Query active recommendations (not dismissed)
    let active_count: i32 = conn
        .query_row(
            "SELECT COUNT(*) FROM recommendations WHERE dismissed_at IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(active_count, 3, "Should have 3 active recommendations");

    // Query by role
    let builder_count: i32 = conn
        .query_row(
            "SELECT COUNT(*) FROM recommendations WHERE context_role = 'builder' AND dismissed_at IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(builder_count, 2, "Should have 2 active builder recommendations");

    // Query by type
    let warning_count: i32 = conn
        .query_row(
            "SELECT COUNT(*) FROM recommendations WHERE type = 'warning' AND dismissed_at IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(warning_count, 1, "Should have 1 active warning");

    // Query sorted by confidence
    let top_rec: String = conn
        .query_row(
            "SELECT title FROM recommendations WHERE dismissed_at IS NULL ORDER BY confidence DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(top_rec, "Warning 1", "Highest confidence should be Warning 1");
}

#[test]
fn test_default_recommendation_rules() {
    let (_temp, workspace_path) = create_temp_workspace();
    let conn = open_test_db(&workspace_path).unwrap();

    // Create recommendation_rules table
    conn.execute(
        "CREATE TABLE recommendation_rules (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL UNIQUE,
            rule_type TEXT NOT NULL,
            description TEXT,
            threshold_value REAL,
            threshold_count INTEGER,
            recommendation_template TEXT NOT NULL,
            priority INTEGER DEFAULT 5,
            enabled INTEGER DEFAULT 1
        )",
        [],
    )
    .unwrap();

    // Insert default rules (simulating what migrate_v5_to_v6 does)
    conn.execute(
        "INSERT INTO recommendation_rules (name, rule_type, description, threshold_value, threshold_count, recommendation_template, priority)
         VALUES ('low_success_pattern', 'warning', 'Warns about patterns with low success rate', 0.5, 5, 'Pattern has low success', 1)",
        [],
    ).unwrap();

    conn.execute(
        "INSERT INTO recommendation_rules (name, rule_type, description, threshold_value, threshold_count, recommendation_template, priority)
         VALUES ('high_cost_alert', 'cost', 'Alerts when a feature costs significantly more than average', 2.0, 3, 'High cost detected', 2)",
        [],
    ).unwrap();

    conn.execute(
        "INSERT INTO recommendation_rules (name, rule_type, description, threshold_value, threshold_count, recommendation_template, priority)
         VALUES ('optimal_timing', 'timing', 'Suggests optimal times based on success correlation', 0.7, 10, 'Best time is...', 3)",
        [],
    ).unwrap();

    conn.execute(
        "INSERT INTO recommendation_rules (name, rule_type, description, threshold_value, threshold_count, recommendation_template, priority)
         VALUES ('similar_prompt', 'prompt', 'Suggests similar prompts that had higher success', 0.8, 3, 'Try this prompt...', 4)",
        [],
    ).unwrap();

    // Verify all 4 default rules exist
    let rule_count: i32 = conn
        .query_row("SELECT COUNT(*) FROM recommendation_rules", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rule_count, 4, "Should have 4 default rules");

    // Verify all are enabled by default
    let enabled_count: i32 = conn
        .query_row(
            "SELECT COUNT(*) FROM recommendation_rules WHERE enabled = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(enabled_count, 4, "All rules should be enabled by default");

    // Verify rule types
    let rule_types: Vec<String> = conn
        .prepare("SELECT DISTINCT rule_type FROM recommendation_rules ORDER BY rule_type")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(rule_types.len(), 4);
    assert!(rule_types.contains(&"cost".to_string()));
    assert!(rule_types.contains(&"prompt".to_string()));
    assert!(rule_types.contains(&"timing".to_string()));
    assert!(rule_types.contains(&"warning".to_string()));
}

#[test]
fn test_recommendation_evidence_json() {
    let (_temp, workspace_path) = create_temp_workspace();
    let conn = open_test_db(&workspace_path).unwrap();

    // Create recommendations table
    conn.execute(
        "CREATE TABLE recommendations (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            type TEXT NOT NULL,
            title TEXT NOT NULL,
            evidence TEXT
        )",
        [],
    )
    .unwrap();

    // Insert with JSON evidence
    let evidence = r#"{"pattern_id": 42, "pattern_text": "build issue #n", "success_rate": 0.3, "times_used": 10}"#;
    conn.execute(
        "INSERT INTO recommendations (type, title, evidence)
         VALUES ('warning', 'Low success pattern', ?1)",
        [evidence],
    )
    .unwrap();

    let rec_id = conn.last_insert_rowid();

    // Retrieve and verify evidence
    let stored_evidence: String = conn
        .query_row(
            "SELECT evidence FROM recommendations WHERE id = ?1",
            [rec_id],
            |row| row.get(0),
        )
        .unwrap();

    // Verify it's valid JSON by parsing
    let parsed: serde_json::Value = serde_json::from_str(&stored_evidence).unwrap();
    assert_eq!(parsed["pattern_id"], 42);
    assert_eq!(parsed["pattern_text"], "build issue #n");
    assert!((parsed["success_rate"].as_f64().unwrap() - 0.3).abs() < 0.01);
    assert_eq!(parsed["times_used"], 10);
}

#[test]
fn test_recommendations_indexes() {
    let (_temp, workspace_path) = create_temp_workspace();
    let conn = open_test_db(&workspace_path).unwrap();

    // Create table with indexes
    conn.execute(
        "CREATE TABLE recommendations (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            type TEXT NOT NULL,
            title TEXT NOT NULL,
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
            dismissed_at TIMESTAMP
        )",
        [],
    )
    .unwrap();

    conn.execute(
        "CREATE INDEX idx_recommendations_type ON recommendations(type)",
        [],
    )
    .unwrap();

    conn.execute(
        "CREATE INDEX idx_recommendations_created_at ON recommendations(created_at DESC)",
        [],
    )
    .unwrap();

    conn.execute(
        "CREATE INDEX idx_recommendations_dismissed ON recommendations(dismissed_at)",
        [],
    )
    .unwrap();

    // Verify indexes exist
    let index_count: i32 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name LIKE 'idx_recommendations%'",
            [],
            |row| row.get(0),
        )
        .unwrap();

    assert_eq!(index_count, 3, "Should have 3 indexes on recommendations table");
}
