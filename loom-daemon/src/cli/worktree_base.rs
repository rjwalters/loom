//! `loom-daemon worktree-base` — `worktree.sh`'s default-branch fetch and
//! `--base` resolution (#8195 slice 13). Exit 0 resolved, 1 refused; the
//! record protocol is documented on [`loom_daemon::worktree_cli::base`].
//!
//! clap because the only caller is `worktree.sh`; `--repo` exists so tests need
//! not `chdir`.

use std::path::PathBuf;

use anyhow::Result;

use loom_daemon::worktree_cli::base;

#[derive(clap::Args)]
pub(crate) struct WorktreeBaseArgs {
    /// The resolved default branch (`loom_default_branch`'s answer).
    #[arg(long)]
    default_branch: String,

    /// The `--base <branch>` value; empty or absent means "no stacking".
    #[arg(long)]
    base_branch: Option<String>,

    /// `--json` mode: suppress message records.
    #[arg(long)]
    quiet: bool,

    /// Repository to operate in. Defaults to the current directory.
    #[arg(long)]
    repo: Option<PathBuf>,
}

impl WorktreeBaseArgs {
    /// Never returns.
    pub(crate) fn run(self) -> Result<()> {
        let repo = self.repo.unwrap_or_else(|| PathBuf::from("."));
        let out =
            base::resolve(&repo, &self.default_branch, self.base_branch.as_deref(), self.quiet);
        std::process::exit(base::emit(&out));
    }
}
