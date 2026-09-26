//! `loom-daemon worktree-upstream` — `worktree.sh`'s upstream-tracking
//! correction (#6095/#6100) and the stale-worktree drift report (#6257/#6291),
//! which were two hand-maintained copies of the same fix (#8195 slice 9, epic
//! #7810).
//!
//! # Exit-code contract
//!
//! 0, always. See [`worktree_cli::upstream`]'s module docs: every git call in
//! the retired shell was `|| true`, both call sites read nothing back, and a
//! repo that cannot be fetched from must not block worktree creation. A
//! missing binary means the block does not run — the pre-#6095/pre-#6257
//! behaviour, a lost diagnosis rather than a lost file.
//!
//! # Why clap here, like `worktree-link` / `worktree-branch-conflict`
//!
//! Two callers, both generated command lines inside `worktree.sh`, and no
//! human types either — so clap's own usage error is the right answer for a
//! malformed invocation, and `--arm` can be a strict enum rather than a
//! forgiving string.

use std::path::PathBuf;

use anyhow::Result;

use loom_daemon::worktree_cli::upstream;

/// Which of the two retired `worktree.sh` blocks to run. Kebab-case on the
/// command line: `local-branch`, `registered-worktree`.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum ArmArg {
    /// The "local branch already exists, reusing it" arm (#6095/#6100).
    LocalBranch,
    /// The "worktree directory exists and is registered with git" fast path
    /// (#6257/#6291) — adds the behind-the-pushed-tip drift report.
    RegisteredWorktree,
}

#[derive(clap::Args)]
pub(crate) struct WorktreeUpstreamArgs {
    /// Directory to run git in. `$WORKTREE_REPO_ROOT` for `local-branch`;
    /// `$WORKTREE_PATH` for `registered-worktree`, where it is also the path
    /// quoted into the drift hints.
    #[arg(long)]
    repo: PathBuf,

    /// `$BRANCH_NAME`.
    #[arg(long)]
    branch: String,

    /// Which retired block this invocation is.
    #[arg(long, value_enum)]
    arm: ArmArg,

    /// `$ISSUE_NUMBER`, quoted into the `snapshot` remediation hint. Required
    /// for `registered-worktree`, unused by `local-branch`.
    #[arg(long, required_if_eq("arm", "registered-worktree"))]
    issue: Option<String>,

    /// The caller's own `git status --porcelain` verdict, taken BEFORE the
    /// fetch — see the module docs for why it is passed in rather than
    /// recomputed here. Selects which remediation hint block the drift report
    /// prints; `registered-worktree` only.
    #[arg(long)]
    uncommitted: bool,

    /// Print nothing. Passed by `worktree.sh` in `--json` mode, where the
    /// pre-port script suppressed every `print_*` line this emits. The git
    /// side effects still run — that is what the retired shell did.
    #[arg(long)]
    quiet: bool,
}

impl WorktreeUpstreamArgs {
    /// Never returns.
    pub(crate) fn run(self) -> Result<()> {
        std::process::exit(upstream::run(&upstream::Options {
            repo: self.repo,
            branch: self.branch,
            arm: match self.arm {
                ArmArg::LocalBranch => upstream::Arm::LocalBranch,
                ArmArg::RegisteredWorktree => upstream::Arm::RegisteredWorktree,
            },
            quiet: self.quiet,
            issue: self.issue.unwrap_or_default(),
            uncommitted: self.uncommitted,
        }));
    }
}
