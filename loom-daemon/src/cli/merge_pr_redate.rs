//! `loom-daemon merge-pr redate-checks` (#8508; #8914's re-run removed by #8919).
//!
//! The automated remedy for the #8248 required-check-freshness guard: push a
//! **tree-identical** no-op commit onto the head branch. Any push re-triggers
//! every `pull_request` CI run on the new head, and — crucially — makes GitHub
//! rebuild the test merge commit against the **current** base, which is the only
//! thing that produces genuinely fresh evidence. When the remedy has already run
//! against this exact head and the guard STILL blocks, nothing automated is
//! making progress, so the PR is escalated to a durable `loom:operator` hold
//! instead of re-pushing forever.
//!
//! | outcome | stdout | exit |
//! |---|---|---|
//! | pushed the re-date commit | `LOOM-REDATE-PUSHED sha=<new-sha>` | 0 |
//! | could not read/write the forge state needed | the reason | 1 |
//! | branch already moved past `--expected-head-sha` | the reason | 3 |
//! | bound reached; escalated to `loom:operator` | `LOOM-REDATE-ESCALATED …` | 4 |
//!
//! # Why the in-place re-run is gone (#8919)
//!
//! #8914 re-ran the workflow runs holding the stale required checks in place,
//! to keep the head SHA and with it the Judge verdict, and answered exit **5**
//! ("evidence is fresh, proceed"). That was unsound: GitHub re-runs a workflow
//! run with the ORIGINAL `GITHUB_SHA`, and for a `pull_request` run that SHA is
//! the test merge commit built when the event fired — built on the OLD base.
//! Verified 2026-09-25 on run 36145858487 (PR #8692): attempts 1 and 7, two
//! hours and several `main` merges apart, both checked out
//! `Merge 162b0f05… into 803f0c7d…`. So a re-run moved `started_at` without
//! re-validating anything, and the 2026-09-18 incident could have been merged
//! straight through it. **Exit 5 is therefore never returned.**
//!
//! `--rerun-wait-secs` and `LOOM_REDATE_ALLOW_PROCEED` are still accepted and
//! do nothing, so a `merge-pr.sh` of either vintage keeps working across the
//! version skew a fleet always has mid-rollout (`merge-pr.sh` itself is frozen
//! by the file-size ratchet and does not change here; its exit-5 arm is simply
//! unreachable).
//!
//! Exit 3 mirrors `merge-pr.sh`'s own #5579 contract: it is NOT a failure —
//! the branch moving out from under a stale-evidence remedy means either a
//! human already acted or a fresh push already landed, so the caller should
//! simply re-evaluate the PR fresh on the next pass rather than report an
//! error or retry this push.
//!
//! Exit 0 means "do not merge this pass, re-queue"; exits 1, 3 and 4 all leave
//! the caller's original #8248 refusal standing. This subcommand never decides
//! whether a merge may proceed, only whether fresh evidence could be produced
//! for it. The guard is unweakened in every path (see `merge_pr::redate`'s
//! module header).

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

    /// Accepted and IGNORED since #8919 removed the in-place re-run: a re-run
    /// replays the original test merge commit, so waiting for one to come back
    /// green proved nothing about the current base. Kept so a `merge-pr.sh` of
    /// either vintage can pass it across a mid-rollout version skew.
    #[arg(
        long,
        value_name = "SECS",
        env = "LOOM_REDATE_RERUN_WAIT_SECS",
        default_value_t = DEFAULT_RERUN_WAIT_SECS
    )]
    rerun_wait_secs: u64,

    /// Accepted and IGNORED since #8919: exit 5 ("the guard's evidence is now
    /// fresh, proceed") is never returned, because nothing this subcommand can
    /// do produces fresh evidence without moving the head.
    ///
    /// Parsed "falsey" (`0`/`false`/`no`/`off`/empty = off, anything else =
    /// on) so the `LOOM_REDATE_ALLOW_PROCEED=1` merge-pr.sh sets is accepted:
    /// clap's default bool parser takes only `true`/`false` and would reject
    /// `1` with exit 2, silently disabling the remedy.
    #[arg(
        long,
        env = "LOOM_REDATE_ALLOW_PROCEED",
        value_parser = clap::builder::FalseyValueParser::new()
    )]
    allow_proceed: bool,
}

impl RedateChecksArgs {
    pub(crate) fn run(self) -> Result<()> {
        // Named on stderr rather than silently dropped: an operator who set
        // these expecting an in-place re-run needs to learn it is gone.
        if self.allow_proceed || self.rerun_wait_secs != DEFAULT_RERUN_WAIT_SECS {
            eprintln!(
                "Note: --rerun-wait-secs / LOOM_REDATE_ALLOW_PROCEED are accepted but do nothing \
since #8919. An in-place workflow re-run replays the ORIGINAL test merge commit, so it re-dates \
the checks without re-testing the current base; only a new push does that. Going straight to the \
#8508 tree-identical re-date push."
            );
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
exhausted: applied {HOLD_LABEL} and {}. A human must merge it with an elevated token, rebase it \
onto the current base, or push any commit (which rebuilds the merge commit against the current \
base and re-runs every required check).",
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

/// The historical default for the now-ignored `--rerun-wait-secs`, kept so the
/// "you set this and it does nothing" note only fires when a caller actually
/// passed a value.
const DEFAULT_RERUN_WAIT_SECS: u64 = 300;

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
