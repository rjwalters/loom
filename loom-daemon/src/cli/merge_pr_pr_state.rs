//! `loom-daemon merge-pr pr-state` (#8191 slice): the already-merged /
//! closed-unmerged terminal-state gate at the top of `merge-pr.sh`.
//!
//! # Protocol
//!
//! Prints exactly one line, `LOOM-PR-STATE MERGED|CLOSED|OPEN`, and exits 0.
//! The shell acts on MERGED (exit 0), CLOSED (refuse) or OPEN (proceed); any
//! other outcome, including a missing or older daemon, falls back to the
//! retired shell PR_MERGED/PR_STATE predicate, so exit codes never change.

use anyhow::Result;

use loom_daemon::merge_pr::pr_state::classify;

#[derive(clap::Args)]
pub(crate) struct PrStateArgs {
    /// The PR's `.state` as `jq -r` rendered it (`open`, `closed`, `null`).
    #[arg(long, value_name = "STATE", allow_hyphen_values = true)]
    state: String,

    /// The PR's `.merged` as `jq -r` rendered it (`true`, `false`, `null`).
    #[arg(long, value_name = "BOOL", allow_hyphen_values = true)]
    merged: String,
}

impl PrStateArgs {
    pub(crate) fn run(self) -> Result<()> {
        println!("{}", classify(&self.state, &self.merged).token());
        Ok(())
    }
}
