//! `loom-daemon merge-pr worktree-preserve` (#6694/#6264, a slice of the
//! merge-pr port #8191): the remove-vs-preserve decision behind THREE of
//! `merge-pr.sh`'s post-merge worktree-cleanup call sites, consolidated from
//! three near-identical copies into one.
//!
//! # Protocol
//!
//! Takes every fact the decision needs as flags — no stdin, no forge or git
//! call happens here, both stay in the shell (`_issue_is_closed_for_cleanup`
//! and `branch_has_landed`, both already run by the time the shell calls this)
//! — and prints the action token on the first line, then one
//! `LEVEL<TAB>message` line per record to replay:
//!
//! | outcome | first line | exit |
//! |---|---|---|
//! | proceed with `_remove_loom_worktree` | `REMOVE` | 0 |
//! | leave the worktree in place | `PRESERVE` | 0 |
//!
//! # Fail direction
//!
//! This has no failure mode of its own — it is a pure function of its
//! arguments, so the shell wrapper's fault handling (any exit other than 0, or
//! a first line that is neither token — a missing/older binary, or a future
//! protocol change) is what fails toward preserve, matching
//! [`super::merge_pr_issue_close_gate`] and [`super::merge_pr_dirty_guard`] at
//! the same post-merge choke point: a skipped cleanup is always recoverable
//! later, a wrongly-removed worktree is not.

use anyhow::Result;

use loom_daemon::merge_pr::worktree_preserve::{decide, render, Context, Kind};

#[derive(clap::Args)]
pub(crate) struct WorktreePreserveArgs {
    /// Which of the three call sites this is — only affects wording.
    #[arg(long, value_enum)]
    kind: KindArg,

    /// The worktree path under consideration.
    #[arg(long, value_name = "PATH")]
    path: String,

    /// The repository root, for the preserve message's manual-removal hint.
    #[arg(long, value_name = "PATH")]
    repo_root: String,

    /// The merged PR's number.
    #[arg(long, value_name = "N")]
    pr: String,

    /// The merged PR's branch.
    #[arg(long, value_name = "BRANCH")]
    branch: String,

    /// The referenced issue number. Empty (the default) models the
    /// external-fork/ad-hoc-branch case, where no `feature/issue-<N>` match
    /// exists (#4186).
    #[arg(long, value_name = "N", default_value = "")]
    issue_num: String,

    /// `-n "$ISSUE_NUM" && ! _issue_is_closed_for_cleanup "$ISSUE_NUM"`,
    /// already answered by the caller.
    #[arg(long)]
    preserve_check: bool,

    /// `branch_has_landed`'s verdict (#7812/#6694), consulted only when
    /// `--preserve-check` is set.
    #[arg(long)]
    landed: bool,

    /// `$BRANCH_LANDED_VERDICT`, quoted in the preserve message.
    #[arg(long, value_name = "VERDICT", default_value = "")]
    landed_verdict: String,

    /// `$BRANCH_LANDED_EVIDENCE`, quoted in both #6694 messages.
    #[arg(long, value_name = "EVIDENCE", default_value = "")]
    landed_evidence: String,
}

/// `clap`'s view of [`Kind`] — a separate type because `clap::ValueEnum`
/// cannot be derived on a type this crate re-exports from `loom_daemon`
/// without also depending on `clap` there.
#[derive(Clone, Copy, clap::ValueEnum)]
enum KindArg {
    Default,
    Discovered,
    JudgePr,
}

impl From<KindArg> for Kind {
    fn from(value: KindArg) -> Self {
        match value {
            KindArg::Default => Kind::Default,
            KindArg::Discovered => Kind::Discovered,
            KindArg::JudgePr => Kind::JudgePr,
        }
    }
}

impl WorktreePreserveArgs {
    pub(crate) fn run(self) -> Result<()> {
        let ctx = Context {
            kind: self.kind.into(),
            path: &self.path,
            repo_root: &self.repo_root,
            pr_number: &self.pr,
            branch: &self.branch,
            issue_num: &self.issue_num,
            preserve_check: self.preserve_check,
            landed: self.landed,
            landed_verdict: &self.landed_verdict,
            landed_evidence: &self.landed_evidence,
        };
        let (action, lines) = decide(&ctx);
        print!("{}", render(action, &lines));
        Ok(())
    }
}
