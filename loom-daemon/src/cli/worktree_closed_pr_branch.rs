//! `loom-daemon worktree-closed-pr-branch` — the closed-unmerged arm of
//! `worktree.sh`'s branch-resolution contract (#9083).
//!
//! # Exit-code contract
//!
//! 0 = proceed (reuse `origin/<branch>`), 1 = refuse (a message was printed).
//! Nothing else, and every inability to decide is 0 — see
//! [`worktree_cli::closed_pr_branch`]'s module docs for why that direction is
//! fixed rather than chosen.
//!
//! A daemon predating this subcommand answers clap's exit 2, which is neither
//! code. `worktree.sh`'s dispatch therefore probes `--help` first and skips the
//! guard entirely when it is absent — degrading to the pre-#9083 behaviour
//! (reuse) with no stray clap usage text, the same shape
//! `worktree-branch-conflict` uses.
//!
//! # Why clap here
//!
//! One caller, a generated command line inside `worktree.sh`, no human types
//! it — the same argument as `worktree-stale-ref` / `worktree-upstream`.

use std::path::PathBuf;

use anyhow::Result;

use loom_daemon::worktree_cli::closed_pr_branch;

#[derive(clap::Args)]
pub(crate) struct WorktreeClosedPrBranchArgs {
    /// `$BRANCH_NAME` — the branch whose `origin/` tip is under question.
    #[arg(long)]
    branch: String,

    /// `$ISSUE_NUMBER`, quoted into the message and the JSON document.
    #[arg(long)]
    issue: String,

    /// `$BASE_DISPLAY` — how the base ref is spelled to a human (`main`).
    #[arg(long)]
    base_display: String,

    /// The main workspace root every `git` and forge call runs in.
    #[arg(long)]
    repo_root: PathBuf,

    /// `$JSON_OUTPUT`, verbatim. A value rather than a flag so the shell
    /// dispatch stays a single line — see [`closed_pr_branch::Options`].
    #[arg(long, default_value = "false")]
    json_output: String,
}

impl WorktreeClosedPrBranchArgs {
    /// Never returns.
    pub(crate) fn run(self) -> Result<()> {
        std::process::exit(closed_pr_branch::run(&closed_pr_branch::Options {
            branch: self.branch,
            issue: self.issue,
            base_display: self.base_display,
            repo: self.repo_root,
            json_output: self.json_output,
        }));
    }
}
