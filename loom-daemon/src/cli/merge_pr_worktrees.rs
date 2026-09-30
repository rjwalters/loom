//! `loom-daemon merge-pr worktree-*` (#8191 slice) — the porcelain parsing
//! behind `merge-pr.sh`'s post-merge worktree and branch cleanup, plus the
//! `--worktree-path` PRE-flight registered-worktree check ([`WorktreeContainsArgs`]).
//!
//! # Why the porcelain arrives on stdin
//!
//! The `git worktree list --porcelain` invocation stays in the shell. Only the
//! *parse* moves, which is where all three of this family's shipped defects
//! were (#3671 double-print, #3717 space-truncated paths, #4171 primary-vs-
//! linked confusion). Keeping the `git` call in `merge-pr.sh` means this slice
//! cannot change WHICH repository is inspected or how `git` is invoked — the
//! diff is exactly the parser and nothing else, which is what makes the
//! differential against the frozen `awk` a complete statement about it.
//!
//! # Output contract
//!
//! One line on stdout, or nothing. `printf '%s'`-shaped consumers in the shell
//! read it through `$(...)`, which strips the trailing newline either way.
//!
//! | outcome | stdout | exit |
//! |---|---|---|
//! | a match | the path / branch short-name | 0 |
//! | parsed, no match | empty | 0 |
//! | could not run at all | — | non-zero (clap/exec failure) |
//!
//! **Empty-at-exit-0 is an answer, not an error**, and the two must stay
//! distinguishable. "No worktree is checked out on that branch" and "the
//! parser never ran" lead to opposite decisions at the one call site that
//! matters: `_remove_loom_worktree`'s #3710 guard reads an empty
//! `worktree-primary` as "the target is not the primary checkout" and proceeds
//! to `git worktree remove --force`. The shell therefore refuses the removal
//! when the invocation itself fails, and only that call site needs to — the
//! other two degrade to "delete nothing", which is already the safe side.
//!
//! That is also why this is not one subcommand with a `--query` flag: three
//! verbs means a stale binary that predates this slice fails on the verb (exit
//! 2, "unrecognized subcommand") rather than on a flag value it might have
//! ignored.

use std::io::Read;

use anyhow::Result;

use loom_daemon::merge_pr::worktrees;

/// Read `git worktree list --porcelain` from stdin.
///
/// Lossy rather than fatal, matching [`super::merge_pr_refs`]: a worktree path
/// is whatever bytes the filesystem holds, and refusing to parse the list
/// because one path is not UTF-8 would fail cleanup for a reason unrelated to
/// cleanup. `awk` processed those bytes too.
fn porcelain() -> Result<String> {
    let mut raw = Vec::new();
    std::io::stdin().read_to_end(&mut raw)?;
    Ok(String::from_utf8_lossy(&raw).into_owned())
}

#[derive(clap::Args)]
pub(crate) struct WorktreePrimaryArgs {}

impl WorktreePrimaryArgs {
    pub(crate) fn run(self) -> Result<()> {
        if let Some(path) = worktrees::primary_path(&porcelain()?) {
            println!("{path}");
        }
        Ok(())
    }
}

#[derive(clap::Args)]
pub(crate) struct WorktreeBranchForArgs {
    /// The worktree path to look up, already canonicalised by the caller
    /// (`cd … && pwd -P`) because `git` prints canonical paths and the
    /// comparison is exact, as it was in `awk`.
    #[arg(long, value_name = "PATH")]
    path: String,
}

impl WorktreeBranchForArgs {
    pub(crate) fn run(self) -> Result<()> {
        if let Some(branch) = worktrees::branch_for_path(&porcelain()?, &self.path) {
            println!("{branch}");
        }
        Ok(())
    }
}

#[derive(clap::Args)]
pub(crate) struct WorktreeFindByBranchArgs {
    /// The branch SHORT name (`feature/issue-42`); matched as
    /// `refs/heads/<name>`.
    #[arg(long, value_name = "BRANCH")]
    branch: String,
}

impl WorktreeFindByBranchArgs {
    pub(crate) fn run(self) -> Result<()> {
        if let Some(path) = worktrees::find_by_branch(&porcelain()?, &self.branch) {
            println!("{path}");
        }
        Ok(())
    }
}

/// `--worktree-path`'s PRE-flight registered-worktree check. Unlike the three
/// verbs above, the retired `awk` this replaces communicated its answer
/// through exit status alone — no line printed either way — so this does too:
/// exit 0 = registered, 1 = parsed and not registered. `merge-pr.sh` treats
/// any OTHER exit (missing binary, or one predating this verb) as "the check
/// did not run" and decides its own fallback from there, exactly as it does
/// for every other guard in this family.
#[derive(clap::Args)]
pub(crate) struct WorktreeContainsArgs {
    /// The path to look up, already canonicalised by the caller (`cd … &&
    /// pwd -P`), matching the other two path-keyed verbs above.
    #[arg(long, value_name = "PATH")]
    path: String,
}

impl WorktreeContainsArgs {
    pub(crate) fn run(self) -> Result<()> {
        if worktrees::contains_path(&porcelain()?, &self.path) {
            std::process::exit(0);
        }
        std::process::exit(1);
    }
}
