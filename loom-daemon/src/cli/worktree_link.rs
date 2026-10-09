//! `loom-daemon worktree-link` — the shared-artifact symlink provisioning
//! `worktree.sh` runs after a successful `git worktree add` (#8195 slice 4,
//! epic #7810).
//!
//! # Output contract
//!
//! Inherited from the shell it replaces: the four link families print the
//! same `ℹ`/`✓`/`⚠` lines, in the same order, to stdout; `--quiet` prints
//! nothing at all, which is what the script's `--json` mode asked for by
//! wrapping every one of those lines in `if [[ "$JSON_OUTPUT" != "true" ]]`.
//!
//! **Exit 0, always** — see [`link`]'s module docs for the argument. This is
//! a best-effort provisioning step whose worst outcome is a worktree that has
//! to rebuild something, and the call site has already created the worktree
//! it would otherwise be told to abandon.
//!
//! # Why clap here, unlike the `remove` / `wip` slices
//!
//! Those two are operator-facing VERBS reached by name from a role prompt, so
//! their argv grammar had to answer a typo with the shell's exit 1 rather than
//! clap's exit 2. This one has exactly one caller — a generated command line
//! inside `worktree.sh` — and no human types it, so clap's own usage error is
//! the right answer for a malformed invocation and needs no hand-rolled
//! parser to preserve a code nobody branches on.
//!
//! The one operator-typed form, `--retire-aliases` (#9152), keeps that
//! grammar: it is run by hand once per repo, and clap's usage error is an
//! adequate answer to a typo there too.
//!
//! # `--retire-aliases`
//!
//! Unlinks the `node_modules` symlinks into the main workspace that worktrees
//! created before #8944 still carry on a pnpm workspace — `--worktree` names
//! one, omitting it covers every worktree. Removes symlinks only, never their
//! targets and never a real directory; see [`link::retire_aliases`]. Unlike
//! provisioning it exits 1 if an unlink failed.

use anyhow::Result;

use loom_daemon::worktree_cli::link;

#[derive(clap::Args)]
pub(crate) struct WorktreeLinkArgs {
    /// The main workspace root (`git rev-parse --show-toplevel`), whose
    /// gitignored artifacts are the link sources.
    #[arg(long)]
    repo_root: std::path::PathBuf,

    /// Absolute path of the worktree that was just created. With
    /// `--retire-aliases`, optional: omit it to cover every worktree.
    #[arg(long, required_unless_present = "retire_aliases")]
    worktree: Option<std::path::PathBuf>,

    /// Instead of provisioning, retire the `node_modules` symlinks into the
    /// main workspace that pre-#8944 worktrees still carry on a pnpm
    /// workspace (#9152). Unlinks symlinks only — never their targets, never
    /// a real directory. Then run `pnpm install` in each reported worktree.
    #[arg(long)]
    retire_aliases: bool,

    /// Print nothing. Passed by `worktree.sh` in `--json` mode, where the
    /// pre-port script suppressed every one of these lines outright.
    #[arg(long)]
    quiet: bool,
}

impl WorktreeLinkArgs {
    /// Never returns.
    pub(crate) fn run(self) -> Result<()> {
        if self.retire_aliases {
            std::process::exit(link::retire_aliases(&link::RetireOptions {
                repo_root: self.repo_root,
                worktree: self.worktree,
                quiet: self.quiet,
            }));
        }
        let Some(worktree) = self.worktree else {
            anyhow::bail!("--worktree is required unless --retire-aliases is given");
        };
        std::process::exit(link::run(&link::Options {
            repo_root: self.repo_root,
            worktree,
            quiet: self.quiet,
        }));
    }
}
