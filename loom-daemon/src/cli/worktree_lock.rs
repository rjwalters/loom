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

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;

use loom_daemon::worktree_cli::lock;

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
        }
    }
}
