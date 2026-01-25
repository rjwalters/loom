//! Database schema definitions for activity tracking.
//!
//! This module contains the SQL schema used to create and migrate
//! the activity database tables.

use anyhow::Result;
use rusqlite::Connection;

/// Initialize the database schema with all required tables and indexes.
///
/// This function is idempotent - it uses CREATE TABLE IF NOT EXISTS
/// so it can be safely called on an existing database.
#[allow(clippy::too_many_lines)]
pub fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r"
            CREATE TABLE IF NOT EXISTS agent_inputs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                terminal_id TEXT NOT NULL,
                timestamp DATETIME NOT NULL,
                input_type TEXT NOT NULL,
                content TEXT NOT NULL,
                agent_role TEXT,
                context TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_inputs_terminal_id ON agent_inputs(terminal_id);
            CREATE INDEX IF NOT EXISTS idx_inputs_timestamp ON agent_inputs(timestamp);
            CREATE INDEX IF NOT EXISTS idx_inputs_type ON agent_inputs(input_type);

            CREATE TABLE IF NOT EXISTS agent_outputs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                input_id INTEGER REFERENCES agent_inputs(id),
                terminal_id TEXT NOT NULL,
                timestamp DATETIME NOT NULL,
                content TEXT,
                content_preview TEXT,
                exit_code INTEGER,
                metadata TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_outputs_input_id ON agent_outputs(input_id);
            CREATE INDEX IF NOT EXISTS idx_outputs_terminal_id ON agent_outputs(terminal_id);
            CREATE INDEX IF NOT EXISTS idx_outputs_timestamp ON agent_outputs(timestamp);

            CREATE TABLE IF NOT EXISTS agent_results (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                input_id INTEGER NOT NULL,
                result_type TEXT NOT NULL,
                timestamp DATETIME NOT NULL,
                status TEXT,
                data TEXT,
                FOREIGN KEY(input_id) REFERENCES agent_inputs(id)
            );

            CREATE INDEX IF NOT EXISTS idx_results_input_id ON agent_results(input_id);
            CREATE INDEX IF NOT EXISTS idx_results_timestamp ON agent_results(timestamp);

            CREATE TABLE IF NOT EXISTS agent_tasks (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                description TEXT,
                started_at DATETIME,
                completed_at DATETIME,
                github_issue INTEGER,
                github_pr INTEGER
            );

            CREATE TABLE IF NOT EXISTS task_inputs (
                task_id INTEGER NOT NULL,
                input_id INTEGER NOT NULL,
                FOREIGN KEY(task_id) REFERENCES agent_tasks(id),
                FOREIGN KEY(input_id) REFERENCES agent_inputs(id),
                PRIMARY KEY(task_id, input_id)
            );

            -- Agent productivity metrics per task
            CREATE TABLE IF NOT EXISTS agent_metrics (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                terminal_id TEXT NOT NULL,
                agent_role TEXT NOT NULL,
                agent_system TEXT NOT NULL,

                -- Task identification
                task_type TEXT,
                github_issue INTEGER,
                github_pr INTEGER,

                -- Time tracking
                started_at DATETIME NOT NULL,
                completed_at DATETIME,
                wall_time_seconds INTEGER,
                active_time_seconds INTEGER,

                -- Token usage
                input_tokens INTEGER DEFAULT 0,
                output_tokens INTEGER DEFAULT 0,
                total_tokens INTEGER DEFAULT 0,
                estimated_cost_usd REAL DEFAULT 0.0,

                -- Outcome tracking
                status TEXT NOT NULL DEFAULT 'in_progress',
                outcome_type TEXT,

                -- Quality indicators
                test_failures INTEGER DEFAULT 0,
                ci_failures INTEGER DEFAULT 0,
                commits_count INTEGER DEFAULT 0,
                lines_changed INTEGER DEFAULT 0,

                -- Metadata
                context TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_metrics_agent_system ON agent_metrics(agent_system);
            CREATE INDEX IF NOT EXISTS idx_metrics_task_type ON agent_metrics(task_type);
            CREATE INDEX IF NOT EXISTS idx_metrics_completed ON agent_metrics(completed_at);
            CREATE INDEX IF NOT EXISTS idx_metrics_github_issue ON agent_metrics(github_issue);
            CREATE INDEX IF NOT EXISTS idx_metrics_status ON agent_metrics(status);

            -- Token usage per API request (enhanced for LLM resource tracking)
            CREATE TABLE IF NOT EXISTS token_usage (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                input_id INTEGER REFERENCES agent_inputs(id),
                metric_id INTEGER REFERENCES agent_metrics(id),
                timestamp DATETIME NOT NULL,
                prompt_tokens INTEGER NOT NULL,
                completion_tokens INTEGER NOT NULL,
                total_tokens INTEGER NOT NULL,
                model TEXT,
                estimated_cost_usd REAL DEFAULT 0.0,
                -- Enhanced fields for resource tracking (Issue #1013)
                tokens_cache_read INTEGER,    -- Cache read tokens (prompt caching)
                tokens_cache_write INTEGER,   -- Cache write tokens (prompt caching)
                duration_ms INTEGER,          -- API response time in milliseconds
                provider TEXT                 -- 'anthropic', 'openai', etc.
            );

            CREATE INDEX IF NOT EXISTS idx_token_usage_input_id ON token_usage(input_id);
            CREATE INDEX IF NOT EXISTS idx_token_usage_metric_id ON token_usage(metric_id);
            CREATE INDEX IF NOT EXISTS idx_token_usage_timestamp ON token_usage(timestamp);
            CREATE INDEX IF NOT EXISTS idx_token_usage_model ON token_usage(model);
            CREATE INDEX IF NOT EXISTS idx_token_usage_provider ON token_usage(provider);

            -- GitHub events for correlating agent activity with GitHub actions
            CREATE TABLE IF NOT EXISTS github_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                activity_id INTEGER,
                event_type TEXT NOT NULL,
                event_time TEXT NOT NULL,
                pr_number INTEGER,
                issue_number INTEGER,
                commit_sha TEXT,
                author TEXT,
                FOREIGN KEY (activity_id) REFERENCES agent_metrics(id)
            );

            CREATE INDEX IF NOT EXISTS idx_github_events_activity_id ON github_events(activity_id);
            CREATE INDEX IF NOT EXISTS idx_github_events_event_type ON github_events(event_type);
            CREATE INDEX IF NOT EXISTS idx_github_events_event_time ON github_events(event_time);
            CREATE INDEX IF NOT EXISTS idx_github_events_pr_number ON github_events(pr_number);
            CREATE INDEX IF NOT EXISTS idx_github_events_issue_number ON github_events(issue_number);

            -- Per-prompt git change tracking
            -- Links individual prompts to the git changes they caused
            CREATE TABLE IF NOT EXISTS prompt_changes (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                input_id INTEGER REFERENCES agent_inputs(id),
                before_commit TEXT,
                after_commit TEXT,
                files_changed INTEGER DEFAULT 0,
                lines_added INTEGER DEFAULT 0,
                lines_removed INTEGER DEFAULT 0,
                tests_added INTEGER DEFAULT 0,
                tests_modified INTEGER DEFAULT 0
            );

            CREATE INDEX IF NOT EXISTS idx_prompt_changes_input_id ON prompt_changes(input_id);
            CREATE INDEX IF NOT EXISTS idx_prompt_changes_after_commit ON prompt_changes(after_commit);

            -- Prompt-GitHub correlation table for linking prompts to GitHub actions
            CREATE TABLE IF NOT EXISTS prompt_github (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                input_id INTEGER REFERENCES agent_inputs(id),
                issue_number INTEGER,
                pr_number INTEGER,
                label_before TEXT,  -- JSON array of labels
                label_after TEXT,   -- JSON array of labels
                event_type TEXT NOT NULL  -- 'issue_created', 'pr_created', 'pr_merged', 'label_changed', etc.
            );

            CREATE INDEX IF NOT EXISTS idx_prompt_github_input_id ON prompt_github(input_id);
            CREATE INDEX IF NOT EXISTS idx_prompt_github_issue_number ON prompt_github(issue_number);
            CREATE INDEX IF NOT EXISTS idx_prompt_github_pr_number ON prompt_github(pr_number);
            CREATE INDEX IF NOT EXISTS idx_prompt_github_event_type ON prompt_github(event_type);

            -- Quality metrics for tracking test outcomes and code quality
            CREATE TABLE IF NOT EXISTS quality_metrics (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                input_id INTEGER REFERENCES agent_inputs(id),
                timestamp DATETIME NOT NULL,

                -- Test results
                tests_passed INTEGER,
                tests_failed INTEGER,
                tests_skipped INTEGER,
                test_runner TEXT,

                -- Lint/format results
                lint_errors INTEGER,
                format_errors INTEGER,

                -- Build status
                build_success BOOLEAN,

                -- PR review outcomes (populated later)
                pr_approved BOOLEAN,
                pr_changes_requested BOOLEAN,

                -- Rework tracking (Issue #1054)
                -- Counts how many review cycles a PR goes through
                rework_count INTEGER DEFAULT 0,

                -- Human rating (optional, for future use)
                human_rating INTEGER
            );

            CREATE INDEX IF NOT EXISTS idx_quality_metrics_input_id ON quality_metrics(input_id);
            CREATE INDEX IF NOT EXISTS idx_quality_metrics_timestamp ON quality_metrics(timestamp);
            CREATE INDEX IF NOT EXISTS idx_quality_metrics_test_runner ON quality_metrics(test_runner);

            -- Resource usage tracking for LLM API calls (Issue #1053)
            -- Links terminal output parsing to token consumption and costs
            CREATE TABLE IF NOT EXISTS resource_usage (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                input_id INTEGER REFERENCES agent_inputs(id),
                timestamp DATETIME NOT NULL,
                model TEXT NOT NULL,
                tokens_input INTEGER NOT NULL,
                tokens_output INTEGER NOT NULL,
                tokens_cache_read INTEGER,
                tokens_cache_write INTEGER,
                cost_usd REAL NOT NULL,
                duration_ms INTEGER,
                provider TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_resource_usage_input_id ON resource_usage(input_id);
            CREATE INDEX IF NOT EXISTS idx_resource_usage_timestamp ON resource_usage(timestamp);
            CREATE INDEX IF NOT EXISTS idx_resource_usage_model ON resource_usage(model);
            CREATE INDEX IF NOT EXISTS idx_resource_usage_provider ON resource_usage(provider);

            -- Budget configuration for cost tracking (Issue #1064)
            -- Allows setting budget limits per period with alert thresholds
            CREATE TABLE IF NOT EXISTS budget_config (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                period TEXT NOT NULL CHECK(period IN ('daily', 'weekly', 'monthly')),
                limit_usd REAL NOT NULL,
                alert_threshold REAL NOT NULL DEFAULT 0.8,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                updated_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                is_active BOOLEAN NOT NULL DEFAULT 1
            );

            CREATE INDEX IF NOT EXISTS idx_budget_config_period ON budget_config(period);
            CREATE INDEX IF NOT EXISTS idx_budget_config_active ON budget_config(is_active);

            -- Issue claim registry for reliable work distribution (Issue #1159)
            -- Tracks which agent/terminal has claimed which issue/PR
            -- Prevents race conditions and enables crash recovery
            CREATE TABLE IF NOT EXISTS issue_claims (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                number INTEGER NOT NULL,
                claim_type TEXT NOT NULL CHECK(claim_type IN ('issue', 'pr')),
                terminal_id TEXT NOT NULL,
                claimed_at DATETIME NOT NULL,
                last_heartbeat DATETIME NOT NULL,
                label TEXT,
                agent_role TEXT,
                -- Unique constraint: only one claim per issue/PR type combo
                UNIQUE(number, claim_type)
            );

            CREATE INDEX IF NOT EXISTS idx_issue_claims_terminal ON issue_claims(terminal_id);
            CREATE INDEX IF NOT EXISTS idx_issue_claims_heartbeat ON issue_claims(last_heartbeat);
            CREATE INDEX IF NOT EXISTS idx_issue_claims_type ON issue_claims(claim_type);
            ",
    )?;

    // Run migrations for existing databases
    migrate_token_usage_table(conn);
    migrate_quality_metrics_table(conn);

    // Create cost analytics views
    create_cost_analytics_views(conn);

    // Create tuning schema (Issue #1074)
    create_tuning_schema(conn);

    Ok(())
}

/// Create tuning-related database tables (Issue #1074).
///
/// Wraps the tuning module's schema creation and handles errors gracefully.
fn create_tuning_schema(conn: &Connection) {
    match super::tuning::create_tuning_schema(conn) {
        Ok(()) => log::debug!("Created or verified tuning schema"),
        Err(e) => {
            log::warn!("Could not create tuning schema: {e}");
        }
    }
}

/// Create SQL views for cost analytics (Issue #1064).
///
/// Creates views for aggregating costs by:
/// - Agent role (`cost_by_role`)
/// - GitHub issue (`cost_by_issue`)
/// - PR (`cost_by_pr`)
/// - Time period (`cost_by_day`, `cost_by_week`, `cost_by_month`)
fn create_cost_analytics_views(conn: &Connection) {
    // Cost by role view - aggregates costs from resource_usage joined with agent_inputs
    let views: [(&str, &str); 6] = [
        (
            "cost_by_role",
            r"
            CREATE VIEW IF NOT EXISTS cost_by_role AS
            SELECT
                COALESCE(ai.agent_role, 'unknown') as agent_role,
                SUM(ru.cost_usd) as total_cost,
                COUNT(*) as request_count,
                AVG(ru.cost_usd) as avg_cost,
                SUM(ru.tokens_input) as total_input_tokens,
                SUM(ru.tokens_output) as total_output_tokens,
                MIN(ru.timestamp) as first_usage,
                MAX(ru.timestamp) as last_usage
            FROM resource_usage ru
            LEFT JOIN agent_inputs ai ON ru.input_id = ai.id
            GROUP BY COALESCE(ai.agent_role, 'unknown')
            ",
        ),
        (
            "cost_by_issue",
            r"
            CREATE VIEW IF NOT EXISTS cost_by_issue AS
            SELECT
                pg.issue_number,
                SUM(ru.cost_usd) as total_cost,
                COUNT(DISTINCT ru.input_id) as prompt_count,
                SUM(ru.tokens_input) as total_input_tokens,
                SUM(ru.tokens_output) as total_output_tokens,
                MIN(ru.timestamp) as first_usage,
                MAX(ru.timestamp) as last_usage
            FROM resource_usage ru
            JOIN agent_inputs ai ON ru.input_id = ai.id
            JOIN prompt_github pg ON ai.id = pg.input_id
            WHERE pg.issue_number IS NOT NULL
            GROUP BY pg.issue_number
            ",
        ),
        (
            "cost_by_pr",
            r"
            CREATE VIEW IF NOT EXISTS cost_by_pr AS
            SELECT
                pg.pr_number,
                SUM(ru.cost_usd) as total_cost,
                COUNT(DISTINCT ru.input_id) as prompt_count,
                SUM(ru.tokens_input) as total_input_tokens,
                SUM(ru.tokens_output) as total_output_tokens,
                MIN(ru.timestamp) as first_usage,
                MAX(ru.timestamp) as last_usage
            FROM resource_usage ru
            JOIN agent_inputs ai ON ru.input_id = ai.id
            JOIN prompt_github pg ON ai.id = pg.input_id
            WHERE pg.pr_number IS NOT NULL
            GROUP BY pg.pr_number
            ",
        ),
        (
            "cost_by_day",
            r"
            CREATE VIEW IF NOT EXISTS cost_by_day AS
            SELECT
                DATE(ru.timestamp) as date,
                SUM(ru.cost_usd) as total_cost,
                COUNT(*) as request_count,
                SUM(ru.tokens_input) as total_input_tokens,
                SUM(ru.tokens_output) as total_output_tokens
            FROM resource_usage ru
            GROUP BY DATE(ru.timestamp)
            ORDER BY date DESC
            ",
        ),
        (
            "cost_by_week",
            r"
            CREATE VIEW IF NOT EXISTS cost_by_week AS
            SELECT
                strftime('%Y-W%W', ru.timestamp) as week,
                SUM(ru.cost_usd) as total_cost,
                COUNT(*) as request_count,
                SUM(ru.tokens_input) as total_input_tokens,
                SUM(ru.tokens_output) as total_output_tokens
            FROM resource_usage ru
            GROUP BY strftime('%Y-W%W', ru.timestamp)
            ORDER BY week DESC
            ",
        ),
        (
            "cost_by_month",
            r"
            CREATE VIEW IF NOT EXISTS cost_by_month AS
            SELECT
                strftime('%Y-%m', ru.timestamp) as month,
                SUM(ru.cost_usd) as total_cost,
                COUNT(*) as request_count,
                SUM(ru.tokens_input) as total_input_tokens,
                SUM(ru.tokens_output) as total_output_tokens
            FROM resource_usage ru
            GROUP BY strftime('%Y-%m', ru.timestamp)
            ORDER BY month DESC
            ",
        ),
    ];

    for (view_name, sql) in views {
        match conn.execute_batch(sql) {
            Ok(()) => log::debug!("Created or verified view: {view_name}"),
            Err(e) => {
                // Views already existing is fine
                let err_str = e.to_string();
                if err_str.contains("already exists") {
                    log::debug!("View {view_name} already exists");
                } else {
                    log::warn!("Could not create view {view_name}: {e}");
                }
            }
        }
    }
}

/// Migrate `token_usage` table to add new columns for resource tracking.
///
/// This function adds columns that may not exist in older databases:
/// - `tokens_cache_read`
/// - `tokens_cache_write`
/// - `duration_ms`
/// - provider
///
/// Uses ALTER TABLE ADD COLUMN which is idempotent-ish (ignores errors for
/// existing columns).
/// Migrate `quality_metrics` table to add new columns.
///
/// This function adds columns that may not exist in older databases:
/// - `rework_count` (Issue #1054)
///
/// Uses ALTER TABLE ADD COLUMN which is idempotent-ish (ignores errors for
/// existing columns).
fn migrate_quality_metrics_table(conn: &Connection) {
    // Add rework_count column for tracking PR review cycles
    let sql = "ALTER TABLE quality_metrics ADD COLUMN rework_count INTEGER DEFAULT 0";

    match conn.execute(sql, []) {
        Ok(_) => log::info!("Added column rework_count to quality_metrics table"),
        Err(e) => {
            let err_str = e.to_string();
            if err_str.contains("duplicate column") || err_str.contains("already exists") {
                log::debug!("Column rework_count already exists in quality_metrics table");
            } else {
                log::debug!(
                    "Could not add column rework_count to quality_metrics: {e} (may be expected)"
                );
            }
        }
    }
}

fn migrate_token_usage_table(conn: &Connection) {
    // List of new columns to add (column_name, column_type, default_value)
    let new_columns: [(&str, &str, Option<&str>); 4] = [
        ("tokens_cache_read", "INTEGER", None),
        ("tokens_cache_write", "INTEGER", None),
        ("duration_ms", "INTEGER", None),
        ("provider", "TEXT", None),
    ];

    for (column_name, column_type, default) in new_columns {
        let sql = match default {
            Some(def) => format!(
                "ALTER TABLE token_usage ADD COLUMN {column_name} {column_type} DEFAULT {def}"
            ),
            None => format!("ALTER TABLE token_usage ADD COLUMN {column_name} {column_type}"),
        };

        // SQLite will error if column already exists, which is fine
        match conn.execute(&sql, []) {
            Ok(_) => log::info!("Added column {column_name} to token_usage table"),
            Err(e) => {
                // Check if it's a "duplicate column" error (expected for existing DBs)
                let err_str = e.to_string();
                if err_str.contains("duplicate column") || err_str.contains("already exists") {
                    log::debug!("Column {column_name} already exists in token_usage table");
                } else {
                    // Unexpected error - log but don't fail (table might not exist yet)
                    log::debug!(
                        "Could not add column {column_name} to token_usage: {e} (may be expected)"
                    );
                }
            }
        }
    }
}
