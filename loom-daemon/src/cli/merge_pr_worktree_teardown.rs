//! `loom-daemon merge-pr worktree-teardown` (#6372, an #8191 slice): the
//! `git worktree remove --force` at the end of `merge-pr.sh`'s
//! `_remove_loom_worktree`, with its one prune-and-retry and its report.
//!
//! # Protocol
//!
//! | outcome | first stdout line | then | exit |
//! |---|---|---|---|
//! | removed (first try, or after a prune) | `LOOM-WORKTREE-TEARDOWN REMOVED` | one `SUCCESS<TAB>…` record | 0 |
//! | still there | `LOOM-WORKTREE-TEARDOWN FAILED` | `WARNING<TAB>…` records: the diagnosis, git's error (one record per line), two remediations | 0 |
//!
//! Every guard has already run by the time this is called — the shell calls
//! it only after `remove-gate`/the sentinel and `dirty-guard` passed — so this
//! verb decides nothing about WHETHER to remove; it only removes and reports.
//!
//! # Fail direction
//!
//! Both outcomes exit 0, so a non-zero exit or any other first line means the
//! verb did not run (a missing or older binary — `_mp_worktree` returns 3).
//! The shell treats that as not removed: it warns, names the manual
//! remediation and the roll hint, and skips the success-only steps (ledger,
//! `--worktree-path` branch delete, cargo target reclaim). A skipped removal
//! is recoverable; the merge has already succeeded either way.

use anyhow::Result;

use loom_daemon::merge_pr::worktree_teardown::teardown;

#[derive(clap::Args)]
pub(crate) struct WorktreeTeardownArgs {
    /// The repository root every `git -C` runs against (`$REPO_ROOT`).
    #[arg(long, value_name = "PATH")]
    repo_root: String,

    /// The worktree to remove, exactly as `_remove_loom_worktree` was given it.
    #[arg(long, value_name = "PATH")]
    path: String,
}

impl WorktreeTeardownArgs {
    pub(crate) fn run(self) -> Result<()> {
        print!("{}", teardown(&self.repo_root, &self.path));
        Ok(())
    }
}
