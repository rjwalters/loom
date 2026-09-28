//! `loom-daemon merge-pr zero-checks-settle` (#9091).
//!
//! One zero-row check-runs poll in `merge-pr.sh --auto`'s settle wait, decided.
//! The rule itself is [`loom_daemon::merge_pr::zero_checks`]; this is the
//! process boundary around it.
//!
//! # The line contract
//!
//! Exactly one line on stdout, exit 0:
//!
//! ```text
//! <SENTINEL> <sleep-seconds> <required-state> <narration…>
//! ```
//!
//! | sentinel | the caller must |
//! |---|---|
//! | `LOOM-ZERO-CHECKS-SETTLE` | trust the empty rollup and merge (`info`) |
//! | `LOOM-ZERO-CHECKS-TIMEOUT` | merge, saying nothing was confirmed (`warning`) |
//! | `LOOM-ZERO-CHECKS-WAIT` | `sleep $2` and poll again (`info`) |
//!
//! Field 3 is an opaque cache token: the caller stores it and passes it back as
//! `--required-state` on the next poll, which is what makes the two-read
//! branch-protection lookup happen once per wait rather than once per poll.
//!
//! # Failing to answer is a positive refusal, not a silent one
//!
//! Every reachable decision exits 0 WITH a sentinel. There is no "pass by
//! exiting 0 quietly", for the reason spelled out in
//! `cli::merge_pr_labels`: the binary can be missing, older than this
//! subcommand, or substituted, and none of those may be readable as "settle".
//! The caller therefore accepts only output beginning with one of the three
//! sentinels above, and treats anything else — including a clap usage error, an
//! unknown subcommand, or no binary at all — as "the bounded settle is
//! unavailable", falling back to #6169's full `LOOM_AUTO_MERGE_TIMEOUT` wait.
//! That degraded mode is the status quo ante this change narrows, so a fault
//! here can only ever cost time, never a skipped gate.

use anyhow::Result;
use loom_daemon::merge_pr::stale_checks;
use loom_daemon::merge_pr::zero_checks::{
    decide, render, settle_interval, settle_polls, Inputs, Required,
};

#[derive(clap::Args)]
pub(crate) struct ZeroChecksSettleArgs {
    /// The PR number, for the narration.
    #[arg(long, value_name = "N")]
    pr: String,

    /// The repository as owner/repo, for the required-context lookup.
    #[arg(long, value_name = "OWNER/REPO", default_value = "")]
    repo: String,

    /// The PR's base branch — the branch whose protection decides whether the
    /// bounded settle applies at all.
    #[arg(long, value_name = "REF", default_value = "")]
    base_ref: String,

    /// How many consecutive zero-row polls have been taken, including this one.
    #[arg(long, value_name = "N", default_value_t = 1)]
    polls: u64,

    /// The cached answer from a previous poll of this same wait
    /// (`none`/`present`/`lookup-failed`). `unknown` — the default, and what
    /// the first poll passes — performs the live lookup. An unrecognised value
    /// is treated as `unknown` and re-resolved rather than guessed.
    #[arg(long, value_name = "STATE", default_value = "unknown")]
    required_state: String,

    /// `LOOM_AUTO_MERGE_POLL_INTERVAL`: the spacing used when the bounded
    /// settle does not apply, and the fallback for a garbage settle interval.
    #[arg(long, value_name = "SECS", default_value_t = 30)]
    poll_interval: u64,

    /// `LOOM_AUTO_MERGE_TIMEOUT`, for the narration.
    #[arg(long, value_name = "SECS", default_value_t = 600)]
    timeout: u64,

    /// The caller's clock, as epoch seconds. Passed in rather than read here so
    /// the whole decision is a function of its arguments — which is what lets
    /// the shell suite drive it with a stubbed `date`.
    #[arg(long, value_name = "EPOCH")]
    now: Option<i64>,

    /// The caller's `LOOM_AUTO_MERGE_TIMEOUT` deadline, as epoch seconds.
    #[arg(long, value_name = "EPOCH")]
    deadline: Option<i64>,
}

impl ZeroChecksSettleArgs {
    pub(crate) fn run(self) -> Result<()> {
        let required = match Required::parse(&self.required_state) {
            Required::Unknown => self.resolve_required(),
            cached => cached,
        };

        // Absent either half, the deadline is simply not reached — the wait
        // continues, which is the side that cannot skip a gate.
        let deadline_reached = match (self.now, self.deadline) {
            (Some(now), Some(deadline)) => now >= deadline,
            _ => false,
        };

        let decision = decide(&Inputs {
            pr: self.pr.clone(),
            base_ref: self.base_ref.clone(),
            polls: self.polls,
            required,
            deadline_reached,
            settle_polls: settle_polls(env("LOOM_ZERO_CHECKS_SETTLE_POLLS").as_deref()),
            settle_interval: settle_interval(
                env("LOOM_ZERO_CHECKS_SETTLE_INTERVAL").as_deref(),
                self.poll_interval,
            ),
            poll_interval: self.poll_interval,
            timeout: self.timeout,
        });
        println!("{}", render(&decision));
        Ok(())
    }

    /// The live two-source required-context lookup, shared verbatim with the
    /// #8248 freshness guard so the two guards cannot disagree about what this
    /// base branch requires.
    ///
    /// An error is [`Required::LookupFailed`] — never `None`. The distinction
    /// is the whole fail-closed property: "the branch requires nothing" shortens
    /// the wait, "we could not find out" must not.
    fn resolve_required(&self) -> Required {
        match stale_checks::fetch::required_contexts(&self.repo, &self.base_ref) {
            Ok((contexts, notices)) => {
                // A relaxation nobody can see is how a fail-open ships
                // unnoticed (same rule the freshness guard states): the plan
                // gate that makes a source provably ruleless is said out loud.
                for n in notices {
                    eprintln!("merge-pr zero-checks-settle: {n}");
                }
                if contexts.is_empty() {
                    Required::None
                } else {
                    Required::Present
                }
            }
            Err(why) => {
                eprintln!(
                    "merge-pr zero-checks-settle: could not resolve required status checks for \
                     {}@{}: {why} — failing closed onto the full wait",
                    self.repo, self.base_ref
                );
                Required::LookupFailed
            }
        }
    }
}

/// An env var, with unset and empty treated alike (the shell's `:=` default
/// fires on both, and the knob validators keep that behaviour).
fn env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}
