//! `loom-daemon merge-pr delete-branch` (#8191): `merge-pr.sh`'s
//! `_maybe_delete_local_branch` (#4100/#5015/#7812), now a thin call into the
//! same [`loom_daemon::worktree_cli::branch_delete`] rule `worktree.sh
//! remove` already uses (#8195 slice 3) — one implementation instead of two.
//!
//! # Why a text protocol instead of icon-formatted output
//!
//! `worktree.sh remove` was replaced wholesale, so its Rust verb can print
//! straight to stdout in `Out`'s own icon/color style. `merge-pr.sh` was
//! not: it interleaves this guard's messages with dozens of others through
//! its own un-iconned `info`/`warning`/`success` shell functions, and its
//! retained test suite (`test-merge-pr-local-branch-cleanup.sh`) stubs
//! exactly those three names and asserts on the text they were called with.
//!
//! So this subcommand emits one `LEVEL<TAB>message` line per decision to
//! stdout — `INFO`, `WARNING`, or `SUCCESS` — and the shell wrapper replays
//! each line through its own logging function, preserving the exact
//! operator-visible text and coloring merge-pr.sh has always used.
//!
//! # Exit code
//!
//! Always 0. The rule it wraps "never fails the cleanup pipeline" (its own
//! doc comment) — a branch that could not be deleted is reported via a
//! `WARNING` line, not a failure. A non-zero exit from this subcommand
//! therefore means only one thing to the caller: the guard itself could not
//! run at all (missing/stale binary), which `lib/script-helper.sh`'s
//! `LOOM_SCRIPT_HELPER_MISSING_RC` convention already treats as "warn and
//! proceed" for an advisory-only guard like this one — never as "checked,
//! clean".

use std::path::PathBuf;

use anyhow::Result;

use loom_daemon::worktree_cli::branch_delete::{self, DeleteContext};
use loom_daemon::worktree_cli::wip::Sink;

#[derive(clap::Args)]
pub(crate) struct DeleteBranchArgs {
    /// The repository root `_maybe_delete_local_branch` operated against
    /// (`$REPO_ROOT` in the shell).
    #[arg(long, value_name = "PATH")]
    repo_root: PathBuf,

    /// The local branch to consider deleting.
    #[arg(long, value_name = "BRANCH")]
    branch: String,

    /// The merged PR's `head.sha`, when the caller has one. A tip matching it
    /// is landed with no forge round-trip. Empty (the default) mirrors the
    /// pre-#4100 `--worktree-path` caller shape, which had none.
    #[arg(long, value_name = "SHA", default_value = "")]
    expected_head_sha: String,

    /// The repo's default branch, when it resolved. Omitting it disables both
    /// the default-branch guard's named arm and the #5015 auto-cleanup — the
    /// conservative direction in both cases, matching an empty
    /// `$DEFAULT_BRANCH_NAME` in the shell.
    #[arg(long, value_name = "BRANCH")]
    default_branch: Option<String>,

    /// `--no-cleanup-primary` in the shell (`CLEANUP_PRIMARY_CHECKOUT=false`):
    /// opt out of the #5015 primary-checkout auto-cleanup.
    #[arg(long)]
    no_cleanup_primary: bool,
}

/// Collects [`Sink`] calls as `LEVEL<TAB>message` lines and prints each one
/// immediately, so ordering matches exactly what the underlying rule did.
struct ReplaySink;

/// Render one message as protocol lines.
///
/// A message is not guaranteed to be one line: the `-D` refusal arm
/// interpolates git's raw stderr, which can span several. A bare continuation
/// line would reach the shell's `read -r level text` with no tab, be taken as
/// an unknown LEVEL, and have its text silently dropped. So every line of the
/// message carries the message's own level: the operator sees each line, at
/// the right severity, as consecutive `warning`/`info`/`success` calls. A git
/// ref name cannot contain a newline or tab (`git check-ref-format`), so the
/// only multi-line source is git's own output.
fn render(level: &str, msg: &str) -> String {
    let mut s = String::new();
    let mut lines = msg.lines().peekable();
    if lines.peek().is_none() {
        s.push_str(level);
        s.push_str("\t\n");
    }
    for line in lines {
        s.push_str(level);
        s.push('\t');
        s.push_str(line);
        s.push('\n');
    }
    s
}

impl Sink for ReplaySink {
    fn info(&self, msg: &str) {
        print!("{}", render("INFO", msg));
    }
    fn warning(&self, msg: &str) {
        print!("{}", render("WARNING", msg));
    }
    fn success(&self, msg: &str) {
        print!("{}", render("SUCCESS", msg));
    }
}

#[cfg(test)]
mod tests {
    use super::render;

    #[test]
    fn a_single_line_message_is_one_protocol_line() {
        assert_eq!(render("INFO", "hello"), "INFO\thello\n");
    }

    #[test]
    fn every_line_of_a_multi_line_message_keeps_its_level() {
        // The `-D` refusal arm's shape: a prefix plus git's own multi-line
        // stderr. No line may reach the shell without a LEVEL<TAB> prefix.
        let got = render(
            "WARNING",
            "Could not delete local branch 'b': error: one\nhint: two\r\nhint: three",
        );
        assert_eq!(
            got,
            "WARNING\tCould not delete local branch 'b': error: one\nWARNING\thint: two\nWARNING\thint: three\n"
        );
        assert!(got.lines().all(|l| l.starts_with("WARNING\t")));
    }

    #[test]
    fn an_empty_message_still_emits_its_level() {
        assert_eq!(render("SUCCESS", ""), "SUCCESS\t\n");
    }
}

impl DeleteBranchArgs {
    pub(crate) fn run(self) -> Result<()> {
        let ctx = DeleteContext {
            repo_root: &self.repo_root,
            default_branch: self.default_branch.as_deref(),
            cleanup_primary_checkout: !self.no_cleanup_primary,
        };
        let sink = ReplaySink;
        let _outcome = branch_delete::maybe_delete_local_branch(
            &ctx,
            &sink,
            &self.branch,
            &self.expected_head_sha,
        );
        Ok(())
    }
}
