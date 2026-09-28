//! `loom-daemon merge-pr mergeable-recheck` (#6104, slice of #8191).
//!
//! The process boundary around [`loom_daemon::merge_pr::mergeable_recheck::decide`].
//! The shell keeps the I/O — the backoff loop, the uncached PR re-reads, the
//! `git fetch` / `git merge-tree` corroboration — because the retained suite
//! drives exactly that loop through its own stubs, and this split keeps its
//! cases (a)–(f) exercising the shell loop AND the Rust decision together.
//!
//! # The line contract
//!
//! Exactly one `<action>:<reason>` line on stdout, always exit 0 — the
//! decision is conveyed via stdout, not exit status, so the caller can
//! capture it with `$(...)` under `set -e` (the pre-port shell function's own
//! contract, preserved byte for byte).
//!
//! # A missing or old binary is a positive refusal, not a silent pass
//!
//! The caller's fault path (daemon unresolvable, non-zero exit, or output
//! without an `action:` prefix) falls back to a `refuse-stale:` line naming
//! the fault. That is the only safe degraded answer: an unanswered
//! corroboration must never read as "confirmed clean", and a refusal is
//! recoverable (re-run once the binary is rolled) while a wrong merge is not.
//! This mirrors the fail-closed choice every `merge-pr` guard in this family
//! makes — see `cli::merge_pr_labels` for the canonical statement.

use anyhow::Result;
use loom_daemon::merge_pr::mergeable_recheck::{decide, Evidence, TreeOutcome};

#[derive(clap::Args)]
pub(crate) struct MergeableRecheckArgs {
    /// The configured recheck budget, echoed into every unresolved reason.
    #[arg(long, value_name = "N", default_value_t = 3)]
    retries: u32,

    /// The 1-based attempt at which the post-backoff uncached recheck
    /// resolved to mergeable=true. Absent when it never resolved.
    #[arg(long, value_name = "N")]
    resolved_attempt: Option<u32>,

    /// Base and head refs were both unavailable for local corroboration.
    #[arg(long)]
    refs_missing: bool,

    /// `git fetch origin <base> <head>` failed.
    #[arg(long)]
    fetch_failed: bool,

    /// The `git merge-tree` corroboration result: `clean`, `conflict`, or
    /// `not-run` (resolved early / refs missing / fetch failed).
    #[arg(long, value_name = "OUTCOME", default_value = "not-run")]
    tree: String,

    /// The base ref name, interpolated into the corroborating reasons.
    #[arg(long, value_name = "REF", default_value = "")]
    base_ref: String,

    /// The head ref name, interpolated into the fetch-failure reason.
    #[arg(long, value_name = "REF", default_value = "")]
    head_ref: String,
}

impl MergeableRecheckArgs {
    pub(crate) fn run(self) -> Result<()> {
        let tree = match self.tree.as_str() {
            "clean" => TreeOutcome::Clean,
            "conflict" => TreeOutcome::Conflict,
            // clap already restricts nothing here (a free-form string), so an
            // unrecognized value degrades to NotRun — the outcome every
            // caller passes when corroboration never happened. A typo'd flag
            // value therefore cannot fabricate a clean/conflict answer.
            _ => TreeOutcome::NotRun,
        };
        let evidence = Evidence {
            resolved_attempt: self.resolved_attempt,
            retries: self.retries,
            refs_available: !self.refs_missing,
            fetch_ok: !self.fetch_failed,
            tree,
            base_ref: self.base_ref,
            head_ref: self.head_ref,
        };
        println!("{}", decide(&evidence));
        Ok(())
    }
}
