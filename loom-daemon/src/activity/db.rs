//! Database operations for activity tracking.
//!
//! This module contains the `ActivityDb` struct and core methods for
//! recording and querying agent activity data.
//!
//! Domain-specific operations are delegated to submodules:
//! - [`super::claims`]: Issue claim registry
//! - [`super::cost_analytics`]: Budget and cost analysis
//! - [`super::metrics`]: Task metrics and productivity
//! - [`super::prompts`]: Prompt tracking (git changes, GitHub events)
//! - [`super::quality`]: Quality metrics (tests, PR reviews, rework)

use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use std::path::PathBuf;

use super::models::{
    ActivityEntry, AgentInput, AgentMetric, AgentOutput, BudgetConfig, BudgetPeriod, BudgetStatus,
    ClaimResult, ClaimType, CostByIssue, CostByPr, CostByRole, CostSummary, InputContext,
    InputType, IssueClaim, PrReworkStats, ProductivitySummary, PromptChanges, PromptGitHubEvent,
    PromptSuccessStats, QualityMetrics, RunwayProjection, TokenUsage,
};
use super::schema::init_schema;
use super::{claims, cost_analytics, metrics, prompts, quality};

/// Activity database for tracking agent inputs and results
pub struct ActivityDb {
    pub(super) conn: Connection,
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

    // ========================================================================
    // Task Metrics Methods
    // Delegated to metrics module
    // ========================================================================

    /// Start tracking a new task.
    #[allow(dead_code)]
    pub fn start_task(&self, metric: &AgentMetric) -> Result<i64> {
        metrics::start_task(&self.conn, metric)
    }

    /// Complete a task and calculate wall time.
    #[allow(dead_code)]
    pub fn complete_task(
        &self,
        metric_id: i64,
        status: &str,
        outcome_type: Option<&str>,
    ) -> Result<()> {
        metrics::complete_task(&self.conn, metric_id, status, outcome_type)
    }

    /// Update task quality metrics (commits, CI failures, etc.).
    #[allow(dead_code)]
    pub fn update_task_metrics(
        &self,
        metric_id: i64,
        test_failures: Option<i32>,
        ci_failures: Option<i32>,
        commits_count: Option<i32>,
        lines_changed: Option<i32>,
    ) -> Result<()> {
        metrics::update_task_metrics(
            &self.conn,
            metric_id,
            test_failures,
            ci_failures,
            commits_count,
            lines_changed,
        )
    }

    /// Record token usage for an API request.
    #[allow(dead_code)]
    pub fn record_token_usage(&self, usage: &TokenUsage) -> Result<i64> {
        metrics::record_token_usage(&self.conn, usage)
    }

    /// Get metrics for a specific task.
    #[allow(dead_code)]
    pub fn get_task_metrics(&self, metric_id: i64) -> Result<AgentMetric> {
        metrics::get_task_metrics(&self.conn, metric_id)
    }

    /// Get productivity summary grouped by agent system.
    #[allow(dead_code)]
    pub fn get_productivity_summary(&self) -> Result<ProductivitySummary> {
        metrics::get_productivity_summary(&self.conn)
    }

    // ========================================================================
    // Prompt Tracking Methods
    // Delegated to prompts module
    // ========================================================================

    /// Record a prompt-GitHub correlation event.
    pub fn record_prompt_github_event(&self, event: &PromptGitHubEvent) -> Result<i64> {
        prompts::record_prompt_github_event(&self.conn, event)
    }

    /// Get prompt-GitHub events for a specific input.
    #[allow(dead_code)]
    pub fn get_prompt_github_events(&self, input_id: i64) -> Result<Vec<PromptGitHubEvent>> {
        prompts::get_prompt_github_events(&self.conn, input_id)
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

    /// Record git changes associated with a prompt.
    pub fn record_prompt_changes(&self, changes: &PromptChanges) -> Result<i64> {
        prompts::record_prompt_changes(&self.conn, changes)
    }

    /// Get prompt changes for a specific input.
    #[allow(dead_code)]
    pub fn get_prompt_changes(&self, input_id: i64) -> Result<Option<PromptChanges>> {
        prompts::get_prompt_changes(&self.conn, input_id)
    }

    /// Get total lines changed across all prompts for a terminal.
    #[allow(dead_code)]
    pub fn get_terminal_changes_summary(&self, terminal_id: &str) -> Result<(i64, i64, i64)> {
        prompts::get_terminal_changes_summary(&self.conn, terminal_id)
    }

    // ========================================================================
    // Quality Metrics Methods
    // Delegated to quality module
    // ========================================================================

    /// Record quality metrics for an input/output.
    #[allow(dead_code)]
    pub fn record_quality_metrics(&self, metrics: &QualityMetrics) -> Result<i64> {
        quality::record_quality_metrics(&self.conn, metrics)
    }

    /// Parse and record quality metrics from terminal output.
    #[allow(dead_code)]
    pub fn record_quality_from_output(&self, input_id: i64, output: &str) -> Result<Option<i64>> {
        quality::record_quality_from_output(&self.conn, input_id, output)
    }

    /// Get quality metrics for a specific input.
    #[allow(dead_code)]
    pub fn get_quality_metrics(&self, input_id: i64) -> Result<Option<QualityMetrics>> {
        quality::get_quality_metrics(&self.conn, input_id)
    }

    /// Get aggregate test outcomes for a terminal.
    #[allow(dead_code)]
    pub fn get_terminal_test_summary(&self, terminal_id: &str) -> Result<(i64, i64, i64)> {
        quality::get_terminal_test_summary(&self.conn, terminal_id)
    }

    /// Update PR review status for a quality metrics record.
    #[allow(dead_code)]
    pub fn update_pr_review_status(
        &self,
        input_id: i64,
        approved: Option<bool>,
        changes_requested: Option<bool>,
    ) -> Result<()> {
        quality::update_pr_review_status(&self.conn, input_id, approved, changes_requested)
    }

    /// Increment the rework count for a quality metrics record.
    #[allow(dead_code)]
    pub fn increment_rework_count(&self, input_id: i64) -> Result<i32> {
        quality::increment_rework_count(&self.conn, input_id)
    }

    /// Get PR rework statistics.
    #[allow(dead_code)]
    pub fn get_pr_rework_stats(&self) -> Result<PrReworkStats> {
        quality::get_pr_rework_stats(&self.conn)
    }

    /// Get quality metrics correlation: prompts that led to passing tests.
    #[allow(dead_code)]
    pub fn get_prompt_success_correlation(&self) -> Result<PromptSuccessStats> {
        quality::get_prompt_success_correlation(&self.conn)
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
    // Delegated to cost_analytics module
    // ========================================================================

    /// Save or update a budget configuration.
    #[allow(dead_code)]
    pub fn save_budget_config(&self, config: &BudgetConfig) -> Result<i64> {
        cost_analytics::save_budget_config(&self.conn, config)
    }

    /// Get the active budget configuration for a period.
    #[allow(dead_code)]
    pub fn get_budget_config(&self, period: BudgetPeriod) -> Result<Option<BudgetConfig>> {
        cost_analytics::get_budget_config(&self.conn, period)
    }

    /// Get all active budget configurations.
    #[allow(dead_code)]
    pub fn get_all_budget_configs(&self) -> Result<Vec<BudgetConfig>> {
        cost_analytics::get_all_budget_configs(&self.conn)
    }

    /// Get cost summary for a date range.
    #[allow(dead_code)]
    pub fn get_cost_summary(
        &self,
        start_date: DateTime<Utc>,
        end_date: DateTime<Utc>,
    ) -> Result<CostSummary> {
        cost_analytics::get_cost_summary(&self.conn, start_date, end_date)
    }

    /// Get cost breakdown by agent role.
    #[allow(dead_code)]
    pub fn get_cost_by_role(&self) -> Result<Vec<CostByRole>> {
        cost_analytics::get_cost_by_role(&self.conn)
    }

    /// Get cost breakdown by GitHub issue.
    #[allow(dead_code)]
    pub fn get_cost_by_issue(&self, issue_number: Option<i32>) -> Result<Vec<CostByIssue>> {
        cost_analytics::get_cost_by_issue(&self.conn, issue_number)
    }

    /// Get cost breakdown by pull request.
    #[allow(dead_code)]
    pub fn get_cost_by_pr(&self, pr_number: Option<i32>) -> Result<Vec<CostByPr>> {
        cost_analytics::get_cost_by_pr(&self.conn, pr_number)
    }

    /// Get budget status for a specific period.
    #[allow(dead_code)]
    pub fn get_budget_status(&self, period: BudgetPeriod) -> Result<Option<BudgetStatus>> {
        cost_analytics::get_budget_status(&self.conn, period)
    }

    /// Project runway based on recent burn rate.
    #[allow(dead_code)]
    pub fn project_runway(
        &self,
        period: BudgetPeriod,
        lookback_days: i32,
    ) -> Result<Option<RunwayProjection>> {
        cost_analytics::project_runway(&self.conn, period, lookback_days)
    }

    // ========================================================================
    // Issue Claim Registry Methods (Issue #1159)
    // Delegated to claims module
    // ========================================================================

    /// Attempt to claim an issue or PR for a terminal.
    pub fn claim_issue(
        &self,
        number: i32,
        claim_type: ClaimType,
        terminal_id: &str,
        label: Option<&str>,
        agent_role: Option<&str>,
        stale_threshold_secs: Option<i64>,
    ) -> Result<ClaimResult> {
        claims::claim_issue(
            &self.conn,
            number,
            claim_type,
            terminal_id,
            label,
            agent_role,
            stale_threshold_secs,
        )
    }

    /// Release a claim on an issue or PR.
    pub fn release_claim(
        &self,
        number: i32,
        claim_type: ClaimType,
        terminal_id: Option<&str>,
    ) -> Result<bool> {
        claims::release_claim(&self.conn, number, claim_type, terminal_id)
    }

    /// Update the heartbeat for an active claim.
    pub fn heartbeat_claim(
        &self,
        number: i32,
        claim_type: ClaimType,
        terminal_id: &str,
    ) -> Result<bool> {
        claims::heartbeat_claim(&self.conn, number, claim_type, terminal_id)
    }

    /// Get a specific claim if it exists.
    pub fn get_claim(&self, number: i32, claim_type: ClaimType) -> Result<Option<IssueClaim>> {
        claims::get_claim(&self.conn, number, claim_type)
    }

    /// Get all active claims.
    pub fn get_all_claims(&self) -> Result<Vec<IssueClaim>> {
        claims::get_all_claims(&self.conn)
    }

    /// Get claims for a specific terminal.
    pub fn get_claims_by_terminal(&self, terminal_id: &str) -> Result<Vec<IssueClaim>> {
        claims::get_claims_by_terminal(&self.conn, terminal_id)
    }

    /// Get stale claims (those without heartbeat for longer than threshold).
    #[allow(dead_code)]
    pub fn get_stale_claims(&self, stale_threshold_secs: i64) -> Result<Vec<IssueClaim>> {
        claims::get_stale_claims(&self.conn, stale_threshold_secs)
    }

    /// Release all stale claims and return the count of claims released.
    pub fn release_stale_claims(&self, stale_threshold_secs: i64) -> Result<usize> {
        claims::release_stale_claims(&self.conn, stale_threshold_secs)
    }

    /// Release all claims for a specific terminal.
    pub fn release_terminal_claims(&self, terminal_id: &str) -> Result<usize> {
        claims::release_terminal_claims(&self.conn, terminal_id)
    }

    /// Get a summary of all claims for visibility.
    pub fn get_claims_summary(
        &self,
        stale_threshold_secs: i64,
    ) -> Result<super::models::ClaimsSummary> {
        claims::get_claims_summary(&self.conn, stale_threshold_secs)
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
#[allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::redundant_closure_for_method_calls
)]
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
        let output = r"
running 10 tests
test test_one ... ok
test test_two ... ok
test test_three ... FAILED
test result: FAILED. 9 passed; 1 failed; 0 ignored
";

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
            cost_usd: 0.010_837_5,
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
        assert!((r.cost_usd - 0.010_837_5).abs() < 0.0001);
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

    // Cost Analytics tests: see cost_analytics.rs
    // Issue Claim Registry tests: see claims.rs
}
