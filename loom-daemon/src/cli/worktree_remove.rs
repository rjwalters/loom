//! `loom-daemon worktree-remove` — the operator-facing single-worktree removal
//! verb behind `worktree.sh remove <N>` (#8195 slice 3, epic #7810).
//!
//! # Output contract
//!
//! Inherited wholesale from the shell it replaces, because `CLAUDE.md`,
//! `builder-worktree.md` and `defaults/docs/troubleshooting.md` all name the
//! verb by path and operators chain it with `&&`:
//!
//! - **Exit 0** = removed, or the idempotent no-op (no worktree at that path),
//!   or any `--dry-run`.
//! - **Exit 1** = refused, or the removal failed. Every refusal is
//!   non-destructive by construction: nothing has been deleted when it prints.
//! - `--json` moves every human-readable line to stderr so stdout carries
//!   exactly one JSON document.
//!
//! # Why argv is taken raw
//!
//! Same reason as `worktree-wip` (slice 2): the shell answers a malformed
//! invocation with exit **1** and its own message; clap answers with exit
//! **2**, which is the code the stub reserves for
//! `LOOM_SCRIPT_HELPER_MISSING_RC` ("no binary could be resolved").
//! `allow_hyphen_values` + `trailing_var_arg` hands the tail through untouched
//! so `remove::parse_args` sees exactly what bash's `case` saw.

use anyhow::Result;

use loom_daemon::worktree_cli::remove;

#[derive(clap::Args)]
pub(crate) struct WorktreeRemoveArgs {
    /// `<issue-number>` plus any of `--keep-branch` / `--force` / `--dry-run`
    /// / `--json`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    args: Vec<String>,
}

impl WorktreeRemoveArgs {
    /// Never returns: exits with the code the shell verb exited with, which
    /// operators and `CLAUDE.md`'s documented `&&` chains branch on.
    pub(crate) fn run(self) -> Result<()> {
        std::process::exit(remove::run(&self.args));
    }
}
