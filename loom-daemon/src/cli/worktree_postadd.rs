//! `loom-daemon worktree-postadd` — the post-`git worktree add` finalization
//! steps `worktree.sh` runs once the shared artifacts are in place (#8195
//! slice 16, epic #7810): `core.hooksPath` (#3638), the per-worktree Cargo
//! target dir (#8458), and the project `post-worktree.sh` hook.
//!
//! # Output contract
//!
//! Inherited from the shell it replaces: the hook's two lines print to stdout
//! in the same order and wording; `--quiet` prints nothing at all, which is
//! what the script's `--json` mode asked for by wrapping each of them in
//! `if [[ "$JSON_OUTPUT" != "true" ]]`. The [`postadd`] module doc covers the
//! two streams `--quiet` deliberately does NOT silence (the hook's own output,
//! and the `#8458` report line on stderr).
//!
//! **Exit 0, always** — see [`postadd::run`]'s docs. `git worktree add` has
//! already succeeded by the time this runs, so there is nothing the caller
//! could usefully do with a failure except suppress it.
//!
//! # Why clap here, like `worktree-link` / `worktree-submodules`
//!
//! The `remove` / `wip` slices are operator-facing verbs reached by name from a
//! role prompt, so their argv grammar had to answer a typo with the shell's
//! exit 1 rather than clap's exit 2. This one has exactly one caller — a
//! generated command line inside `worktree.sh` — and no human types it, so
//! clap's own usage error is the right answer for a malformed invocation.

use anyhow::Result;

use loom_daemon::worktree_cli::postadd;

#[derive(clap::Args)]
pub(crate) struct WorktreePostaddArgs {
    /// The main workspace root (`git rev-parse --show-toplevel`), which owns
    /// `.githooks/` and `.loom/hooks/post-worktree.sh`.
    #[arg(long)]
    repo_root: std::path::PathBuf,

    /// Absolute path of the worktree that was just created.
    #[arg(long)]
    worktree: std::path::PathBuf,

    /// The branch checked out in it — the hook's `$2`.
    #[arg(long)]
    branch: String,

    /// The issue number — the hook's `$3`. A string, because the shell passed
    /// `$ISSUE_NUMBER` verbatim and an arbitrary project hook may care about
    /// its exact spelling.
    #[arg(long)]
    issue: String,

    /// Print nothing. Passed by `worktree.sh` in `--json` mode, where the
    /// pre-port script suppressed each of these lines outright.
    #[arg(long)]
    quiet: bool,
}

impl WorktreePostaddArgs {
    /// Never returns.
    pub(crate) fn run(self) -> Result<()> {
        std::process::exit(postadd::run(&postadd::Options {
            repo_root: self.repo_root,
            worktree: self.worktree,
            branch: self.branch,
            issue: self.issue,
            quiet: self.quiet,
        }));
    }
}
