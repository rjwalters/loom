//! `loom-daemon worktree-stale-ref` — the staleness reference `worktree.sh`'s
//! already-registered-worktree fast path measures and resets against (#8287,
//! ported per the shell language policy in #8354).
//!
//! # Exit-code contract
//!
//! 0, always — see [`worktree_cli::stale_ref`]'s module docs. `BASE_REF` is
//! always a usable answer, so there is no "could not decide" state to report
//! and nothing for a caller to branch on. The one non-zero a caller can
//! observe is clap's own exit 2 for an unknown subcommand, i.e. a daemon
//! predating this port, and `worktree.sh`'s `|| echo …` arm substitutes the
//! pre-#8287 reading for exactly that case.
//!
//! # Why clap here
//!
//! One caller, a generated command line inside `worktree.sh`, no human types
//! it — the same argument as `worktree-upstream` / `worktree-link`: clap's
//! usage error is the right answer to a malformed invocation, and the flags can
//! be required rather than positionally guessed.

use std::path::PathBuf;

use anyhow::Result;

use loom_daemon::worktree_cli::stale_ref;

#[derive(clap::Args)]
pub(crate) struct WorktreeStaleRefArgs {
    /// `$WORKTREE_PATH` — the existing worktree. Every git call runs here, and
    /// its `HEAD` is what the ahead/behind counts are measured from.
    #[arg(long)]
    worktree: PathBuf,

    /// `$BRANCH_NAME` — the worktree's own branch (`feature/issue-<N>`).
    #[arg(long)]
    branch: String,

    /// `$DEFAULT_BRANCH` — the bare default-branch name (`main`) the
    /// landed-check measures against. Dynamic, not the literal `origin/main`:
    /// a stacked child measures against its parent (#3729).
    #[arg(long)]
    default_branch: String,

    /// `$BASE_REF` — the reference to fall back to when no live, unmerged
    /// `origin/<branch>` applies.
    #[arg(long)]
    base_ref: String,

    /// `$BASE_DISPLAY` — how `--base-ref` is spelled in a human-facing message
    /// (`main`, not `origin/main`).
    #[arg(long)]
    base_display: String,
}

impl WorktreeStaleRefArgs {
    /// Never returns.
    pub(crate) fn run(self) -> Result<()> {
        std::process::exit(stale_ref::run(&stale_ref::Options {
            worktree: self.worktree,
            branch: self.branch,
            default_branch: self.default_branch,
            base_ref: self.base_ref,
            base_display: self.base_display,
        }));
    }
}
