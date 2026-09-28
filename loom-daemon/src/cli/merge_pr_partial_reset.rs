//! `loom-daemon merge-pr partial-reset` (#3667/#4569, a slice of the merge-pr
//! port #8191): the decision half of `merge-pr.sh`'s
//! `_reset_one_partial_issue`.
//!
//! # Protocol
//!
//! Reads the issue's fresh `gh api repos/<nwo>/issues/<n>` body on stdin and
//! prints the ordered steps the shell performs, one per line:
//!
//! | line | the shell does |
//! |---|---|
//! | `INFO<TAB>text` | `info "$text"` |
//! | `WARNING<TAB>text` | `warning "$text"` |
//! | `REOPEN` | reopen + premature-close comment; on failure, warn and STOP |
//! | `SWAP` | `loom:building` -> `loom:issue` + partial-increment comment |
//!
//! No lines at all is the silent skip for a PR served by the issues endpoint.
//!
//! # Exit code
//!
//! 0 whenever a plan was printed, 2 when stdin could not be read. The step
//! this serves is post-merge and best-effort — the merge already happened —
//! so the shell wrapper treats ANY non-zero exit (including a binary that
//! predates this verb) as "the reset did not run": a loud warning naming the
//! manual label swap, and no mutation. Never a guess: an unread plan is not
//! replayed as an empty one.
//!
//! The issue body arrives on stdin, never in argv: it is forge-controlled
//! text, and an argument vector is the wrong place for it.

use anyhow::Result;
use loom_daemon::merge_pr::partial_reset::{plan, render, IssueView, PreMerge};
use std::io::{Read, Write};

#[derive(clap::Args)]
pub(crate) struct PartialResetArgs {
    /// The referenced issue number, for log text.
    #[arg(long, value_name = "N")]
    issue: String,

    /// The merged PR's number, for log text.
    #[arg(long, value_name = "N")]
    pr: String,

    /// `owner/repo`, for the manual-reopen hint in the log text.
    #[arg(long, value_name = "NWO", default_value = "")]
    repo: String,

    /// The pre-merge guard found a closing reference to this issue on the PR
    /// (`$PARTIAL_CONFLICT_ISSUES` membership) — a close is attributable to
    /// this merge and is reverted.
    #[arg(long)]
    conflicted: bool,

    /// The issue was open when the pre-merge guard ran
    /// (`$PARTIAL_OPEN_BEFORE_MERGE` membership).
    #[arg(long)]
    open_before_merge: bool,
}

impl PartialResetArgs {
    pub(crate) fn run(self) -> Result<()> {
        let mut body = String::new();
        if std::io::stdin().read_to_string(&mut body).is_err() {
            // An unreadable body is not the same as `{}` — say so rather than
            // planning a "not open" skip over a read that failed.
            eprintln!("merge-pr partial-reset: could not read the issue body from stdin");
            std::process::exit(2);
        }
        let view = IssueView::from_json(&body);
        let pre = PreMerge {
            conflicted: self.conflicted,
            open_before_merge: self.open_before_merge,
        };
        let steps = plan(&self.issue, &self.pr, &self.repo, &view, pre);
        let mut out = std::io::stdout().lock();
        out.write_all(render(&steps).as_bytes())?;
        out.flush()?;
        Ok(())
    }
}
