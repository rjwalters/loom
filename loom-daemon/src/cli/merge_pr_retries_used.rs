//! `loom-daemon merge-pr retries-used` (#8191 slice): the backoff-attempt count
//! for the merge-admission telemetry record, parsed from the stale-mergeable
//! recheck's reason text.
//!
//! # Protocol
//!
//! Prints exactly one line: the figure. Exits 0. The figure is telemetry only,
//! so the shell falls back to the configured budget when this verb is missing,
//! older, or exits non-zero: a lost figure can never refuse or alter a merge.

use anyhow::Result;

use loom_daemon::merge_pr::retries_used::retries_used;

#[derive(clap::Args)]
pub(crate) struct RetriesUsedArgs {
    /// The recheck decision's reason text (the part after `<action>:`).
    #[arg(long, value_name = "TEXT", allow_hyphen_values = true)]
    reason: String,

    /// The configured retry budget, reported when the reason names no attempt.
    #[arg(long, value_name = "N", allow_hyphen_values = true)]
    configured: String,
}

impl RetriesUsedArgs {
    pub(crate) fn run(self) -> Result<()> {
        println!("{}", retries_used(&self.reason, &self.configured));
        Ok(())
    }
}
