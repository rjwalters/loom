//! `loom-daemon merge-pr redate-checks` (#8508).
//!
//! The automated remedy for the #8248 required-check-freshness guard: when
//! that guard blocks a merge and the fleet has no `actions:write` to re-run
//! the stale check directly, push a tree-identical no-op commit instead — any
//! push re-triggers every `pull_request` CI run on the new head, which is
//! exactly `stale_checks::stale_message`'s own documented remedy.
//!
//! | outcome | stdout | exit |
//! |---|---|---|
//! | pushed the re-date commit | `LOOM-REDATE-PUSHED sha=<new-sha>` | 0 |
//! | branch already moved past `--expected-head-sha` | the reason | 3 |
//! | could not read/write the forge state needed | the reason | 1 |
//!
//! Exit 3 mirrors `merge-pr.sh`'s own #5579 contract: it is NOT a failure —
//! the branch moving out from under a stale-evidence remedy means either a
//! human already acted or a fresh push already landed, so the caller should
//! simply re-evaluate the PR fresh on the next pass rather than report an
//! error or retry this push.

use anyhow::Result;
use loom_daemon::merge_pr::redate::{redate, RedateOutcome};

#[derive(clap::Args)]
pub(crate) struct RedateChecksArgs {
    /// The PR number, used only to compose the commit message.
    #[arg(long, value_name = "N")]
    pr: String,

    /// The repository as owner/repo.
    #[arg(long, value_name = "OWNER/REPO")]
    repo: String,

    /// The PR's head branch name (not the PR number) — the ref this pushes
    /// onto.
    #[arg(long, value_name = "BRANCH")]
    branch: String,

    /// The head SHA the blocked merge attempt actually gated on. If the
    /// branch's live tip no longer matches this, nothing is pushed (see exit
    /// 3 above).
    #[arg(long, value_name = "SHA")]
    expected_head_sha: String,
}

impl RedateChecksArgs {
    pub(crate) fn run(self) -> Result<()> {
        match redate(&self.repo, &self.branch, &self.expected_head_sha, &self.pr) {
            RedateOutcome::Pushed { new_sha } => {
                println!("LOOM-REDATE-PUSHED sha={new_sha}");
                std::process::exit(0);
            }
            RedateOutcome::HeadMoved { current } => {
                println!(
                    "PR #{}'s branch '{}' already moved to {current} — expected {} \
(the caller's stale-evidence gate is no longer current). Not pushing; re-evaluate the \
PR fresh next pass.",
                    self.pr, self.branch, self.expected_head_sha
                );
                std::process::exit(3);
            }
            RedateOutcome::Failed(why) => {
                println!(
                    "Could not push a re-date commit for PR #{} branch '{}': {why}",
                    self.pr, self.branch
                );
                std::process::exit(1);
            }
        }
    }
}
