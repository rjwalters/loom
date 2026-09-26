//! `loom-daemon worktree-submodules` — the submodule initialization
//! `worktree.sh` runs after a successful `git worktree add` (#8195 slice 8,
//! epic #7810).
//!
//! # Output contract
//!
//! Inherited from the shell it replaces: the same `ℹ`/`✓`/`⚠` lines, in the
//! same order, on stdout; the child `git submodule update`'s own stdout and
//! stderr relayed verbatim; `--quiet` suppresses this command's own lines and
//! only those, which is what the script's `--json` mode asked for by wrapping
//! every one of them in `if [[ "$JSON_OUTPUT" != "true" ]]`.
//!
//! **Exit 0, always** — see [`submodules`]'s module docs for the argument.
//! The retired block's only failure signal was a warning line; the call site
//! is inside a `set -e` script that has already created the worktree a
//! non-zero code would tell it to abandon.
//!
//! # Why clap here
//!
//! Same as the `worktree-link` slice: this has exactly one caller, a command
//! line generated inside `worktree.sh`, and no human types it — so clap's own
//! usage error is the right answer for a malformed invocation, and there is
//! no hand-rolled parser needed to preserve an exit code nobody branches on.

use anyhow::Result;

use loom_daemon::worktree_cli::submodules;

#[derive(clap::Args)]
pub(crate) struct WorktreeSubmodulesArgs {
    /// The main workspace root (`git rev-parse --show-toplevel`), whose
    /// `modules/` object stores are borrowed via `--reference`.
    #[arg(long)]
    repo_root: std::path::PathBuf,

    /// Absolute path of the worktree that was just created.
    #[arg(long)]
    worktree: std::path::PathBuf,

    /// Print nothing of this command's own. Passed by `worktree.sh` in
    /// `--json` mode, where the pre-port script suppressed every one of these
    /// lines outright. The child git's output is relayed either way, exactly
    /// as the retired block left it inherited.
    #[arg(long)]
    quiet: bool,

    /// Per-submodule deadline in seconds. `worktree.sh` passes
    /// `${LOOM_SUBMODULE_TIMEOUT:-300}`; the default here is the same 300 so
    /// the two cannot drift.
    #[arg(long, default_value_t = submodules::DEFAULT_TIMEOUT_SECS)]
    timeout: u64,
}

impl WorktreeSubmodulesArgs {
    /// Never returns.
    pub(crate) fn run(self) -> Result<()> {
        std::process::exit(submodules::run(&submodules::Options {
            repo_root: self.repo_root,
            worktree: self.worktree,
            quiet: self.quiet,
            timeout: std::time::Duration::from_secs(self.timeout),
        }));
    }
}
