//! `loom-daemon merge-pr mergeable-recheck-run` (#6104, slice of #8191): the
//! whole stale-cached-mergeable recheck loop in one invocation — backoff,
//! uncached re-reads, ref vet, fetch, `git merge-tree` — printing exactly one
//! `<action>:<reason>` line (see [`loom_daemon::merge_pr::mergeable_recheck_run`]).
//! The caller treats exit != 0 or an unrecognised first token as "did not run".

use anyhow::Result;

use crate::merge_pr::mergeable_recheck_run::{run, Inputs, SystemHost};

#[derive(clap::Args)]
pub(crate) struct MergeableRecheckRunArgs {
    /// `owner/repo`.
    #[arg(long, value_name = "NWO")]
    repo: String,

    /// PR number.
    #[arg(long, value_name = "N")]
    pr: String,

    /// The `gh` command (`gh` or the `gh-cached` wrapper).
    #[arg(long, value_name = "CMD", default_value = "gh")]
    gh_cmd: String,

    /// The PR's base ref (`.base.ref`); may be empty.
    #[arg(long, value_name = "REF", default_value = "")]
    base_ref: String,

    /// The PR's head ref; may be empty.
    #[arg(long, value_name = "REF", default_value = "")]
    head_ref: String,

    /// Checkout the fetch and merge-tree run in.
    #[arg(long, value_name = "DIR")]
    repo_root: String,

    /// Recheck budget.
    #[arg(long, value_name = "N", default_value_t = 3)]
    retries: u32,

    /// Seconds slept before each re-read.
    #[arg(long, value_name = "SECS", default_value_t = 3)]
    delay: u64,

    /// Path to `lib/forge-helpers.sh`.
    #[arg(long, value_name = "PATH")]
    forge_helpers: String,
}

impl MergeableRecheckRunArgs {
    pub(crate) fn run(self) -> Result<()> {
        let inputs = Inputs {
            pr_number: self.pr.clone(),
            base_ref: self.base_ref,
            head_ref: self.head_ref,
            retries: self.retries,
            delay_secs: self.delay,
        };
        let host = SystemHost {
            repo: self.repo,
            pr_number: self.pr,
            gh_cmd: self.gh_cmd,
            repo_root: self.repo_root,
            forge_helpers: self.forge_helpers,
        };
        println!("{}", run(&inputs, &host));
        Ok(())
    }
}
