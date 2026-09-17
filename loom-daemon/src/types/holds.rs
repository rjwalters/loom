//! Status-wire leaf types for the two conditions where the daemon has
//! **stopped doing something and cannot restart it on its own judgement
//! alone** — split out of `types.rs` in the same spirit as
//! [`super::RosterStatus`]'s own split (#7852):
//!
//! | type | condition |
//! |------|-----------|
//! | [`StuckWorktreeReclaim`] | a worktree removal the reaper backed off after a permission-class failure or a retry cap (#7590) |
//! | [`PoolExhaustionHoldStatus`] | a token pool with zero spawnable accounts, holding every sweep dispatch that resolves to it (#7708) |
//!
//! Both are pure projections onto [`super::DaemonStatusReport`]: the state
//! itself lives in the module that owns it
//! ([`crate::worktree_reaper`], [`crate::work_finder::pool_preflight`]), and
//! is copied onto the wire once per status call.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One [`super::DaemonStatusReport::stuck_worktree_reclaims`] entry (Issue
/// #7590) — a worktree removal the reaper has backed off after either a
/// permission-class failure or [`crate::worktree_reaper::REMOVAL_FAILURE_CAP`]
/// consecutive failures of any cause. Surfaced so an operator sees the
/// specific stuck repo + issue/PR number + cause on `loom-daemon
/// health`/`status` instead of the removal silently retrying and failing
/// identically forever.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StuckWorktreeReclaim {
    /// The owning repo's primary-checkout root.
    pub repo_root: PathBuf,
    /// `"issue"` or `"pr"` — which reap pass owns this worktree.
    pub kind: String,
    /// The issue or PR number (interpretation depends on [`Self::kind`]).
    pub number: u32,
    /// The worktree's on-disk path.
    pub path: PathBuf,
    /// The most recent removal failure's cause, verbatim from
    /// `clean::cleanup_worktree`/`cleanup_pr_worktree`'s `Err`.
    pub cause: String,
    /// When the first failed removal attempt for this path was recorded this
    /// process (removal-failure state is not persisted across a daemon
    /// restart — see [`crate::worktree_reaper::stuck_worktree_removals`]'s
    /// doc comment).
    pub first_failure_at: DateTime<Utc>,
    /// When the most recent failed removal attempt was recorded.
    pub last_attempt_at: DateTime<Utc>,
    /// Total consecutive failed removal attempts recorded for this path.
    pub attempt_count: u32,
}

/// One [`super::DaemonStatusReport::pool_exhaustion_holds`] entry (Issue
/// #7708, surfaced by #7990) — the wire projection of
/// [`crate::work_finder::pool_preflight::PoolExhaustionHold`], a live "this
/// token pool cannot spawn anything" hold keyed by resolved pool directory.
///
/// While such a hold is armed the work finder dispatches **no sweep at all**
/// for any workspace resolving to [`Self::dir`], so an operator asking why
/// nothing is moving needs this on the status surface; before #7990 the only
/// evidence was an edge-triggered daemon log line.
///
/// This type is deliberately **not** folded into the `tokens` capacity
/// section of `loom-daemon health`: a held pool is a distinct condition from
/// a degraded/failing account, and merging the two would inflate the token
/// failure tallies with a single host-level fact — exactly the separation
/// [`crate::health::RoleTickSummary::pool_exhausted`] already keeps for
/// #7607's role-tick skips.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PoolExhaustionHoldStatus {
    /// The resolved pool directory this hold covers — every workspace root
    /// whose `resolve_tokens_dir` lands here is held together.
    pub dir: PathBuf,
    /// Total `*.token` files in [`Self::dir`] when the hold was last
    /// refreshed. `0` spawnable of this many is the whole condition.
    pub total: usize,
    /// When the hold first armed — preserved across refreshes, so this reads
    /// as "how long has this pool been dead", not "when was it last
    /// re-observed".
    pub since: DateTime<Utc>,
    /// Best-effort estimate of when the pool might regain a spawnable
    /// account (`pool_clear_estimate`, capped at 900 s out). Diagnostic for a
    /// pre-flight-armed hold — the next tick's live read is what actually
    /// clears it.
    pub next_clear_at: DateTime<Utc>,
    /// `true` when a real sweep's death at `spawn-claude.sh`'s token-selection
    /// step armed (or last refreshed) this hold, as opposed to this daemon's
    /// own live pre-flight read. A wrapper-observed hold outranks the live
    /// read until [`Self::next_clear_at`].
    pub wrapper_observed: bool,
}
