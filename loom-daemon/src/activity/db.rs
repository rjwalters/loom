//! Database operations for activity tracking.
//!
//! This module contains the `ActivityDb` struct and core methods for
//! recording and querying agent activity data.
//!
//! Domain-specific operations are delegated to submodules:
//! - [`super::claims`]: Issue claim registry
//! - [`super::cost_analytics`]: Budget and cost analysis
//! - [`super::prompts`]: Prompt tracking (git changes, forge event recording)
//! - [`super::quality`]: Quality metrics parsing from terminal output

use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use std::path::PathBuf;

use super::models::{
    ActivityEntry, AgentInput, AgentOutput, BudgetConfig, BudgetPeriod, BudgetStatus, ClaimResult,
    ClaimType, CostByIssue, CostByPr, CostByRole, CostSummary, InputContext, InputType, IssueClaim,
    PromptChanges, PromptForgeEvent, RunwayProjection,
};
use super::schema::init_schema;
use super::{claims, cost_analytics, prompts, quality, usage_report, weekly_point_history};

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

    // ========================================================================
    // Prompt Tracking Methods
    // Delegated to prompts module
    // ========================================================================

    /// Record a prompt-GitHub correlation event.
    pub fn record_prompt_forge_event(&self, event: &PromptForgeEvent) -> Result<i64> {
        prompts::record_prompt_forge_event(&self.conn, event)
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

        // rusqlite 0.39 dropped the `ToSql` impl for `usize`; convert to i64 for SQLite.
        // Saturate at i64::MAX on 64-bit platforms where `usize` can exceed i64.
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        let entries = stmt.query_map(params![terminal_id, limit_i64], |row| {
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

    // ========================================================================
    // Quality Metrics Methods
    // Delegated to quality module
    // ========================================================================

    /// Parse and record quality metrics from terminal output.
    #[allow(dead_code)]
    pub fn record_quality_from_output(&self, input_id: i64, output: &str) -> Result<Option<i64>> {
        quality::record_quality_from_output(&self.conn, input_id, output)
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

    /// Token/cost usage report (Issue #8062): every `resource_usage` row at
    /// or after `since`, grouped by `group_by`. See
    /// [`super::usage_report`] for the aggregation and why the cost column
    /// is summed rather than recomputed.
    pub fn get_usage_report(
        &self,
        since: DateTime<Utc>,
        group_by: usage_report::UsageReportGroupBy,
    ) -> Result<Vec<usage_report::UsageReportRow>> {
        usage_report::get_usage_report(&self.conn, since, group_by)
    }

    /// Record one UTC day's weekly-limit-point sample (Issue #8347), keeping
    /// the day's maximum on conflict. See
    /// [`super::weekly_point_history`] for what a point is and why the day's
    /// maximum — not its latest reading — is the stored summary.
    ///
    /// Callers that must not fail (the `tokens check` probe path) go through
    /// [`super::weekly_point_history::record_daily_sample_best_effort_in`],
    /// which wraps this.
    pub fn record_weekly_point_sample(
        &self,
        day: NaiveDate,
        points: f64,
        account_count: i64,
    ) -> Result<()> {
        weekly_point_history::record_weekly_point_sample(
            &self.conn,
            day,
            points,
            account_count,
            Utc::now(),
        )
    }

    /// The daily weekly-limit-point series from `since` (inclusive) onward,
    /// oldest day first (Issue #8347).
    pub fn get_weekly_point_series(
        &self,
        since: NaiveDate,
    ) -> Result<Vec<weekly_point_history::WeeklyPointSample>> {
        weekly_point_history::get_weekly_point_series(&self.conn, since)
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

    fn get_summary_metrics(
        &self,
        role: Option<&str>,
        period: &str,
    ) -> rusqlite::Result<stats::SummaryMetrics> {
        stats::query_summary_metrics(&self.conn, role, period)
    }

    fn get_effectiveness_rows(
        &self,
        role: Option<&str>,
        period: &str,
        by_model: bool,
    ) -> rusqlite::Result<Vec<stats::EffectivenessRow>> {
        stats::query_effectiveness_rows(&self.conn, role, period, by_model)
    }

    fn get_cost_rows(
        &self,
        issue_number: Option<i32>,
        by_model: bool,
    ) -> rusqlite::Result<Vec<stats::CostRow>> {
        stats::query_cost_rows(&self.conn, issue_number, by_model)
    }

    fn get_velocity_rows(&self) -> rusqlite::Result<Vec<stats::VelocityRow>> {
        stats::query_velocity_rows(&self.conn)
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
    fn test_github_event_without_input_link() -> Result<()> {
        use super::super::models::PromptForgeEventType;

        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

        // Record a GitHub event without linking to a specific input
        let event = PromptForgeEvent {
            id: None,
            input_id: None,
            issue_number: Some(100),
            pr_number: None,
            label_before: Some(vec!["loom:issue".to_string()]),
            label_after: Some(vec!["loom:building".to_string()]),
            event_type: PromptForgeEventType::LabelAdded,
        };

        let event_id = db.record_prompt_forge_event(&event)?;
        assert!(event_id > 0);

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

        // Verify parsed metrics were persisted, querying the table directly
        // (the `get_quality_metrics` accessor was removed as dead code, #7567)
        let (tests_passed, tests_failed, tests_skipped, test_runner): (i64, i64, i64, String) =
            db.conn.query_row(
                "SELECT tests_passed, tests_failed, tests_skipped, test_runner \
             FROM quality_metrics WHERE input_id = ?1",
                params![input_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
        assert_eq!(tests_passed, 9);
        assert_eq!(tests_failed, 1);
        assert_eq!(tests_skipped, 0);
        assert_eq!(test_runner, "cargo");

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

        // Retrieve and verify, querying the table directly
        // (the `get_resource_usage` accessor was removed as dead code, #7567)
        struct Row {
            model: String,
            tokens_input: i64,
            tokens_output: i64,
            tokens_cache_read: Option<i64>,
            tokens_cache_write: Option<i64>,
            cost_usd: f64,
            duration_ms: Option<i64>,
            provider: String,
        }
        let row = db.conn.query_row(
            "SELECT model, tokens_input, tokens_output, tokens_cache_read, tokens_cache_write, cost_usd, duration_ms, provider \
             FROM resource_usage WHERE input_id = ?1",
            params![input_id],
            |r| {
                Ok(Row {
                    model: r.get(0)?,
                    tokens_input: r.get(1)?,
                    tokens_output: r.get(2)?,
                    tokens_cache_read: r.get(3)?,
                    tokens_cache_write: r.get(4)?,
                    cost_usd: r.get(5)?,
                    duration_ms: r.get(6)?,
                    provider: r.get(7)?,
                })
            },
        )?;
        assert_eq!(row.model, "claude-3-5-sonnet");
        assert_eq!(row.tokens_input, 1000);
        assert_eq!(row.tokens_output, 500);
        assert_eq!(row.tokens_cache_read, Some(200));
        assert_eq!(row.tokens_cache_write, Some(50));
        assert!((row.cost_usd - 0.010_837_5).abs() < 0.0001);
        assert_eq!(row.duration_ms, Some(1500));
        assert_eq!(row.provider, "anthropic");

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

        // Verify parsed data was stored, querying the table directly
        // (the `get_resource_usage` accessor was removed as dead code, #7567)
        let (model, tokens_input, tokens_output, duration_ms): (String, i64, i64, Option<i64>) =
            db.conn.query_row(
                "SELECT model, tokens_input, tokens_output, duration_ms \
                 FROM resource_usage WHERE input_id = ?1",
                params![input_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
        assert_eq!(model, "claude-3-5-sonnet");
        assert_eq!(tokens_input, 1000);
        assert_eq!(tokens_output, 500);
        assert_eq!(duration_ms, Some(1500));

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

    // Cost Analytics tests: see cost_analytics.rs
    // Issue Claim Registry tests: see claims.rs
}
