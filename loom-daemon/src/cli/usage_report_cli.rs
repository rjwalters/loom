//! `loom-daemon usage-report` (Issue #8062) — token/cost breakdown by role,
//! model, repo, or day, read from the `resource_usage` table #8059's
//! transcript ingestion (and, historically, the managed-terminal IPC path)
//! populates.
//!
//! Reached through [`super::telemetry::TelemetryCommand`], which is
//! flattened (see that module's doc comment), so this is a top-level
//! subcommand with no `telemetry` prefix: `loom-daemon usage-report`, not
//! `loom-daemon usage report`. `usage` itself stays a leaf command (its
//! `--status` flag, unchanged) rather than growing a `report` sub-subcommand,
//! so `check-usage.sh`'s existing `loom-daemon usage [--status]` contract is
//! never at risk of a parse ambiguity.
//!
//! Data source: exclusively the already-ingested `resource_usage` /
//! `agent_inputs` tables (via [`loom_daemon::activity::ActivityDb::get_usage_report`]).
//! Nothing here re-parses a transcript or re-derives a price — see
//! [`loom_daemon::activity::usage_report`]'s module doc (re-exported here as
//! `UsageReportGroupBy`/`UsageReportRow`).

use std::path::PathBuf;

use anyhow::Result;
use chrono::Utc;

use loom_daemon::activity::{ActivityDb, UsageReportGroupBy, UsageReportRow};

/// `loom-daemon usage-report` arguments.
#[derive(clap::Args)]
pub(crate) struct UsageReportArgs {
    /// Only include usage since this point: `7d`, `36h`, `90m`, `2w`, or an
    /// absolute `YYYY-MM-DD` (00:00 UTC).
    #[arg(long, value_name = "SPEC", default_value = "7d")]
    pub since: String,

    /// Grouping dimension: role, model, repo, day.
    #[arg(long, value_name = "DIM", default_value = "role")]
    pub by: String,

    /// Activity database path (default: `~/.loom/activity.db`, or
    /// `LOOM_ACTIVITY_DB` if set — same override `loom-daemon stats
    /// agent-metrics` honors).
    #[arg(long, value_name = "PATH")]
    pub db: Option<PathBuf>,

    /// Emit machine-readable JSON instead of the human-readable table.
    #[arg(long)]
    pub json: bool,
}

impl UsageReportArgs {
    /// # Errors
    /// Propagates an invalid `--since`/`--by` value or a database failure.
    pub(crate) fn run(self) -> Result<()> {
        let group_by = UsageReportGroupBy::parse(&self.by)?;
        let since = loom_daemon::sweep_outcome_summary::parse_since(&self.since, Utc::now())?;
        let db_path = self.db.unwrap_or_else(default_activity_db_path);

        if !db_path.is_file() {
            if self.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "error": "activity database not found",
                        "dbPath": db_path.display().to_string(),
                    })
                );
            } else {
                println!(
                    "No activity database found at {}. Run `loom-daemon ingest-transcripts` \
                     (or enable transcript ingestion) first.",
                    db_path.display()
                );
            }
            std::process::exit(1);
        }

        let db = ActivityDb::new(db_path.clone())?;
        let rows = db.get_usage_report(since, group_by)?;

        if self.json {
            println!("{}", serde_json::to_string_pretty(&rows)?);
        } else {
            print_report(&db_path, &self.since, group_by, &rows);
        }
        Ok(())
    }
}

/// `~/.loom/activity.db`, or `LOOM_ACTIVITY_DB` if set — matches
/// [`crate::cli::stats::handle_agent_metrics_command`]'s resolution.
fn default_activity_db_path() -> PathBuf {
    std::env::var_os("LOOM_ACTIVITY_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".loom")
                .join("activity.db")
        })
}

fn print_report(
    db_path: &std::path::Path,
    since_spec: &str,
    group_by: UsageReportGroupBy,
    rows: &[UsageReportRow],
) {
    println!("\n=== Usage report (by {}, since {since_spec}) ===\n", group_by.as_str());

    if rows.is_empty() {
        println!(
            "No usage data found since {since_spec} in {}.\n\
             (Run `loom-daemon ingest-transcripts` to populate resource_usage from transcripts.)",
            db_path.display()
        );
        return;
    }

    let group_width = rows
        .iter()
        .map(|r| r.group.len())
        .max()
        .unwrap_or(5)
        .max(group_by.as_str().len());

    println!(
        "{:<group_width$} {:>8} {:>12} {:>12} {:>10} {:>10} {:>12}",
        group_by.as_str().to_uppercase(),
        "REQUESTS",
        "TOKENS_IN",
        "TOKENS_OUT",
        "CACHE_R",
        "CACHE_W",
        "COST_USD",
        group_width = group_width
    );
    println!("{:-<width$}", "", width = group_width + 71);

    let mut total = UsageReportRow {
        group: "TOTAL".to_string(),
        request_count: 0,
        tokens_input: 0,
        tokens_output: 0,
        tokens_cache_read: 0,
        tokens_cache_write: 0,
        cost_usd: 0.0,
    };

    for row in rows {
        println!(
            "{:<group_width$} {:>8} {:>12} {:>12} {:>10} {:>10} {:>12.4}",
            row.group,
            row.request_count,
            row.tokens_input,
            row.tokens_output,
            row.tokens_cache_read,
            row.tokens_cache_write,
            row.cost_usd,
            group_width = group_width
        );
        total.request_count += row.request_count;
        total.tokens_input += row.tokens_input;
        total.tokens_output += row.tokens_output;
        total.tokens_cache_read += row.tokens_cache_read;
        total.tokens_cache_write += row.tokens_cache_write;
        total.cost_usd += row.cost_usd;
    }

    println!("{:-<width$}", "", width = group_width + 71);
    println!(
        "{:<group_width$} {:>8} {:>12} {:>12} {:>10} {:>10} {:>12.4}",
        total.group,
        total.request_count,
        total.tokens_input,
        total.tokens_output,
        total.tokens_cache_read,
        total.tokens_cache_write,
        total.cost_usd,
        group_width = group_width
    );
    println!("\nDatabase: {}\n", db_path.display());
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> crate::Commands {
        crate::Cli::try_parse_from(args)
            .expect("parse")
            .command
            .expect("a subcommand")
    }

    #[test]
    fn usage_report_parses_with_defaults() {
        match parse(&["loom-daemon", "usage-report"]) {
            crate::Commands::Telemetry(super::super::telemetry::TelemetryCommand::UsageReport(
                a,
            )) => {
                assert_eq!(a.since, "7d");
                assert_eq!(a.by, "role");
                assert!(!a.json);
                assert!(a.db.is_none());
            }
            _ => panic!("usage-report did not dispatch through the flattened Telemetry variant"),
        }
    }

    #[test]
    fn usage_report_parses_every_flag() {
        match parse(&[
            "loom-daemon",
            "usage-report",
            "--since",
            "30d",
            "--by",
            "model",
            "--json",
        ]) {
            crate::Commands::Telemetry(super::super::telemetry::TelemetryCommand::UsageReport(
                a,
            )) => {
                assert_eq!(a.since, "30d");
                assert_eq!(a.by, "model");
                assert!(a.json);
            }
            _ => panic!("usage-report --since/--by/--json did not parse"),
        }
    }

    #[test]
    fn invalid_by_value_is_rejected_at_run_time() {
        let err = UsageReportGroupBy::parse("bogus").unwrap_err();
        assert!(err.to_string().contains("unknown --by value"));
    }

    #[test]
    fn default_db_path_honors_the_env_override() {
        let dir = tempfile::tempdir().unwrap();
        let want = dir.path().join("custom-activity.db");
        std::env::set_var("LOOM_ACTIVITY_DB", &want);
        assert_eq!(default_activity_db_path(), want);
        std::env::remove_var("LOOM_ACTIVITY_DB");
    }
}
