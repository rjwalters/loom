//! `loom-daemon merge-pr merge-route` (#8191 slice): the per-attempt route of
//! `merge-pr.sh`'s synchronous merge-retry loop, after a failed
//! `forge_merge_pr` has been classified by `merge-pr classify-response`.
//!
//! # Protocol
//!
//! Reads `$MERGE_RESPONSE` (raw bytes) on stdin. Prints one route block and
//! exits 0 — see [`loom_daemon::merge_pr::merge_route::Route::render`]:
//! `LOOM-MERGE-ROUTE AWAIT <sleep>` / `SYNC <sleep> <next-delay>` followed by
//! keyed narration lines, or `LOOM-MERGE-ROUTE FAIL` followed by the verbatim
//! refusal message.
//!
//! # Fail direction
//!
//! OPEN, onto the retired behaviour. A missing/older binary, a clap error or
//! silence is read by the shell as "the verb could not run": it routes on the
//! classifier's kind and the same `attempt < max` budget itself and narrates
//! generic lines. The degraded path still never retries a moved head, still
//! syncs-and-retries a stale base within the same budget, and still refuses
//! (exit 1) everything else; only the wording degrades.

use std::io::{Read, Write};

use anyhow::Result;
use loom_daemon::merge_pr::merge_route::{decide, Inputs, Kind};

#[derive(clap::ValueEnum, Clone, Copy)]
enum KindArg {
    MergeInProgress,
    HeadMismatch,
    BaseModified,
    Other,
}

#[derive(clap::Args)]
pub(crate) struct MergeRouteArgs {
    /// The `merge-pr classify-response` token for this failure.
    #[arg(long, value_enum)]
    kind: KindArg,

    /// The PR number (text only).
    #[arg(long, value_name = "N")]
    pr: String,

    /// `$MERGE_ATTEMPT` (1-based).
    #[arg(long, value_name = "N", allow_hyphen_values = true)]
    attempt: i64,

    /// `$MAX_MERGE_RETRIES`.
    #[arg(long, value_name = "N", allow_hyphen_values = true)]
    max: i64,

    /// `$MERGE_RETRY_DELAY`, seconds — the current backoff.
    #[arg(long, value_name = "SECS", allow_hyphen_values = true)]
    delay: i64,
}

impl MergeRouteArgs {
    pub(crate) fn run(self) -> Result<()> {
        let mut response = Vec::new();
        std::io::stdin().read_to_end(&mut response)?;
        let kind = match self.kind {
            KindArg::MergeInProgress => Kind::MergeInProgress,
            KindArg::HeadMismatch => Kind::HeadMismatch,
            KindArg::BaseModified => Kind::BaseModified,
            KindArg::Other => Kind::Other,
        };
        let route = decide(&Inputs {
            kind,
            pr: &self.pr,
            attempt: self.attempt,
            max: self.max,
            delay: self.delay,
            response: &response,
        });
        let mut out = std::io::stdout().lock();
        out.write_all(&route.render())?;
        out.flush()?;
        Ok(())
    }
}
