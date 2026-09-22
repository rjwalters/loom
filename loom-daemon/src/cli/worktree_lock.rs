//! `loom-daemon worktree-lock` — the repo-global worktree-add lock behind
//! `worktree.sh` (#8195, epic #7810 slice 1).
//!
//! # Output contract
//!
//! `KEY=value` lines on stdout, because the caller is bash and has to get the
//! token back into a variable. Values here are a token (pid/digits/digits) and
//! a pid — neither can contain a newline, an `=`, or a shell metacharacter, so
//! the shape is safe to `read` without quoting games.
//!
//! - `acquire` → exit 0 with `TOKEN=<token>`, or exit 1 with `HOLDER_PID=<pid>`
//!   when the deadline passes (the pid is omitted when the holder's metadata
//!   was unreadable). Exit 2 when the locks directory cannot be created.
//! - `release` → exit 0, always. Releasing a lock you no longer own is a
//!   no-op by design, not an error.
//! - `check-issue` → the DIFFERENT, per-issue sweep-claim lock cross-check
//!   (#8553): exit 0 when free (or `--force` downgraded a live conflict to a
//!   warning printed on stderr), exit 1 when a live conflict refused and
//!   `--force` was not passed. On refusal, `--json` writes the refusal as a
//!   JSON object to stdout (matching `worktree.sh`'s own `--json` contract);
//!   without it, a human-readable message goes to stderr.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;

use loom_daemon::worktree_cli::{issue_lock, lock};

#[derive(clap::Subcommand)]
pub(crate) enum WorktreeLockCommand {
    /// Take the repo-global worktree-add lock.
    Acquire {
        /// Recorded as owner metadata. The lock is repo-global, not per-issue.
        #[arg(long, value_name = "N")]
        issue: u32,
        /// The pid that will HOLD the lock — the caller's `$$`, not this
        /// process's. This CLI exits as soon as it prints the token, so a lock
        /// recording its pid is owned by a dead process and the next acquire
        /// reclaims it as stale. Defaults to this process only so a direct
        /// invocation is not silently broken; every real caller passes `$$`.
        #[arg(long, value_name = "PID")]
        owner_pid: Option<u32>,
        /// Give up after this many seconds.
        #[arg(long, default_value_t = 30, value_name = "SECS")]
        timeout: u64,
        /// Seconds between attempts.
        #[arg(long, default_value = "0.2", value_name = "SECS")]
        poll: String,
        /// Repo to lock. Defaults to the current directory.
        #[arg(long, value_name = "PATH")]
        repo: Option<PathBuf>,
    },
    /// Release it, but only if this token still owns it (#6014).
    Release {
        /// The token `acquire` printed. An empty token releases nothing.
        #[arg(long, default_value = "", value_name = "TOKEN")]
        token: String,
        #[arg(long, value_name = "PATH")]
        repo: Option<PathBuf>,
    },
    /// Cross-check the daemon's PER-ISSUE sweep-claim lock (#8553) — a
    /// different, longer-lived lock than the one `acquire`/`release` above
    /// manage. Read-only: never acquires, never releases.
    CheckIssue {
        #[arg(long, value_name = "N")]
        issue: u32,
        #[arg(long, value_name = "PATH")]
        repo: Option<PathBuf>,
        /// Proceed even though a live claim lock was found — print a warning
        /// instead of refusing.
        #[arg(long)]
        force: bool,
        /// On refusal, write the JSON error object `worktree.sh --json`
        /// expects instead of a human-readable stderr message.
        #[arg(long)]
        json: bool,
    },
}

impl WorktreeLockCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            WorktreeLockCommand::Acquire {
                issue,
                owner_pid,
                timeout,
                poll,
                repo,
            } => {
                let repo = repo.unwrap_or_else(|| PathBuf::from("."));
                // A non-numeric poll degrades to the default rather than
                // failing: the shell passes whatever
                // LOOM_WORKTREE_LOCK_POLL_INTERVAL holds, and a malformed knob
                // must not turn lock acquisition into an error.
                let poll = poll.parse::<f64>().ok().filter(|v| *v > 0.0).unwrap_or(0.2);
                match lock::acquire(
                    &repo,
                    issue,
                    owner_pid.unwrap_or_else(std::process::id),
                    Duration::from_secs(timeout),
                    Duration::from_secs_f64(poll),
                ) {
                    Ok(token) => {
                        println!("TOKEN={token}");
                        std::process::exit(0);
                    }
                    Err(lock::AcquireError::Timeout { holder_pid }) => {
                        if let Some(p) = holder_pid {
                            println!("HOLDER_PID={p}");
                        }
                        std::process::exit(1);
                    }
                    Err(lock::AcquireError::Unusable(why)) => {
                        eprintln!("worktree-lock: {why}");
                        std::process::exit(2);
                    }
                }
            }
            WorktreeLockCommand::Release { token, repo } => {
                let repo = repo.unwrap_or_else(|| PathBuf::from("."));
                lock::release(&repo, &token);
                std::process::exit(0);
            }
            WorktreeLockCommand::CheckIssue {
                issue,
                repo,
                force,
                json,
            } => {
                let repo = repo.unwrap_or_else(|| PathBuf::from("."));
                let Some(live) = issue_lock::check(&repo, issue) else {
                    std::process::exit(0);
                };
                let age = live.age_desc();
                if force {
                    eprintln!(
                        "worktree-lock: issue #{issue} has a live claim lock (sweep '{}', pid \
                         {}, acquired {}, {age} old) — proceeding anyway (--force). This \
                         worktree may be shared with another active session (#8553).",
                        live.sweep_id, live.owner_pid, live.acquired_at
                    );
                    std::process::exit(0);
                }
                if json {
                    println!(
                        "{{\"success\": false, \"error\": \"issue-claim-lock-live\", \
                         \"issueNumber\": {issue}, \"sweepId\": {}, \"ownerPid\": {}, \
                         \"acquiredAt\": {}}}",
                        serde_json::to_string(&live.sweep_id).unwrap_or_else(|_| "\"\"".into()),
                        live.owner_pid,
                        serde_json::to_string(&live.acquired_at).unwrap_or_else(|_| "\"\"".into()),
                    );
                } else {
                    eprintln!(
                        "worktree-lock: issue #{issue} already has a LIVE claim lock: sweep \
                         '{}', pid {}, acquired {} ({age} old).",
                        live.sweep_id, live.owner_pid, live.acquired_at
                    );
                    eprintln!(
                        "A second concurrent session checking out .loom/worktrees/issue-{issue} \
                         would interleave writes with that live sweep (#8553)."
                    );
                    eprintln!(
                        "Re-run with --force if you are certain it is safe to proceed anyway."
                    );
                }
                std::process::exit(1);
            }
        }
    }
}
