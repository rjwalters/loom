//! `loom-daemon ingest-transcripts` (Issue #8059) — read Claude Code
//! transcripts and persist their token usage into `~/.loom/activity.db`'s
//! `resource_usage` table, the dispatch-path writer those tables never had.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use chrono::{DateTime, Duration, Utc};

use loom_daemon::activity::transcript_ingest::{ingest, IngestOptions, IngestStats};
use loom_daemon::activity::ActivityDb;
use loom_daemon::transcript_tokens::claude_projects_dir;

/// Parse a `--since` value: `7d`, `12h`, `90m`, `all`, or an RFC-3339 instant.
///
/// # Errors
///
/// Returns an error for a value matching none of those forms.
pub fn parse_since(raw: &str) -> Result<Option<DateTime<Utc>>> {
    let raw = raw.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("all") {
        return Ok(None);
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return Ok(Some(dt.with_timezone(&Utc)));
    }
    let (value, unit) = raw.split_at(raw.len() - 1);
    let amount: i64 = value.parse().map_err(|_| {
        anyhow!("invalid --since value '{raw}' (try 7d, 12h, 90m, all, or RFC 3339)")
    })?;
    let delta = match unit {
        "d" => Duration::days(amount),
        "h" => Duration::hours(amount),
        "m" => Duration::minutes(amount),
        _ => return Err(anyhow!("invalid --since unit in '{raw}' (expected d, h or m)")),
    };
    Ok(Some(Utc::now() - delta))
}

/// Run one ingestion pass and print what it did.
///
/// # Errors
///
/// Fails when the activity database cannot be opened or a write fails.
pub fn handle_ingest_transcripts_command(
    since: Option<&str>,
    projects_dir: Option<&str>,
    workspace: Option<&str>,
    db: Option<&str>,
    force: bool,
    dry_run: bool,
    format: &str,
) -> Result<()> {
    let db_path = match db {
        Some(path) => PathBuf::from(path),
        None => dirs::home_dir()
            .ok_or_else(|| anyhow!("No home directory"))?
            .join(".loom")
            .join("activity.db"),
    };

    let projects = match projects_dir {
        Some(path) => PathBuf::from(path),
        None => claude_projects_dir().ok_or_else(|| {
            anyhow!("Could not resolve the Claude projects directory (set CLAUDE_CONFIG_DIR or --projects-dir)")
        })?,
    };
    if !projects.is_dir() {
        return Err(anyhow!("No Claude transcripts directory at {}", projects.display()));
    }

    let opts = IngestOptions {
        projects_dir: projects,
        workspace: workspace.map(PathBuf::from),
        since: since.map(parse_since).transpose()?.flatten(),
        force,
        dry_run,
        ..IngestOptions::default()
    };

    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let activity = ActivityDb::new(db_path.clone())?;
    let stats = ingest(&activity, &opts)?;

    if format == "json" {
        println!("{}", serde_json::to_string_pretty(&stats)?);
    } else {
        print_stats(&db_path, &opts, &stats);
    }
    Ok(())
}

fn print_stats(db_path: &std::path::Path, opts: &IngestOptions, stats: &IngestStats) {
    let mode = if opts.dry_run { " (dry run)" } else { "" };
    println!("\n=== Transcript token ingestion{mode} ===\n");
    println!("Transcripts:   {} seen", stats.transcripts_seen);
    println!(
        "               {} ingested, {} unchanged, {} outside window, {} without usage, {} oversized",
        stats.transcripts_ingested,
        stats.skipped_unchanged,
        stats.skipped_by_window,
        stats.transcripts_without_usage,
        stats.skipped_oversize
    );
    println!(
        "Usage records: {} kept, {} duplicate chunks collapsed, {} synthetic skipped",
        stats.usage_records, stats.duplicate_records, stats.synthetic_skipped
    );
    println!("Rows written:  {} (resource_usage)", stats.rows_written);
    println!(
        "Tokens:        input {}, output {}, cache read {}, cache write {}",
        stats.tokens_input, stats.tokens_output, stats.tokens_cache_read, stats.tokens_cache_write
    );
    // List-price proxy from the table in `activity::resource_usage`, which
    // #8060 reports as one to two generations stale for several models.
    println!("Cost proxy:    ${:.4} (list-price estimate)", stats.cost_usd);
    println!("Database:      {}\n", db_path.display());
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn since_accepts_relative_units_and_all() {
        assert!(parse_since("all").unwrap().is_none());
        assert!(parse_since("").unwrap().is_none());

        let day = parse_since("7d").unwrap().unwrap();
        let expected = Utc::now() - Duration::days(7);
        assert!((day - expected).num_seconds().abs() < 5);

        let hour = parse_since("12h").unwrap().unwrap();
        assert!(
            (hour - (Utc::now() - Duration::hours(12)))
                .num_seconds()
                .abs()
                < 5
        );

        let minute = parse_since("90m").unwrap().unwrap();
        assert!(
            (minute - (Utc::now() - Duration::minutes(90)))
                .num_seconds()
                .abs()
                < 5
        );
    }

    #[test]
    fn since_accepts_an_absolute_instant() {
        let at = parse_since("2026-09-01T00:00:00Z").unwrap().unwrap();
        assert_eq!(at.to_rfc3339(), "2026-09-01T00:00:00+00:00");
    }

    #[test]
    fn since_rejects_nonsense() {
        assert!(parse_since("7 days").is_err());
        assert!(parse_since("7y").is_err());
        assert!(parse_since("yesterday").is_err());
    }
}
