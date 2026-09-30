//! `loom-daemon worktree-existing` — `worktree.sh`'s "the worktree directory
//! already exists" arm, whole (#8195 slice 12, epic #7810).
//!
//! # Exit-code contract
//!
//! `0` the worktree is usable (preserved, reset, or reset-refused-and-left-
//! alone — all three of which the retired shell reported as exit 0), `1` the
//! directory is not a registered worktree, or its `.loom-managed` sentinel
//! could not be written. `worktree.sh` maps any non-zero to its own exit 1,
//! which is also the safe reading of a crash: a worktree whose state could not
//! be established is not one to hand to a Builder.
//!
//! # Why clap here
//!
//! One caller, a generated command line inside `worktree.sh`, no human types
//! it — the same argument `worktree-upstream` / `worktree-stale-ref` /
//! `worktree-sparse` make. Every value is already resolved by the script
//! (branch, base ref, base display, the caller's `git status` reading), so the
//! flags can be required rather than re-derived here, where a second derivation
//! could disagree with the script's own.

use std::path::PathBuf;

use anyhow::Result;

use loom_daemon::worktree_cli::existing;

#[derive(clap::Args)]
pub(crate) struct WorktreeExistingArgs {
    /// `$WORKTREE_PATH` — the existing directory under decision. Quoted into
    /// messages exactly as given.
    #[arg(long)]
    worktree: PathBuf,

    /// `$WORKTREE_REPO_ROOT` — the main workspace, where the retired
    /// `git worktree list` ran.
    #[arg(long)]
    repo: PathBuf,

    /// `$ISSUE_NUMBER`, verbatim (it reaches the sentinel and a hint as text).
    #[arg(long)]
    issue: String,

    /// `$BRANCH_NAME` — `feature/issue-<N>` or the custom name.
    #[arg(long)]
    branch: String,

    /// `$DEFAULT_BRANCH` — the bare default-branch name (`main`).
    #[arg(long)]
    default_branch: String,

    /// `$BASE_REF` — the staleness reference when no live unmerged
    /// `origin/<branch>` applies.
    #[arg(long)]
    base_ref: String,

    /// `$BASE_DISPLAY` — how `--base-ref` is spelled to a human.
    #[arg(long)]
    base_display: String,

    /// `$BASE_BRANCH` — the `--base` override; empty when absent. Only the
    /// pre-reset `git fetch` reads it.
    #[arg(long, default_value = "")]
    base_branch: String,

    /// `--json` mode: suppress the messages the shell gated, and route the
    /// ungated ones to stderr.
    #[arg(long)]
    quiet: bool,

    /// The calling shell's `$$` / `$BASHPID`, so the reset guard's liveness
    /// probe (#7463) does not count the invoker as a foreign holder. Repeatable.
    #[arg(long = "ignore-pid")]
    ignore_pid: Vec<u32>,
}

impl WorktreeExistingArgs {
    /// Never returns.
    pub(crate) fn run(self) -> Result<()> {
        std::process::exit(existing::run(&existing::Options {
            worktree: self.worktree,
            repo: self.repo,
            issue: self.issue,
            branch: self.branch,
            default_branch: self.default_branch,
            base_ref: self.base_ref,
            base_display: self.base_display,
            base_branch: self.base_branch,
            quiet: self.quiet,
            ignore_pids: self.ignore_pid,
        }));
    }
}
