//! `loom-daemon codex-usage` (Issue #8594) — per-model token usage read back
//! out of Codex's own rollout session store, for a directory and a window.
//!
//! # Why this exists
//!
//! The Codex sibling of `loom-daemon opencode-usage` (#8507), giving the new
//! reader the same operator-runnable path its OpenCode counterpart has:
//!
//! 1. **Backfill.** Every sweep and role tick dispatched on Codex before this
//!    reader landed is already downstream with no token rows, because the
//!    daemon had no Codex reader at the time. `$CODEX_HOME/sessions/` still
//!    holds the numbers, and this is the path that gets them out — a
//!    human-readable table, or `--json` to feed a reconciliation job.
//! 2. **Verification.** It is the same
//!    [`loom_daemon::codex_usage::tokens_by_model`] the sweep and role-tick
//!    journals call, so running it against a worktree after a Codex sweep
//!    shows exactly what that sweep's `tokens_by_model` will carry. That is
//!    how #8594's "matches the numbers in Codex's own store for that launch"
//!    acceptance criterion is checked by hand.
//!
//! Read-only in the strongest sense the store allows: the underlying reader's
//! whole file-open surface is one guarded function that refuses any path
//! outside `sessions/**/rollout-*.jsonl`, so `$CODEX_HOME`'s `auth.json`,
//! `config.toml`, `history.jsonl` and SQLite stores are unreachable from here.
//! This command adds no file read of its own.
//!
//! It scans the same home set the daemon does
//! ([`loom_daemon::codex_usage::codex_homes`]): the ambient `$CODEX_HOME` (or
//! `~/.codex`) **and** every pooled profile under `~/.loom/codex-profiles/`,
//! which is where a pool-selected sweep's rollouts actually land. Set
//! `LOOM_CODEX_HOME` to pin exactly one home instead.
//!
//! # `--session-id`, the exact-attribution path
//!
//! Unlike OpenCode's store, Codex's rollouts carry the session id, so
//! `--session-id` attributes an exact session set rather than a
//! directory+window approximation (#8507's deferred design note). Nothing in
//! the daemon captures a launch's Codex session ids yet, so this is an
//! operator-facing affordance today: given a rollout filename's uuid, it
//! reports that session alone.

use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use loom_daemon::codex_usage::{self, CodexSessionUsage, SessionFilter};

/// `loom-daemon codex-usage` arguments.
#[derive(clap::Args)]
pub(crate) struct CodexUsageArgs {
    /// Working directory to attribute, matched exactly against the rollout's
    /// `session_meta.cwd`. Repeatable. Defaults to the current directory
    /// unless `--session-id` is given.
    #[arg(long, value_name = "PATH")]
    pub directory: Vec<PathBuf>,

    /// Also attribute this issue's worktree under the (single) `--directory`
    /// workspace root — the same two-directory set a sweep's own telemetry
    /// uses.
    #[arg(long, value_name = "N")]
    pub issue: Option<u32>,

    /// Attribute this exact `session_meta` id (the rollout filename's uuid).
    /// Repeatable. Narrows a `--directory` set, or stands alone as the whole
    /// key.
    #[arg(long, value_name = "UUID")]
    pub session_id: Vec<String>,

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

impl CodexUsageArgs {
    /// # Errors
    /// Propagates an invalid `--since`/`--until`, or a current-directory
    /// lookup failure when neither `--directory` nor `--session-id` was given.
    pub(crate) fn run(self) -> Result<()> {
        let window = self.window()?;
        let filter = self.filter()?;
        let homes = codex_usage::codex_homes(None);
        let rollouts = codex_usage::discover_rollouts(None, window);
        let sessions = codex_usage::sessions(&filter, window, None);
        let totals = codex_usage::fold_sessions(sessions.clone()).unwrap_or_default();

        if self.json {
            println!(
                "{}",
                serde_json::json!({
                    "codex_homes": homes,
                    "rollouts_scanned": rollouts.len(),
                    "directories": filter.directories,
                    "session_ids": filter.ids,
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

        if homes.is_empty() {
            println!("No $CODEX_HOME resolved (set LOOM_CODEX_HOME to pin one).");
            return Ok(());
        }
        println!("Session stores:");
        for home in &homes {
            println!("  {}", home.join(codex_usage::SESSIONS_DIR).display());
        }
        println!("Rollouts scanned: {}", rollouts.len());
        if !filter.directories.is_empty() {
            println!("Attributed directories:");
            for dir in &filter.directories {
                println!("  {}", dir.display());
            }
        }
        if !filter.ids.is_empty() {
            println!("Attributed session ids:");
            for id in &filter.ids {
                println!("  {id}");
            }
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
            // 30 wide: an RFC3339 instant with sub-second precision and a
            // numeric offset runs to 29 characters, so a narrower column would
            // push every later field out of alignment on every row.
            println!(
                "\n{:<30} {:<24} {:<38} {:>12} {:>12}",
                "CREATED", "MODEL", "SESSION", "INPUT", "OUTPUT"
            );
            for s in &sessions {
                println!(
                    "{:<30} {:<24} {:<38} {:>12} {:>12}",
                    s.created_at.to_rfc3339(),
                    s.model,
                    s.session_id.as_deref().unwrap_or("-"),
                    s.input,
                    s.output
                );
            }
        }
        Ok(())
    }

    /// `None` under `--all-time`; otherwise the resolved `[since, until]`.
    ///
    /// Identical to `opencode-usage`'s own window resolution, deliberately:
    /// an operator reconciling one window across both runtimes must not have
    /// to learn two spellings of it.
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

    /// The exact session key to attribute.
    ///
    /// The current directory is defaulted in ONLY when no `--session-id` was
    /// given: with ids present, defaulting a directory in would silently AND
    /// the two and report nothing for a session opened elsewhere.
    fn filter(&self) -> Result<SessionFilter> {
        let roots = if self.directory.is_empty() {
            if self.session_id.is_empty() {
                vec![std::env::current_dir().context("resolving the current directory")?]
            } else {
                Vec::new()
            }
        } else {
            self.directory.clone()
        };
        let directories = match self.issue {
            None => roots,
            Some(issue) => {
                anyhow::ensure!(
                    roots.len() == 1,
                    "--issue names one workspace root's worktree; pass exactly one --directory \
                     with it"
                );
                loom_daemon::usage_source::sweep_directories(&roots[0], issue)
            }
        };
        Ok(SessionFilter {
            directories,
            ids: self.session_id.clone(),
        })
    }
}

/// One session row's JSON form. Spelled out rather than derived so the wire
/// shape is reviewable here and cannot silently grow a field the reader adds.
fn session_json(s: &CodexSessionUsage) -> serde_json::Value {
    serde_json::json!({
        "created_at": s.created_at.to_rfc3339(),
        "directory": s.directory,
        "session_id": s.session_id,
        "model": s.model,
        "provider": s.provider,
        "input": s.input,
        "output": s.output,
        "cache_read": s.cache_read,
    })
}
