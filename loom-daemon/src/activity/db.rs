//! Database operations for activity tracking.
//!
//! This module contains the `ActivityDb` struct and all methods for
//! recording and querying agent activity data.

use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use std::path::PathBuf;

use super::models::{
    ActivityEntry, AgentInput, AgentMetric, AgentOutput, BudgetConfig, BudgetPeriod,
    BudgetStatus, CostByIssue, CostByPr, CostByRole, CostSummary, InputContext, InputType,
    PrReworkStats, ProductivitySummary, PromptChanges, PromptGitHubEvent, PromptSuccessStats,
    QualityMetrics, RunwayProjection, TokenUsage,
};
use super::schema::init_schema;
use super::test_parser;

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
    /// duration, and cost. If a `metric_id` is provided, also updates the aggregate
    /// token counts in `agent_metrics`.
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

    /// Record a prompt-GitHub correlation event
    ///
    /// Links a prompt (agent input) with a GitHub action it triggered,
    /// such as creating an issue, opening a PR, or changing labels.
    pub fn record_prompt_github_event(&self, event: &PromptGitHubEvent) -> Result<i64> {
        let label_before_json = event
            .label_before
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;

        let label_after_json = event
            .label_after
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;

        self.conn.execute(
            r"
            INSERT INTO prompt_github (input_id, issue_number, pr_number, label_before, label_after, event_type)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ",
            params![
                event.input_id,
                event.issue_number,
                event.pr_number,
                label_before_json,
                label_after_json,
                event.event_type.as_str(),
            ],
        )?;

        Ok(self.conn.last_insert_rowid())
    }

    /// Get prompt-GitHub events for a specific input
    #[allow(dead_code)]
    pub fn get_prompt_github_events(&self, input_id: i64) -> Result<Vec<PromptGitHubEvent>> {
        use super::models::PromptGitHubEventType;

        let mut stmt = self.conn.prepare(
            r"
            SELECT id, input_id, issue_number, pr_number, label_before, label_after, event_type
            FROM prompt_github
            WHERE input_id = ?1
            ORDER BY id
            ",
        )?;

        let events = stmt.query_map(params![input_id], |row| {
            let id: i64 = row.get(0)?;
            let input_id: Option<i64> = row.get(1)?;
            let issue_number: Option<i32> = row.get(2)?;
            let pr_number: Option<i32> = row.get(3)?;
            let label_before_json: Option<String> = row.get(4)?;
            let label_after_json: Option<String> = row.get(5)?;
            let event_type_str: String = row.get(6)?;

            let label_before = label_before_json
                .map(|json| serde_json::from_str(&json))
                .transpose()
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;

            let label_after = label_after_json
                .map(|json| serde_json::from_str(&json))
                .transpose()
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;

            let event_type = PromptGitHubEventType::from_str(&event_type_str).ok_or_else(|| {
                rusqlite::Error::ToSqlConversionFailure(
                    format!("Invalid event_type: {event_type_str}").into(),
                )
            })?;

            Ok(PromptGitHubEvent {
                id: Some(id),
                input_id,
                issue_number,
                pr_number,
                label_before,
                label_after,
                event_type,
            })
        })?;

        events.collect::<Result<Vec<_>, _>>().map_err(Into::into)
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
    pub fn get_terminal_changes_summary(&self, terminal_id: &str) -> Result<(i64, i64, i64)> {
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

    /// Record quality metrics for an input/output
    #[allow(dead_code)]
    pub fn record_quality_metrics(&self, metrics: &QualityMetrics) -> Result<i64> {
        self.conn.execute(
            r"
            INSERT INTO quality_metrics (
                input_id, timestamp, tests_passed, tests_failed, tests_skipped,
                test_runner, lint_errors, format_errors, build_success,
                pr_approved, pr_changes_requested, rework_count, human_rating
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
            ",
            params![
                metrics.input_id,
                metrics.timestamp.to_rfc3339(),
                metrics.tests_passed,
                metrics.tests_failed,
                metrics.tests_skipped,
                &metrics.test_runner,
                metrics.lint_errors,
                metrics.format_errors,
                metrics.build_success,
                metrics.pr_approved,
                metrics.pr_changes_requested,
                metrics.rework_count,
                metrics.human_rating,
            ],
        )?;

        Ok(self.conn.last_insert_rowid())
    }

    /// Parse and record quality metrics from terminal output
    ///
    /// Automatically parses test results, lint errors, and build status from
    /// the output content and stores them in the database.
    #[allow(dead_code)]
    pub fn record_quality_from_output(&self, input_id: i64, output: &str) -> Result<Option<i64>> {
        let test_results = test_parser::parse_test_results(output);
        let lint_results = test_parser::parse_lint_results(output);
        let build_status = test_parser::parse_build_status(output);

        // Only record if we found at least some quality metrics
        if test_results.is_none() && lint_results.is_none() && build_status.is_none() {
            return Ok(None);
        }

        let metrics = QualityMetrics {
            id: None,
            input_id: Some(input_id),
            timestamp: Utc::now(),
            tests_passed: test_results.as_ref().map(|t| t.passed),
            tests_failed: test_results.as_ref().map(|t| t.failed),
            tests_skipped: test_results.as_ref().map(|t| t.skipped),
            test_runner: test_results.and_then(|t| t.runner),
            lint_errors: lint_results.as_ref().map(|l| l.lint_errors),
            format_errors: lint_results.map(|l| l.format_errors),
            build_success: build_status,
            pr_approved: None,
            pr_changes_requested: None,
            rework_count: None, // Rework count is updated separately via increment_rework_count
            human_rating: None,
        };

        Ok(Some(self.record_quality_metrics(&metrics)?))
    }

    /// Get quality metrics for a specific input
    #[allow(dead_code)]
    pub fn get_quality_metrics(&self, input_id: i64) -> Result<Option<QualityMetrics>> {
        let result = self.conn.query_row(
            r"
            SELECT id, input_id, timestamp, tests_passed, tests_failed, tests_skipped,
                   test_runner, lint_errors, format_errors, build_success,
                   pr_approved, pr_changes_requested, rework_count, human_rating
            FROM quality_metrics
            WHERE input_id = ?1
            ",
            params![input_id],
            |row| {
                let timestamp_str: String = row.get(2)?;
                let timestamp = DateTime::parse_from_rfc3339(&timestamp_str)
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?
                    .with_timezone(&Utc);

                Ok(QualityMetrics {
                    id: Some(row.get(0)?),
                    input_id: row.get(1)?,
                    timestamp,
                    tests_passed: row.get(3)?,
                    tests_failed: row.get(4)?,
                    tests_skipped: row.get(5)?,
                    test_runner: row.get(6)?,
                    lint_errors: row.get(7)?,
                    format_errors: row.get(8)?,
                    build_success: row.get(9)?,
                    pr_approved: row.get(10)?,
                    pr_changes_requested: row.get(11)?,
                    rework_count: row.get(12)?,
                    human_rating: row.get(13)?,
                })
            },
        );

        match result {
            Ok(metrics) => Ok(Some(metrics)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get aggregate test outcomes for a terminal
    ///
    /// Returns a tuple of (passed, failed, skipped) counts across all quality metrics
    /// for the given terminal.
    #[allow(dead_code)]
    pub fn get_terminal_test_summary(&self, terminal_id: &str) -> Result<(i64, i64, i64)> {
        let result = self.conn.query_row(
            r"
            SELECT
                COALESCE(SUM(qm.tests_passed), 0) as total_passed,
                COALESCE(SUM(qm.tests_failed), 0) as total_failed,
                COALESCE(SUM(qm.tests_skipped), 0) as total_skipped
            FROM quality_metrics qm
            JOIN agent_inputs ai ON qm.input_id = ai.id
            WHERE ai.terminal_id = ?1
            ",
            params![terminal_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;

        Ok(result)
    }

    /// Update PR review status for a quality metrics record
    #[allow(dead_code)]
    pub fn update_pr_review_status(
        &self,
        input_id: i64,
        approved: Option<bool>,
        changes_requested: Option<bool>,
    ) -> Result<()> {
        self.conn.execute(
            r"
            UPDATE quality_metrics
            SET pr_approved = COALESCE(?1, pr_approved),
                pr_changes_requested = COALESCE(?2, pr_changes_requested)
            WHERE input_id = ?3
            ",
            params![approved, changes_requested, input_id],
        )?;

        Ok(())
    }

    /// Increment the rework count for a quality metrics record
    ///
    /// Called when a PR receives `changes_requested` feedback, indicating
    /// another review cycle is needed.
    #[allow(dead_code)]
    pub fn increment_rework_count(&self, input_id: i64) -> Result<i32> {
        self.conn.execute(
            r"
            UPDATE quality_metrics
            SET rework_count = COALESCE(rework_count, 0) + 1
            WHERE input_id = ?1
            ",
            params![input_id],
        )?;

        // Return the new rework count
        let count: i32 = self.conn.query_row(
            "SELECT COALESCE(rework_count, 0) FROM quality_metrics WHERE input_id = ?1",
            params![input_id],
            |row| row.get(0),
        )?;

        Ok(count)
    }

    /// Get PR rework statistics
    ///
    /// Returns summary statistics for PR review cycles:
    /// - Average rework count per PR
    /// - Max rework count
    /// - Total PRs with rework (count > 0)
    /// - Total PRs tracked
    #[allow(dead_code)]
    pub fn get_pr_rework_stats(&self) -> Result<PrReworkStats> {
        let result = self.conn.query_row(
            r"
            SELECT
                COALESCE(AVG(CASE WHEN rework_count > 0 THEN rework_count END), 0) as avg_rework,
                COALESCE(MAX(rework_count), 0) as max_rework,
                SUM(CASE WHEN rework_count > 0 THEN 1 ELSE 0 END) as prs_with_rework,
                COUNT(*) as total_prs
            FROM quality_metrics
            WHERE pr_approved IS NOT NULL OR pr_changes_requested IS NOT NULL
            ",
            [],
            |row| {
                Ok(PrReworkStats {
                    avg_rework_count: row.get(0)?,
                    max_rework_count: row.get(1)?,
                    prs_with_rework: row.get(2)?,
                    total_prs_tracked: row.get(3)?,
                })
            },
        )?;

        Ok(result)
    }

    /// Get quality metrics correlation: prompts that led to passing tests
    ///
    /// Returns the count and average test pass rate for prompts with quality metrics.
    #[allow(dead_code)]
    pub fn get_prompt_success_correlation(&self) -> Result<PromptSuccessStats> {
        let result = self.conn.query_row(
            r"
            SELECT
                COUNT(*) as total_prompts,
                SUM(CASE WHEN tests_failed = 0 AND tests_passed > 0 THEN 1 ELSE 0 END) as passing_prompts,
                AVG(CASE WHEN tests_passed + tests_failed > 0
                    THEN CAST(tests_passed AS REAL) / (tests_passed + tests_failed)
                    ELSE NULL END) as avg_pass_rate
            FROM quality_metrics
            WHERE tests_passed IS NOT NULL OR tests_failed IS NOT NULL
            ",
            [],
            |row| {
                Ok(PromptSuccessStats {
                    total_prompts_with_tests: row.get(0)?,
                    prompts_with_all_passing: row.get(1)?,
                    avg_test_pass_rate: row.get::<_, Option<f64>>(2)?.unwrap_or(0.0),
                })
            },
        )?;

        Ok(result)
    }

    /// Record resource usage parsed from terminal output
    ///
    /// Stores LLM resource consumption metrics including token counts, model,
    /// cost, duration, and provider information.
    pub fn record_resource_usage(
        &self,
        usage: &super::resource_usage::ResourceUsage,
    ) -> Result<i64> {
        self.conn.execute(
            r"
            INSERT INTO resource_usage (
                input_id, timestamp, model, tokens_input, tokens_output,
                tokens_cache_read, tokens_cache_write, cost_usd, duration_ms, provider
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            ",
            params![
                usage.input_id,
                usage.timestamp.to_rfc3339(),
                &usage.model,
                usage.tokens_input,
                usage.tokens_output,
                usage.tokens_cache_read,
                usage.tokens_cache_write,
                usage.cost_usd,
                usage.duration_ms,
                &usage.provider,
            ],
        )?;

        Ok(self.conn.last_insert_rowid())
    }

    /// Parse and record resource usage from terminal output
    ///
    /// Automatically parses token usage, model, and cost information from
    /// Claude Code terminal output and stores it in the database.
    pub fn record_resource_usage_from_output(
        &self,
        input_id: Option<i64>,
        output: &str,
        duration_ms: Option<i64>,
    ) -> Result<Option<i64>> {
        use super::resource_usage::parse_resource_usage;

        if let Some(mut usage) = parse_resource_usage(output, duration_ms) {
            usage.input_id = input_id;
            Ok(Some(self.record_resource_usage(&usage)?))
        } else {
            Ok(None)
        }
    }

    /// Get resource usage records for a specific input
    #[allow(dead_code)]
    pub fn get_resource_usage(
        &self,
        input_id: i64,
    ) -> Result<Vec<super::resource_usage::ResourceUsage>> {
        let mut stmt = self.conn.prepare(
            r"
            SELECT id, input_id, timestamp, model, tokens_input, tokens_output,
                   tokens_cache_read, tokens_cache_write, cost_usd, duration_ms, provider
            FROM resource_usage
            WHERE input_id = ?1
            ORDER BY timestamp
            ",
        )?;

        let usages = stmt.query_map(params![input_id], |row| {
            let timestamp_str: String = row.get(2)?;
            let timestamp = DateTime::parse_from_rfc3339(&timestamp_str)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?
                .with_timezone(&Utc);

            Ok(super::resource_usage::ResourceUsage {
                input_id: row.get(1)?,
                model: row.get(3)?,
                tokens_input: row.get(4)?,
                tokens_output: row.get(5)?,
                tokens_cache_read: row.get(6)?,
                tokens_cache_write: row.get(7)?,
                cost_usd: row.get(8)?,
                duration_ms: row.get(9)?,
                provider: row.get(10)?,
                timestamp,
            })
        })?;

        usages.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Get total resource usage summary for a terminal
    ///
    /// Returns a tuple of (input tokens, output tokens, total cost in USD)
    #[allow(dead_code)]
    pub fn get_terminal_resource_summary(&self, terminal_id: &str) -> Result<(i64, i64, f64)> {
        let result = self.conn.query_row(
            r"
            SELECT
                COALESCE(SUM(ru.tokens_input), 0) as total_input,
                COALESCE(SUM(ru.tokens_output), 0) as total_output,
                COALESCE(SUM(ru.cost_usd), 0.0) as total_cost
            FROM resource_usage ru
            JOIN agent_inputs ai ON ru.input_id = ai.id
            WHERE ai.terminal_id = ?1
            ",
            params![terminal_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;

        Ok(result)
    }

    // ========================================================================
    // Cost Analytics Methods (Issue #1064)
    // ========================================================================

    /// Save or update a budget configuration.
    ///
    /// If the budget for the given period already exists, it will be updated.
    /// Otherwise, a new record will be created.
    pub fn save_budget_config(&self, config: &BudgetConfig) -> Result<i64> {
        // Deactivate any existing budget for this period
        self.conn.execute(
            r"UPDATE budget_config SET is_active = 0 WHERE period = ?1 AND is_active = 1",
            params![config.period.as_str()],
        )?;

        // Insert new budget
        self.conn.execute(
            r"
            INSERT INTO budget_config (period, limit_usd, alert_threshold, is_active, created_at, updated_at)
            VALUES (?1, ?2, ?3, 1, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
            ",
            params![
                config.period.as_str(),
                config.limit_usd,
                config.alert_threshold,
            ],
        )?;

        Ok(self.conn.last_insert_rowid())
    }

    /// Get the active budget configuration for a period.
    #[allow(dead_code)]
    pub fn get_budget_config(&self, period: BudgetPeriod) -> Result<Option<BudgetConfig>> {
        let result = self.conn.query_row(
            r"
            SELECT id, period, limit_usd, alert_threshold, created_at, updated_at, is_active
            FROM budget_config
            WHERE period = ?1 AND is_active = 1
            ORDER BY created_at DESC
            LIMIT 1
            ",
            params![period.as_str()],
            |row| {
                let period_str: String = row.get(1)?;
                let created_str: String = row.get(4)?;
                let updated_str: String = row.get(5)?;

                let period = BudgetPeriod::from_str(&period_str).ok_or_else(|| {
                    rusqlite::Error::ToSqlConversionFailure(
                        format!("Invalid period: {period_str}").into(),
                    )
                })?;

                let created_at = DateTime::parse_from_rfc3339(&created_str)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now());

                let updated_at = DateTime::parse_from_rfc3339(&updated_str)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now());

                Ok(BudgetConfig {
                    id: row.get(0)?,
                    period,
                    limit_usd: row.get(2)?,
                    alert_threshold: row.get(3)?,
                    created_at,
                    updated_at,
                    is_active: row.get(6)?,
                })
            },
        );

        match result {
            Ok(config) => Ok(Some(config)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get all active budget configurations.
    #[allow(dead_code)]
    pub fn get_all_budget_configs(&self) -> Result<Vec<BudgetConfig>> {
        let mut stmt = self.conn.prepare(
            r"
            SELECT id, period, limit_usd, alert_threshold, created_at, updated_at, is_active
            FROM budget_config
            WHERE is_active = 1
            ORDER BY period
            ",
        )?;

        let configs = stmt.query_map([], |row| {
            let period_str: String = row.get(1)?;
            let created_str: String = row.get(4)?;
            let updated_str: String = row.get(5)?;

            let period = BudgetPeriod::from_str(&period_str).ok_or_else(|| {
                rusqlite::Error::ToSqlConversionFailure(
                    format!("Invalid period: {period_str}").into(),
                )
            })?;

            let created_at = DateTime::parse_from_rfc3339(&created_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());

            let updated_at = DateTime::parse_from_rfc3339(&updated_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());

            Ok(BudgetConfig {
                id: row.get(0)?,
                period,
                limit_usd: row.get(2)?,
                alert_threshold: row.get(3)?,
                created_at,
                updated_at,
                is_active: row.get(6)?,
            })
        })?;

        configs.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Get cost summary for a date range.
    ///
    /// Returns aggregated cost metrics including total cost, request count,
    /// token usage, and average cost per request.
    #[allow(dead_code)]
    pub fn get_cost_summary(
        &self,
        start_date: DateTime<Utc>,
        end_date: DateTime<Utc>,
    ) -> Result<CostSummary> {
        let result = self.conn.query_row(
            r"
            SELECT
                COALESCE(SUM(cost_usd), 0.0) as total_cost,
                COALESCE(COUNT(*), 0) as request_count,
                COALESCE(SUM(tokens_input), 0) as total_input_tokens,
                COALESCE(SUM(tokens_output), 0) as total_output_tokens
            FROM resource_usage
            WHERE timestamp >= ?1 AND timestamp < ?2
            ",
            params![start_date.to_rfc3339(), end_date.to_rfc3339()],
            |row| {
                let total_cost: f64 = row.get(0)?;
                let request_count: i64 = row.get(1)?;
                Ok((
                    total_cost,
                    request_count,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )?;

        let (total_cost, request_count, total_input_tokens, total_output_tokens) = result;
        let avg_cost_per_request = if request_count > 0 {
            total_cost / request_count as f64
        } else {
            0.0
        };

        Ok(CostSummary {
            start_date,
            end_date,
            total_cost,
            request_count,
            total_input_tokens,
            total_output_tokens,
            avg_cost_per_request,
        })
    }

    /// Get cost breakdown by agent role.
    #[allow(dead_code)]
    pub fn get_cost_by_role(&self) -> Result<Vec<CostByRole>> {
        let mut stmt = self.conn.prepare(
            r"
            SELECT
                COALESCE(ai.agent_role, 'unknown') as agent_role,
                COALESCE(SUM(ru.cost_usd), 0.0) as total_cost,
                COUNT(*) as request_count,
                COALESCE(AVG(ru.cost_usd), 0.0) as avg_cost,
                COALESCE(SUM(ru.tokens_input), 0) as total_input_tokens,
                COALESCE(SUM(ru.tokens_output), 0) as total_output_tokens,
                MIN(ru.timestamp) as first_usage,
                MAX(ru.timestamp) as last_usage
            FROM resource_usage ru
            LEFT JOIN agent_inputs ai ON ru.input_id = ai.id
            GROUP BY COALESCE(ai.agent_role, 'unknown')
            ORDER BY total_cost DESC
            ",
        )?;

        let results = stmt.query_map([], |row| {
            let first_usage_str: Option<String> = row.get(6)?;
            let last_usage_str: Option<String> = row.get(7)?;

            let first_usage = first_usage_str
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|dt| dt.with_timezone(&Utc));

            let last_usage = last_usage_str
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|dt| dt.with_timezone(&Utc));

            Ok(CostByRole {
                agent_role: row.get(0)?,
                total_cost: row.get(1)?,
                request_count: row.get(2)?,
                avg_cost: row.get(3)?,
                total_input_tokens: row.get(4)?,
                total_output_tokens: row.get(5)?,
                first_usage,
                last_usage,
            })
        })?;

        results.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Get cost breakdown by GitHub issue.
    #[allow(dead_code)]
    pub fn get_cost_by_issue(&self, issue_number: Option<i32>) -> Result<Vec<CostByIssue>> {
        if let Some(issue_num) = issue_number {
            let mut stmt = self.conn.prepare(
                r"
                SELECT
                    pg.issue_number,
                    COALESCE(SUM(ru.cost_usd), 0.0) as total_cost,
                    COUNT(DISTINCT ru.input_id) as prompt_count,
                    COALESCE(SUM(ru.tokens_input), 0) as total_input_tokens,
                    COALESCE(SUM(ru.tokens_output), 0) as total_output_tokens,
                    MIN(ru.timestamp) as first_usage,
                    MAX(ru.timestamp) as last_usage
                FROM resource_usage ru
                JOIN agent_inputs ai ON ru.input_id = ai.id
                JOIN prompt_github pg ON ai.id = pg.input_id
                WHERE pg.issue_number = ?1
                GROUP BY pg.issue_number
                ",
            )?;
            let results = stmt.query_map(params![issue_num], |row| parse_cost_by_issue(row))?;
            results.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        } else {
            let mut stmt = self.conn.prepare(
                r"
                SELECT
                    pg.issue_number,
                    COALESCE(SUM(ru.cost_usd), 0.0) as total_cost,
                    COUNT(DISTINCT ru.input_id) as prompt_count,
                    COALESCE(SUM(ru.tokens_input), 0) as total_input_tokens,
                    COALESCE(SUM(ru.tokens_output), 0) as total_output_tokens,
                    MIN(ru.timestamp) as first_usage,
                    MAX(ru.timestamp) as last_usage
                FROM resource_usage ru
                JOIN agent_inputs ai ON ru.input_id = ai.id
                JOIN prompt_github pg ON ai.id = pg.input_id
                WHERE pg.issue_number IS NOT NULL
                GROUP BY pg.issue_number
                ORDER BY total_cost DESC
                ",
            )?;
            let results = stmt.query_map([], |row| parse_cost_by_issue(row))?;
            results.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        }
    }

    /// Get cost breakdown by pull request.
    #[allow(dead_code)]
    pub fn get_cost_by_pr(&self, pr_number: Option<i32>) -> Result<Vec<CostByPr>> {
        if let Some(pr_num) = pr_number {
            let mut stmt = self.conn.prepare(
                r"
                SELECT
                    pg.pr_number,
                    COALESCE(SUM(ru.cost_usd), 0.0) as total_cost,
                    COUNT(DISTINCT ru.input_id) as prompt_count,
                    COALESCE(SUM(ru.tokens_input), 0) as total_input_tokens,
                    COALESCE(SUM(ru.tokens_output), 0) as total_output_tokens,
                    MIN(ru.timestamp) as first_usage,
                    MAX(ru.timestamp) as last_usage
                FROM resource_usage ru
                JOIN agent_inputs ai ON ru.input_id = ai.id
                JOIN prompt_github pg ON ai.id = pg.input_id
                WHERE pg.pr_number = ?1
                GROUP BY pg.pr_number
                ",
            )?;
            let results = stmt.query_map(params![pr_num], |row| parse_cost_by_pr(row))?;
            results.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        } else {
            let mut stmt = self.conn.prepare(
                r"
                SELECT
                    pg.pr_number,
                    COALESCE(SUM(ru.cost_usd), 0.0) as total_cost,
                    COUNT(DISTINCT ru.input_id) as prompt_count,
                    COALESCE(SUM(ru.tokens_input), 0) as total_input_tokens,
                    COALESCE(SUM(ru.tokens_output), 0) as total_output_tokens,
                    MIN(ru.timestamp) as first_usage,
                    MAX(ru.timestamp) as last_usage
                FROM resource_usage ru
                JOIN agent_inputs ai ON ru.input_id = ai.id
                JOIN prompt_github pg ON ai.id = pg.input_id
                WHERE pg.pr_number IS NOT NULL
                GROUP BY pg.pr_number
                ORDER BY total_cost DESC
                ",
            )?;
            let results = stmt.query_map([], |row| parse_cost_by_pr(row))?;
            results.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        }
    }

    /// Get budget status for a specific period.
    ///
    /// Calculates current spend within the period and compares against the budget limit.
    #[allow(dead_code)]
    pub fn get_budget_status(&self, period: BudgetPeriod) -> Result<Option<BudgetStatus>> {
        let config = match self.get_budget_config(period)? {
            Some(c) => c,
            None => return Ok(None),
        };

        let (period_start, period_end) = get_period_bounds(period);
        let summary = self.get_cost_summary(period_start, period_end)?;

        let spent = summary.total_cost;
        let remaining = (config.limit_usd - spent).max(0.0);
        let usage_percent = if config.limit_usd > 0.0 {
            (spent / config.limit_usd) * 100.0
        } else {
            0.0
        };
        let alert_triggered = (spent / config.limit_usd) >= config.alert_threshold;
        let budget_exceeded = spent >= config.limit_usd;

        Ok(Some(BudgetStatus {
            config,
            spent,
            remaining,
            usage_percent,
            alert_triggered,
            budget_exceeded,
            period_start,
            period_end,
        }))
    }

    /// Project runway based on recent burn rate.
    ///
    /// Uses the last N days of spending to calculate average daily cost and
    /// estimate when the budget will be exhausted.
    #[allow(dead_code)]
    pub fn project_runway(&self, period: BudgetPeriod, lookback_days: i32) -> Result<Option<RunwayProjection>> {
        let config = match self.get_budget_config(period)? {
            Some(c) => c,
            None => return Ok(None),
        };

        // Get current period status
        let (period_start, _) = get_period_bounds(period);
        let status = self.get_budget_status(period)?;
        let remaining_budget = status.map(|s| s.remaining).unwrap_or(config.limit_usd);

        // Calculate average daily cost over lookback period
        let lookback_start = Utc::now() - chrono::Duration::days(i64::from(lookback_days));
        let lookback_end = Utc::now();
        let lookback_summary = self.get_cost_summary(lookback_start, lookback_end)?;

        let avg_daily_cost = if lookback_days > 0 {
            lookback_summary.total_cost / f64::from(lookback_days)
        } else {
            0.0
        };

        let days_remaining = if avg_daily_cost > 0.0 {
            remaining_budget / avg_daily_cost
        } else {
            f64::INFINITY
        };

        let exhaustion_date = if avg_daily_cost > 0.0 && days_remaining.is_finite() {
            Some(period_start + chrono::Duration::days(days_remaining as i64))
        } else {
            None
        };

        Ok(Some(RunwayProjection {
            remaining_budget,
            avg_daily_cost,
            days_remaining,
            exhaustion_date,
            lookback_days,
        }))
    }
}

/// Helper function to parse `CostByIssue` from a row
fn parse_cost_by_issue(row: &rusqlite::Row) -> rusqlite::Result<CostByIssue> {
    let first_usage_str: Option<String> = row.get(5)?;
    let last_usage_str: Option<String> = row.get(6)?;

    let first_usage = first_usage_str
        .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
        .map(|dt| dt.with_timezone(&Utc));

    let last_usage = last_usage_str
        .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
        .map(|dt| dt.with_timezone(&Utc));

    Ok(CostByIssue {
        issue_number: row.get(0)?,
        total_cost: row.get(1)?,
        prompt_count: row.get(2)?,
        total_input_tokens: row.get(3)?,
        total_output_tokens: row.get(4)?,
        first_usage,
        last_usage,
    })
}

/// Helper function to parse `CostByPr` from a row
fn parse_cost_by_pr(row: &rusqlite::Row) -> rusqlite::Result<CostByPr> {
    let first_usage_str: Option<String> = row.get(5)?;
    let last_usage_str: Option<String> = row.get(6)?;

    let first_usage = first_usage_str
        .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
        .map(|dt| dt.with_timezone(&Utc));

    let last_usage = last_usage_str
        .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
        .map(|dt| dt.with_timezone(&Utc));

    Ok(CostByPr {
        pr_number: row.get(0)?,
        total_cost: row.get(1)?,
        prompt_count: row.get(2)?,
        total_input_tokens: row.get(3)?,
        total_output_tokens: row.get(4)?,
        first_usage,
        last_usage,
    })
}

/// Calculate the start and end timestamps for a budget period
fn get_period_bounds(period: BudgetPeriod) -> (DateTime<Utc>, DateTime<Utc>) {
    use chrono::{Datelike, Duration, TimeZone};

    let now = Utc::now();
    let today = Utc
        .with_ymd_and_hms(now.year(), now.month(), now.day(), 0, 0, 0)
        .unwrap();

    match period {
        BudgetPeriod::Daily => (today, today + Duration::days(1)),
        BudgetPeriod::Weekly => {
            // Start from Monday of current week
            let days_since_monday = now.weekday().num_days_from_monday();
            let week_start = today - Duration::days(i64::from(days_since_monday));
            (week_start, week_start + Duration::weeks(1))
        }
        BudgetPeriod::Monthly => {
            // Start from first day of current month
            let month_start = Utc
                .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
                .unwrap();
            // End is first day of next month
            let next_month = if now.month() == 12 {
                Utc.with_ymd_and_hms(now.year() + 1, 1, 1, 0, 0, 0).unwrap()
            } else {
                Utc.with_ymd_and_hms(now.year(), now.month() + 1, 1, 0, 0, 0)
                    .unwrap()
            };
            (month_start, next_month)
        }
    }
}

// Implement StatsQueries trait for ActivityDb
use super::stats::{
    self, AgentEffectiveness, CostPerIssue, DailyVelocity, StatsQueries, StatsSummary,
    WeeklyVelocity,
};
use chrono::NaiveDate;

impl StatsQueries for ActivityDb {
    fn get_agent_effectiveness(
        &self,
        role: Option<&str>,
    ) -> rusqlite::Result<Vec<AgentEffectiveness>> {
        stats::query_agent_effectiveness(&self.conn, role)
    }

    fn get_cost_per_issue(&self, issue_number: Option<i32>) -> rusqlite::Result<Vec<CostPerIssue>> {
        stats::query_cost_per_issue(&self.conn, issue_number)
    }

    fn get_daily_velocity(
        &self,
        start_date: Option<NaiveDate>,
        end_date: Option<NaiveDate>,
    ) -> rusqlite::Result<Vec<DailyVelocity>> {
        stats::query_daily_velocity(&self.conn, start_date, end_date)
    }

    fn get_weekly_velocity(&self) -> rusqlite::Result<Vec<WeeklyVelocity>> {
        stats::query_weekly_velocity(&self.conn)
    }

    fn get_stats_summary(&self) -> rusqlite::Result<StatsSummary> {
        stats::query_stats_summary(&self.conn)
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
        if let Some(retrieved) = retrieved {
            assert_eq!(retrieved.input_id, input_id);
            assert_eq!(retrieved.before_commit, Some("abc123".to_string()));
            assert_eq!(retrieved.after_commit, Some("def456".to_string()));
            assert_eq!(retrieved.files_changed, 5);
            assert_eq!(retrieved.lines_added, 150);
            assert_eq!(retrieved.lines_removed, 30);
            assert_eq!(retrieved.tests_added, 2);
            assert_eq!(retrieved.tests_modified, 1);
        }

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
        assert_eq!(files, 6); // 3 * 2
        assert_eq!(added, 150); // 3 * 50
        assert_eq!(removed, 30); // 3 * 10

        // Different terminal should have no changes
        let (files, added, removed) = db.get_terminal_changes_summary("terminal-2")?;
        assert_eq!(files, 0);
        assert_eq!(added, 0);
        assert_eq!(removed, 0);

        Ok(())
    }

    #[test]
    fn test_record_prompt_github_event() -> Result<()> {
        use super::super::models::PromptGitHubEventType;

        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // First record an input to link to
        let input = AgentInput {
            id: None,
            terminal_id: "terminal-1".to_string(),
            timestamp: Utc::now(),
            input_type: InputType::Manual,
            content: "gh issue edit 42 --add-label loom:building".to_string(),
            agent_role: Some("builder".to_string()),
            context: InputContext::default(),
        };
        let input_id = db.record_input(&input)?;

        // Record a GitHub event linked to this input
        let event = PromptGitHubEvent {
            id: None,
            input_id: Some(input_id),
            issue_number: Some(42),
            pr_number: None,
            label_before: None,
            label_after: Some(vec!["loom:building".to_string()]),
            event_type: PromptGitHubEventType::LabelAdded,
        };

        let event_id = db.record_prompt_github_event(&event)?;
        assert!(event_id > 0);

        // Retrieve the events
        let events = db.get_prompt_github_events(input_id)?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].issue_number, Some(42));
        assert_eq!(events[0].event_type, PromptGitHubEventType::LabelAdded);
        assert_eq!(events[0].label_after, Some(vec!["loom:building".to_string()]));

        Ok(())
    }

    #[test]
    fn test_record_multiple_github_events() -> Result<()> {
        use super::super::models::PromptGitHubEventType;

        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // Record an input
        let input = AgentInput {
            id: None,
            terminal_id: "terminal-1".to_string(),
            timestamp: Utc::now(),
            input_type: InputType::Manual,
            content: "gh pr create".to_string(),
            agent_role: Some("builder".to_string()),
            context: InputContext::default(),
        };
        let input_id = db.record_input(&input)?;

        // Record multiple events from the same input
        let event1 = PromptGitHubEvent {
            id: None,
            input_id: Some(input_id),
            issue_number: None,
            pr_number: Some(123),
            label_before: None,
            label_after: None,
            event_type: PromptGitHubEventType::PrCreated,
        };
        db.record_prompt_github_event(&event1)?;

        let event2 = PromptGitHubEvent {
            id: None,
            input_id: Some(input_id),
            issue_number: None,
            pr_number: Some(123),
            label_before: None,
            label_after: Some(vec!["loom:review-requested".to_string()]),
            event_type: PromptGitHubEventType::LabelAdded,
        };
        db.record_prompt_github_event(&event2)?;

        // Retrieve all events for this input
        let retrieved_events = db.get_prompt_github_events(input_id)?;
        assert_eq!(retrieved_events.len(), 2);

        let pr_created = retrieved_events
            .iter()
            .find(|e| e.event_type == PromptGitHubEventType::PrCreated);
        assert!(pr_created.is_some());
        if let Some(evt) = pr_created {
            assert_eq!(evt.pr_number, Some(123));
        }

        let label_added = retrieved_events
            .iter()
            .find(|e| e.event_type == PromptGitHubEventType::LabelAdded);
        assert!(label_added.is_some());

        Ok(())
    }

    #[test]
    fn test_github_event_without_input_link() -> Result<()> {
        use super::super::models::PromptGitHubEventType;

        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // Record a GitHub event without linking to a specific input
        let event = PromptGitHubEvent {
            id: None,
            input_id: None,
            issue_number: Some(100),
            pr_number: None,
            label_before: Some(vec!["loom:issue".to_string()]),
            label_after: Some(vec!["loom:building".to_string()]),
            event_type: PromptGitHubEventType::LabelAdded,
        };

        let event_id = db.record_prompt_github_event(&event)?;
        assert!(event_id > 0);

        Ok(())
    }

    #[test]
    fn test_record_and_get_quality_metrics() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // First record an input to link to
        let input = AgentInput {
            id: None,
            terminal_id: "terminal-1".to_string(),
            timestamp: Utc::now(),
            input_type: InputType::Manual,
            content: "cargo test".to_string(),
            agent_role: Some("builder".to_string()),
            context: InputContext::default(),
        };
        let input_id = db.record_input(&input)?;

        // Record quality metrics
        let metrics = QualityMetrics {
            id: None,
            input_id: Some(input_id),
            timestamp: Utc::now(),
            tests_passed: Some(42),
            tests_failed: Some(3),
            tests_skipped: Some(1),
            test_runner: Some("cargo test".to_string()),
            lint_errors: Some(0),
            format_errors: Some(2),
            build_success: Some(true),
            pr_approved: None,
            pr_changes_requested: None,
            rework_count: None,
            human_rating: None,
        };

        let metrics_id = db.record_quality_metrics(&metrics)?;
        assert!(metrics_id > 0);

        // Retrieve and verify
        let retrieved = db.get_quality_metrics(input_id)?;
        assert!(retrieved.is_some());
        if let Some(m) = retrieved {
            assert_eq!(m.tests_passed, Some(42));
            assert_eq!(m.tests_failed, Some(3));
            assert_eq!(m.tests_skipped, Some(1));
            assert_eq!(m.test_runner, Some("cargo test".to_string()));
            assert_eq!(m.lint_errors, Some(0));
            assert_eq!(m.format_errors, Some(2));
            assert_eq!(m.build_success, Some(true));
        }

        Ok(())
    }

    #[test]
    fn test_record_quality_from_output() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // First record an input
        let input = AgentInput {
            id: None,
            terminal_id: "terminal-1".to_string(),
            timestamp: Utc::now(),
            input_type: InputType::Manual,
            content: "cargo test".to_string(),
            agent_role: Some("builder".to_string()),
            context: InputContext::default(),
        };
        let input_id = db.record_input(&input)?;

        // Simulate cargo test output
        let output = r#"
running 10 tests
test test_one ... ok
test test_two ... ok
test test_three ... FAILED
test result: FAILED. 9 passed; 1 failed; 0 ignored
"#;

        let metrics_id = db.record_quality_from_output(input_id, output)?;
        assert!(metrics_id.is_some());

        // Verify parsed metrics
        let retrieved = db.get_quality_metrics(input_id)?;
        assert!(retrieved.is_some());
        if let Some(m) = retrieved {
            assert_eq!(m.tests_passed, Some(9));
            assert_eq!(m.tests_failed, Some(1));
            assert_eq!(m.tests_skipped, Some(0));
            assert_eq!(m.test_runner, Some("cargo".to_string()));
        }

        Ok(())
    }

    #[test]
    fn test_record_quality_from_output_no_tests() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // First record an input
        let input = AgentInput {
            id: None,
            terminal_id: "terminal-1".to_string(),
            timestamp: Utc::now(),
            input_type: InputType::Manual,
            content: "ls -la".to_string(),
            agent_role: Some("builder".to_string()),
            context: InputContext::default(),
        };
        let input_id = db.record_input(&input)?;

        // Output with no test results
        let output = "total 42\ndrwxr-xr-x  5 user staff 160 Jan 23 10:00 .\n";

        let metrics_id = db.record_quality_from_output(input_id, output)?;
        assert!(metrics_id.is_none()); // No metrics recorded for non-test output

        Ok(())
    }

    #[test]
    fn test_terminal_test_summary() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // Record multiple test runs for the same terminal
        for i in 0..3 {
            let input = AgentInput {
                id: None,
                terminal_id: "terminal-1".to_string(),
                timestamp: Utc::now(),
                input_type: InputType::Manual,
                content: format!("cargo test run {i}"),
                agent_role: Some("builder".to_string()),
                context: InputContext::default(),
            };
            let input_id = db.record_input(&input)?;

            let metrics = QualityMetrics {
                id: None,
                input_id: Some(input_id),
                timestamp: Utc::now(),
                tests_passed: Some(10),
                tests_failed: Some(i),
                tests_skipped: Some(1),
                test_runner: Some("cargo".to_string()),
                lint_errors: None,
                format_errors: None,
                build_success: None,
                pr_approved: None,
                pr_changes_requested: None,
                rework_count: None,
                human_rating: None,
            };
            db.record_quality_metrics(&metrics)?;
        }

        // Get summary
        let (passed, failed, skipped) = db.get_terminal_test_summary("terminal-1")?;
        assert_eq!(passed, 30); // 3 * 10
        assert_eq!(failed, 3); // 0 + 1 + 2
        assert_eq!(skipped, 3); // 3 * 1

        // Different terminal should have no tests
        let (passed, failed, skipped) = db.get_terminal_test_summary("terminal-2")?;
        assert_eq!(passed, 0);
        assert_eq!(failed, 0);
        assert_eq!(skipped, 0);

        Ok(())
    }

    #[test]
    fn test_update_pr_review_status() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // Record an input and quality metrics
        let input = AgentInput {
            id: None,
            terminal_id: "terminal-1".to_string(),
            timestamp: Utc::now(),
            input_type: InputType::Manual,
            content: "gh pr create".to_string(),
            agent_role: Some("builder".to_string()),
            context: InputContext::default(),
        };
        let input_id = db.record_input(&input)?;

        let metrics = QualityMetrics {
            id: None,
            input_id: Some(input_id),
            timestamp: Utc::now(),
            tests_passed: Some(10),
            tests_failed: Some(0),
            tests_skipped: Some(0),
            test_runner: Some("cargo".to_string()),
            lint_errors: None,
            format_errors: None,
            build_success: Some(true),
            pr_approved: None,
            pr_changes_requested: None,
            rework_count: None,
            human_rating: None,
        };
        db.record_quality_metrics(&metrics)?;

        // Update PR review status to approved
        db.update_pr_review_status(input_id, Some(true), Some(false))?;

        let retrieved = db.get_quality_metrics(input_id)?;
        assert!(retrieved.is_some());
        if let Some(m) = retrieved {
            assert_eq!(m.pr_approved, Some(true));
            assert_eq!(m.pr_changes_requested, Some(false));
        }

        // Update to changes requested
        db.update_pr_review_status(input_id, Some(false), Some(true))?;

        let retrieved = db.get_quality_metrics(input_id)?;
        assert!(retrieved.is_some());
        if let Some(m) = retrieved {
            assert_eq!(m.pr_approved, Some(false));
            assert_eq!(m.pr_changes_requested, Some(true));
        }

        Ok(())
    }

    #[test]
    fn test_increment_rework_count() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // Record an input and quality metrics
        let input = AgentInput {
            id: None,
            terminal_id: "terminal-1".to_string(),
            timestamp: Utc::now(),
            input_type: InputType::Manual,
            content: "gh pr create".to_string(),
            agent_role: Some("builder".to_string()),
            context: InputContext::default(),
        };
        let input_id = db.record_input(&input)?;

        let metrics = QualityMetrics {
            id: None,
            input_id: Some(input_id),
            timestamp: Utc::now(),
            tests_passed: Some(10),
            tests_failed: Some(0),
            tests_skipped: Some(0),
            test_runner: Some("cargo".to_string()),
            lint_errors: None,
            format_errors: None,
            build_success: Some(true),
            pr_approved: None,
            pr_changes_requested: Some(true),
            rework_count: Some(0),
            human_rating: None,
        };
        db.record_quality_metrics(&metrics)?;

        // Increment rework count
        let count1 = db.increment_rework_count(input_id)?;
        assert_eq!(count1, 1);

        // Increment again
        let count2 = db.increment_rework_count(input_id)?;
        assert_eq!(count2, 2);

        // Verify via get_quality_metrics
        let retrieved = db.get_quality_metrics(input_id)?;
        assert!(retrieved.is_some());
        if let Some(m) = retrieved {
            assert_eq!(m.rework_count, Some(2));
        }

        Ok(())
    }

    #[test]
    fn test_get_pr_rework_stats() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // Create several PRs with varying rework counts
        for i in 0..3 {
            let input = AgentInput {
                id: None,
                terminal_id: "terminal-1".to_string(),
                timestamp: Utc::now(),
                input_type: InputType::Manual,
                content: format!("gh pr create {i}"),
                agent_role: Some("builder".to_string()),
                context: InputContext::default(),
            };
            let input_id = db.record_input(&input)?;

            let metrics = QualityMetrics {
                id: None,
                input_id: Some(input_id),
                timestamp: Utc::now(),
                tests_passed: Some(10),
                tests_failed: Some(0),
                tests_skipped: Some(0),
                test_runner: Some("cargo".to_string()),
                lint_errors: None,
                format_errors: None,
                build_success: Some(true),
                pr_approved: Some(true),
                pr_changes_requested: Some(false),
                rework_count: Some(i), // 0, 1, 2 rework cycles
                human_rating: None,
            };
            db.record_quality_metrics(&metrics)?;
        }

        let stats = db.get_pr_rework_stats()?;
        assert_eq!(stats.total_prs_tracked, 3);
        assert_eq!(stats.prs_with_rework, 2); // Only PRs with rework > 0
        assert_eq!(stats.max_rework_count, 2);
        // Average of 1 and 2 = 1.5
        assert!((stats.avg_rework_count - 1.5).abs() < 0.01);

        Ok(())
    }

    #[test]
    fn test_get_prompt_success_correlation() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // Create prompts with varying test results
        // Prompt 1: All passing (10 passed, 0 failed)
        let input1 = AgentInput {
            id: None,
            terminal_id: "terminal-1".to_string(),
            timestamp: Utc::now(),
            input_type: InputType::Manual,
            content: "cargo test".to_string(),
            agent_role: Some("builder".to_string()),
            context: InputContext::default(),
        };
        let input1_id = db.record_input(&input1)?;

        let metrics1 = QualityMetrics {
            id: None,
            input_id: Some(input1_id),
            timestamp: Utc::now(),
            tests_passed: Some(10),
            tests_failed: Some(0),
            tests_skipped: Some(0),
            test_runner: Some("cargo".to_string()),
            lint_errors: None,
            format_errors: None,
            build_success: Some(true),
            pr_approved: None,
            pr_changes_requested: None,
            rework_count: None,
            human_rating: None,
        };
        db.record_quality_metrics(&metrics1)?;

        // Prompt 2: Some failing (8 passed, 2 failed) -> 80% pass rate
        let input2 = AgentInput {
            id: None,
            terminal_id: "terminal-1".to_string(),
            timestamp: Utc::now(),
            input_type: InputType::Manual,
            content: "cargo test".to_string(),
            agent_role: Some("builder".to_string()),
            context: InputContext::default(),
        };
        let input2_id = db.record_input(&input2)?;

        let metrics2 = QualityMetrics {
            id: None,
            input_id: Some(input2_id),
            timestamp: Utc::now(),
            tests_passed: Some(8),
            tests_failed: Some(2),
            tests_skipped: Some(0),
            test_runner: Some("cargo".to_string()),
            lint_errors: None,
            format_errors: None,
            build_success: Some(true),
            pr_approved: None,
            pr_changes_requested: None,
            rework_count: None,
            human_rating: None,
        };
        db.record_quality_metrics(&metrics2)?;

        let stats = db.get_prompt_success_correlation()?;
        assert_eq!(stats.total_prompts_with_tests, 2);
        assert_eq!(stats.prompts_with_all_passing, 1);
        // Average pass rate: (1.0 + 0.8) / 2 = 0.9
        assert!((stats.avg_test_pass_rate - 0.9).abs() < 0.01);

        Ok(())
    }

    #[test]
    fn test_record_resource_usage() -> Result<()> {
        use super::super::resource_usage::ResourceUsage;

        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // First record an input to link to
        let input = AgentInput {
            id: None,
            terminal_id: "terminal-1".to_string(),
            timestamp: Utc::now(),
            input_type: InputType::Manual,
            content: "Build the feature".to_string(),
            agent_role: Some("builder".to_string()),
            context: InputContext::default(),
        };
        let input_id = db.record_input(&input)?;

        // Record resource usage
        let usage = ResourceUsage {
            input_id: Some(input_id),
            model: "claude-3-5-sonnet".to_string(),
            tokens_input: 1000,
            tokens_output: 500,
            tokens_cache_read: Some(200),
            tokens_cache_write: Some(50),
            cost_usd: 0.0108375,
            duration_ms: Some(1500),
            provider: "anthropic".to_string(),
            timestamp: Utc::now(),
        };

        let usage_id = db.record_resource_usage(&usage)?;
        assert!(usage_id > 0);

        // Retrieve and verify
        let retrieved = db.get_resource_usage(input_id)?;
        assert_eq!(retrieved.len(), 1);
        let r = &retrieved[0];
        assert_eq!(r.model, "claude-3-5-sonnet");
        assert_eq!(r.tokens_input, 1000);
        assert_eq!(r.tokens_output, 500);
        assert_eq!(r.tokens_cache_read, Some(200));
        assert_eq!(r.tokens_cache_write, Some(50));
        assert!((r.cost_usd - 0.0108375).abs() < 0.0001);
        assert_eq!(r.duration_ms, Some(1500));
        assert_eq!(r.provider, "anthropic");

        Ok(())
    }

    #[test]
    fn test_record_resource_usage_from_output() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // First record an input
        let input = AgentInput {
            id: None,
            terminal_id: "terminal-1".to_string(),
            timestamp: Utc::now(),
            input_type: InputType::Manual,
            content: "Build the feature".to_string(),
            agent_role: Some("builder".to_string()),
            context: InputContext::default(),
        };
        let input_id = db.record_input(&input)?;

        // Simulate terminal output with token usage
        let output = "Model: claude-3-5-sonnet\nTokens: 1000 in / 500 out\nDuration: 1.5s";

        let usage_id = db.record_resource_usage_from_output(Some(input_id), output, None)?;
        assert!(usage_id.is_some());

        // Verify parsed data was stored
        let retrieved = db.get_resource_usage(input_id)?;
        assert_eq!(retrieved.len(), 1);
        let r = &retrieved[0];
        assert_eq!(r.model, "claude-3-5-sonnet");
        assert_eq!(r.tokens_input, 1000);
        assert_eq!(r.tokens_output, 500);
        assert_eq!(r.duration_ms, Some(1500));

        Ok(())
    }

    #[test]
    fn test_record_resource_usage_from_output_no_tokens() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // First record an input
        let input = AgentInput {
            id: None,
            terminal_id: "terminal-1".to_string(),
            timestamp: Utc::now(),
            input_type: InputType::Manual,
            content: "ls -la".to_string(),
            agent_role: Some("builder".to_string()),
            context: InputContext::default(),
        };
        let input_id = db.record_input(&input)?;

        // Output with no token information
        let output = "total 42\ndrwxr-xr-x  5 user staff 160 Jan 23 10:00 .\n";

        let usage_id = db.record_resource_usage_from_output(Some(input_id), output, None)?;
        assert!(usage_id.is_none()); // No usage recorded

        Ok(())
    }

    #[test]
    fn test_terminal_resource_summary() -> Result<()> {
        use super::super::resource_usage::ResourceUsage;

        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // Record multiple resource usages for the same terminal
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

            let usage = ResourceUsage {
                input_id: Some(input_id),
                model: "claude-3-5-sonnet".to_string(),
                tokens_input: 1000,
                tokens_output: 500,
                tokens_cache_read: None,
                tokens_cache_write: None,
                cost_usd: 0.0105, // 1000 * 0.003/1k + 500 * 0.015/1k
                duration_ms: Some(1000),
                provider: "anthropic".to_string(),
                timestamp: Utc::now(),
            };
            db.record_resource_usage(&usage)?;
        }

        // Get summary
        let (input_tokens, output_tokens, total_cost) =
            db.get_terminal_resource_summary("terminal-1")?;
        assert_eq!(input_tokens, 3000); // 3 * 1000
        assert_eq!(output_tokens, 1500); // 3 * 500
        assert!((total_cost - 0.0315).abs() < 0.0001); // 3 * 0.0105

        // Different terminal should have no usage
        let (input_tokens, output_tokens, total_cost) =
            db.get_terminal_resource_summary("terminal-2")?;
        assert_eq!(input_tokens, 0);
        assert_eq!(output_tokens, 0);
        assert!((total_cost - 0.0).abs() < 0.0001);

        Ok(())
    }

    // ========================================================================
    // Cost Analytics Tests (Issue #1064)
    // ========================================================================

    #[test]
    fn test_save_and_get_budget_config() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // Create a monthly budget
        let config = BudgetConfig {
            id: None,
            period: BudgetPeriod::Monthly,
            limit_usd: 100.0,
            alert_threshold: 0.8,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            is_active: true,
        };

        let id = db.save_budget_config(&config)?;
        assert!(id > 0);

        // Retrieve and verify
        let retrieved = db.get_budget_config(BudgetPeriod::Monthly)?;
        assert!(retrieved.is_some());
        if let Some(c) = retrieved {
            assert_eq!(c.period, BudgetPeriod::Monthly);
            assert!((c.limit_usd - 100.0).abs() < 0.001);
            assert!((c.alert_threshold - 0.8).abs() < 0.001);
            assert!(c.is_active);
        }

        Ok(())
    }

    #[test]
    fn test_budget_config_replaces_previous() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // Create initial budget
        let config1 = BudgetConfig {
            id: None,
            period: BudgetPeriod::Monthly,
            limit_usd: 100.0,
            alert_threshold: 0.8,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            is_active: true,
        };
        db.save_budget_config(&config1)?;

        // Create new budget - should deactivate the old one
        let config2 = BudgetConfig {
            id: None,
            period: BudgetPeriod::Monthly,
            limit_usd: 200.0,
            alert_threshold: 0.9,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            is_active: true,
        };
        db.save_budget_config(&config2)?;

        // Should get the new budget
        let retrieved = db.get_budget_config(BudgetPeriod::Monthly)?;
        assert!(retrieved.is_some());
        if let Some(c) = retrieved {
            assert!((c.limit_usd - 200.0).abs() < 0.001);
            assert!((c.alert_threshold - 0.9).abs() < 0.001);
        }

        Ok(())
    }

    #[test]
    fn test_get_all_budget_configs() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // Create budgets for different periods
        for (period, limit) in [
            (BudgetPeriod::Daily, 10.0),
            (BudgetPeriod::Weekly, 50.0),
            (BudgetPeriod::Monthly, 200.0),
        ] {
            let config = BudgetConfig {
                id: None,
                period,
                limit_usd: limit,
                alert_threshold: 0.8,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                is_active: true,
            };
            db.save_budget_config(&config)?;
        }

        let configs = db.get_all_budget_configs()?;
        assert_eq!(configs.len(), 3);

        Ok(())
    }

    #[test]
    fn test_get_cost_summary() -> Result<()> {
        use super::super::resource_usage::ResourceUsage;

        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // Record some resource usage
        for i in 0..5 {
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

            let usage = ResourceUsage {
                input_id: Some(input_id),
                model: "claude-3-5-sonnet".to_string(),
                tokens_input: 1000,
                tokens_output: 500,
                tokens_cache_read: None,
                tokens_cache_write: None,
                cost_usd: 0.01,
                duration_ms: Some(1000),
                provider: "anthropic".to_string(),
                timestamp: Utc::now(),
            };
            db.record_resource_usage(&usage)?;
        }

        // Get summary for a wide range
        let start = Utc::now() - chrono::Duration::hours(1);
        let end = Utc::now() + chrono::Duration::hours(1);
        let summary = db.get_cost_summary(start, end)?;

        assert_eq!(summary.request_count, 5);
        assert!((summary.total_cost - 0.05).abs() < 0.001);
        assert_eq!(summary.total_input_tokens, 5000);
        assert_eq!(summary.total_output_tokens, 2500);

        Ok(())
    }

    #[test]
    fn test_get_cost_by_role() -> Result<()> {
        use super::super::resource_usage::ResourceUsage;

        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // Record usage for different roles
        for (role, cost) in [("builder", 0.05), ("judge", 0.02), ("curator", 0.01)] {
            let input = AgentInput {
                id: None,
                terminal_id: "terminal-1".to_string(),
                timestamp: Utc::now(),
                input_type: InputType::Manual,
                content: "test command".to_string(),
                agent_role: Some(role.to_string()),
                context: InputContext::default(),
            };
            let input_id = db.record_input(&input)?;

            let usage = ResourceUsage {
                input_id: Some(input_id),
                model: "claude-3-5-sonnet".to_string(),
                tokens_input: 1000,
                tokens_output: 500,
                tokens_cache_read: None,
                tokens_cache_write: None,
                cost_usd: cost,
                duration_ms: Some(1000),
                provider: "anthropic".to_string(),
                timestamp: Utc::now(),
            };
            db.record_resource_usage(&usage)?;
        }

        let by_role = db.get_cost_by_role()?;
        assert_eq!(by_role.len(), 3);

        // Builder should have highest cost
        let builder = by_role.iter().find(|r| r.agent_role == "builder");
        assert!(builder.is_some());
        if let Some(b) = builder {
            assert!((b.total_cost - 0.05).abs() < 0.001);
        }

        Ok(())
    }

    #[test]
    fn test_get_budget_status() -> Result<()> {
        use super::super::resource_usage::ResourceUsage;

        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // Set up a monthly budget of $100 with 80% alert threshold
        let config = BudgetConfig {
            id: None,
            period: BudgetPeriod::Monthly,
            limit_usd: 100.0,
            alert_threshold: 0.8,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            is_active: true,
        };
        db.save_budget_config(&config)?;

        // Record $50 of usage
        for i in 0..5 {
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

            let usage = ResourceUsage {
                input_id: Some(input_id),
                model: "claude-3-5-sonnet".to_string(),
                tokens_input: 1000,
                tokens_output: 500,
                tokens_cache_read: None,
                tokens_cache_write: None,
                cost_usd: 10.0, // $10 each = $50 total
                duration_ms: Some(1000),
                provider: "anthropic".to_string(),
                timestamp: Utc::now(),
            };
            db.record_resource_usage(&usage)?;
        }

        let status = db.get_budget_status(BudgetPeriod::Monthly)?;
        assert!(status.is_some());
        if let Some(s) = status {
            assert!((s.spent - 50.0).abs() < 0.001);
            assert!((s.remaining - 50.0).abs() < 0.001);
            assert!((s.usage_percent - 50.0).abs() < 0.1);
            assert!(!s.alert_triggered); // 50% < 80%
            assert!(!s.budget_exceeded);
        }

        Ok(())
    }

    #[test]
    fn test_get_budget_status_alert_triggered() -> Result<()> {
        use super::super::resource_usage::ResourceUsage;

        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // Set up budget with 80% threshold
        let config = BudgetConfig {
            id: None,
            period: BudgetPeriod::Monthly,
            limit_usd: 100.0,
            alert_threshold: 0.8,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            is_active: true,
        };
        db.save_budget_config(&config)?;

        // Record $85 of usage (85% > 80% threshold)
        let input = AgentInput {
            id: None,
            terminal_id: "terminal-1".to_string(),
            timestamp: Utc::now(),
            input_type: InputType::Manual,
            content: "expensive operation".to_string(),
            agent_role: Some("builder".to_string()),
            context: InputContext::default(),
        };
        let input_id = db.record_input(&input)?;

        let usage = ResourceUsage {
            input_id: Some(input_id),
            model: "claude-opus-4".to_string(),
            tokens_input: 10000,
            tokens_output: 5000,
            tokens_cache_read: None,
            tokens_cache_write: None,
            cost_usd: 85.0,
            duration_ms: Some(5000),
            provider: "anthropic".to_string(),
            timestamp: Utc::now(),
        };
        db.record_resource_usage(&usage)?;

        let status = db.get_budget_status(BudgetPeriod::Monthly)?;
        assert!(status.is_some());
        if let Some(s) = status {
            assert!(s.alert_triggered); // 85% > 80%
            assert!(!s.budget_exceeded); // But not over 100%
        }

        Ok(())
    }

    #[test]
    fn test_project_runway() -> Result<()> {
        use super::super::resource_usage::ResourceUsage;

        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // Set up a monthly budget of $100
        let config = BudgetConfig {
            id: None,
            period: BudgetPeriod::Monthly,
            limit_usd: 100.0,
            alert_threshold: 0.8,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            is_active: true,
        };
        db.save_budget_config(&config)?;

        // Record $10/day for 5 days
        for day in 0..5 {
            let input = AgentInput {
                id: None,
                terminal_id: "terminal-1".to_string(),
                timestamp: Utc::now() - chrono::Duration::days(day),
                input_type: InputType::Manual,
                content: format!("day {day} work"),
                agent_role: Some("builder".to_string()),
                context: InputContext::default(),
            };
            let input_id = db.record_input(&input)?;

            let usage = ResourceUsage {
                input_id: Some(input_id),
                model: "claude-3-5-sonnet".to_string(),
                tokens_input: 1000,
                tokens_output: 500,
                tokens_cache_read: None,
                tokens_cache_write: None,
                cost_usd: 10.0,
                duration_ms: Some(1000),
                provider: "anthropic".to_string(),
                timestamp: Utc::now() - chrono::Duration::days(day),
            };
            db.record_resource_usage(&usage)?;
        }

        let projection = db.project_runway(BudgetPeriod::Monthly, 7)?;
        assert!(projection.is_some());
        if let Some(p) = projection {
            // Average daily cost should be around $50/7 days (not all days had usage)
            assert!(p.avg_daily_cost > 0.0);
            assert!(p.days_remaining > 0.0);
            assert!(p.exhaustion_date.is_some());
        }

        Ok(())
    }

    #[test]
    fn test_budget_not_configured() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // No budget configured
        let status = db.get_budget_status(BudgetPeriod::Monthly)?;
        assert!(status.is_none());

        let projection = db.project_runway(BudgetPeriod::Monthly, 7)?;
        assert!(projection.is_none());

        Ok(())
    }
}
