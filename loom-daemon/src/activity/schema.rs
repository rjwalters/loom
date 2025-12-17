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

            -- Token usage per API request
            CREATE TABLE IF NOT EXISTS token_usage (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                input_id INTEGER REFERENCES agent_inputs(id),
                metric_id INTEGER REFERENCES agent_metrics(id),
                timestamp DATETIME NOT NULL,
                prompt_tokens INTEGER NOT NULL,
                completion_tokens INTEGER NOT NULL,
                total_tokens INTEGER NOT NULL,
                model TEXT,
                estimated_cost_usd REAL DEFAULT 0.0
            );

            CREATE INDEX IF NOT EXISTS idx_token_usage_input_id ON token_usage(input_id);
            CREATE INDEX IF NOT EXISTS idx_token_usage_metric_id ON token_usage(metric_id);
            CREATE INDEX IF NOT EXISTS idx_token_usage_timestamp ON token_usage(timestamp);

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
            ",
    )?;

    Ok(())
}
