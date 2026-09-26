//! `loom-daemon pi-usage` (Issue #8594, the Pi half) — per-model token usage
//! read back out of the Pi `--mode json` event stream a launch's own log
//! captured.
//!
//! # Why this exists
//!
//! The Pi sibling of `opencode-usage` / `codex-usage`, with the same two jobs:
//!
//! 1. **Backfill.** A Pi sweep or role tick that ran before this reader landed
//!    published labels but no `tokens_by_model`. Its log still holds the
//!    stream, so this gets the numbers out — a table, or `--json`.
//! 2. **Verification.** It calls the same [`loom_daemon::pi_usage`] functions
//!    the sweep and role-tick journals do, against the same log, so running it
//!    after a Pi sweep shows exactly what that sweep's `tokens_by_model`
//!    carries — how #8594's "matches the numbers in Pi's own stream for that
//!    launch" criterion is checked by hand.
//!
//! Read-only: the reader opens nothing but a `.loom/logs/sweep-issue-<N>.log`
//! or `role-<role>.log` (its single open is gated on
//! [`loom_daemon::pi_usage::is_launch_log_path`]), so Pi's agent directory and
//! its auth store are unreachable from here. This command adds no read of its
//! own.
//!
//! `--messages` lists each contributing assistant message with the **exact**
//! Pi session id it belonged to (the stream's `session` header) — the precise
//! attribution #8507's design note deferred.

use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use loom_daemon::pi_usage::{self, PiMessageUsage};

/// `loom-daemon pi-usage` arguments.
#[derive(clap::Args)]
pub(crate) struct PiUsageArgs {
    /// Workspace root whose `.loom/logs/` holds the launch log. Defaults to
    /// the current directory.
    #[arg(long, value_name = "PATH")]
    pub directory: Option<PathBuf>,

    /// Read this issue's sweep log (`sweep-issue-<N>.log`) — the log a sweep's
    /// own `tokens_by_model` is read from.
    #[arg(
        long,
        value_name = "N",
        conflicts_with = "role",
        required_unless_present = "role"
    )]
    pub issue: Option<u32>,

    /// Read this role's tick log (`role-<ROLE>.log`) instead.
    #[arg(long, value_name = "ROLE")]
    pub role: Option<String>,

    /// Only include messages since this point: `7d`, `36h`, `90m`, `2w`, or an
    /// absolute `YYYY-MM-DD` (00:00 UTC).
    #[arg(long, value_name = "SPEC", default_value = "30d")]
    pub since: String,

    /// Only include messages before this RFC3339 instant (default: now).
    #[arg(long, value_name = "RFC3339")]
    pub until: Option<String>,

    /// Ignore the window entirely and report every message in the log.
    #[arg(long, conflicts_with_all = ["since", "until"])]
    pub all_time: bool,

    /// List each contributing assistant message, not just per-model totals.
    #[arg(long)]
    pub messages: bool,

    /// Emit machine-readable JSON instead of the human-readable table.
    #[arg(long)]
    pub json: bool,
}

impl PiUsageArgs {
    /// # Errors
    /// Propagates an invalid `--since`/`--until`, or a current-directory
    /// lookup failure when `--directory` was not given.
    pub(crate) fn run(self) -> Result<()> {
        let window = self.window()?;
        let log = self.log_path()?;
        let messages = pi_usage::messages_in_log(&log, window);
        let readable = messages.is_some();
        let messages = messages.unwrap_or_default();
        let totals = pi_usage::fold_messages(messages.clone()).unwrap_or_default();

        if self.json {
            println!(
                "{}",
                serde_json::json!({
                    "log": log,
                    "log_readable": readable,
                    "window": window.map(|(s, e)| serde_json::json!({
                        "since": s.to_rfc3339(),
                        "until": e.to_rfc3339(),
                    })),
                    "tokens_by_model": totals,
                    "messages": messages.iter().map(message_json).collect::<Vec<_>>(),
                })
            );
            return Ok(());
        }

        println!("Launch log: {}", log.display());
        if !readable {
            println!("\nLog not found or not readable.");
            return Ok(());
        }
        match window {
            Some((since, until)) => println!("Window: {since} .. {until}"),
            None => println!("Window: all time"),
        }
        if totals.is_empty() {
            println!("\nNo attributable Pi usage found.");
            return Ok(());
        }
        println!(
            "\n{:<34} {:>12} {:>12} {:>12} {:>12}",
            "MODEL", "INPUT", "OUTPUT", "CACHE READ", "CACHE WRITE"
        );
        for row in &totals {
            println!(
                "{:<34} {:>12} {:>12} {:>12} {:>12}",
                row.model,
                row.input,
                row.output,
                row.cache_read,
                row.cache_write_5m + row.cache_write_1h
            );
        }
        if self.messages {
            println!(
                "\n{:<30} {:<24} {:<38} {:>10} {:>10}",
                "AT", "MODEL", "SESSION", "INPUT", "OUTPUT"
            );
            for m in &messages {
                println!(
                    "{:<30} {:<24} {:<38} {:>10} {:>10}",
                    m.at.to_rfc3339(),
                    m.model,
                    m.session_id.as_deref().unwrap_or("-"),
                    m.input,
                    m.output
                );
            }
        }
        Ok(())
    }

    fn log_path(&self) -> Result<PathBuf> {
        let root = match &self.directory {
            Some(dir) => dir.clone(),
            None => std::env::current_dir().context("resolving the current directory")?,
        };
        Ok(match (&self.role, self.issue) {
            (Some(role), _) => {
                loom_daemon::role_runner::role_log_path(&root.join(".loom").join("logs"), role)
            }
            (None, Some(issue)) => loom_daemon::launch_record::sweep_log_path(&root, issue),
            (None, None) => anyhow::bail!("pass --issue <N> or --role <ROLE>"),
        })
    }

    /// Identical to `opencode-usage`'s and `codex-usage`'s window resolution,
    /// so one window has one spelling across every runtime.
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
}

/// One message row's JSON form. Spelled out rather than derived so the wire
/// shape is reviewable here and cannot silently grow a field.
fn message_json(m: &PiMessageUsage) -> serde_json::Value {
    serde_json::json!({
        "at": m.at.to_rfc3339(),
        "session_id": m.session_id,
        "cwd": m.cwd,
        "model": m.model,
        "provider": m.provider,
        "input": m.input,
        "output": m.output,
        "reasoning": m.reasoning,
        "cache_read": m.cache_read,
        "cache_write": m.cache_write,
        "cache_write_1h": m.cache_write_1h,
    })
}
