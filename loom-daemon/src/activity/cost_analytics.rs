//! Cost analytics.
//!
//! Extracted from `db.rs` (Issue #1064) — provides the cost breakdown by issue.

use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};

use super::models::CostByIssue;

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
