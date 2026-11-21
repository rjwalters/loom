//! `loom-daemon worktree-branch-reuse` — `worktree.sh`'s local-branch reuse
//! arm (#8195 slice 14). Exit 0 reuse, 1 refuse; the full contract is on
//! [`crate::worktree_cli::branch_reuse`].
//!
//! clap because the only caller is `worktree.sh`; `--repo` exists so tests
//! need not `chdir`.

use std::path::PathBuf;

use anyhow::Result;

use crate::worktree_cli::{branch_reuse, pr_list::PrList};

#[derive(clap::Args)]
pub struct WorktreeBranchReuseArgs {
    /// `$BRANCH_NAME` — the existing local branch under question.
    #[arg(long)]
    branch: String,

    /// `$ISSUE_NUMBER`.
    #[arg(long)]
    issue: String,

    /// `$DEFAULT_BRANCH` — what "already landed" is measured against.
    #[arg(long)]
    default_branch: String,

    /// `$BASE_REF` — the ref the divergence warning compares against.
    #[arg(long)]
    base_ref: String,

    /// `$BASE_DISPLAY` — how that ref is spelled to a human.
    #[arg(long)]
    base_display: String,

    /// `$JSON_OUTPUT`, verbatim. A string rather than a flag so the shell
    /// dispatch needs no conditional — see the module doc on
    /// `worktree_cli::branch_reuse`.
    #[arg(long, default_value = "false")]
    json_output: String,

    /// The main workspace root. Defaults to the current directory.
    #[arg(long)]
    repo: Option<PathBuf>,
}

impl WorktreeBranchReuseArgs {
    /// Exits with the worktree result, or returns a mode configuration error.
    pub(crate) fn run(self, pr_list: PrList) -> Result<()> {
        let opts = branch_reuse::Options {
            repo: self.repo.unwrap_or_else(|| PathBuf::from(".")),
            branch: self.branch,
            issue: self.issue,
            default_branch: self.default_branch,
            base_ref: self.base_ref,
            base_display: self.base_display,
            json_output: self.json_output,
        };
        std::process::exit(
            branch_reuse::try_run_with(&opts, &|branch| {
                crate::worktree_cli::branch_landed::forge_probe_with(
                    &opts.repo, &opts.repo, branch, pr_list,
                )
            })
            .map_err(anyhow::Error::msg)?,
        );
    }
}
