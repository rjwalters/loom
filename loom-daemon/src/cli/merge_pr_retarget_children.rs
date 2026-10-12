//! `loom-daemon merge-pr retarget-children` (#9372): the gate in front of
//! `merge-pr.sh`'s post-merge remote-branch delete. See
//! [`loom_daemon::merge_pr::retarget_children`] for the failure it closes.
//!
//! # Exit codes
//!
//! | outcome | stdout | exit |
//! |---|---|---|
//! | no open PR targets the branch | nothing | 0 |
//! | every open child retargeted, re-check empty | `INFO<TAB>…` per child | 0 |
//! | keep the branch (any uncertainty) | `INFO` lines, then one `WARNING<TAB>…` | 1 |
//!
//! Exit 0 is the ONLY answer that authorizes the delete. The shell treats any
//! other exit — including clap's 2 from a binary that predates this verb — as
//! "keep the branch", because the delete is irreversible and the branch is
//! cheap to leave behind.

use anyhow::Result;

use loom_daemon::merge_pr::retarget_children::{prepare_delete, GhForge, Level, Verdict};

#[derive(clap::Args)]
pub(crate) struct RetargetChildrenArgs {
    /// The repository as owner/repo (`$REPO_NWO`).
    #[arg(long, value_name = "OWNER/REPO")]
    repo: String,

    /// The merged parent's head branch (`$PR_BRANCH`) about to be deleted.
    #[arg(long, value_name = "BRANCH")]
    parent_branch: String,

    /// Where open children are moved: the merged parent's own base branch.
    /// Empty means unknown, which keeps the branch if any child exists.
    #[arg(long, value_name = "BRANCH", default_value = "")]
    base: String,
}

impl RetargetChildrenArgs {
    pub(crate) fn run(self) -> Result<()> {
        let report = prepare_delete(&GhForge, &self.repo, &self.parent_branch, &self.base);
        for (level, msg) in &report.lines {
            let tag = match level {
                Level::Info => "INFO",
                Level::Warning => "WARNING",
            };
            // Messages are single-line by construction, but a forge error
            // interpolated into one must not smuggle an untagged line through.
            println!("{tag}\t{}", msg.replace(['\n', '\r'], " "));
        }
        if report.verdict == Verdict::Keep {
            std::process::exit(1);
        }
        Ok(())
    }
}
