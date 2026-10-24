//! `loom-daemon merge-pr post-merge-bookkeeping` (#8191, slice 2 of
//! `docs/migration/merge-pr-orchestrator-port.md`): the issue bookkeeping that
//! follows a confirmed merge — the partial-increment reset, the branch-derived
//! issue close and the closed-issue label strip — one pass per invocation.
//!
//! # Protocol
//!
//! | outcome | stdout | exit |
//! |---|---|---|
//! | the pass ran (or was correctly skipped) | `LEVEL<TAB>message` records, `LEVEL` one of `INFO` `WARNING` `SUCCESS`; then the terminator [`DONE`] | 0 |
//! | the PR's close targets are needed first | one `LOOM-NEED close-targets` line, NOTHING else, nothing done | 3 |
//!
//! Any other exit, or exit 0 without the terminator, means the verb did not
//! run (a missing or older binary). The shell then leaves every issue as it
//! was and names the manual remedy: the merge already succeeded and each pass
//! is advisory. See [`loom_daemon::merge_pr::post_merge_bookkeeping`].

use anyhow::Result;

use crate::merge_pr::post_merge_bookkeeping::{
    print_records, run, Facts, Inputs, Outcome, Pass, SystemHost, NEED_EXIT,
};

#[derive(clap::Args)]
pub(crate) struct PostMergeBookkeepingArgs {
    /// Which pass to run.
    #[arg(long, value_enum)]
    step: Pass,

    /// `owner/repo`.
    #[arg(long, value_name = "NWO")]
    repo: String,

    /// The merged PR's number.
    #[arg(long, value_name = "N")]
    pr: String,

    /// The merged PR's head branch (`branch-close`).
    #[arg(long, value_name = "BRANCH", default_value = "")]
    branch: String,

    /// `$PARTIAL_CONFLICT_ISSUES`: space-separated issue numbers.
    #[arg(long, value_name = "NUMS", default_value = "")]
    conflicted: String,

    /// `$PARTIAL_OPEN_BEFORE_MERGE`: space-separated issue numbers.
    #[arg(long, value_name = "NUMS", default_value = "")]
    open_before_merge: String,

    /// Path to `lib/forge-helpers.sh`, whose rate-limit-safe writers carry out
    /// the mutations.
    #[arg(long, value_name = "PATH")]
    forge_helpers: String,

    /// `forge_pr_close_targets`' output. Pass it (empty included) once fetched.
    #[arg(long, value_name = "TEXT", allow_hyphen_values = true)]
    close_targets: Option<String>,
}

impl PostMergeBookkeepingArgs {
    pub(crate) fn run(self) -> Result<()> {
        // The PR body is read from stdin, never argv: it is arbitrary text.
        let mut pr_body = String::new();
        if std::io::Read::read_to_string(&mut std::io::stdin(), &mut pr_body).is_err() {
            eprintln!("merge-pr post-merge-bookkeeping: could not read the PR body from stdin");
            std::process::exit(2);
        }
        let inputs = Inputs {
            repo: self.repo,
            pr_number: self.pr,
            pr_branch: self.branch,
            pr_body,
            conflicted: self.conflicted,
            open_before_merge: self.open_before_merge,
        };
        let facts = Facts {
            close_targets: self.close_targets,
        };
        let host = SystemHost {
            forge_helpers: self.forge_helpers,
        };
        match run(self.step, &inputs, &facts, &host) {
            Outcome::Need(need) => {
                println!("LOOM-NEED {}", need.token());
                std::process::exit(NEED_EXIT);
            }
            Outcome::Done(records) => {
                print_records(&records);
                Ok(())
            }
        }
    }
}
