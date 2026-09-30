//! `loom-daemon merge-pr cleanup-paths` (#6264/#3530, a slice of the merge-pr
//! port #8191): which worktree paths a merged PR owns, named for
//! `merge-pr.sh`'s post-merge cleanup block.
//!
//! # Protocol
//!
//! Takes the three facts the classification needs as flags — no stdin, no forge
//! call, and the only filesystem touch is the worktree-root resolution the
//! retired `lib/worktree-root.sh` also performed — and prints exactly one line:
//!
//! ```text
//! LOOM-CLEANUP-PATHS<TAB><default-path><TAB><issue-num><TAB><judge-pr-path>
//! ```
//!
//! Fields 2 and 3 are empty for a non-`feature/issue-<N>` branch, which is what
//! the shell's `ISSUE_NUM` / `JUDGE_PR_WT_PATH` held there. The separator count
//! is fixed at three, but that alone is NOT what makes `IFS=$'\t' read -r a b c`
//! fill all three names: **tab is IFS whitespace**, so bash strips a leading run
//! of it and an empty *leading* field is unrecoverable. `<default-path>` — the
//! only field with no absent case — therefore leads, which pushes both possible
//! empties to the tail, where `read` does preserve them. See
//! [`loom_daemon::merge_pr::cleanup_paths::render`] for the full reasoning and
//! the bug this ordering fixes.
//!
//! # Exit code
//!
//! 0 with the line above; 2 when the answer cannot be framed unambiguously
//! (a worktree root containing a tab or newline — see
//! [`loom_daemon::merge_pr::cleanup_paths::render`]).
//!
//! # Fail direction: OPEN
//!
//! The shell treats a non-zero exit, or a first line that is not
//! `LOOM-CLEANUP-PATHS<TAB>…`, as "no targets": it warns, and cleans up
//! nothing. That is the safe direction at this point in the script — the merge
//! has already happened, skipped cleanup is recoverable (`loom-clean`, the
//! daemon's reaper, the next merge), and every guard that would authorise a
//! removal needs this same binary anyway.
//!
//! "Cleans up nothing" is load-bearing on the shell side gating BOTH of its
//! removal call sites on a non-empty `$DEFAULT_WT_PATH`, not just the
//! convention one: an empty path fails `[[ -d ]]` and would otherwise fall into
//! the porcelain discovery fallback, which rediscovers the same worktree by
//! branch with `$ISSUE_NUM` empty and thus WITHOUT #4186's still-open-issue
//! protection. See `merge_pr::cleanup_paths`'s module docs for why that would
//! make the degraded path more destructive than the healthy one.

use anyhow::Result;

use loom_daemon::merge_pr::cleanup_paths::{plan, render};
use loom_daemon::worktree_root::worktree_root_readable;
use std::path::Path;

#[derive(clap::Args)]
pub(crate) struct CleanupPathsArgs {
    /// The main repository root — `$REPO_ROOT`, the base the worktree root is
    /// resolved against (and namespaced by, when an override is configured).
    #[arg(long, value_name = "PATH")]
    repo_root: String,

    /// The merged PR's head branch, classified against
    /// `^feature/issue-([0-9]+)$`.
    #[arg(long, value_name = "BRANCH")]
    branch: String,

    /// The merged PR's number, as written — it names `pr-<PR>`.
    #[arg(long, value_name = "N")]
    pr: String,
}

impl CleanupPathsArgs {
    pub(crate) fn run(self) -> Result<()> {
        let root = worktree_root_readable(Path::new(&self.repo_root));
        let Some(line) = render(&plan(&self.branch, &self.pr, &root)) else {
            eprintln!(
                "merge-pr cleanup-paths: the resolved worktree root contains a tab or newline \
                 ('{}'), so the cleanup targets cannot be framed unambiguously; refusing rather \
                 than naming a path something later force-removes",
                root.display()
            );
            std::process::exit(2);
        };
        print!("{line}");
        Ok(())
    }
}
