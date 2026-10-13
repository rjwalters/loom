//! `loom-daemon merge-pr closed-by-merge` (#8942): did THIS PR's merge close
//! the issue? The gate in front of Champion Step 4's negated-reference reopen
//! (#1057), which used to fire on `state = CLOSED` alone.
//!
//! # Protocol
//!
//! `--print-query` prints the GraphQL document to fetch the facts with and
//! exits 0. Otherwise the fetched facts arrive on stdin — that query's
//! response, and/or REST issue + pull objects (all a Gitea caller has) — and
//! exactly one line is printed, naming the issue in every outcome:
//! `LOOM-CLOSED-BY-MERGE <token> issue #N: <why>`.
//!
//! | answer | token | exit |
//! |---|---|---|
//! | this merge closed it | `YES` | 0 |
//! | not closed, closed before the merge, or closed by something else | `NO` | 1 |
//! | no closer readable and closed too long after the merge to tie to it | `UNATTRIBUTED` | 1 |
//! | facts missing, unparseable, or about another issue/PR; stdin unreadable | `UNANSWERED` | 3 |
//!
//! Only exit 0 authorizes the reopen. Exit 2 is never produced here: it is
//! `clap`'s "unrecognized subcommand" on a binary predating this verb, and the
//! caller treats every code other than 0 and 1 as "could not answer" — no
//! reopen, and a line saying so. The decision lives in
//! [`loom_daemon::merge_pr::closed_by_merge`]; this verb makes no forge call
//! and no write.
//!
//! The facts arrive on stdin, never in argv: they are forge-controlled text.

use anyhow::Result;
use loom_daemon::merge_pr::closed_by_merge::{decide, query, render, Facts, Verdict};
use std::io::{Read, Write};

#[derive(clap::Args)]
pub(crate) struct ClosedByMergeArgs {
    /// The issue whose close is being attributed.
    #[arg(long, value_name = "N")]
    issue: u64,

    /// The merged PR the close would be attributed to.
    #[arg(long, value_name = "N")]
    pr: u64,

    /// Print the GraphQL query that fetches the facts for `--issue`/`--pr`
    /// (variables left to the caller: `$o` owner, `$r` repo) and exit,
    /// reading nothing.
    #[arg(long)]
    print_query: bool,
}

impl ClosedByMergeArgs {
    pub(crate) fn run(self) -> Result<()> {
        let mut out = std::io::stdout().lock();
        if self.print_query {
            writeln!(out, "{}", query(self.issue, self.pr))?;
            return Ok(());
        }
        let mut raw = Vec::new();
        let verdict = if std::io::stdin().read_to_end(&mut raw).is_err() {
            // An unreadable stream is not the same as an empty one, but both
            // are "no facts" — and neither may be replayed as a "no".
            Verdict::Unanswered("stdin could not be read".into())
        } else {
            // Lossy rather than fatal on bad UTF-8: forge text, like every
            // other merge-pr stdin protocol.
            decide(self.issue, self.pr, &Facts::from_json(&String::from_utf8_lossy(&raw)))
        };
        out.write_all(render(self.issue, self.pr, &verdict).as_bytes())?;
        out.flush()?;
        drop(out);
        std::process::exit(verdict.exit_code())
    }
}
