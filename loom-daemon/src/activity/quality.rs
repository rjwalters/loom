//! Quality metrics tracking for test results, PR reviews, and rework cycles.
//!
//! Extracted from `db.rs` — provides recording of quality metrics including
//! test results, lint/format status, and build status parsed from terminal
//! output.

use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};

use super::models::QualityMetrics;
use super::test_parser;

// ========================================================================
// Quality Metrics Recording
// ========================================================================

/// Record quality metrics for an input/output.
fn record_quality_metrics(conn: &Connection, metrics: &QualityMetrics) -> Result<i64> {
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
