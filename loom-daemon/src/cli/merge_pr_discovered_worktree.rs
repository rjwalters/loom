//! `loom-daemon merge-pr discovered-worktree` (#8191 slice): what post-merge
//! cleanup does with a worktree it found by branch name because the
//! Loom-convention path was missing — primary checkout (note only), managed
//! (hand to the remove-vs-preserve decision) or user-owned (advice only).
//!
//! # Protocol
//!
//! First stdout line `LOOM-DISCOVERED DECIDE` or `LOOM-DISCOVERED NOTE`, then
//! one `LEVEL<TAB>message` line per record to replay. Always exits 0. The shell
//! treats anything else as "no verdict" and removes nothing.

use std::path::Path;

use anyhow::Result;

use loom_daemon::merge_pr::discovered_worktree::{decide, render};

#[derive(clap::Args)]
pub(crate) struct DiscoveredWorktreeArgs {
    /// The PR's head branch (named in the operator-visible messages).
    #[arg(long, value_name = "BRANCH")]
    branch: String,

    /// The discovered worktree's path.
    #[arg(long, value_name = "PATH")]
    path: String,

    /// Whether the path is the PRIMARY checkout (`true`/`false`). The
    /// `git worktree list` comparison stays in the shell.
    #[arg(long, action = clap::ArgAction::Set)]
    primary: bool,
}

impl DiscoveredWorktreeArgs {
    pub(crate) fn run(self) -> Result<()> {
        let managed = Path::new(&self.path).join(".loom-managed").is_file();
        let (action, lines) = decide(&self.branch, &self.path, self.primary, managed);
        print!("{}", render(action, &lines));
        Ok(())
    }
}
