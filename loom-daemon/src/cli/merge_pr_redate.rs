//! `loom-daemon merge-pr redate-checks` (#8508, #8914).
//!
//! The automated remedy for the #8248 required-check-freshness guard. FIRST
//! (#8914) it re-runs the workflow runs holding the stale required checks IN
//! PLACE (`merge_pr::rerun`) — no commit, so the head SHA and the Judge
//! verdict survive. Only when the forge refuses that (no Actions: write on the
//! merge identity), or a stale check is not an Actions job, does it fall back
//! to #8508: push a tree-identical no-op commit instead — any
//! push re-triggers every `pull_request` CI run on the new head, which is
//! exactly `stale_checks::stale_message`'s own documented remedy. When the
//! remedy has already run against this exact head and the guard STILL blocks,
//! nothing automated is making progress, so the PR is escalated to a durable
//! `loom:operator` hold instead of re-pushing forever.
//!
//! | outcome | stdout | exit |
//! |---|---|---|
//! | re-ran in place, now fresh, caller opted in (`LOOM_REDATE_ALLOW_PROCEED=1`) | `LOOM-RERUN-FRESH …` | 5 |
//! | re-ran in place, now fresh, no opt-in | `LOOM-RERUN-FRESH …` | 0 |
//! | re-ran in place, still running when the wait budget ran out | `LOOM-RERUN-PENDING …` | 0 |
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
//! Exit 0 always means "do not merge this pass, re-queue" — that is how every
//! pre-#8914 `merge-pr.sh` reads it, so a newer binary under an older script
//! stays correct. Exit 5 is the ONLY "the guard's evidence is now fresh,
//! proceed" answer, and only a caller that sets `LOOM_REDATE_ALLOW_PROCEED=1`
//! gets it — an env var rather than a flag so an OLDER binary under a newer
//! script ignores it and still pushes, instead of rejecting an unknown flag
//! and losing the remedy.
//!
//! Exits 1, 3 and 4 all leave the caller's original #8248 refusal standing:
//! this subcommand never decides whether a merge may proceed, only whether
//! fresh evidence could be produced for it. The guard is unweakened in every
//! path (see `merge_pr::redate`'s module header).

use anyhow::Result;
use loom_daemon::merge_pr::redate::{remedy, RemedyOutcome, HOLD_LABEL};
use loom_daemon::merge_pr::rerun::{rerun_in_place, RerunOutcome};
use std::time::Duration;

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

    /// How long the in-place re-run (#8914) may wait for the re-run required
    /// checks to come back fresh before reporting them pending (exit 0,
    /// re-queue). 0 = trigger the re-run and return.
    #[arg(
        long,
        value_name = "SECS",
        env = "LOOM_REDATE_RERUN_WAIT_SECS",
        default_value_t = 300
    )]
    rerun_wait_secs: u64,

    /// Answer exit 5 (proceed with the merge) when the in-place re-run leaves
    /// every required check fresh. Without it that case exits 0 (re-queue).
    ///
    /// Parsed "falsey" (`0`/`false`/`no`/`off`/empty = off, anything else =
    /// on) so the `LOOM_REDATE_ALLOW_PROCEED=1` merge-pr.sh sets is accepted:
    /// clap's default bool parser takes only `true`/`false` and would reject
    /// `1` with exit 2, silently disabling BOTH remedies.
    #[arg(
        long,
        env = "LOOM_REDATE_ALLOW_PROCEED",
        value_parser = clap::builder::FalseyValueParser::new()
    )]
    allow_proceed: bool,
}

impl RedateChecksArgs {
    pub(crate) fn run(self) -> Result<()> {
        match rerun_in_place(
            &self.repo,
            &self.pr,
            &self.expected_head_sha,
            Duration::from_secs(self.rerun_wait_secs),
            Duration::from_secs(10),
        ) {
            RerunOutcome::Fresh { reran } => {
                println!(
                    "LOOM-RERUN-FRESH pr={} head={} runs={}\nPR #{}'s stale required checks were \
re-run in place (#8914) and are now fresh against the current base tip. No commit was pushed: \
the head and the Judge verdict are unchanged.",
                    self.pr,
                    self.expected_head_sha,
                    join_ids(&reran),
                    self.pr
                );
                std::process::exit(if self.allow_proceed { 5 } else { 0 });
            }
            RerunOutcome::Pending { reran, waiting_on } => {
                println!(
                    "LOOM-RERUN-PENDING pr={} head={} runs={}\nPR #{}'s stale required checks \
are being re-run in place (#8914), still waiting on: {}. No commit was pushed: the head and the \
Judge verdict are unchanged; re-attempt the merge on a later pass.",
                    self.pr,
                    self.expected_head_sha,
                    join_ids(&reran),
                    self.pr,
                    waiting_on.join(", ")
                );
                std::process::exit(0);
            }
            RerunOutcome::HeadMoved { current } => {
                println!(
                    "PR #{}'s head already moved to {current} — expected {} (the caller's \
stale-evidence gate is no longer current). Not re-running; re-evaluate the PR fresh next pass.",
                    self.pr, self.expected_head_sha
                );
                std::process::exit(3);
            }
            RerunOutcome::Failed(why) => {
                println!(
                    "Could not re-run PR #{}'s stale required checks in place (#8914): {why}",
                    self.pr
                );
                std::process::exit(1);
            }
            RerunOutcome::Refused(why) | RerunOutcome::NotApplicable(why) => {
                // The one path that falls through to the #8508 push. Say why
                // on stderr (merge-pr.sh captures 2>&1) so an install missing
                // Actions: write sees what it would take to keep verdicts.
                eprintln!(
                    "In-place re-run unavailable ({why}); falling back to the #8508 \
tree-identical re-date push. Granting the merge identity Actions: write lets the checks be \
re-run in place instead, keeping the head and the Judge verdict (#8914)."
                );
            }
        }
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

fn join_ids(ids: &[u64]) -> String {
    if ids.is_empty() {
        return "none".to_string();
    }
    ids.iter().map(u64::to_string).collect::<Vec<_>>().join(",")
}

#[cfg(test)]
mod tests {
    use super::RedateChecksArgs;
    use clap::Parser;

    #[derive(Parser)]
    struct Harness {
        #[command(flatten)]
        args: RedateChecksArgs,
    }

    const BASE: [&str; 9] = [
        "x",
        "--pr",
        "1",
        "--repo",
        "o/r",
        "--branch",
        "b",
        "--expected-head-sha",
        "abc",
    ];

    // `env` is process-global, so the env-driven case lives in ONE test
    // (no parallel sibling can race it) and restores the variable.
    #[test]
    #[serial_test::serial]
    fn allow_proceed_accepts_the_env_value_merge_pr_sh_sets() {
        let prev = std::env::var_os("LOOM_REDATE_ALLOW_PROCEED");
        for (val, want) in [("1", true), ("true", true), ("0", false), ("false", false)] {
            std::env::set_var("LOOM_REDATE_ALLOW_PROCEED", val);
            let h = Harness::try_parse_from(BASE)
                .unwrap_or_else(|e| panic!("LOOM_REDATE_ALLOW_PROCEED={val} must parse: {e}"));
            assert_eq!(h.args.allow_proceed, want, "LOOM_REDATE_ALLOW_PROCEED={val}");
        }
        std::env::remove_var("LOOM_REDATE_ALLOW_PROCEED");
        let h = Harness::try_parse_from(BASE).expect("parses without the opt-in");
        assert!(!h.args.allow_proceed, "unset = no exit 5 (old-script contract)");
        if let Some(v) = prev {
            std::env::set_var("LOOM_REDATE_ALLOW_PROCEED", v);
        }
    }
}
