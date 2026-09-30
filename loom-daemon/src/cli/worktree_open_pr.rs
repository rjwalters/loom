//! `loom-daemon worktree-open-pr` — the forge round-trip behind
//! `worktree.sh`'s #7765 fresh-branch-shadow guard (#8195 slice 15).
//!
//! # Output
//!
//! A `TOKEN<TAB>text` record stream on stdout, replayed by
//! `lib/worktree-forge-pr-check.sh`'s `_worktree_open_pr_for_branch` into its
//! `_WT_OPEN_PR_*` globals — always exactly one `STATUS` record, plus
//! `NUMBER`/`CROSS_REPO`/`HEAD_REPO`/`HEAD_REF`/`URL` when (and only when)
//! `STATUS` is `found`. See [`loom_daemon::worktree_cli::open_pr`] for the
//! four-way status contract.
//!
//! # Exit code
//!
//! Always 0 — this is a query, never a refusal. `worktree.sh`'s dispatch
//! probes `--help` first and falls through to its own (unchanged) forge query
//! when a resolvable daemon does not know this subcommand, so a daemon
//! predating this slice never sees anything but clap's own exit 2 from the
//! probe itself.
//!
//! # Why clap here
//!
//! One caller, a generated command line inside a sourced library, no human
//! types it — the same argument as `worktree-stale-ref` / `worktree-upstream`
//! / `worktree-closed-pr-branch`.

use std::path::PathBuf;

use anyhow::Result;

use loom_daemon::worktree_cli::open_pr;

#[derive(clap::Args)]
pub(crate) struct WorktreeOpenPrArgs {
    /// The branch name being checked for a shadowing open PR.
    #[arg(long)]
    branch: String,

    /// The main workspace root every `git` and forge call runs in.
    #[arg(long)]
    repo: PathBuf,
}

impl WorktreeOpenPrArgs {
    /// Never returns.
    pub(crate) fn run(self) -> Result<()> {
        std::process::exit(open_pr::run(&self.repo, &self.branch));
    }
}
