//! `loom-daemon forge check-open-pr <issue>` — the #4123 open-linked-PR guard,
//! exposed as a command a human or an in-session agent can run **before** a
//! hand-claim (#8551).
//!
//! # Why this exists
//!
//! The daemon's dispatch path already refuses to start a sweep on an issue that
//! already has an open linked PR (`refusing to dispatch issue #N: it already
//! has an open linked PR`). The **manual** claim path — `gh issue list
//! --label=loom:issue`, then `gh issue edit N --add-label loom:building` —
//! consulted nothing. On 2026-09-21 an operator-session builder hand-claimed
//! issue #8413 and spent a full verification pass on work PR #8462 had already
//! shipped ~4h earlier from another lane. This subcommand is the same guard,
//! reachable from a shell.
//!
//! # One code path, not a second query
//!
//! Everything here is a thin presentation layer over
//! [`crate::worktree_ops::gh::probe_open_linked_pr`] — the same two-transport
//! union (GraphQL closes-graph ∪ REST issue timeline) the registry guard and
//! orphan recovery use. The closes-graph query, its `state == "OPEN"` filter,
//! the `Part of #N` timeline leg, and that leg's #6216/#8940 bare-mention
//! phrase filter are NOT reimplemented here; a divergence between "what the
//! daemon refuses to dispatch" and "what an agent is told is safe to claim" is
//! precisely the defect this command exists to remove.
//!
//! # Exit-code contract (the whole public surface)
//!
//! | Exit | Meaning | stdout | What the caller must do |
//! |---|---|---|---|
//! | `0` | Verified: an open linked PR exists | the PR number | **Do not claim.** The work is already in flight. |
//! | [`EX_NO_OPEN_PR`] (1) | Verified: no open linked PR | empty | Safe to claim. |
//! | [`EX_PROBE_FAILED`] (5) | No verdict — `gh` missing/failed/rate-limited, repo unresolvable, unparseable answer | empty | **Fail closed.** Not a verified absence; check by hand. |
//! | [`EX_FORGE_DECLINED`] (3) | Gitea — the probe is GitHub-only | empty | Check by hand. |
//!
//! `0` is "found" rather than "all clear" on purpose: it makes the number
//! directly capturable (`PR=$(loom-daemon forge check-open-pr 42)`) and makes
//! the *unsafe* state the one a bare `if` fires on. Every non-zero code that is
//! not exactly `1` means **the question was not answered**, which a caller must
//! treat as "may already be in flight" — mirroring the fail-direction
//! [`crate::worktree_ops::gh::OpenPrProbe`] encodes for orphan recovery, where a
//! `ProbeFailed` blocks a `loom:building` reset exactly like a verified `Open`.

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::forge_cmd::{detect_forge, ForgeType, EX_FORGE_DECLINED};
use crate::worktree_ops::gh::{probe_open_linked_pr, OpenPrProbe};

/// Exit code for a **verified** "no open linked PR" — the only safe-to-claim
/// answer. Deliberately distinct from every other non-zero code so a caller can
/// tell "verified absence" from "could not tell".
pub const EX_NO_OPEN_PR: i32 = 1;

/// Exit code for "the probe could not produce a verdict" — fail CLOSED.
///
/// `5` rather than `2`/`3`/`4` so it collides with nothing else on the `forge`
/// surface: `3` is [`EX_FORGE_DECLINED`] and `4` is
/// `crate::forge_cmd::EX_FORGE_HEAD_MISMATCH`.
pub const EX_PROBE_FAILED: i32 = 5;

/// What the CLI prints and exits with for one probe verdict.
///
/// Split out from [`handle`] so the contract is unit-testable without a live
/// `gh` and without `std::process::exit` — the same seam
/// `crate::worktree_ops::gh`'s `parse_*` functions provide for the transports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// Process exit status.
    pub code: i32,
    /// Machine-readable stdout (the PR number, or empty).
    pub stdout: String,
    /// Human-readable explanation, always non-empty.
    pub stderr: String,
}

/// Render a [`OpenPrProbe`] into its [`Verdict`].
///
/// The wording is load-bearing, not decoration: a `ProbeFailed` message must
/// never read as an absence, because the failure mode this command exists to
/// prevent is exactly an agent concluding "nothing in flight" from a question
/// that was never answered.
#[must_use]
pub fn verdict(issue: u32, probe: OpenPrProbe) -> Verdict {
    match probe {
        OpenPrProbe::Open(pr) => Verdict {
            code: 0,
            stdout: pr.to_string(),
            stderr: format!(
                "issue #{issue} already has an open linked PR: #{pr} — do NOT claim it. \
                 The daemon's dispatch path refuses this issue for the same reason (#4123); \
                 a hand-claim is not exempt."
            ),
        },
        OpenPrProbe::NoneOpen => Verdict {
            code: EX_NO_OPEN_PR,
            stdout: String::new(),
            stderr: format!("issue #{issue} has no open linked PR — safe to claim."),
        },
        OpenPrProbe::ProbeFailed => Verdict {
            code: EX_PROBE_FAILED,
            stdout: String::new(),
            stderr: format!(
                "could not determine whether issue #{issue} has an open linked PR \
                 (gh missing, failed, rate-limited, or the repository could not be \
                 resolved). This is NOT a verified absence — treat the issue as possibly \
                 already in flight and check by hand before claiming it."
            ),
        },
    }
}

/// Handle `loom-daemon forge check-open-pr <issue>`. Never returns (exits the
/// process with the code from [`verdict`]); returns `Err` only when the current
/// directory cannot be resolved.
///
/// Resolution is cwd-scoped: run it from anywhere inside the repository (a
/// managed worktree included) and it answers about that repository. Like
/// [`crate::worktree_ops::gh::resolve_owner_repo`] it deliberately does **not**
/// honor `LOOM_REPO`, so an ambient override pointing at another repo cannot
/// produce a confident answer from the wrong closes-graph.
pub fn handle(issue: u32) -> Result<()> {
    let root: PathBuf = std::env::current_dir().context(
        "loom-daemon forge check-open-pr: could not resolve the current directory; \
         run it from inside the repository whose issue you are about to claim",
    )?;

    // GitHub-only by construction: both transports are GitHub APIs. Declining
    // is honest; silently reporting `NoneOpen` on Gitea would be the exact
    // false all-clear this command exists to prevent.
    if detect_forge(Some(&root)) == ForgeType::Gitea {
        eprintln!(
            "loom-daemon forge check-open-pr: the open-linked-PR probe is GitHub-only; \
             check issue #{issue} for an open linked PR by hand before claiming it."
        );
        std::process::exit(EX_FORGE_DECLINED);
    }

    let v = verdict(issue, probe_open_linked_pr(&root, issue));
    if !v.stdout.is_empty() {
        println!("{}", v.stdout);
    }
    eprintln!("{}", v.stderr);
    std::process::exit(v.code);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn open_pr_is_exit_zero_and_prints_only_the_number() {
        let v = verdict(8413, OpenPrProbe::Open(8462));
        assert_eq!(v.code, 0);
        // Bare number so `PR=$(… check-open-pr N)` captures something usable.
        assert_eq!(v.stdout, "8462");
        assert_eq!(v.stdout.parse::<u32>().unwrap(), 8462);
        assert!(v.stderr.contains("do NOT claim"), "{}", v.stderr);
    }

    #[test]
    fn verified_absence_is_exit_one_with_empty_stdout() {
        let v = verdict(42, OpenPrProbe::NoneOpen);
        assert_eq!(v.code, EX_NO_OPEN_PR);
        assert!(v.stdout.is_empty(), "{:?}", v.stdout);
        assert!(v.stderr.contains("safe to claim"), "{}", v.stderr);
    }

    #[test]
    fn probe_failure_fails_closed_and_is_distinguishable_from_absence() {
        let v = verdict(42, OpenPrProbe::ProbeFailed);
        assert_eq!(v.code, EX_PROBE_FAILED);
        assert_ne!(
            v.code, EX_NO_OPEN_PR,
            "a probe failure must not share the verified-absence exit code"
        );
        assert!(v.stdout.is_empty(), "{:?}", v.stdout);
        assert!(
            !v.stderr.contains("safe to claim"),
            "a failed probe must never read as an all-clear: {}",
            v.stderr
        );
        assert!(v.stderr.contains("NOT a verified absence"), "{}", v.stderr);
    }

    /// The three verdicts must occupy three distinct exit codes, and none may
    /// collide with the other `forge` surface codes a caller may also see.
    #[test]
    fn exit_codes_are_mutually_distinct() {
        let codes = [
            verdict(1, OpenPrProbe::Open(2)).code,
            verdict(1, OpenPrProbe::NoneOpen).code,
            verdict(1, OpenPrProbe::ProbeFailed).code,
        ];
        let mut sorted = codes.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 3, "{codes:?}");
        assert!(!codes.contains(&EX_FORGE_DECLINED), "{codes:?}");
        assert!(!codes.contains(&crate::forge_cmd::EX_FORGE_HEAD_MISMATCH), "{codes:?}");
    }
}
