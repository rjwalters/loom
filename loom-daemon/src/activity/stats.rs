//! Statistics and metrics queries for agent effectiveness analysis.
//!
//! This module provides query functions that aggregate correlation data
//! (querying the base tables directly) into actionable metrics:
//! - Agent effectiveness by role (optionally per model)
//! - Cost per issue
//! - Weekly velocity
//! - Overall summary metrics
//!
//! # Example
//!
//! ```ignore
//! use activity::stats::StatsQueries;
//!
//! let db = ActivityDb::new("activity.db".into())?;
//! let rows = db.get_effectiveness_rows(None, "all", false)?;
//! for row in rows {
//!     println!("{}: {}% success rate", row.role, row.success_rate);
//! }
//! ```

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// `agent-metrics` CLI parity (epic #4081 Phase 3 family 4, issue #4274).
//
// The types and queries below are the native port of
// `loom_tools.agent_metrics` — the CLI contract `agent-metrics.sh` and
// `mcp__loom__get_agent_metrics` invoke, and the sole source for the
// `loom-daemon stats` dashboard. They query the base tables directly
// (mirroring the Python SQL), with period filter and per-model dimension.
// ---------------------------------------------------------------------------

/// Summary metrics for the `agent-metrics summary` command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummaryMetrics {
    pub total_prompts: i64,
    pub total_tokens: i64,
    pub total_cost: f64,
    pub issues_count: i64,
    pub prs_count: i64,
    pub success_rate: f64,
    /// At least one `resource_usage` row falls in scope. Gates the cost/token
    /// fields in the `loom-daemon stats` dashboard (`n/a` instead of `0`,
    /// #6128). `serde(skip)`: not part of the `agent-metrics` JSON contract.
    #[serde(skip)]
    pub cost_data_available: bool,
    /// At least one `prompt_github` row falls in scope (gates issues/PRs).
    #[serde(skip)]
    pub github_data_available: bool,
    /// At least one `quality_metrics` row falls in scope (gates success rate).
    #[serde(skip)]
    pub quality_data_available: bool,
}

/// Effectiveness row for the `agent-metrics effectiveness` command,
/// optionally grouped by model (`--by-model`, #3482).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectivenessRow {
    pub role: String,
    pub total_prompts: i64,
    pub successful_prompts: i64,
    pub success_rate: f64,
    pub avg_cost: f64,
    pub avg_duration_sec: f64,
    /// Populated only when queried with `by_model = true`; NULL/absent model
    /// values in the DB render as `"default"`. Omitted from JSON output
    /// (rather than emitted as `null`) when absent, matching the pre-#3482
    /// JSON shape for existing consumers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// At least one `resource_usage` row backs this group (gates
    /// `avg_cost`/`avg_duration_sec`, #6128). `serde(skip)`: dashboard only.
    #[serde(skip)]
    pub cost_data_available: bool,
    /// At least one `quality_metrics` row backs this group (gates
    /// `success_rate`, #6128). `serde(skip)`: dashboard only.
    #[serde(skip)]
    pub quality_data_available: bool,
}

/// Cost row for the `agent-metrics costs` command, optionally grouped by
/// model (`--by-model`, #3482). `model` semantics match [`EffectivenessRow`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostRow {
    pub issue_number: i64,
    pub prompt_count: i64,
    pub total_cost: f64,
    pub total_tokens: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Earliest prompt timestamp for the issue (raw DB text). `serde(skip)`:
    /// used only by the `loom-daemon stats --issue` breakdown.
    #[serde(skip)]
    pub started: Option<String>,
    /// Latest prompt timestamp for the issue (raw DB text). `serde(skip)`.
    #[serde(skip)]
    pub completed: Option<String>,
}

/// Weekly velocity row for the `agent-metrics velocity` command (last 8
/// weeks / 56 days).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VelocityRow {
    pub week: String,
    pub prompts: i64,
    pub issues: i64,
    pub prs_merged: i64,
    pub cost: f64,
}

/// SQL WHERE-clause fragment for a `--period` value. Unknown values behave
/// like `"all"` (no filter) — the CLI layer is responsible for rejecting
/// invalid `--period` values before calling into these queries.
fn period_sql_filter(period: &str) -> &'static str {
    match period {
        "today" => "AND i.timestamp >= datetime('now', 'start of day')",
        "week" => "AND i.timestamp >= datetime('now', '-7 days')",
        "month" => "AND i.timestamp >= datetime('now', '-30 days')",
        _ => "",
    }
}

/// SQL WHERE-clause fragment for a `role` filter. Quotes are doubled
/// defensively; the CLI layer additionally validates `role` against a fixed
/// allow-list before this is ever reached.
fn role_sql_filter(role: Option<&str>) -> String {
    match role {
        Some(r) => format!("AND i.agent_role = '{}'", r.replace('\'', "''")),
        None => String::new(),
    }
}

const MODEL_EXPR: &str = "COALESCE(NULLIF(r.model, ''), 'default')";

/// Query summary metrics (`agent_metrics.get_summary` parity).
pub fn query_summary_metrics(
    conn: &Connection,
    role: Option<&str>,
    period: &str,
) -> rusqlite::Result<SummaryMetrics> {
    let role_filter = role_sql_filter(role);
    let period_filter = period_sql_filter(period);

    let (
        total_prompts,
        total_tokens,
        total_cost,
        issues_count,
        prs_count,
        resource_rows,
        github_rows,
    ): (i64, i64, f64, i64, i64, i64, i64) = conn.query_row(
        &format!(
            r"SELECT
                COUNT(*),
                COALESCE(SUM(r.tokens_input + r.tokens_output), 0),
                ROUND(COALESCE(SUM(r.cost_usd), 0), 4),
                COUNT(DISTINCT pg.issue_number),
                COUNT(DISTINCT pg.pr_number),
                COUNT(r.input_id),
                COUNT(pg.input_id)
              FROM agent_inputs i
              LEFT JOIN resource_usage r ON i.id = r.input_id
              LEFT JOIN prompt_github pg ON i.id = pg.input_id
              WHERE 1=1 {role_filter} {period_filter}"
        ),
        [],
        |row| {
            Ok((
                row.get(0)?,
                row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                row.get::<_, Option<f64>>(2)?.unwrap_or(0.0),
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        },
    )?;

    let (success_rate, quality_rows): (f64, i64) = conn.query_row(
        &format!(
            r"SELECT
                    ROUND(
                        100.0 * SUM(
                            CASE WHEN q.tests_passed > 0
                                 AND (q.tests_failed IS NULL OR q.tests_failed = 0)
                            THEN 1 ELSE 0 END
                        ) / NULLIF(COUNT(*), 0),
                        1
                    ),
                    COUNT(q.input_id)
                  FROM agent_inputs i
                  LEFT JOIN quality_metrics q ON i.id = q.input_id
                  WHERE 1=1 {role_filter} {period_filter}"
        ),
        [],
        |row| Ok((row.get::<_, Option<f64>>(0)?.unwrap_or(0.0), row.get::<_, i64>(1)?)),
    )?;

    Ok(SummaryMetrics {
        total_prompts,
        total_tokens,
        total_cost,
        issues_count,
        prs_count,
        success_rate,
        cost_data_available: resource_rows > 0,
        github_data_available: github_rows > 0,
        quality_data_available: quality_rows > 0,
    })
}

/// Query effectiveness rows (`agent_metrics.get_effectiveness` parity).
pub fn query_effectiveness_rows(
    conn: &Connection,
    role: Option<&str>,
    period: &str,
    by_model: bool,
) -> rusqlite::Result<Vec<EffectivenessRow>> {
    let role_filter = role_sql_filter(role);
    let period_filter = period_sql_filter(period);
    let model_select = if by_model {
        format!("{MODEL_EXPR} as model,")
    } else {
        String::new()
    };
    let model_group = if by_model {
        format!(", {MODEL_EXPR}")
    } else {
        String::new()
    };

    let sql = format!(
        r"SELECT
            COALESCE(i.agent_role, 'unknown') as role,
            {model_select}
            COUNT(*) as total_prompts,
            SUM(CASE WHEN q.tests_passed > 0 AND (q.tests_failed IS NULL OR q.tests_failed = 0) THEN 1 ELSE 0 END) as successful_prompts,
            ROUND(100.0 * SUM(CASE WHEN q.tests_passed > 0 AND (q.tests_failed IS NULL OR q.tests_failed = 0) THEN 1 ELSE 0 END) / NULLIF(COUNT(*), 0), 1) as success_rate,
            ROUND(COALESCE(AVG(r.cost_usd), 0), 4) as avg_cost,
            ROUND(COALESCE(AVG(r.duration_ms / 1000.0), 0), 1) as avg_duration_sec,
            COUNT(r.input_id) as resource_rows,
            COUNT(q.input_id) as quality_rows
          FROM agent_inputs i
          LEFT JOIN quality_metrics q ON i.id = q.input_id
          LEFT JOIN resource_usage r ON i.id = r.input_id
          WHERE 1=1 {role_filter} {period_filter}
          GROUP BY COALESCE(i.agent_role, 'unknown'){model_group}
          ORDER BY success_rate DESC"
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        Ok(EffectivenessRow {
            role: row.get("role")?,
            total_prompts: row.get("total_prompts")?,
            successful_prompts: row
                .get::<_, Option<i64>>("successful_prompts")?
                .unwrap_or(0),
            success_rate: row.get::<_, Option<f64>>("success_rate")?.unwrap_or(0.0),
            avg_cost: row.get::<_, Option<f64>>("avg_cost")?.unwrap_or(0.0),
            avg_duration_sec: row
                .get::<_, Option<f64>>("avg_duration_sec")?
                .unwrap_or(0.0),
            model: if by_model {
                Some(
                    row.get::<_, Option<String>>("model")?
                        .unwrap_or_else(|| "default".to_string()),
                )
            } else {
                None
            },
            cost_data_available: row.get::<_, i64>("resource_rows")? > 0,
            quality_data_available: row.get::<_, i64>("quality_rows")? > 0,
        })
    })?;

    rows.collect()
}

/// Query cost rows (`agent_metrics.get_costs` parity).
pub fn query_cost_rows(
    conn: &Connection,
    issue_number: Option<i32>,
    by_model: bool,
) -> rusqlite::Result<Vec<CostRow>> {
    let issue_filter = match issue_number {
        Some(n) => format!("WHERE pg.issue_number = {n}"),
        None => String::new(),
    };
    let model_select = if by_model {
        format!("{MODEL_EXPR} as model,")
    } else {
        String::new()
    };
    let model_group = if by_model {
        format!(", {MODEL_EXPR}")
    } else {
        String::new()
    };

    let sql = format!(
        r"SELECT
            pg.issue_number,
            {model_select}
            COUNT(DISTINCT i.id) as prompt_count,
            ROUND(COALESCE(SUM(r.cost_usd), 0), 4) as total_cost,
            COALESCE(SUM(r.tokens_input + r.tokens_output), 0) as total_tokens,
            MIN(i.timestamp) as started,
            MAX(i.timestamp) as completed
          FROM prompt_github pg
          JOIN agent_inputs i ON pg.input_id = i.id
          LEFT JOIN resource_usage r ON i.id = r.input_id
          {issue_filter}
          GROUP BY pg.issue_number{model_group}
          ORDER BY total_cost DESC
          LIMIT 20"
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        let issue_number: Option<i64> = row.get("issue_number")?;
        Ok((
            issue_number,
            CostRow {
                issue_number: issue_number.unwrap_or(0),
                prompt_count: row.get("prompt_count")?,
                total_cost: row.get::<_, Option<f64>>("total_cost")?.unwrap_or(0.0),
                total_tokens: row.get::<_, Option<i64>>("total_tokens")?.unwrap_or(0),
                model: if by_model {
                    Some(
                        row.get::<_, Option<String>>("model")?
                            .unwrap_or_else(|| "default".to_string()),
                    )
                } else {
                    None
                },
                started: row.get("started")?,
                completed: row.get("completed")?,
            },
        ))
    })?;

    let mut out = Vec::new();
    for r in rows {
        let (issue_number, row) = r?;
        if issue_number.is_some() {
            out.push(row);
        }
    }
    Ok(out)
}

/// Query velocity rows (`agent_metrics.get_velocity` parity): last 8 weeks
/// (56 days), including per-week issues touched and PRs merged.
pub fn query_velocity_rows(conn: &Connection) -> rusqlite::Result<Vec<VelocityRow>> {
    let mut stmt = conn.prepare(
        r"SELECT
            strftime('%Y-W%W', i.timestamp) as week,
            COUNT(*) as prompts,
            COUNT(DISTINCT pg.issue_number) as issues,
            COUNT(DISTINCT CASE WHEN pg.event_type = 'pr_merged' THEN pg.pr_number END) as prs_merged,
            ROUND(SUM(r.cost_usd), 2) as cost
          FROM agent_inputs i
          LEFT JOIN prompt_github pg ON i.id = pg.input_id
          LEFT JOIN resource_usage r ON i.id = r.input_id
          WHERE i.timestamp >= datetime('now', '-56 days')
          GROUP BY week
          ORDER BY week DESC
          LIMIT 8",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(VelocityRow {
            week: row.get("week")?,
            prompts: row.get("prompts")?,
            issues: row.get::<_, Option<i64>>("issues")?.unwrap_or(0),
            prs_merged: row.get::<_, Option<i64>>("prs_merged")?.unwrap_or(0),
            cost: row.get::<_, Option<f64>>("cost")?.unwrap_or(0.0),
        })
    })?;

    rows.collect()
}

/// Query methods for stats views.
///
/// These methods are implemented on `ActivityDb` via extension trait
/// to keep the stats functionality modular.
pub trait StatsQueries {
    /// `agent-metrics summary` parity: role/period-filtered summary metrics
    /// (issue #4274).
    fn get_summary_metrics(
        &self,
        role: Option<&str>,
        period: &str,
    ) -> rusqlite::Result<SummaryMetrics>;

    /// `agent-metrics effectiveness` parity: role/period-filtered
    /// effectiveness rows, optionally grouped by model (issue #4274).
    fn get_effectiveness_rows(
        &self,
        role: Option<&str>,
        period: &str,
        by_model: bool,
    ) -> rusqlite::Result<Vec<EffectivenessRow>>;

    /// `agent-metrics costs` parity: cost rows, optionally filtered by issue
    /// and grouped by model (issue #4274).
    fn get_cost_rows(
        &self,
        issue_number: Option<i32>,
        by_model: bool,
    ) -> rusqlite::Result<Vec<CostRow>>;

    /// `agent-metrics velocity` parity: weekly velocity rows for the last 8
    /// weeks (issue #4274).
    fn get_velocity_rows(&self) -> rusqlite::Result<Vec<VelocityRow>>;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::float_cmp)]
mod tests {
    use super::*;
    use crate::activity::schema::init_schema;
    use tempfile::tempdir;

    fn setup_test_db() -> rusqlite::Result<(Connection, tempfile::TempDir)> {
        let temp_dir = tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");
        let conn = Connection::open(&db_path)?;
        init_schema(&conn)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.to_string().into()))?;
        // Return both connection and tempdir to keep tempdir alive
        Ok((conn, temp_dir))
    }

    // -----------------------------------------------------------------
    // `agent-metrics` CLI parity tests (issue #4274)
    // -----------------------------------------------------------------

    fn insert_sample_input(
        conn: &Connection,
        role: &str,
        model: &str,
        cost: f64,
        tests_passed: i64,
        tests_failed: i64,
        issue_number: i64,
    ) -> rusqlite::Result<i64> {
        conn.execute(
            r"INSERT INTO agent_inputs (terminal_id, timestamp, input_type, content, agent_role, context)
              VALUES ('t1', datetime('now'), 'manual', 'test', ?1, '{}')",
            [role],
        )?;
        let input_id = conn.last_insert_rowid();

        conn.execute(
            r"INSERT INTO quality_metrics (input_id, timestamp, tests_passed, tests_failed)
              VALUES (?1, datetime('now'), ?2, ?3)",
            rusqlite::params![input_id, tests_passed, tests_failed],
        )?;

        conn.execute(
            r"INSERT INTO resource_usage (input_id, timestamp, model, tokens_input, tokens_output, cost_usd, duration_ms, provider)
              VALUES (?1, datetime('now'), ?2, 1000, 500, ?3, 1500, 'anthropic')",
            rusqlite::params![input_id, model, cost],
        )?;

        conn.execute(
            r"INSERT INTO prompt_github (input_id, issue_number, event_type)
              VALUES (?1, ?2, 'label_added')",
            rusqlite::params![input_id, issue_number],
        )?;

        Ok(input_id)
    }

    #[test]
    fn test_query_summary_metrics() -> rusqlite::Result<()> {
        let (conn, _temp_dir) = setup_test_db()?;
        insert_sample_input(&conn, "builder", "claude-sonnet", 0.05, 10, 0, 42)?;
        insert_sample_input(&conn, "judge", "claude-sonnet", 0.02, 0, 1, 43)?;

        let all = query_summary_metrics(&conn, None, "all")?;
        assert_eq!(all.total_prompts, 2);
        assert_eq!(all.issues_count, 2);
        assert!((all.total_cost - 0.07).abs() < 0.0001);
        assert!((all.success_rate - 50.0).abs() < 0.0001);

        let builder_only = query_summary_metrics(&conn, Some("builder"), "all")?;
        assert_eq!(builder_only.total_prompts, 1);
        assert!((builder_only.success_rate - 100.0).abs() < 0.0001);

        // "today" period filter should still include rows inserted "now".
        let today = query_summary_metrics(&conn, None, "today")?;
        assert_eq!(today.total_prompts, 2);

        Ok(())
    }

    #[test]
    fn test_query_effectiveness_rows_by_model() -> rusqlite::Result<()> {
        let (conn, _temp_dir) = setup_test_db()?;
        insert_sample_input(&conn, "builder", "claude-sonnet", 0.05, 10, 0, 42)?;
        insert_sample_input(&conn, "builder", "claude-opus", 0.20, 10, 0, 43)?;

        let unmodeled = query_effectiveness_rows(&conn, None, "all", false)?;
        assert_eq!(unmodeled.len(), 1);
        assert_eq!(unmodeled[0].total_prompts, 2);
        assert!(unmodeled[0].model.is_none());

        let by_model = query_effectiveness_rows(&conn, None, "all", true)?;
        assert_eq!(by_model.len(), 2);
        for row in &by_model {
            assert!(row.model.is_some());
        }

        Ok(())
    }

    #[test]
    fn test_query_cost_rows() -> rusqlite::Result<()> {
        let (conn, _temp_dir) = setup_test_db()?;
        insert_sample_input(&conn, "builder", "claude-sonnet", 0.05, 10, 0, 42)?;
        insert_sample_input(&conn, "builder", "claude-opus", 0.20, 10, 0, 42)?;
        insert_sample_input(&conn, "builder", "claude-sonnet", 0.01, 10, 0, 99)?;

        let all = query_cost_rows(&conn, None, false)?;
        assert_eq!(all.len(), 2);
        assert!(all[0].total_cost >= all[1].total_cost);

        let filtered = query_cost_rows(&conn, Some(42), false)?;
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].issue_number, 42);
        assert_eq!(filtered[0].prompt_count, 2);
        assert!((filtered[0].total_cost - 0.25).abs() < 0.0001);

        let by_model = query_cost_rows(&conn, Some(42), true)?;
        assert_eq!(by_model.len(), 2);

        Ok(())
    }

    #[test]
    fn test_query_velocity_rows() -> rusqlite::Result<()> {
        let (conn, _temp_dir) = setup_test_db()?;
        insert_sample_input(&conn, "builder", "claude-sonnet", 0.05, 10, 0, 42)?;

        let rows = query_velocity_rows(&conn)?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].prompts, 1);
        assert_eq!(rows[0].issues, 1);

        Ok(())
    }

    /// #6128: an empty database has recorded nothing, so every availability
    /// gate must be false (renders `n/a`, not `0`); recording rows flips only
    /// the gates whose table has data.
    #[test]
    fn test_summary_availability_gates() -> rusqlite::Result<()> {
        let (conn, _temp_dir) = setup_test_db()?;
        let empty = query_summary_metrics(&conn, None, "all")?;
        assert_eq!(empty.total_prompts, 0);
        assert!(!empty.cost_data_available);
        assert!(!empty.github_data_available);
        assert!(!empty.quality_data_available);
        assert!(query_effectiveness_rows(&conn, None, "all", false)?.is_empty());
        assert!(query_cost_rows(&conn, None, false)?.is_empty());

        // resource_usage + prompt_github recorded, no quality_metrics.
        for i in 0..3 {
            conn.execute(
                r"INSERT INTO agent_inputs (terminal_id, timestamp, input_type, content, agent_role, context)
                  VALUES ('t1', datetime('now'), 'manual', 'test', 'builder', '{}')",
                [],
            )?;
            let id = conn.last_insert_rowid();
            conn.execute(
                r"INSERT INTO resource_usage (input_id, timestamp, model, tokens_input, tokens_output, cost_usd, provider)
                  VALUES (?1, datetime('now'), 'claude-sonnet', 1000, 500, 0.01, 'anthropic')",
                [id],
            )?;
            conn.execute(
                r"INSERT INTO prompt_github (input_id, issue_number, event_type)
                  VALUES (?1, ?2, 'label_added')",
                rusqlite::params![id, 100 + i],
            )?;
        }
        let s = query_summary_metrics(&conn, None, "all")?;
        assert_eq!(s.total_prompts, 3);
        assert_eq!(s.total_tokens, 4500);
        assert_eq!(s.issues_count, 3);
        assert!(s.cost_data_available);
        assert!(s.github_data_available);
        assert!(!s.quality_data_available);
        Ok(())
    }

    /// #6128: a role whose prompts have no `resource_usage`/`quality_metrics`
    /// rows must report its rate/cost fields as unavailable, not a real zero.
    #[test]
    fn test_effectiveness_without_output_data_is_unavailable() -> rusqlite::Result<()> {
        let (conn, _temp_dir) = setup_test_db()?;
        conn.execute(
            r"INSERT INTO agent_inputs (terminal_id, timestamp, input_type, content, agent_role, context)
              VALUES ('t1', datetime('now'), 'manual', 'test', 'judge', '{}')",
            [],
        )?;
        let rows = query_effectiveness_rows(&conn, Some("judge"), "all", false)?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].total_prompts, 1);
        assert!(!rows[0].quality_data_available);
        assert!(!rows[0].cost_data_available);
        Ok(())
    }

    /// #13100: a quality row recorded through `record_quality_from_output`
    /// with a real input id must join to its `agent_inputs` row.
    #[test]
    fn test_quality_from_output_joins_effectiveness_rows() -> rusqlite::Result<()> {
        let (conn, _temp_dir) = setup_test_db()?;
        conn.execute(
            r"INSERT INTO agent_inputs (terminal_id, timestamp, input_type, content, agent_role, context)
              VALUES ('t1', datetime('now'), 'manual', 'test', 'builder', '{}')",
            [],
        )?;
        let input_id: i64 = conn.last_insert_rowid();
        let output = "running 2 tests\ntest result: ok. 2 passed; 0 failed; 0 ignored\n";
        let recorded = super::super::quality::record_quality_from_output(&conn, input_id, output)
            .expect("record quality");
        assert!(recorded.is_some());

        let rows = query_effectiveness_rows(&conn, Some("builder"), "all", false)?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].successful_prompts, 1);
        assert!(rows[0].quality_data_available);
        Ok(())
    }
}
