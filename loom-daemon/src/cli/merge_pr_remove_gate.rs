//! `loom-daemon merge-pr remove-gate` (#8191 slice): the #3710 primary-worktree
//! guard and the `.loom-managed` sentinel guard in front of
//! `merge-pr.sh`'s `_remove_loom_worktree`.
//!
//! # Protocol
//!
//! `git worktree list --porcelain` arrives on stdin (the `git` call stays in
//! the shell, like the sibling `worktree-*` verbs). The first stdout line is
//! `LOOM-REMOVE-GATE PROCEED` or `LOOM-REMOVE-GATE REFUSE`, then one
//! `LEVEL<TAB>message` line per record to replay. Always exits 0.
//!
//! Fail direction: the shell treats anything else (non-zero exit, a first line
//! that is neither token — a missing or older binary) as REFUSE.

use std::io::Read;
use std::path::Path;

use anyhow::Result;

use loom_daemon::merge_pr::remove_gate::{decide, render, Context};

#[derive(clap::Args)]
pub(crate) struct RemoveGateArgs {
    /// The target path as given to `_remove_loom_worktree`.
    #[arg(long, value_name = "PATH")]
    path: String,

    /// The target's canonical path (`pwd -P`), computed by the caller because
    /// it is reused for the CWD-inside-worktree check.
    #[arg(long, value_name = "PATH")]
    real: String,

    /// `--worktree-path` explicit opt-in (`true`/`false`): bypass a missing
    /// sentinel. A value rather than a bare flag so the shell passes its
    /// `allow_unmanaged` variable straight through.
    #[arg(long, action = clap::ArgAction::Set)]
    allow_unmanaged: bool,
}

impl RemoveGateArgs {
    pub(crate) fn run(self) -> Result<()> {
        let mut raw = Vec::new();
        std::io::stdin().read_to_end(&mut raw)?;
        let porcelain = String::from_utf8_lossy(&raw);
        let ctx = Context {
            porcelain: &porcelain,
            path: &self.path,
            real: &self.real,
            allow_unmanaged: self.allow_unmanaged,
            sentinel_present: Path::new(&self.path).join(".loom-managed").is_file(),
        };
        let (verdict, lines) = decide(&ctx);
        print!("{}", render(verdict, &lines));
        Ok(())
    }
}
