//! `loom-daemon worktree-cleanup` — the crash-debris pre-flight `worktree.sh`
//! runs before (and again under) its worktree-add lock (#8195 slice 5, epic
//! #7810).
//!
//! # Output contract
//!
//! Inherited from the shell it replaces: the two warning families print the
//! same `⚠` lines, in the same order, to stdout; `--quiet` prints nothing at
//! all, which is what the script's `--json` mode asked for by wrapping both in
//! `if [[ "$JSON_OUTPUT" != "true" ]]`.
//!
//! **Exit 0, always** — see [`cleanup`]'s module docs for the argument. Both
//! of the shell's call sites already discard the status with `|| true`, and
//! the caller keeps doing so, so the two agree even if that ever changes.
//!
//! # Why clap here, like `worktree-link` and unlike `remove` / `wip`
//!
//! Those two are operator-facing VERBS reached by name from a role prompt, so
//! their argv grammar had to answer a typo with the shell's exit 1 rather than
//! clap's exit 2. This one has exactly one caller — a generated command line
//! inside `worktree.sh` — and no human types it, so clap's own usage error is
//! the right answer for a malformed invocation.
//!
//! That is also what makes `<ISSUE>` a `u64` rather than a string: it is the
//! one strengthening this slice takes over the shell, and clap is where it is
//! enforced. See [`cleanup`]'s "The one strengthening".

use anyhow::Result;

use loom_daemon::worktree_cli::cleanup;

#[derive(clap::Args)]
pub(crate) struct WorktreeCleanupArgs {
    /// Issue number whose `issue-<N>` crash debris should be cleared.
    issue: u64,

    /// Print nothing. Passed by `worktree.sh` in `--json` mode, where the
    /// pre-port script suppressed every one of these lines outright.
    #[arg(long)]
    quiet: bool,
}

impl WorktreeCleanupArgs {
    /// Never returns.
    pub(crate) fn run(self) -> Result<()> {
        std::process::exit(cleanup::run(&cleanup::Options {
            issue: self.issue,
            quiet: self.quiet,
        }));
    }
}
