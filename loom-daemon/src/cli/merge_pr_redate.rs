//! `loom-daemon merge-pr redate-checks` (#8508).
//!
//! The automated remedy for the #8248 required-check-freshness guard: when
//! that guard blocks a merge and the fleet has no `actions:write` to re-run
//! the stale check directly, push a tree-identical no-op commit instead — any
//! push re-triggers every `pull_request` CI run on the new head, which is
//! exactly `stale_checks::stale_message`'s own documented remedy. When the
//! remedy has already run against this exact head and the guard STILL blocks,
//! nothing automated is making progress, so the PR is escalated to a durable
//! `loom:operator` hold instead of re-pushing forever.
//!
//! | outcome | stdout | exit |
//! |---|---|---|
//! | pushed the re-date commit | `LOOM-REDATE-PUSHED sha=<new-sha>` | 0 |
//! | could not read/write the forge state needed | the reason | 1 |
//! | branch already moved past `--expected-head-sha` | the reason | 3 |
//! | bound reached; escalated to `loom:operator` | `LOOM-REDATE-ESCALATED …` | 4 |
//!
//! Exit 3 mirrors `merge-pr.sh`'s own #5579 contract: it is NOT a failure —
//! the branch moving out from under a stale-evidence remedy means either a
//! human already acted or a fresh push already landed, so the caller should
//! simply re-evaluate the PR fresh on the next pass rather than report an
//! error or retry this push.
//!
//! Exits 1, 3 and 4 all leave the caller's original #8248 refusal standing:
//! this subcommand never decides whether a merge may proceed, only whether
//! fresh evidence could be produced for it. The guard is unweakened in every
//! path (see `merge_pr::redate`'s module header).

use anyhow::Result;
use loom_daemon::merge_pr::redate::{remedy, RemedyOutcome, HOLD_LABEL};

#[derive(clap::Args)]
pub(crate) struct RedateChecksArgs {
    /// The PR number: the commit message names it, and its comment thread is
    /// where the one-remedy-per-head bound and the hold notice are recorded.
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
        match remedy(&self.repo, &self.branch, &self.expected_head_sha, &self.pr) {
            RemedyOutcome::Pushed { new_sha } => {
                println!("LOOM-REDATE-PUSHED sha={new_sha}");
                std::process::exit(0);
            }
            RemedyOutcome::Escalated { notice_posted } => {
                println!(
                    "LOOM-REDATE-ESCALATED pr={} head={} label={HOLD_LABEL} notice={}\n\
PR #{}'s #8248 block survived an automated re-date of this exact head, so the remedy is \
exhausted: applied {HOLD_LABEL} and {}. A human must merge with an actions:write/elevated \
token, or push any commit to re-date the checks.",
                    self.pr,
                    self.expected_head_sha,
                    if notice_posted {
                        "posted"
                    } else {
                        "already-present"
                    },
                    self.pr,
                    if notice_posted {
                        "posted the hold notice"
                    } else {
                        "left the existing hold notice in place (idempotent)"
                    }
                );
                std::process::exit(4);
            }
            RemedyOutcome::HeadMoved { current } => {
                println!(
                    "PR #{}'s branch '{}' already moved to {current} — expected {} \
(the caller's stale-evidence gate is no longer current). Not pushing; re-evaluate the \
PR fresh next pass.",
                    self.pr, self.branch, self.expected_head_sha
                );
                std::process::exit(3);
            }
            RemedyOutcome::Failed(why) => {
                println!(
                    "Could not run the #8508 re-date remedy for PR #{} branch '{}': {why}",
                    self.pr, self.branch
                );
                std::process::exit(1);
            }
        }
    }
}
