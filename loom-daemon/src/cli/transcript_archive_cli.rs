//! `loom-daemon archive-transcripts` (Issue #8494) — roll raw Claude Code
//! transcripts into a verified, incremental `.tar.zst` archive before
//! Claude Code's `cleanupPeriodDays` fuse deletes them.

use std::path::PathBuf;

use anyhow::{anyhow, Result};

use loom_daemon::activity::transcript_archive::{
    archive, default_archive_dir, ArchiveOptions, ArchiveStats, DEFAULT_ZSTD_LEVEL,
};
use loom_daemon::activity::ActivityDb;
use loom_daemon::transcript_tokens::claude_projects_dir;

/// `loom-daemon archive-transcripts` — reached through
/// [`super::telemetry::TelemetryCommand`], which is flattened, so this is a
/// top-level subcommand with no `telemetry` prefix (same reasoning as
/// `IngestTranscriptsArgs`, whose module doc explains why).
#[derive(clap::Args)]
pub(crate) struct ArchiveTranscriptsArgs {
    /// Claude projects directory (default: `${CLAUDE_CONFIG_DIR:-~/.claude}/projects`).
    #[arg(long = "projects-dir")]
    pub projects_dir: Option<String>,

    /// Restrict to one workspace's transcripts (default: every project).
    #[arg(long)]
    pub workspace: Option<String>,

    /// Where the `.tar.zst` + `.manifest.json` pair is written
    /// (default: `~/.loom/transcript-archives`).
    #[arg(long = "archive-dir")]
    pub archive_dir: Option<String>,

    /// Activity database path, which holds the archive ledger
    /// (default: `~/.loom/activity.db`).
    #[arg(long)]
    pub db: Option<String>,

    /// Skip transcripts modified more recently than this many hours ago — a
    /// still-growing (actively in-use) session is left for a later run.
    #[arg(long = "min-age-hours", default_value_t = 24)]
    pub min_age_hours: i64,

    /// Zstd compression level (1-22). Higher is smaller but slower.
    #[arg(long, default_value_t = DEFAULT_ZSTD_LEVEL)]
    pub level: i32,

    /// Re-archive transcripts the ledger records as already archived.
    #[arg(long)]
    pub force: bool,

    /// Report what would be archived without writing anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Output format: table (default), json
    #[arg(long, default_value = "table")]
    pub format: String,
}

impl ArchiveTranscriptsArgs {
    /// Run one archive pass and print what it did.
    ///
    /// # Errors
    ///
    /// Fails when the activity database cannot be opened, the transcripts
    /// directory does not exist, or archive writing/verification fails.
    pub(crate) fn run(self) -> Result<()> {
        let db_path = match self.db {
            Some(path) => PathBuf::from(path),
            None => dirs::home_dir()
                .ok_or_else(|| anyhow!("No home directory"))?
                .join(".loom")
                .join("activity.db"),
        };

        let projects_dir = match self.projects_dir {
            Some(path) => PathBuf::from(path),
            None => claude_projects_dir().ok_or_else(|| {
                anyhow!("Could not resolve the Claude projects directory (set CLAUDE_CONFIG_DIR or --projects-dir)")
            })?,
        };
        if !projects_dir.is_dir() {
            return Err(anyhow!("No Claude transcripts directory at {}", projects_dir.display()));
        }

        let opts = ArchiveOptions {
            projects_dir,
            workspace: self.workspace.map(PathBuf::from),
            archive_dir: self
                .archive_dir
                .map_or_else(default_archive_dir, PathBuf::from),
            min_age_hours: self.min_age_hours,
            force: self.force,
            dry_run: self.dry_run,
            zstd_level: self.level,
            ..ArchiveOptions::default()
        };

        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let activity = ActivityDb::new(db_path.clone())?;
        let stats = archive(&activity, &opts)?;

        if self.format == "json" {
            println!("{}", serde_json::to_string_pretty(&stats)?);
        } else {
            print_stats(&db_path, &opts, &stats);
        }
        Ok(())
    }
}

fn print_stats(db_path: &std::path::Path, opts: &ArchiveOptions, stats: &ArchiveStats) {
    let mode = if opts.dry_run { " (dry run)" } else { "" };
    println!("\n=== Transcript archive{mode} ===\n");
    println!("Transcripts:   {} seen", stats.transcripts_seen);
    println!(
        "               {} archived, {} already archived, {} too recent, {} oversized",
        stats.archived,
        stats.skipped_already_archived,
        stats.skipped_too_recent,
        stats.skipped_oversize
    );
    println!(
        "Bytes:         {} raw, {} compressed",
        format_bytes(stats.bytes_raw),
        format_bytes(stats.bytes_compressed)
    );
    if let Some(path) = &stats.archive_path {
        println!("Archive:       {path}");
    }
    if let Some(path) = &stats.manifest_path {
        println!("Manifest:      {path}");
    }
    println!("Database:      {}\n", db_path.display());
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn format_bytes_scales_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.00 KiB");
        assert_eq!(format_bytes(10 * 1024 * 1024), "10.00 MiB");
    }
}
