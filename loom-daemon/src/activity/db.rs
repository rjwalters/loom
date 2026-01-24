//! Database operations for activity tracking.
//!
//! This module contains the `ActivityDb` struct and all methods for
//! recording and querying agent activity data.

use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use std::path::PathBuf;

use super::models::{
    ActivityEntry, AgentInput, AgentMetric, AgentOutput, InputContext, InputType,
    ProductivitySummary, PromptChanges, TokenUsage,
};
use super::schema::init_schema;

/// Activity database for tracking agent inputs and results
pub struct ActivityDb {
    conn: Connection,
}

impl ActivityDb {
    /// Create or open activity database at the given path
    pub fn new(db_path: PathBuf) -> Result<Self> {
        let conn = Connection::open(db_path)?;
        let db = Self { conn };
        init_schema(&db.conn)?;
        Ok(db)
    }

    /// Record a new agent input
    pub fn record_input(&self, input: &AgentInput) -> Result<i64> {
        let context_json = serde_json::to_string(&input.context)?;

        self.conn.execute(
            r"
            INSERT INTO agent_inputs (terminal_id, timestamp, input_type, content, agent_role, context)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ",
            params![
                &input.terminal_id,
                input.timestamp.to_rfc3339(),
                input.input_type.as_str(),
                &input.content,
                &input.agent_role,
                &context_json,
            ],
        )?;

        Ok(self.conn.last_insert_rowid())
    }

    /// Record terminal output sample
    pub fn record_output(&self, output: &AgentOutput) -> Result<i64> {
        self.conn.execute(
            r"
            INSERT INTO agent_outputs (input_id, terminal_id, timestamp, content, content_preview, exit_code, metadata)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ",
            params![
                output.input_id,
                &output.terminal_id,
                output.timestamp.to_rfc3339(),
                &output.content,
                &output.content_preview,
                output.exit_code,
                &output.metadata,
            ],
        )?;

        Ok(self.conn.last_insert_rowid())
    }

    /// Get recent inputs for a terminal
    #[allow(dead_code)]
    pub fn get_recent_inputs(&self, terminal_id: &str, limit: usize) -> Result<Vec<AgentInput>> {
        let mut stmt = self.conn.prepare(
            r"
            SELECT id, terminal_id, timestamp, input_type, content, agent_role, context
            FROM agent_inputs
            WHERE terminal_id = ?1
            ORDER BY timestamp DESC
            LIMIT ?2
            ",
        )?;

        let inputs = stmt.query_map(params![terminal_id, limit], |row| {
            let id: i64 = row.get(0)?;
            let terminal_id: String = row.get(1)?;
            let timestamp_str: String = row.get(2)?;
            let input_type_str: String = row.get(3)?;
            let content: String = row.get(4)?;
            let agent_role: Option<String> = row.get(5)?;
            let ctx_json: String = row.get(6)?;

            let timestamp = DateTime::parse_from_rfc3339(&timestamp_str)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?
                .with_timezone(&Utc);

            let input_type = InputType::from_str(&input_type_str).ok_or_else(|| {
                rusqlite::Error::ToSqlConversionFailure(
                    format!("Invalid input_type: {input_type_str}").into(),
                )
            })?;

            let ctx: InputContext = serde_json::from_str(&ctx_json)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;

            Ok(AgentInput {
                id: Some(id),
                terminal_id,
                timestamp,
                input_type,
                content,
                agent_role,
                context: ctx,
            })
        })?;

        inputs.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Get total count of inputs
    #[allow(dead_code)]
    pub fn get_input_count(&self) -> Result<i64> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM agent_inputs", [], |row| row.get(0))?;
        Ok(count)
    }

    /// Start tracking a new task
    #[allow(dead_code)]
    pub fn start_task(&self, metric: &AgentMetric) -> Result<i64> {
        let context_json = metric.context.as_deref();

        self.conn.execute(
            r"
            INSERT INTO agent_metrics (
                terminal_id, agent_role, agent_system, task_type,
                github_issue, github_pr, started_at, status, context
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            ",
            params![
                &metric.terminal_id,
                &metric.agent_role,
                &metric.agent_system,
                &metric.task_type,
                metric.github_issue,
                metric.github_pr,
                metric.started_at.to_rfc3339(),
                &metric.status,
                context_json,
            ],
        )?;

        Ok(self.conn.last_insert_rowid())
    }

    /// Complete a task and calculate wall time
    #[allow(dead_code)]
    pub fn complete_task(
        &self,
        metric_id: i64,
        status: &str,
        outcome_type: Option<&str>,
    ) -> Result<()> {
        let now = Utc::now();

        // Get started_at from database
        let started_at: String = self.conn.query_row(
            "SELECT started_at FROM agent_metrics WHERE id = ?1",
            params![metric_id],
            |row| row.get(0),
        )?;

        let started = DateTime::parse_from_rfc3339(&started_at)?.with_timezone(&Utc);
        let wall_time_seconds = (now - started).num_seconds();

        self.conn.execute(
            r"
            UPDATE agent_metrics
            SET completed_at = ?1,
                wall_time_seconds = ?2,
                status = ?3,
                outcome_type = ?4
            WHERE id = ?5
            ",
            params![
                now.to_rfc3339(),
                wall_time_seconds,
                status,
                outcome_type,
                metric_id
            ],
        )?;

        Ok(())
    }

    /// Update task quality metrics (commits, CI failures, etc.)
    #[allow(dead_code)]
    pub fn update_task_metrics(
        &self,
        metric_id: i64,
        test_failures: Option<i32>,
        ci_failures: Option<i32>,
        commits_count: Option<i32>,
        lines_changed: Option<i32>,
    ) -> Result<()> {
        self.conn.execute(
            r"
            UPDATE agent_metrics
            SET test_failures = COALESCE(?1, test_failures),
                ci_failures = COALESCE(?2, ci_failures),
                commits_count = COALESCE(?3, commits_count),
                lines_changed = COALESCE(?4, lines_changed)
            WHERE id = ?5
            ",
            params![
                test_failures,
                ci_failures,
                commits_count,
                lines_changed,
                metric_id
            ],
        )?;

        Ok(())
    }

    /// Record token usage for an API request
    ///
    /// Records comprehensive LLM resource usage including token counts, cache usage,
    /// duration, and cost. If a metric_id is provided, also updates the aggregate
    /// token counts in agent_metrics.
    #[allow(dead_code)]
    pub fn record_token_usage(&self, usage: &TokenUsage) -> Result<i64> {
        self.conn.execute(
            r"
            INSERT INTO token_usage (
                input_id, metric_id, timestamp, prompt_tokens,
                completion_tokens, total_tokens, model, estimated_cost_usd,
                tokens_cache_read, tokens_cache_write, duration_ms, provider
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
            ",
            params![
                usage.input_id,
                usage.metric_id,
                usage.timestamp.to_rfc3339(),
                usage.prompt_tokens,
                usage.completion_tokens,
                usage.total_tokens,
                &usage.model,
                usage.estimated_cost_usd,
                usage.tokens_cache_read,
                usage.tokens_cache_write,
                usage.duration_ms,
                &usage.provider,
            ],
        )?;

        // Update aggregate token counts in agent_metrics if metric_id is provided
        if let Some(metric_id) = usage.metric_id {
            self.conn.execute(
                r"
                UPDATE agent_metrics
                SET input_tokens = input_tokens + ?1,
                    output_tokens = output_tokens + ?2,
                    total_tokens = total_tokens + ?3,
                    estimated_cost_usd = estimated_cost_usd + ?4
                WHERE id = ?5
                ",
                params![
                    usage.prompt_tokens,
                    usage.completion_tokens,
                    usage.total_tokens,
                    usage.estimated_cost_usd,
                    metric_id
                ],
            )?;
        }

        Ok(self.conn.last_insert_rowid())
    }

    /// Get metrics for a specific task
    #[allow(dead_code)]
    pub fn get_task_metrics(&self, metric_id: i64) -> Result<AgentMetric> {
        Ok(self.conn.query_row(
            r"
            SELECT id, terminal_id, agent_role, agent_system, task_type,
                   github_issue, github_pr, started_at, completed_at,
                   wall_time_seconds, active_time_seconds, input_tokens,
                   output_tokens, total_tokens, estimated_cost_usd, status,
                   outcome_type, test_failures, ci_failures, commits_count,
                   lines_changed, context
            FROM agent_metrics
            WHERE id = ?1
            ",
            params![metric_id],
            |row| {
                let started_str: String = row.get(7)?;
                let completed_str: Option<String> = row.get(8)?;

                let started_at = DateTime::parse_from_rfc3339(&started_str)
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?
                    .with_timezone(&Utc);

                let completed_at = if let Some(completed) = completed_str {
                    Some(
                        DateTime::parse_from_rfc3339(&completed)
                            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?
                            .with_timezone(&Utc),
                    )
                } else {
                    None
                };

                Ok(AgentMetric {
                    id: Some(row.get(0)?),
                    terminal_id: row.get(1)?,
                    agent_role: row.get(2)?,
                    agent_system: row.get(3)?,
                    task_type: row.get(4)?,
                    github_issue: row.get(5)?,
                    github_pr: row.get(6)?,
                    started_at,
                    completed_at,
                    wall_time_seconds: row.get(9)?,
                    active_time_seconds: row.get(10)?,
                    input_tokens: row.get(11)?,
                    output_tokens: row.get(12)?,
                    total_tokens: row.get(13)?,
                    estimated_cost_usd: row.get(14)?,
                    status: row.get(15)?,
                    outcome_type: row.get(16)?,
                    test_failures: row.get(17)?,
                    ci_failures: row.get(18)?,
                    commits_count: row.get(19)?,
                    lines_changed: row.get(20)?,
                    context: row.get(21)?,
                })
            },
        )?)
    }

    /// Get productivity summary grouped by agent system
    #[allow(dead_code)]
    pub fn get_productivity_summary(&self) -> Result<ProductivitySummary> {
        let mut stmt = self.conn.prepare(
            r"
            SELECT
                agent_system,
                COUNT(*) as tasks_completed,
                AVG(wall_time_seconds / 60.0) as avg_minutes,
                AVG(total_tokens) as avg_tokens,
                SUM(estimated_cost_usd) as total_cost
            FROM agent_metrics
            WHERE status = 'success' AND completed_at IS NOT NULL
            GROUP BY agent_system
            ORDER BY tasks_completed DESC
            ",
        )?;

        let results = stmt.query_map([], |row| {
            Ok((
                row.get(0)?, // agent_system
                row.get(1)?, // tasks_completed
                row.get(2)?, // avg_minutes
                row.get(3)?, // avg_tokens
                row.get(4)?, // total_cost
            ))
        })?;

        results.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Get terminal activity history (inputs joined with outputs)
    /// Returns entries in reverse chronological order (most recent first)
    #[allow(dead_code)]
    pub fn get_terminal_activity(
        &self,
        terminal_id: &str,
        limit: usize,
    ) -> Result<Vec<ActivityEntry>> {
        let mut stmt = self.conn.prepare(
            r"
            SELECT
                i.id as input_id,
                i.timestamp as input_timestamp,
                i.input_type,
                i.content as prompt,
                i.agent_role,
                i.context,
                o.content_preview as output_preview,
                o.exit_code,
                o.timestamp as output_timestamp
            FROM agent_inputs i
            LEFT JOIN agent_outputs o ON i.id = o.input_id
            WHERE i.terminal_id = ?1
            ORDER BY i.timestamp DESC
            LIMIT ?2
            ",
        )?;

        let entries = stmt.query_map(params![terminal_id, limit], |row| {
            // Parse context JSON to extract git_branch
            let ctx_json: String = row.get(5)?;
            let ctx: InputContext = serde_json::from_str(&ctx_json).unwrap_or_default();

            // Parse input timestamp
            let input_ts_str: String = row.get(1)?;
            let input_timestamp = DateTime::parse_from_rfc3339(&input_ts_str)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?
                .with_timezone(&Utc);

            // Parse input type
            let input_type_str: String = row.get(2)?;
            let input_type = InputType::from_str(&input_type_str).ok_or_else(|| {
                rusqlite::Error::ToSqlConversionFailure(
                    format!("Invalid input_type: {input_type_str}").into(),
                )
            })?;

            // Parse output timestamp (optional)
            let output_timestamp = if let Ok(Some(ts_str)) = row.get::<_, Option<String>>(8) {
                DateTime::parse_from_rfc3339(&ts_str)
                    .ok()
                    .map(|dt| dt.with_timezone(&Utc))
            } else {
                None
            };

            Ok(ActivityEntry {
                input_id: row.get(0)?,
                timestamp: input_timestamp,
                input_type,
                prompt: row.get(3)?,
                agent_role: row.get(4)?,
                git_branch: ctx.branch,
                output_preview: row.get(6)?,
                exit_code: row.get(7)?,
                output_timestamp,
            })
        })?;

        entries.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Record git changes associated with a prompt
    pub fn record_prompt_changes(&self, changes: &PromptChanges) -> Result<i64> {
        self.conn.execute(
            r"
            INSERT INTO prompt_changes (
                input_id, before_commit, after_commit, files_changed,
                lines_added, lines_removed, tests_added, tests_modified
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ",
            params![
                changes.input_id,
                &changes.before_commit,
                &changes.after_commit,
                changes.files_changed,
                changes.lines_added,
                changes.lines_removed,
                changes.tests_added,
                changes.tests_modified,
            ],
        )?;

        Ok(self.conn.last_insert_rowid())
    }

    /// Get prompt changes for a specific input
    #[allow(dead_code)]
    pub fn get_prompt_changes(&self, input_id: i64) -> Result<Option<PromptChanges>> {
        let result = self.conn.query_row(
            r"
            SELECT id, input_id, before_commit, after_commit, files_changed,
                   lines_added, lines_removed, tests_added, tests_modified
            FROM prompt_changes
            WHERE input_id = ?1
            ",
            params![input_id],
            |row| {
                Ok(PromptChanges {
                    id: Some(row.get(0)?),
                    input_id: row.get(1)?,
                    before_commit: row.get(2)?,
                    after_commit: row.get(3)?,
                    files_changed: row.get(4)?,
                    lines_added: row.get(5)?,
                    lines_removed: row.get(6)?,
                    tests_added: row.get(7)?,
                    tests_modified: row.get(8)?,
                })
            },
        );

        match result {
            Ok(changes) => Ok(Some(changes)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get total lines changed across all prompts for a terminal
    #[allow(dead_code)]
    pub fn get_terminal_changes_summary(
        &self,
        terminal_id: &str,
    ) -> Result<(i64, i64, i64)> {
        // Returns (total_files_changed, total_lines_added, total_lines_removed)
        let result = self.conn.query_row(
            r"
            SELECT
                COALESCE(SUM(pc.files_changed), 0),
                COALESCE(SUM(pc.lines_added), 0),
                COALESCE(SUM(pc.lines_removed), 0)
            FROM prompt_changes pc
            JOIN agent_inputs ai ON pc.input_id = ai.id
            WHERE ai.terminal_id = ?1
            ",
            params![terminal_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_create_database() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // Verify schema created successfully
        let count = db.get_input_count()?;
        assert_eq!(count, 0);
        Ok(())
    }

    #[test]
    fn test_record_and_retrieve_input() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        let input = AgentInput {
            id: None,
            terminal_id: "terminal-1".to_string(),
            timestamp: Utc::now(),
            input_type: InputType::Manual,
            content: "ls -la".to_string(),
            agent_role: Some("worker".to_string()),
            context: InputContext {
                workspace: Some("/path/to/workspace".to_string()),
                branch: Some("main".to_string()),
                ..Default::default()
            },
        };

        let id = db.record_input(&input)?;
        assert!(id > 0);

        let inputs = db.get_recent_inputs("terminal-1", 10)?;
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].content, "ls -la");
        assert_eq!(inputs[0].agent_role, Some("worker".to_string()));
        Ok(())
    }

    #[test]
    fn test_multiple_inputs() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        for i in 0..5 {
            let input = AgentInput {
                id: None,
                terminal_id: "terminal-1".to_string(),
                timestamp: Utc::now(),
                input_type: InputType::Autonomous,
                content: format!("command {i}"),
                agent_role: Some("worker".to_string()),
                context: InputContext::default(),
            };
            db.record_input(&input)?;
        }

        let inputs = db.get_recent_inputs("terminal-1", 3)?;
        assert_eq!(inputs.len(), 3);

        let count = db.get_input_count()?;
        assert_eq!(count, 5);
        Ok(())
    }

    #[test]
    fn test_record_output() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // First record an input
        let input = AgentInput {
            id: None,
            terminal_id: "terminal-1".to_string(),
            timestamp: Utc::now(),
            input_type: InputType::Manual,
            content: "ls -la".to_string(),
            agent_role: Some("worker".to_string()),
            context: InputContext::default(),
        };
        let input_id = db.record_input(&input)?;

        // Now record an output linked to that input
        let output_content = "total 48\ndrwxr-xr-x  8 user  staff  256 Oct 16 00:00 .\n";
        let output = AgentOutput {
            id: None,
            input_id: Some(input_id),
            terminal_id: "terminal-1".to_string(),
            timestamp: Utc::now(),
            content: Some(output_content.to_string()),
            content_preview: Some(output_content[..50.min(output_content.len())].to_string()),
            exit_code: Some(0),
            metadata: None,
        };

        let output_id = db.record_output(&output)?;
        assert!(output_id > 0);

        // Verify output was recorded by querying directly
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM agent_outputs", [], |row| row.get(0))?;
        assert_eq!(count, 1);

        Ok(())
    }

    #[test]
    fn test_start_and_complete_task() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // Start a task
        let metric = AgentMetric {
            id: None,
            terminal_id: "terminal-1".to_string(),
            agent_role: "builder".to_string(),
            agent_system: "claude-code".to_string(),
            task_type: Some("issue".to_string()),
            github_issue: Some(297),
            github_pr: None,
            started_at: Utc::now(),
            completed_at: None,
            wall_time_seconds: None,
            active_time_seconds: None,
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
            estimated_cost_usd: 0.0,
            status: "in_progress".to_string(),
            outcome_type: None,
            test_failures: 0,
            ci_failures: 0,
            commits_count: 0,
            lines_changed: 0,
            context: Some(r#"{"workspace": "/path/to/workspace"}"#.to_string()),
        };

        let metric_id = db.start_task(&metric)?;
        assert!(metric_id > 0);

        // Sleep briefly to ensure wall time is non-zero
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Complete the task
        db.complete_task(metric_id, "success", Some("pr_created"))?;

        // Verify the task was completed with wall time calculated
        let retrieved = db.get_task_metrics(metric_id)?;
        assert_eq!(retrieved.status, "success");
        assert_eq!(retrieved.outcome_type, Some("pr_created".to_string()));
        assert!(retrieved.completed_at.is_some());
        assert!(retrieved.wall_time_seconds.is_some());
        if let Some(wall_time) = retrieved.wall_time_seconds {
            assert!(wall_time >= 0);
        }

        Ok(())
    }

    #[test]
    fn test_record_token_usage() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // Start a task first
        let metric = AgentMetric {
            id: None,
            terminal_id: "terminal-1".to_string(),
            agent_role: "builder".to_string(),
            agent_system: "claude-code".to_string(),
            task_type: Some("issue".to_string()),
            github_issue: Some(297),
            github_pr: None,
            started_at: Utc::now(),
            completed_at: None,
            wall_time_seconds: None,
            active_time_seconds: None,
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
            estimated_cost_usd: 0.0,
            status: "in_progress".to_string(),
            outcome_type: None,
            test_failures: 0,
            ci_failures: 0,
            commits_count: 0,
            lines_changed: 0,
            context: None,
        };

        let metric_id = db.start_task(&metric)?;

        // Record token usage with enhanced fields
        let usage = TokenUsage {
            id: None,
            input_id: None,
            metric_id: Some(metric_id),
            timestamp: Utc::now(),
            prompt_tokens: 1000,
            completion_tokens: 500,
            total_tokens: 1500,
            model: Some("claude-sonnet-4".to_string()),
            estimated_cost_usd: 0.045,
            tokens_cache_read: Some(200),
            tokens_cache_write: Some(50),
            duration_ms: Some(1500),
            provider: Some("anthropic".to_string()),
        };

        let usage_id = db.record_token_usage(&usage)?;
        assert!(usage_id > 0);

        // Verify token counts were aggregated in agent_metrics
        let retrieved = db.get_task_metrics(metric_id)?;
        assert_eq!(retrieved.input_tokens, 1000);
        assert_eq!(retrieved.output_tokens, 500);
        assert_eq!(retrieved.total_tokens, 1500);
        assert!((retrieved.estimated_cost_usd - 0.045).abs() < 0.001);

        // Record another token usage with enhanced fields
        let usage2 = TokenUsage {
            id: None,
            input_id: None,
            metric_id: Some(metric_id),
            timestamp: Utc::now(),
            prompt_tokens: 800,
            completion_tokens: 400,
            total_tokens: 1200,
            model: Some("claude-sonnet-4".to_string()),
            estimated_cost_usd: 0.036,
            tokens_cache_read: Some(100),
            tokens_cache_write: None,
            duration_ms: Some(1200),
            provider: Some("anthropic".to_string()),
        };

        db.record_token_usage(&usage2)?;

        // Verify cumulative totals
        let retrieved = db.get_task_metrics(metric_id)?;
        assert_eq!(retrieved.input_tokens, 1800);
        assert_eq!(retrieved.output_tokens, 900);
        assert_eq!(retrieved.total_tokens, 2700);
        assert!((retrieved.estimated_cost_usd - 0.081).abs() < 0.001);

        Ok(())
    }

    #[test]
    fn test_update_task_metrics() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // Start a task
        let metric = AgentMetric {
            id: None,
            terminal_id: "terminal-1".to_string(),
            agent_role: "builder".to_string(),
            agent_system: "claude-code".to_string(),
            task_type: Some("issue".to_string()),
            github_issue: Some(297),
            github_pr: None,
            started_at: Utc::now(),
            completed_at: None,
            wall_time_seconds: None,
            active_time_seconds: None,
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
            estimated_cost_usd: 0.0,
            status: "in_progress".to_string(),
            outcome_type: None,
            test_failures: 0,
            ci_failures: 0,
            commits_count: 0,
            lines_changed: 0,
            context: None,
        };

        let metric_id = db.start_task(&metric)?;

        // Update quality metrics
        db.update_task_metrics(metric_id, Some(2), Some(1), Some(5), Some(250))?;

        // Verify metrics were updated
        let retrieved = db.get_task_metrics(metric_id)?;
        assert_eq!(retrieved.test_failures, 2);
        assert_eq!(retrieved.ci_failures, 1);
        assert_eq!(retrieved.commits_count, 5);
        assert_eq!(retrieved.lines_changed, 250);

        // Update only some metrics (others should remain unchanged)
        db.update_task_metrics(metric_id, None, None, Some(7), None)?;

        let retrieved = db.get_task_metrics(metric_id)?;
        assert_eq!(retrieved.test_failures, 2); // unchanged
        assert_eq!(retrieved.ci_failures, 1); // unchanged
        assert_eq!(retrieved.commits_count, 7); // updated
        assert_eq!(retrieved.lines_changed, 250); // unchanged

        Ok(())
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn test_productivity_summary() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // Create multiple completed tasks with different agent systems
        for i in 0..3 {
            let metric = AgentMetric {
                id: None,
                terminal_id: "terminal-1".to_string(),
                agent_role: "builder".to_string(),
                agent_system: "claude-code".to_string(),
                task_type: Some("issue".to_string()),
                github_issue: Some(100 + i),
                github_pr: None,
                started_at: Utc::now(),
                completed_at: None,
                wall_time_seconds: None,
                active_time_seconds: None,
                input_tokens: 0,
                output_tokens: 0,
                total_tokens: 0,
                estimated_cost_usd: 0.0,
                status: "in_progress".to_string(),
                outcome_type: None,
                test_failures: 0,
                ci_failures: 0,
                commits_count: 0,
                lines_changed: 0,
                context: None,
            };

            let metric_id = db.start_task(&metric)?;

            // Add token usage
            let usage = TokenUsage {
                id: None,
                input_id: None,
                metric_id: Some(metric_id),
                timestamp: Utc::now(),
                prompt_tokens: 1000,
                completion_tokens: 500,
                total_tokens: 1500,
                model: Some("claude-sonnet-4".to_string()),
                estimated_cost_usd: 0.045,
                tokens_cache_read: None,
                tokens_cache_write: None,
                duration_ms: Some(1000),
                provider: Some("anthropic".to_string()),
            };
            db.record_token_usage(&usage)?;

            std::thread::sleep(std::time::Duration::from_millis(10));
            db.complete_task(metric_id, "success", Some("pr_created"))?;
        }

        // Create tasks for a different agent system
        for i in 0..2 {
            let metric = AgentMetric {
                id: None,
                terminal_id: "terminal-2".to_string(),
                agent_role: "builder".to_string(),
                agent_system: "codex".to_string(),
                task_type: Some("issue".to_string()),
                github_issue: Some(200 + i),
                github_pr: None,
                started_at: Utc::now(),
                completed_at: None,
                wall_time_seconds: None,
                active_time_seconds: None,
                input_tokens: 0,
                output_tokens: 0,
                total_tokens: 0,
                estimated_cost_usd: 0.0,
                status: "in_progress".to_string(),
                outcome_type: None,
                test_failures: 0,
                ci_failures: 0,
                commits_count: 0,
                lines_changed: 0,
                context: None,
            };

            let metric_id = db.start_task(&metric)?;

            // Add token usage
            let usage = TokenUsage {
                id: None,
                input_id: None,
                metric_id: Some(metric_id),
                timestamp: Utc::now(),
                prompt_tokens: 800,
                completion_tokens: 400,
                total_tokens: 1200,
                model: Some("codex".to_string()),
                estimated_cost_usd: 0.024,
                tokens_cache_read: None,
                tokens_cache_write: None,
                duration_ms: Some(800),
                provider: Some("openai".to_string()),
            };
            db.record_token_usage(&usage)?;

            std::thread::sleep(std::time::Duration::from_millis(10));
            db.complete_task(metric_id, "success", Some("pr_created"))?;
        }

        // Get productivity summary
        let summary = db.get_productivity_summary()?;
        assert_eq!(summary.len(), 2);

        // Claude Code should have 3 tasks
        let claude_summary = summary
            .iter()
            .find(|(system, _, _, _, _)| system == "claude-code");
        assert!(claude_summary.is_some(), "Claude Code summary not found");
        if let Some(summary) = claude_summary {
            assert_eq!(summary.1, 3); // tasks_completed
            assert!((summary.3 - 1500.0).abs() < 0.1); // avg_tokens
        }

        // Codex should have 2 tasks
        let codex_summary = summary
            .iter()
            .find(|(system, _, _, _, _)| system == "codex");
        assert!(codex_summary.is_some(), "Codex summary not found");
        if let Some(summary) = codex_summary {
            assert_eq!(summary.1, 2); // tasks_completed
            assert!((summary.3 - 1200.0).abs() < 0.1); // avg_tokens
        }

        Ok(())
    }

    #[test]
    fn test_record_and_retrieve_prompt_changes() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // First record an input to link to
        let input = AgentInput {
            id: None,
            terminal_id: "terminal-1".to_string(),
            timestamp: Utc::now(),
            input_type: InputType::Manual,
            content: "cargo build".to_string(),
            agent_role: Some("builder".to_string()),
            context: InputContext::default(),
        };
        let input_id = db.record_input(&input)?;

        // Record prompt changes
        let changes = PromptChanges {
            id: None,
            input_id,
            before_commit: Some("abc123".to_string()),
            after_commit: Some("def456".to_string()),
            files_changed: 5,
            lines_added: 150,
            lines_removed: 30,
            tests_added: 2,
            tests_modified: 1,
        };

        let changes_id = db.record_prompt_changes(&changes)?;
        assert!(changes_id > 0);

        // Retrieve and verify
        let retrieved = db.get_prompt_changes(input_id)?;
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.input_id, input_id);
        assert_eq!(retrieved.before_commit, Some("abc123".to_string()));
        assert_eq!(retrieved.after_commit, Some("def456".to_string()));
        assert_eq!(retrieved.files_changed, 5);
        assert_eq!(retrieved.lines_added, 150);
        assert_eq!(retrieved.lines_removed, 30);
        assert_eq!(retrieved.tests_added, 2);
        assert_eq!(retrieved.tests_modified, 1);

        Ok(())
    }

    #[test]
    fn test_prompt_changes_not_found() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // Query for non-existent input_id
        let result = db.get_prompt_changes(999)?;
        assert!(result.is_none());

        Ok(())
    }

    #[test]
    fn test_terminal_changes_summary() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // Record multiple inputs and changes for the same terminal
        for i in 0..3 {
            let input = AgentInput {
                id: None,
                terminal_id: "terminal-1".to_string(),
                timestamp: Utc::now(),
                input_type: InputType::Manual,
                content: format!("command {i}"),
                agent_role: Some("builder".to_string()),
                context: InputContext::default(),
            };
            let input_id = db.record_input(&input)?;

            let changes = PromptChanges {
                id: None,
                input_id,
                before_commit: Some(format!("commit{i}")),
                after_commit: Some(format!("commit{}", i + 1)),
                files_changed: 2,
                lines_added: 50,
                lines_removed: 10,
                tests_added: 1,
                tests_modified: 0,
            };
            db.record_prompt_changes(&changes)?;
        }

        // Get summary
        let (files, added, removed) = db.get_terminal_changes_summary("terminal-1")?;
        assert_eq!(files, 6);   // 3 * 2
        assert_eq!(added, 150); // 3 * 50
        assert_eq!(removed, 30); // 3 * 10

        // Different terminal should have no changes
        let (files, added, removed) = db.get_terminal_changes_summary("terminal-2")?;
        assert_eq!(files, 0);
        assert_eq!(added, 0);
        assert_eq!(removed, 0);

        Ok(())
    }
}
