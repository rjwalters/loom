//! `loom-daemon merge-pr closed-building` (#6199, a slice of the merge-pr
//! port #8191): the decision half of `merge-pr.sh`'s
//! `_strip_one_closed_issue_building_label`.
//!
//! # Protocol
//!
//! Reads the issue's fresh `gh api repos/<nwo>/issues/<n>` body on stdin and
//! prints exactly one line:
//!
//! | line | the shell does |
//! |---|---|
//! | `STRIP` | `forge_gh_remove_label_rl_safe … loom:building`, then log |
//! | `SKIP<TAB><reason>` | nothing at all |
//!
//! The reason token (`is-pull-request`, `not-closed`, `not-building`) is for
//! running the verb by hand and for the differential harness; `merge-pr.sh`
//! discards it, because the retired function's skips were silent and the port
//! keeps its stdout byte-identical.
//!
//! # Exit code
//!
//! 0 whenever a decision was printed, 2 when stdin could not be read. The pass
//! this serves is post-merge and best-effort — the merge already happened — so
//! the shell wrapper treats any non-zero exit, and any stdout that is not one
//! of the two recognised lines, as "the cleanup did not run": a warning naming
//! the manual removal, and no mutation. Silence is never read as `SKIP`; a
//! decision not taken is not the same as a decision to do nothing.
//!
//! The issue body arrives on stdin, never in argv: it is forge-controlled
//! text, and an argument vector is the wrong place for it.

use anyhow::Result;
use loom_daemon::merge_pr::closed_building::{plan, render};
use loom_daemon::merge_pr::partial_reset::IssueView;
use std::io::{Read, Write};

#[derive(clap::Args)]
pub(crate) struct ClosedBuildingArgs {}

impl ClosedBuildingArgs {
    pub(crate) fn run(self) -> Result<()> {
        let mut body = String::new();
        if std::io::stdin().read_to_string(&mut body).is_err() {
            // An unreadable body is not the same as `{}`: `{}` is a decidable
            // "not closed", a failed read is no decision at all.
            eprintln!("merge-pr closed-building: could not read the issue body from stdin");
            std::process::exit(2);
        }
        let decision = plan(&IssueView::from_json(&body));
        let mut out = std::io::stdout().lock();
        out.write_all(render(decision).as_bytes())?;
        out.flush()?;
        Ok(())
    }
}
