//! Cost analytics and budget management.
//!
//! Extracted from `db.rs` (Issue #1064) — provides budget configuration,
//! cost breakdowns by role/issue/PR, budget status checks, and runway projections.

use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};

use super::models::{
    BudgetConfig, BudgetPeriod, BudgetStatus, CostByIssue, CostByPr, CostByRole, CostSummary,
    RunwayProjection,
};

// ========================================================================
// Budget Configuration
// ========================================================================

/// Save or update a budget configuration.
///
/// If the budget for the given period already exists, it will be updated.
/// Otherwise, a new record will be created.
pub(super) fn save_budget_config(conn: &Connection, config: &BudgetConfig) -> Result<i64> {
    // Deactivate any existing budget for this period
    conn.execute(
        r"UPDATE budget_config SET is_active = 0 WHERE period = ?1 AND is_active = 1",
        params![config.period.as_str()],
    )?;

    // Insert new budget
    conn.execute(
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

    Ok(conn.last_insert_rowid())
}

/// Get the active budget configuration for a period.
pub(super) fn get_budget_config(
    conn: &Connection,
    period: BudgetPeriod,
) -> Result<Option<BudgetConfig>> {
    let result = conn.query_row(
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
                .map_or_else(|_| Utc::now(), |dt| dt.with_timezone(&Utc));

            let updated_at = DateTime::parse_from_rfc3339(&updated_str)
                .map_or_else(|_| Utc::now(), |dt| dt.with_timezone(&Utc));

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
pub(super) fn get_all_budget_configs(conn: &Connection) -> Result<Vec<BudgetConfig>> {
    let mut stmt = conn.prepare(
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
            rusqlite::Error::ToSqlConversionFailure(format!("Invalid period: {period_str}").into())
        })?;

        let created_at = DateTime::parse_from_rfc3339(&created_str)
            .map_or_else(|_| Utc::now(), |dt| dt.with_timezone(&Utc));

        let updated_at = DateTime::parse_from_rfc3339(&updated_str)
            .map_or_else(|_| Utc::now(), |dt| dt.with_timezone(&Utc));

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

// ========================================================================
// Cost Queries
// ========================================================================

/// Get cost summary for a date range.
///
/// Returns aggregated cost metrics including total cost, request count,
/// token usage, and average cost per request.
pub(super) fn get_cost_summary(
    conn: &Connection,
    start_date: DateTime<Utc>,
    end_date: DateTime<Utc>,
) -> Result<CostSummary> {
    let result = conn.query_row(
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
            Ok((total_cost, request_count, row.get::<_, i64>(2)?, row.get::<_, i64>(3)?))
        },
    )?;

    let (total_cost, request_count, total_input_tokens, total_output_tokens) = result;
    #[allow(clippy::cast_precision_loss)]
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
pub(super) fn get_cost_by_role(conn: &Connection) -> Result<Vec<CostByRole>> {
    let mut stmt = conn.prepare(
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
pub(super) fn get_cost_by_issue(
    conn: &Connection,
    issue_number: Option<i32>,
) -> Result<Vec<CostByIssue>> {
    if let Some(issue_num) = issue_number {
        let mut stmt = conn.prepare(
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
        let results = stmt.query_map(params![issue_num], parse_cost_by_issue)?;
        results.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    } else {
        let mut stmt = conn.prepare(
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
        let results = stmt.query_map([], parse_cost_by_issue)?;
        results.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

/// Get cost breakdown by pull request.
pub(super) fn get_cost_by_pr(conn: &Connection, pr_number: Option<i32>) -> Result<Vec<CostByPr>> {
    if let Some(pr_num) = pr_number {
        let mut stmt = conn.prepare(
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
        let results = stmt.query_map(params![pr_num], parse_cost_by_pr)?;
        results.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    } else {
        let mut stmt = conn.prepare(
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
        let results = stmt.query_map([], parse_cost_by_pr)?;
        results.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

// ========================================================================
// Budget Status & Runway
// ========================================================================

/// Get budget status for a specific period.
///
/// Calculates current spend within the period and compares against the budget limit.
pub(super) fn get_budget_status(
    conn: &Connection,
    period: BudgetPeriod,
) -> Result<Option<BudgetStatus>> {
    let Some(config) = get_budget_config(conn, period)? else {
        return Ok(None);
    };

    let (period_start, period_end) = get_period_bounds(period);
    let summary = get_cost_summary(conn, period_start, period_end)?;

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
pub(super) fn project_runway(
    conn: &Connection,
    period: BudgetPeriod,
    lookback_days: i32,
) -> Result<Option<RunwayProjection>> {
    let Some(config) = get_budget_config(conn, period)? else {
        return Ok(None);
    };

    // Get current period status
    let (period_start, _) = get_period_bounds(period);
    let status = get_budget_status(conn, period)?;
    let remaining_budget = status.map_or(config.limit_usd, |s| s.remaining);

    // Calculate average daily cost over lookback period
    let lookback_start = Utc::now() - chrono::Duration::days(i64::from(lookback_days));
    let lookback_end = Utc::now();
    let lookback_summary = get_cost_summary(conn, lookback_start, lookback_end)?;

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

    #[allow(clippy::cast_possible_truncation)]
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

// ========================================================================
// Helper Functions
// ========================================================================

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
pub(super) fn get_period_bounds(period: BudgetPeriod) -> (DateTime<Utc>, DateTime<Utc>) {
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

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
#[allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::redundant_closure_for_method_calls
)]
mod tests {
    use super::super::db::ActivityDb;
    use super::super::models::{BudgetConfig, BudgetPeriod};
    use super::super::resource_usage::ResourceUsage;
    use super::*;
    use crate::activity::models::{AgentInput, InputContext, InputType};
    use tempfile::NamedTempFile;

    #[test]
    fn test_save_and_get_budget_config() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

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
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

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
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

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

        let builder = by_role.iter().find(|r| r.agent_role == "builder");
        assert!(builder.is_some());
        if let Some(b) = builder {
            assert!((b.total_cost - 0.05).abs() < 0.001);
        }

        Ok(())
    }

    #[test]
    fn test_get_budget_status() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

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
                cost_usd: 10.0,
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
            assert!(!s.alert_triggered);
            assert!(!s.budget_exceeded);
        }

        Ok(())
    }

    #[test]
    fn test_get_budget_status_alert_triggered() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

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
            assert!(s.alert_triggered);
            assert!(!s.budget_exceeded);
        }

        Ok(())
    }

    #[test]
    fn test_project_runway() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let db = ActivityDb::new(temp_file.path().to_path_buf())?;

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

        let status = db.get_budget_status(BudgetPeriod::Monthly)?;
        assert!(status.is_none());

        let projection = db.project_runway(BudgetPeriod::Monthly, 7)?;
        assert!(projection.is_none());

        Ok(())
    }
}
