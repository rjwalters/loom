//! The `.loom-managed` sentinel writer — `worktree.sh`'s `write_loom_sentinel`
//! (#3334/#3548), first needed on the Rust side by #8195 slice 10.
//!
//! # The contract, which this module must not bend
//!
//! The sentinel is what authorizes cleanup tooling (`merge-pr.sh`,
//! `agent-destroy.sh`, `loom-clean`, the daemon's reaper) to remove a
//! worktree. A worktree without it is treated as user-owned and never touched.
//! Two consequences follow, and both are on the **caller**, not here:
//!
//! - It must be written on every path that leaves a usable Loom worktree
//!   behind, not just first creation (#3548 — a re-invocation that exited
//!   before writing it stranded the worktree).
//! - It must **never** be written into a directory that is not a registered
//!   git worktree. Such a directory is crash debris or somebody else's; a
//!   sentinel there turns it into something cleanup tooling is authorized to
//!   `rm -rf`.
//!
//! # Why the bytes are pinned
//!
//! The content is not decorative: `guard-destructive-generic.sh` reads the
//! `# Branch: ` line back out of it to name the branch a destructive command
//! would affect. Until the rest of the create path moves, `worktree.sh` keeps
//! its own heredoc for the three call sites still in shell, so there are two
//! writers for one format. `tests/worktree_sparse_differential.rs` runs the
//! LIVE shell function (extracted from `defaults/scripts/worktree.sh`, not a
//! frozen copy) against [`content`] and fails on any byte of difference, so
//! the two cannot drift silently while both exist.

use std::path::Path;

/// The sentinel's file name, relative to the worktree root.
pub const FILE_NAME: &str = ".loom-managed";

/// The exact bytes `write_loom_sentinel` writes for `issue` / `branch`.
///
/// `issue` is the caller's string verbatim (the shell interpolated
/// `$ISSUE_NUMBER` as-is, leading zeros included), and neither value is
/// re-expanded — an unquoted heredoc expands `$BRANCH_NAME` once and does not
/// re-scan the result.
#[must_use]
pub fn content(issue: &str, branch: &str) -> String {
    format!(
        "# Loom-managed worktree marker\n\
         # Created by .loom/scripts/worktree.sh\n\
         # Issue: {issue}\n\
         # Branch: {branch}\n\
         # Removing this file makes Loom treat the worktree as user-owned and refuse\n\
         # to clean it up automatically.\n"
    )
}

/// Write (overwrite) `<worktree>/.loom-managed`.
///
/// A plain truncating write, like the shell's `cat >`, so it is idempotent and
/// self-heals a deleted sentinel (#3548).
///
/// # Errors
///
/// Propagates the write error. The shell's `cat >` failing under `set -e`
/// aborted the script, so callers treat this as fatal too rather than
/// reporting success over a worktree cleanup tooling will refuse to touch.
pub fn write(worktree: &Path, issue: &str, branch: &str) -> std::io::Result<()> {
    std::fs::write(worktree.join(FILE_NAME), content(issue, branch))
}
