//! `loom-daemon merge-pr issue-close-gate` (#4186, a slice of the merge-pr
//! port #8191): the decision half of `merge-pr.sh`'s
//! `_issue_is_closed_for_cleanup`.
//!
//! # Protocol
//!
//! Reads `forge_pr_close_targets`'s output (one issue number per line) on
//! stdin, takes the issue number via `--issue` and an OPTIONAL live
//! `forge_get_issue_state` reading via `--state` (omitted on the shell's
//! first, common-case call), and prints exactly one line:
//! `LOOM-ISSUE-CLEANUP <token>`.
//!
//! | outcome | token | exit |
//! |---|---|---|
//! | `--issue` is a close target of this PR | `CLOSE-TARGET` | 0 |
//! | `--state CLOSED` (and not a close target) | `STATE-CLOSED` | 0 |
//! | no `--state` supplied (and not a close target) | `NEED-STATE` | 3 |
//! | `--state` supplied and it was not `CLOSED` | `PRESERVE` | 1 |
//!
//! Exit 3 tells the shell to fetch `forge_get_issue_state` and call this verb
//! again with `--state` — the two-call shape `loom-pr-guard` → `hold-state`
//! already uses elsewhere in this port, kept for the same reason: the
//! overwhelming common case (a PR that says `Closes #N`) never pays for the
//! second forge round trip.
//!
//! # Exit code on a read/decision failure
//!
//! 2 when stdin could not be read. The gate this serves defends a
//! **destructive** step (`git worktree remove --force`), so the shell
//! wrapper treats ANY exit other than 0/1/3, and any stdout that is not one
//! of the three recognised tokens, as "the gate did not run" — which resolves
//! to preserve, exactly like exit 1, never to cleanup. Fail-unsafe-to-preserve
//! (#4186): a skipped cleanup is always recoverable later; a wrongly-removed
//! worktree is not.

use anyhow::Result;
use std::io::Read;

use loom_daemon::merge_pr::issue_close_gate::decide;

#[derive(clap::Args)]
pub(crate) struct IssueCloseGateArgs {
    /// The issue number to decide cleanup for.
    #[arg(long, value_name = "N")]
    issue: String,

    /// A live `forge_get_issue_state` reading (`OPEN` / `CLOSED`), when the
    /// shell has already fetched one. Omitted on the fast-path call, which
    /// answers from close-target membership alone whenever it can.
    #[arg(long, value_name = "STATE")]
    state: Option<String>,
}

impl IssueCloseGateArgs {
    pub(crate) fn run(self) -> Result<()> {
        let mut close_targets = String::new();
        if std::io::stdin().read_to_string(&mut close_targets).is_err() {
            // Unreadable stdin is not the same as "no close targets" — the
            // latter is a decidable empty set, the former is no answer.
            eprintln!("merge-pr issue-close-gate: could not read close targets from stdin");
            std::process::exit(2);
        }

        let decision = decide(&close_targets, &self.issue, self.state.as_deref());
        println!("LOOM-ISSUE-CLEANUP {}", decision.token());

        use loom_daemon::merge_pr::issue_close_gate::Decision;
        std::process::exit(match decision {
            Decision::CloseTarget | Decision::StateClosed => 0,
            Decision::NeedState => 3,
            Decision::Preserve => 1,
        })
    }
}
