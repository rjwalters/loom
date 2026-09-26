//! `loom-daemon worktree-reset` — the race-safe stale-worktree reset
//! `worktree.sh` runs instead of a bare `git reset --hard` (#8195 slice 6,
//! epic #7810).
//!
//! # Exit-code contract
//!
//! 0 = reset landed, 1 = refused and nothing changed, 2 = the reset itself
//! failed. All three are answers the caller branches on; see [`reset`]'s module
//! docs for why a missing binary is reported as 1 by the shell wrapper rather
//! than as the epic's usual 2, and why clap's own usage error landing on 2 is
//! acceptable here.
//!
//! # Why clap, like `worktree-cleanup` and unlike `remove` / `wip`
//!
//! Those two are operator-facing verbs reached by name from a role prompt, so
//! their argv grammar had to answer a typo the way the shell did. This one has
//! exactly one caller — a generated command line inside
//! `lib/worktree-race-rescue.sh` — and no human types it.
//!
//! Flags rather than the shell's three positionals, deliberately: the shell's
//! `<worktree> <target-ref> [<label>]` order is only safe because bash has
//! nothing else to confuse them with, and a path, a ref and a filename stem are
//! three strings that all accept each other's values silently. Named flags make
//! a mis-ordered call a usage error instead of a reset onto a ref named after a
//! directory.

use anyhow::Result;

use loom_daemon::worktree_cli::reset;

#[derive(clap::Args)]
pub(crate) struct WorktreeResetArgs {
    /// The worktree to reset. Reported in every message exactly as given.
    #[arg(long)]
    worktree: std::path::PathBuf,

    /// The ref to reset to (`origin/main`, a SHA, a parent feature branch).
    #[arg(long)]
    target_ref: String,

    /// Filename stem for the rescue patch, if foreign tracked changes have to
    /// be captured. Defaults to the shell's own default.
    #[arg(long, default_value = "loom-race-rescue")]
    rescue_label: String,

    /// A PID the liveness probe must not treat as a foreign holder — the
    /// calling shell's `$$` / `$BASHPID`. Repeatable.
    ///
    /// The shell excluded those two inline because its probe ran in the same
    /// process. Here the probe runs in this child, so they have to be named:
    /// see [`reset::Options::ignore_pids`].
    #[arg(long = "ignore-pid")]
    ignore_pid: Vec<u32>,
}

impl WorktreeResetArgs {
    /// Never returns.
    pub(crate) fn run(self) -> Result<()> {
        std::process::exit(reset::run(&reset::Options {
            worktree: self.worktree,
            target_ref: self.target_ref,
            rescue_label: self.rescue_label,
            ignore_pids: self.ignore_pid,
        }));
    }
}
