//! `loom-daemon merge-pr version-policy` (#7827/#8284, a slice of #8191):
//! `merge-pr.sh`'s `_check_defaults_version_bump_collision`.
//!
//! # Exit codes
//!
//! | outcome | stdout | exit |
//! |---|---|---|
//! | pass, skip, or `--dry-run` report | zero or more `WARNING<TAB>…` lines | 0 |
//! | confirmed forbidden version edit | `WARNING` lines, then `BLOCK<TAB>…` lines | 1 |
//!
//! # Why an unrunnable guard is a warning here, not a refusal
//!
//! Every other merge GATE in this family fails closed when its binary is
//! missing (`verdict-contradiction`, `stale-checks`, `loom-pr-guard`). This one
//! does not, and that is the guard's own documented contract rather than a
//! convenience: since #7827 every guard-internal fault — a failed fetch, an
//! unresolvable head, unknown ancestry, a checker that exits 2 — has skipped
//! with a warning, and only a CONFIRMED version edit refuses. A binary that
//! predates this verb is one more guard fault of exactly that kind. The
//! policy's primary enforcement is CI's `defaults-version-bump-check` job,
//! which runs the same checker on every PR; this is its merge-time echo. So
//! the shell wrapper treats any exit other than 0/1 — clap's 2 for an unknown
//! verb, 126/127 for a missing binary — as "did not run, skipping", loudly,
//! and never as "blocked" (which would stall every merge on a host whose
//! daemon lags one release) nor as a silent pass.
//!
//! # The protocol
//!
//! One `LEVEL<TAB>line` per line, the shape
//! [`super::merge_pr_delete_branch`] established: `merge-pr.sh` and its
//! retained suite (`test-merge-pr-defaults-version-bump-collision.sh`) route
//! messages through their own `warning`/`error`, so this subcommand never
//! prints color or prefixes itself. A multi-line message (the checker's own
//! output is interpolated into both the fault warning and the refusal) is
//! split on `\n` exactly, every line carrying its level. The wrapper replays
//! each `WARNING` line through `warning` and joins the `BLOCK` lines back into
//! the single message it hands to `error`.

use std::path::PathBuf;

use anyhow::Result;

use loom_daemon::merge_pr::version_policy::{evaluate, invalid_ref_message, Inputs, Verdict};

#[derive(clap::Args)]
pub(crate) struct VersionPolicyArgs {
    /// The checkout whose on-disk checker is the default oracle
    /// (`$REPO_ROOT`).
    #[arg(long, value_name = "PATH")]
    repo_root: PathBuf,

    /// The repository's default branch (`$DEFAULT_BRANCH_NAME`). Empty =
    /// unknown, which skips the guard.
    #[arg(long, value_name = "BRANCH", default_value = "")]
    default_branch: String,

    /// The PR's head branch (`$PR_BRANCH`). Empty skips the guard.
    #[arg(long, value_name = "BRANCH", default_value = "")]
    branch: String,

    /// The PR's head SHA (`$PR_HEAD_SHA`). Empty skips the guard.
    #[arg(long, value_name = "SHA", default_value = "")]
    head_sha: String,

    /// The PR number, for the operator-facing text.
    #[arg(long, value_name = "N", default_value = "")]
    pr: String,

    /// Report a would-be block as a warning and exit 0.
    #[arg(long)]
    dry_run: bool,
}

/// Render one message as protocol lines, splitting on `\n` exactly so a
/// trailing empty line survives (the retired `warning "$msg"` printed it).
pub(crate) fn render(level: &str, msg: &str) -> String {
    let mut s = String::new();
    for line in msg.split('\n') {
        s.push_str(level);
        s.push('\t');
        s.push_str(line);
        s.push('\n');
    }
    s
}

impl VersionPolicyArgs {
    pub(crate) fn run(self) -> Result<()> {
        let report = evaluate(&Inputs {
            repo_root: &self.repo_root,
            default_branch: &self.default_branch,
            branch: &self.branch,
            head_sha: &self.head_sha,
            pr_number: &self.pr,
            dry_run: self.dry_run,
        });
        for w in &report.warnings {
            print!("{}", render("WARNING", w));
        }
        match report.verdict {
            Verdict::Pass => Ok(()),
            Verdict::Block(msg) => {
                print!("{}", render("BLOCK", &msg));
                std::process::exit(1);
            }
            // #9106/#9479. A refusal, not a guard fault, so it takes the
            // BLOCK channel rather than the WARNING one even under
            // `--dry-run`: the name was never handed to git, so there is no
            // comparison to report and nothing to preview.
            Verdict::InvalidRef(e) => {
                print!("{}", render("BLOCK", &invalid_ref_message(&self.pr, &e)));
                std::process::exit(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::render;

    #[test]
    fn every_line_carries_its_level_including_blank_ones() {
        assert_eq!(render("BLOCK", "a\n\nb"), "BLOCK\ta\nBLOCK\t\nBLOCK\tb\n");
    }

    #[test]
    fn a_trailing_newline_is_a_trailing_empty_line() {
        assert_eq!(render("WARNING", "x:\n"), "WARNING\tx:\nWARNING\t\n");
    }
}
