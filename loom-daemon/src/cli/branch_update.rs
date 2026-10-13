//! `loom-daemon branch-update` CLI surface (#10771). The policy itself lives
//! in [`loom_daemon::branch_update`]; this file is only the clap shape.

use std::path::PathBuf;

use anyhow::Result;

/// `loom-daemon branch-update` — how PR branches are brought up to date.
#[derive(clap::Subcommand)]
pub(crate) enum BranchUpdateCommand {
    /// Print `merge` or `rebase`: may this repository's PR branches be
    /// rebased? `merge` = `git merge` the default branch in and push without
    /// force; `rebase` = rebase and push with a pinned lease (#9487).
    /// Precedence: `LOOM_BRANCH_UPDATE_STRATEGY` > `.loom/config.json`
    /// `branchUpdateStrategy` > `auto` (rebase only when the forge reports
    /// `allow_rebase_merge=true`). Anything unknown is `merge`. Always exits 0.
    Strategy(StrategyArgs),
}

impl BranchUpdateCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            BranchUpdateCommand::Strategy(args) => {
                loom_daemon::branch_update::handle_strategy(
                    args.repo_dir.as_deref(),
                    args.repo.as_deref(),
                    !args.no_cache,
                    args.quiet,
                );
                Ok(())
            }
        }
    }
}

/// `loom-daemon branch-update strategy [--repo-dir DIR] [--repo OWNER/NAME]
/// [--no-cache] [--quiet]`.
#[derive(clap::Args)]
pub(crate) struct StrategyArgs {
    /// Checkout to answer for (default: the current directory).
    #[arg(long, value_name = "DIR")]
    repo_dir: Option<PathBuf>,

    /// `owner/name` to probe (default: parsed from `origin`'s URL).
    #[arg(long, value_name = "OWNER/NAME")]
    repo: Option<String>,

    /// Re-read the forge even when a recent answer is cached.
    #[arg(long)]
    no_cache: bool,

    /// Do not print the reason on stderr.
    #[arg(long)]
    quiet: bool,
}
