//! Token/cost usage report (Issue #8062): the aggregation behind
//! `loom-daemon usage-report`.
//!
//! Aggregates the already-ingested `resource_usage` table — populated by
//! #8059's transcript ingestion and, historically, the managed-terminal IPC
//! path (see [`super::transcript_ingest`] and [`super::resource_usage`]) —
//! into a role/model/repo/day breakdown with token counters and a dollar
//! cost.
//!
//! # Why the cost column is a `SUM`, not a recompute
//!
//! Every `resource_usage.cost_usd` value was already priced via
//! [`super::resource_usage::ModelPricing`] at write time — both writers
//! (transcript ingestion's `bucket_cost` and the IPC path's
//! `parse_resource_usage`) price through it before the row is ever inserted.
//! This module therefore only sums a stored column rather than re-deriving a
//! second copy of the pricing table, which is exactly what #8060's "there is
//! exactly ONE pricing table" invariant forbids.

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde::Serialize;

/// The `--by` grouping dimension for `loom-daemon usage-report`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageReportGroupBy {
    /// `agent_inputs.agent_role` (the attributed role — `builder`, `judge`,
    /// `sweep`, ...).
    Role,
    /// `resource_usage.model`.
    Model,
    /// `owner/repo`, read from `agent_inputs.context`'s `repo` key. Only the
    /// transcript-ingest writer (#8059) populates that key today, so rows
    /// from any other writer land in the `unknown` bucket.
    Repo,
    /// UTC calendar day of `resource_usage.timestamp`.
    Day,
}

impl UsageReportGroupBy {
    /// Parse a `--by` value. Case-insensitive.
    ///
    /// # Errors
    /// Returns an error for anything other than role/model/repo/day.
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "role" => Ok(Self::Role),
            "model" => Ok(Self::Model),
            "repo" => Ok(Self::Repo),
            "day" => Ok(Self::Day),
            other => {
                bail!("unknown --by value {other:?} (expected one of: role, model, repo, day)")
            }
        }
    }

    /// The wire/display spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Role => "role",
            Self::Model => "model",
            Self::Repo => "repo",
            Self::Day => "day",
        }
    }

    /// The SQL expression this dimension groups by, evaluated over the
    /// `resource_usage ru LEFT JOIN agent_inputs ai` join every group shares.
    /// Always yields a non-NULL string — an ungroupable row lands in
    /// `'unknown'` rather than being dropped.
    fn sql_expr(self) -> &'static str {
        match self {
            Self::Role => "COALESCE(ai.agent_role, 'unknown')",
            Self::Model => "COALESCE(NULLIF(ru.model, ''), 'unknown')",
            Self::Repo => "COALESCE(NULLIF(json_extract(ai.context, '$.repo'), ''), 'unknown')",
            Self::Day => "DATE(ru.timestamp)",
        }
    }
}

/// One aggregated row of the usage report.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageReportRow {
    /// The group's label — a role name, model id, `owner/repo`, or
    /// `YYYY-MM-DD`, depending on the report's [`UsageReportGroupBy`].
    pub group: String,
    pub request_count: i64,
    pub tokens_input: i64,
    pub tokens_output: i64,
    pub tokens_cache_read: i64,
    pub tokens_cache_write: i64,
    /// Sum of each contributing row's already-priced `resource_usage.cost_usd`
    /// — see the module doc for why this is not recomputed here.
    pub cost_usd: f64,
}

impl UsageReportRow {
    /// Total tokens across every counter (input + output + both cache axes).
    #[must_use]
    pub fn tokens_total(&self) -> i64 {
        self.tokens_input + self.tokens_output + self.tokens_cache_read + self.tokens_cache_write
    }
}

/// Query the usage report: every `resource_usage` row at or after `since`,
/// grouped by `group_by`, ordered by cost descending (most expensive group
/// first, ties broken by group label).
///
/// Returns an empty vec when nothing matches `since` — the caller decides how
/// to render that ("no data", not a misleading zeroed table).
///
/// # Errors
/// Propagates any SQLite failure.
pub(super) fn get_usage_report(
    conn: &Connection,
    since: DateTime<Utc>,
    group_by: UsageReportGroupBy,
) -> Result<Vec<UsageReportRow>> {
    let sql = format!(
        r"
        SELECT
            {group_expr} as group_key,
            COUNT(*) as request_count,
            COALESCE(SUM(ru.tokens_input), 0) as tokens_input,
            COALESCE(SUM(ru.tokens_output), 0) as tokens_output,
            COALESCE(SUM(ru.tokens_cache_read), 0) as tokens_cache_read,
            COALESCE(SUM(ru.tokens_cache_write), 0) as tokens_cache_write,
            COALESCE(SUM(ru.cost_usd), 0.0) as cost_usd
        FROM resource_usage ru
        LEFT JOIN agent_inputs ai ON ru.input_id = ai.id
        WHERE ru.timestamp >= ?1
        GROUP BY group_key
        ORDER BY cost_usd DESC, group_key ASC
        ",
        group_expr = group_by.sql_expr()
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([since.to_rfc3339()], |row| {
        Ok(UsageReportRow {
            group: row.get("group_key")?,
            request_count: row.get("request_count")?,
            tokens_input: row.get("tokens_input")?,
            tokens_output: row.get("tokens_output")?,
            tokens_cache_read: row.get("tokens_cache_read")?,
            tokens_cache_write: row.get("tokens_cache_write")?,
            cost_usd: row.get("cost_usd")?,
        })
    })?;

    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::db::ActivityDb;
    use super::super::models::{AgentInput, InputContext, InputType};
    use super::super::resource_usage::ResourceUsage;
    use super::*;

    /// Insert one `agent_inputs` + `resource_usage` row pair, with an
    /// optional raw context JSON overriding the default `InputContext`
    /// serialization (so a `repo` key can be seeded, exactly the way
    /// transcript ingestion writes it).
    #[allow(clippy::too_many_arguments)]
    fn seed(
        db: &ActivityDb,
        role: &str,
        model: &str,
        timestamp: DateTime<Utc>,
        tokens_input: i64,
        tokens_output: i64,
        cost_usd: f64,
        repo: Option<&str>,
    ) {
        let input_id = if let Some(repo) = repo {
            let context = serde_json::json!({ "repo": repo }).to_string();
            db.conn
                .execute(
                    "INSERT INTO agent_inputs (terminal_id, timestamp, input_type, content, agent_role, context) \
                     VALUES (?1, ?2, 'system', 'seed', ?3, ?4)",
                    rusqlite::params![
                        "terminal-1",
                        timestamp.to_rfc3339(),
                        role,
                        context
                    ],
                )
                .unwrap();
            db.conn.last_insert_rowid()
        } else {
            let input = AgentInput {
                id: None,
                terminal_id: "terminal-1".to_string(),
                timestamp,
                input_type: InputType::Manual,
                content: "seed".to_string(),
                agent_role: Some(role.to_string()),
                context: InputContext::default(),
            };
            db.record_input(&input).unwrap()
        };

        let usage = ResourceUsage {
            input_id: Some(input_id),
            model: model.to_string(),
            tokens_input,
            tokens_output,
            tokens_cache_read: None,
            tokens_cache_write: None,
            cost_usd,
            duration_ms: Some(1000),
            provider: "anthropic".to_string(),
            timestamp,
        };
        db.record_resource_usage(&usage).unwrap();
    }

    /// The returned `TempDir` must stay alive for as long as `ActivityDb` is
    /// used: dropping it deletes the on-disk file out from under the open
    /// SQLite connection, which then fails writes as "readonly".
    fn open_db() -> (tempfile::TempDir, ActivityDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = ActivityDb::new(dir.path().join("activity.db")).unwrap();
        (dir, db)
    }

    /// A fixture spanning multiple roles/models/days/repos, matching the
    /// Test Plan's "fixture `token_usage` dataset" (read via `resource_usage`
    /// per #8059's either/or decision, not the superseded `token_usage`
    /// table).
    fn seed_fixture(db: &ActivityDb) {
        let day1 = DateTime::parse_from_rfc3339("2026-09-10T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let day2 = DateTime::parse_from_rfc3339("2026-09-11T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let too_old = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        seed(db, "builder", "claude-sonnet-5", day1, 1000, 200, 0.01, Some("rjwalters/loom"));
        seed(db, "builder", "claude-opus-5", day1, 500, 100, 0.02, Some("rjwalters/loom"));
        seed(db, "judge", "claude-sonnet-5", day2, 300, 50, 0.005, Some("rjwalters/other"));
        // No repo context (e.g. the legacy IPC writer) -> falls into `unknown`.
        seed(db, "judge", "claude-sonnet-5", day2, 100, 10, 0.001, None);
        // Outside the `--since` window used by most tests below.
        seed(
            db,
            "builder",
            "claude-sonnet-5",
            too_old,
            9999,
            9999,
            9.99,
            Some("rjwalters/loom"),
        );
    }

    fn since_2026_09_01() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn groups_by_role() {
        let (_tmp, db) = open_db();
        seed_fixture(&db);

        let rows =
            get_usage_report(&db.conn, since_2026_09_01(), UsageReportGroupBy::Role).unwrap();
        // Ordered by cost descending: builder ($0.03) before judge ($0.006).
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].group, "builder");
        assert_eq!(rows[0].request_count, 2);
        assert_eq!(rows[0].tokens_input, 1500);
        assert_eq!(rows[0].tokens_output, 300);
        assert!((rows[0].cost_usd - 0.03).abs() < 1e-9);

        assert_eq!(rows[1].group, "judge");
        assert_eq!(rows[1].request_count, 2);
        assert!((rows[1].cost_usd - 0.006).abs() < 1e-9);
    }

    #[test]
    fn groups_by_model() {
        let (_tmp, db) = open_db();
        seed_fixture(&db);

        let rows =
            get_usage_report(&db.conn, since_2026_09_01(), UsageReportGroupBy::Model).unwrap();
        let sonnet = rows.iter().find(|r| r.group == "claude-sonnet-5").unwrap();
        assert_eq!(sonnet.request_count, 3);
        let opus = rows.iter().find(|r| r.group == "claude-opus-5").unwrap();
        assert_eq!(opus.request_count, 1);
        assert!((opus.cost_usd - 0.02).abs() < 1e-9);
    }

    #[test]
    fn groups_by_repo_with_unknown_bucket() {
        let (_tmp, db) = open_db();
        seed_fixture(&db);

        let rows =
            get_usage_report(&db.conn, since_2026_09_01(), UsageReportGroupBy::Repo).unwrap();
        let groups: Vec<&str> = rows.iter().map(|r| r.group.as_str()).collect();
        assert!(groups.contains(&"rjwalters/loom"));
        assert!(groups.contains(&"rjwalters/other"));
        assert!(groups.contains(&"unknown"), "the repo-less row must not be dropped");

        let unknown = rows.iter().find(|r| r.group == "unknown").unwrap();
        assert_eq!(unknown.request_count, 1);
    }

    #[test]
    fn groups_by_day() {
        let (_tmp, db) = open_db();
        seed_fixture(&db);

        let rows = get_usage_report(&db.conn, since_2026_09_01(), UsageReportGroupBy::Day).unwrap();
        let groups: Vec<&str> = rows.iter().map(|r| r.group.as_str()).collect();
        assert!(groups.contains(&"2026-09-10"));
        assert!(groups.contains(&"2026-09-11"));
        assert!(!groups.contains(&"2026-01-01"), "outside the since window");
    }

    #[test]
    fn since_window_excludes_older_rows() {
        let (_tmp, db) = open_db();
        seed_fixture(&db);

        let rows =
            get_usage_report(&db.conn, since_2026_09_01(), UsageReportGroupBy::Role).unwrap();
        let total_input: i64 = rows.iter().map(|r| r.tokens_input).sum();
        assert_eq!(total_input, 1900, "the 9999-token too_old row must not be counted");
    }

    #[test]
    fn zero_matching_rows_yields_an_empty_vec_not_a_misleading_zero_row() {
        let (_tmp, db) = open_db();
        seed_fixture(&db);

        let future = DateTime::parse_from_rfc3339("2099-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let rows = get_usage_report(&db.conn, future, UsageReportGroupBy::Role).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn tokens_total_sums_all_four_counters() {
        let row = UsageReportRow {
            group: "builder".to_string(),
            request_count: 1,
            tokens_input: 10,
            tokens_output: 20,
            tokens_cache_read: 30,
            tokens_cache_write: 40,
            cost_usd: 0.1,
        };
        assert_eq!(row.tokens_total(), 100);
    }

    #[test]
    fn parse_accepts_every_dimension_case_insensitively() {
        assert_eq!(UsageReportGroupBy::parse("role").unwrap(), UsageReportGroupBy::Role);
        assert_eq!(UsageReportGroupBy::parse("MODEL").unwrap(), UsageReportGroupBy::Model);
        assert_eq!(UsageReportGroupBy::parse("Repo").unwrap(), UsageReportGroupBy::Repo);
        assert_eq!(UsageReportGroupBy::parse("day").unwrap(), UsageReportGroupBy::Day);
        assert!(UsageReportGroupBy::parse("week").is_err());
    }
}
