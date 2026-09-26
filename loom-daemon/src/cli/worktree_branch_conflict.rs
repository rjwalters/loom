//! `loom-daemon worktree-branch-conflict` — the "feature branch checked out
//! in the main worktree" recovery guard `worktree.sh` falls into when `git
//! worktree add` refuses (#8195 slice 7, epic #7810).
//!
//! # Exit-code contract
//!
//! 0 = handled (a message was already printed, unless `--quiet`), 1 = not
//! this error (the caller reports git's own raw error itself), 2 =
//! auto-recovered, retry `git worktree add` once. See
//! [`worktree_cli::branch_conflict`]'s module docs for the full argument,
//! including why a missing binary degrades to 1 rather than the epic's usual
//! 2: 1 is already the shell wrapper's own choice for that case (it never
//! reaches this binary at all), not something clap answers.
//!
//! # Why clap here, like `worktree-link` / `worktree-cleanup`
//!
//! Exactly one caller — a generated command line inside `worktree.sh` — and no
//! human types it, so clap's own usage error is the right answer for a
//! malformed invocation.
//!
//! # Why stdin, not argv, for the error text
//!
//! `git worktree add`'s captured stderr is arbitrary, occasionally
//! multi-line, text — not argv-safe, and not bounded in size the way a flag
//! value should be. It arrives on stdin instead, the same choice
//! `merge-pr-refs` (#8191 slice 1) made for a PR body.

use std::io::Read as _;
use std::path::PathBuf;

use anyhow::Result;

use loom_daemon::worktree_cli::branch_conflict;

#[derive(clap::Args)]
pub(crate) struct WorktreeBranchConflictArgs {
    /// The branch `git worktree add` was trying to create or attach.
    #[arg(long)]
    branch: String,

    /// The repo's already-resolved default branch — the recovery target.
    #[arg(long)]
    default_branch: String,

    /// `$ISSUE_NUMBER`, quoted into two of the message bodies.
    #[arg(long)]
    issue: String,

    /// The main workspace root (`$WORKTREE_REPO_ROOT`).
    #[arg(long)]
    repo_root: PathBuf,

    /// Print nothing. Passed by `worktree.sh` in `--json` mode, where the
    /// pre-port script suppressed every line this prints — including its
    /// `print_error` calls (see the module docs for why that is unlike most
    /// of this file's siblings).
    #[arg(long)]
    quiet: bool,
}

impl WorktreeBranchConflictArgs {
    /// Never returns.
    pub(crate) fn run(self) -> Result<()> {
        let mut error_output = String::new();
        std::io::stdin().read_to_string(&mut error_output)?;

        std::process::exit(branch_conflict::run(&branch_conflict::Options {
            error_output,
            branch: self.branch,
            default_branch: self.default_branch,
            issue: self.issue,
            repo_root: self.repo_root,
            quiet: self.quiet,
        }));
    }
}
