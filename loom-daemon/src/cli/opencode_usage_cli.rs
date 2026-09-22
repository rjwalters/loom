//! `loom-daemon opencode-usage` (Issue #8507) — per-model token usage read
//! back out of OpenCode's own session store, for a directory and a window.
//!
//! # Why this exists
//!
//! Two jobs, both named in #8507's acceptance criteria:
//!
//! 1. **Backfill.** Completions narrated during the 2026-09-20/21 GLM-5.3
//!    trial are already downstream with no token rows, because the daemon had
//!    no OpenCode reader at the time. The session databases on the trial hosts
//!    still hold the numbers, and this is the path that gets them out — a
//!    human-readable table, or `--json` to feed a reconciliation job.
//! 2. **Verification.** It is the same
//!    [`loom_daemon::opencode_usage::tokens_by_model`] the sweep and role-tick
//!    journals call, so running it against a worktree after an OpenCode sweep
//!    shows exactly what that sweep's `tokens_by_model` will carry.
//!
//! Read-only in the strongest sense available: the underlying reader opens the
//! database `?mode=ro` with `SQLITE_OPEN_READ_ONLY` and issues exactly one
//! query, naming `session` alone — never the `credential`/`account` tables
//! that live in the same file. This command adds no query of its own.

use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use loom_daemon::opencode_usage::{self, OpencodeSessionUsage};

/// `loom-daemon opencode-usage` arguments.
#[derive(clap::Args)]
pub(crate) struct OpencodeUsageArgs {
    /// Working directory to attribute, matched exactly against
    /// `session.directory`. Repeatable. Defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    pub directory: Vec<PathBuf>,

    /// Also attribute this issue's worktree under the (single) `--directory`
    /// workspace root — the same two-directory set a sweep's own telemetry
    /// uses.
    #[arg(long, value_name = "N")]
    pub issue: Option<u32>,

    /// Only include sessions created since this point: `7d`, `36h`, `90m`,
    /// `2w`, or an absolute `YYYY-MM-DD` (00:00 UTC).
    #[arg(long, value_name = "SPEC", default_value = "30d")]
    pub since: String,

    /// Only include sessions created before this RFC3339 instant (default:
    /// now).
    #[arg(long, value_name = "RFC3339")]
    pub until: Option<String>,

    /// Ignore the window entirely and report every attributable session.
    #[arg(long, conflicts_with_all = ["since", "until"])]
    pub all_time: bool,

    /// List each contributing session, not just the per-model totals.
    #[arg(long)]
    pub sessions: bool,

    /// Emit machine-readable JSON instead of the human-readable table.
    #[arg(long)]
    pub json: bool,
}

impl OpencodeUsageArgs {
    /// # Errors
    /// Propagates an invalid `--since`/`--until`, or a current-directory
    /// lookup failure when no `--directory` was given.
    pub(crate) fn run(self) -> Result<()> {
        let window = self.window()?;
        let directories = self.directories()?;
        let dbs = opencode_usage::discover_opencode_dbs(None);
        let sessions = opencode_usage::sessions(&directories, window, None);
        let totals = opencode_usage::fold_sessions(sessions.clone()).unwrap_or_default();

        if self.json {
            println!(
                "{}",
                serde_json::json!({
                    "databases": dbs,
                    "directories": directories,
                    "window": window.map(|(s, e)| serde_json::json!({
                        "since": s.to_rfc3339(),
                        "until": e.to_rfc3339(),
                    })),
                    "tokens_by_model": totals,
                    "sessions": sessions.iter().map(session_json).collect::<Vec<_>>(),
                })
            );
            return Ok(());
        }

        if dbs.is_empty() {
            println!("No opencode.db found under ~/.loom/opt (set LOOM_OPENCODE_DB to pin one).");
            return Ok(());
        }
        println!("Session stores:");
        for db in &dbs {
            println!("  {}", db.display());
        }
        println!("Attributed directories:");
        for dir in &directories {
            println!("  {}", dir.display());
        }
        match window {
            Some((since, until)) => println!("Window: {since} .. {until}"),
            None => println!("Window: all time"),
        }
        if totals.is_empty() {
            println!("\nNo attributable session usage found.");
            return Ok(());
        }
        println!("\n{:<34} {:>14} {:>14} {:>14}", "MODEL", "INPUT", "OUTPUT", "CACHE READ");
        for row in &totals {
            println!(
                "{:<34} {:>14} {:>14} {:>14}",
                row.model, row.input, row.output, row.cache_read
            );
        }
        if self.sessions {
            println!("\n{:<26} {:<34} {:<16} {:>12}", "CREATED", "MODEL", "PROVIDER", "INPUT");
            for s in &sessions {
                println!(
                    "{:<26} {:<34} {:<16} {:>12}",
                    s.created_at.to_rfc3339(),
                    s.model,
                    s.provider.as_deref().unwrap_or("-"),
                    s.input
                );
            }
        }
        Ok(())
    }

    /// `None` under `--all-time`; otherwise the resolved `[since, until]`.
    fn window(&self) -> Result<Option<(DateTime<Utc>, DateTime<Utc>)>> {
        if self.all_time {
            return Ok(None);
        }
        let now = Utc::now();
        let since = loom_daemon::sweep_outcome_summary::parse_since(&self.since, now)?;
        let until = match &self.until {
            Some(raw) => DateTime::parse_from_rfc3339(raw)
                .with_context(|| format!("--until {raw:?} is not RFC3339"))?
                .with_timezone(&Utc),
            None => now,
        };
        Ok(Some((since, until)))
    }

    /// The exact directory set to attribute.
    fn directories(&self) -> Result<Vec<PathBuf>> {
        let roots = if self.directory.is_empty() {
            vec![std::env::current_dir().context("resolving the current directory")?]
        } else {
            self.directory.clone()
        };
        let Some(issue) = self.issue else {
            return Ok(roots);
        };
        anyhow::ensure!(
            roots.len() == 1,
            "--issue names one workspace root's worktree; pass exactly one --directory with it"
        );
        Ok(loom_daemon::usage_source::sweep_directories(&roots[0], issue))
    }
}

/// One session row's JSON form. Spelled out rather than derived so the wire
/// shape is reviewable here and cannot silently grow a field the reader adds.
fn session_json(s: &OpencodeSessionUsage) -> serde_json::Value {
    serde_json::json!({
        "created_at": s.created_at.to_rfc3339(),
        "directory": s.directory,
        "model": s.model,
        "provider": s.provider,
        "input": s.input,
        "output": s.output,
        "reasoning": s.reasoning,
        "cache_read": s.cache_read,
        "cache_write": s.cache_write,
    })
}
