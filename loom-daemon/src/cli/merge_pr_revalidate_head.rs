//! `loom-daemon merge-pr revalidate-head` (#8191 slice): the post-`--auto`-wait
//! re-read decision of `merge-pr.sh`'s `_revalidate_merge_guards`.
//!
//! # Protocol
//!
//! Reads the uncached PR payload on stdin. Prints a verdict line, then (for
//! `CLEAR` only) the current label names, one per line:
//!
//! - `LOOM-REVALIDATE MERGED`
//! - `LOOM-REVALIDATE NO-HEAD`
//! - `LOOM-REVALIDATE MOVED <fresh-sha>`
//! - `LOOM-REVALIDATE CLEAR` + labels
//!
//! Always exits 0. A caller that gets no `LOOM-REVALIDATE` line (missing or
//! older binary) must not read that as "head unchanged": `merge-pr.sh` falls
//! back to the retired jq predicate (the pre-verb decision, which still reads
//! the payload and refuses on NO-HEAD), so a host whose daemon predates this
//! verb degrades to the old behaviour instead of refusing every `--auto` merge.

use std::io::Read;

use anyhow::Result;

use loom_daemon::merge_pr::revalidate_head::{revalidate, Revalidation};

#[derive(clap::Args)]
pub(crate) struct RevalidateHeadArgs {
    /// The SHA the merge is gated on (empty = no precondition).
    #[arg(
        long,
        value_name = "SHA",
        default_value = "",
        allow_hyphen_values = true
    )]
    precondition_sha: String,
}

impl RevalidateHeadArgs {
    pub(crate) fn run(self) -> Result<()> {
        let mut payload = String::new();
        // Unreadable stdin reads as an empty payload, i.e. NO-HEAD (refuse).
        let _ = std::io::stdin().read_to_string(&mut payload);
        match revalidate(&payload, &self.precondition_sha) {
            Revalidation::Merged => println!("LOOM-REVALIDATE MERGED"),
            Revalidation::NoHead => println!("LOOM-REVALIDATE NO-HEAD"),
            Revalidation::Moved { fresh_sha } => println!("LOOM-REVALIDATE MOVED {fresh_sha}"),
            Revalidation::Clear { labels } => {
                println!("LOOM-REVALIDATE CLEAR");
                for l in labels {
                    println!("{l}");
                }
            }
        }
        Ok(())
    }
}
