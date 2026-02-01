//! Quality metrics tracking for test results, PR reviews, and rework cycles.
//!
//! Extracted from `db.rs` — provides recording and querying of quality metrics
//! including test results, lint/format status, PR review outcomes, and rework tracking.

use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};

use super::models::{PrReworkStats, PromptSuccessStats, QualityMetrics};
use super::test_parser;

// ========================================================================
// Quality Metrics Recording
// ========================================================================

/// Record quality metrics for an input/output.
pub(super) fn record_quality_metrics(conn: &Connection, metrics: &QualityMetrics) -> Result<i64> {
    conn.execute(
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

    Ok(conn.last_insert_rowid())
}

/// Parse and record quality metrics from terminal output.
///
/// Automatically parses test results, lint errors, and build status from
/// the output content and stores them in the database.
pub(super) fn record_quality_from_output(
    conn: &Connection,
    input_id: i64,
    output: &str,
) -> Result<Option<i64>> {
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
        rework_count: None,
        human_rating: None,
    };

    Ok(Some(record_quality_metrics(conn, &metrics)?))
}

// ========================================================================
// Quality Metrics Queries
// ========================================================================

/// Get quality metrics for a specific input.
pub(super) fn get_quality_metrics(
    conn: &Connection,
    input_id: i64,
) -> Result<Option<QualityMetrics>> {
    let result = conn.query_row(
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

/// Get aggregate test outcomes for a terminal.
///
/// Returns a tuple of (passed, failed, skipped) counts across all quality metrics
/// for the given terminal.
pub(super) fn get_terminal_test_summary(
    conn: &Connection,
    terminal_id: &str,
) -> Result<(i64, i64, i64)> {
    let result = conn.query_row(
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

// ========================================================================
// PR Review Status
// ========================================================================

/// Update PR review status for a quality metrics record.
pub(super) fn update_pr_review_status(
    conn: &Connection,
    input_id: i64,
    approved: Option<bool>,
    changes_requested: Option<bool>,
) -> Result<()> {
    conn.execute(
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

/// Increment the rework count for a quality metrics record.
///
/// Called when a PR receives `changes_requested` feedback, indicating
/// another review cycle is needed.
pub(super) fn increment_rework_count(conn: &Connection, input_id: i64) -> Result<i32> {
    conn.execute(
        r"
        UPDATE quality_metrics
        SET rework_count = COALESCE(rework_count, 0) + 1
        WHERE input_id = ?1
        ",
        params![input_id],
    )?;

    // Return the new rework count
    let count: i32 = conn.query_row(
        "SELECT COALESCE(rework_count, 0) FROM quality_metrics WHERE input_id = ?1",
        params![input_id],
        |row| row.get(0),
    )?;

    Ok(count)
}

/// Get PR rework statistics.
///
/// Returns summary statistics for PR review cycles:
/// - Average rework count per PR
/// - Max rework count
/// - Total PRs with rework (count > 0)
/// - Total PRs tracked
pub(super) fn get_pr_rework_stats(conn: &Connection) -> Result<PrReworkStats> {
    let result = conn.query_row(
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

/// Get quality metrics correlation: prompts that led to passing tests.
///
/// Returns the count and average test pass rate for prompts with quality metrics.
pub(super) fn get_prompt_success_correlation(conn: &Connection) -> Result<PromptSuccessStats> {
    let result = conn.query_row(
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
