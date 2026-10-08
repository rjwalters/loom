//! The per-attempt route of `merge-pr.sh`'s synchronous merge-retry loop
//! (#8191 slice): what to do after `forge_merge_pr` failed, the PR did not
//! merge underneath us, and `merge-pr classify-response` named the failure.
//!
//! Before this port the loop body spelled the route out inline — an `if` per
//! response kind, the `MERGE_ATTEMPT -lt MAX_MERGE_RETRIES` retry budget, the
//! `MERGE_RETRY_DELAY * 2` exponential backoff, and five narration strings
//! interpolating all three. The I/O those routes perform (the 405 re-read, the
//! `forge_update_branch` call, the sleeps, `_refresh_precondition_sha`, the
//! rework marker) stays in the shell; the DECISION and its wording live here,
//! pinned by a differential against the retired loop body
//! (`tests/merge_pr_merge_route_differential.rs`).
//!
//! | kind | route |
//! |---|---|
//! | `merge-in-progress` | [`Route::Await`]: narrate, sleep 5s, re-read `.merged`; merged → success + break, else warn + next attempt |
//! | `base-modified`, attempt < max | [`Route::Sync`]: narrate, update the branch, sleep `delay`, re-read the head, mark the rework, double the delay, next attempt |
//! | `base-modified`, attempt ≥ max | [`Route::Fail`]: "…after M attempts: Branch remains behind base branch" |
//! | `other` | [`Route::Fail`]: "Failed to merge PR #P: <response>" |
//! | `head-mismatch` | [`Route::Fail`], quoting the response |
//!
//! `head-mismatch` never reaches this verb in practice: the loop hands it to
//! `_head_moved_or_resync` first, which either spends #8164's single
//! self-sync retry (`continue`) or exits 3. It is routed anyway, to exactly
//! what the retired loop did had that function ever returned non-zero — fall
//! past the base-modified test into the terminal `error` — so the verb is
//! total over every token the classifier can print and never retries a head
//! that moved (#5579).
//!
//! # Arithmetic fidelity
//!
//! The retired predicates were bash arithmetic: `[[ $a -lt $m ]]` and
//! `$((d * 2))` over signed 64-bit integers, with multiplication wrapping on
//! overflow. [`decide`] uses `i64` and `wrapping_mul` so even inputs the
//! shell never passes (it only ever passes `seq 1 3` and `5`/`10`) agree.

/// The response kinds `merge-pr classify-response` prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    MergeInProgress,
    HeadMismatch,
    BaseModified,
    Other,
}

/// The seconds the 405 route waits before re-reading `.merged` — the retired
/// loop's literal `sleep 5`.
pub const AWAIT_SECS: i64 = 5;

/// What the loop does next, with every line it narrates on that route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// HTTP 405: a concurrent merger holds the PR.
    Await {
        sleep: i64,
        /// `info`, before the sleep.
        before: String,
        /// `success`, when the re-read finds the PR merged (the loop breaks).
        merged: String,
        /// `warning`, when it does not (the loop moves to the next attempt).
        pending: String,
    },
    /// The base moved and retry budget remains: sync the head, then retry.
    Sync {
        /// Seconds to wait for the sync — the CURRENT delay.
        sleep: i64,
        /// The delay the next sync will use (exponential backoff).
        next_delay: i64,
        /// `info`, before `forge_update_branch`.
        before: String,
        /// `info`, after it, before the sleep.
        wait: String,
        /// `record-rework --reason` for the #9444 rebase marker.
        rework: String,
    },
    /// Hard stop: the shell's `error` (exit 1), quoting this message.
    Fail { message: Vec<u8> },
}

/// The decision inputs, as the loop holds them.
pub struct Inputs<'a> {
    pub kind: Kind,
    pub pr: &'a str,
    pub attempt: i64,
    pub max: i64,
    pub delay: i64,
    /// `$MERGE_RESPONSE` — bytes, since the forge's error text need not be
    /// UTF-8 and the shell never required it to be.
    pub response: &'a [u8],
}

/// Decide the loop's next step.
#[must_use]
pub fn decide(i: &Inputs<'_>) -> Route {
    match i.kind {
        Kind::MergeInProgress => Route::Await {
            sleep: AWAIT_SECS,
            before: "Merge already in progress (HTTP 405), waiting for completion...".to_string(),
            merged: format!("PR #{} merged (concurrent merge completed)", i.pr),
            pending: "Concurrent merge not yet complete, retrying...".to_string(),
        },
        Kind::BaseModified if i.attempt < i.max => Route::Sync {
            sleep: i.delay,
            next_delay: i.delay.wrapping_mul(2),
            before: format!(
                "Branch is behind base branch, updating... (attempt {}/{})",
                i.attempt, i.max
            ),
            wait: format!("Waiting {}s for branch to sync...", i.delay),
            rework: format!(
                "base branch was modified; synced before merge retry {}/{}",
                i.attempt, i.max
            ),
        },
        Kind::BaseModified => Route::Fail {
            message: format!(
                "Failed to merge PR #{} after {} attempts: Branch remains behind base branch",
                i.pr, i.max
            )
            .into_bytes(),
        },
        Kind::HeadMismatch | Kind::Other => {
            let mut message = format!("Failed to merge PR #{}: ", i.pr).into_bytes();
            message.extend_from_slice(i.response);
            Route::Fail { message }
        }
    }
}

impl Route {
    /// The wire form `merge-pr.sh` reads:
    ///
    /// ```text
    /// LOOM-MERGE-ROUTE AWAIT <sleep>          LOOM-MERGE-ROUTE SYNC <sleep> <next-delay>
    /// BEFORE\t<text>                          BEFORE\t<text>
    /// MERGED\t<text>                          WAIT\t<text>
    /// PENDING\t<text>                         REWORK\t<text>
    ///
    /// LOOM-MERGE-ROUTE FAIL
    /// <message, verbatim, possibly several lines>
    /// ```
    ///
    /// Each keyed line's text is single-line by construction (it interpolates
    /// only the PR number and integers). Only `FAIL`'s message can span lines —
    /// it quotes the forge response — which is why it is the whole remainder
    /// rather than a keyed line.
    #[must_use]
    pub fn render(&self) -> Vec<u8> {
        match self {
            Self::Await {
                sleep,
                before,
                merged,
                pending,
            } => format!(
                "LOOM-MERGE-ROUTE AWAIT {sleep}\nBEFORE\t{before}\nMERGED\t{merged}\nPENDING\t{pending}\n"
            )
            .into_bytes(),
            Self::Sync {
                sleep,
                next_delay,
                before,
                wait,
                rework,
            } => format!(
                "LOOM-MERGE-ROUTE SYNC {sleep} {next_delay}\nBEFORE\t{before}\nWAIT\t{wait}\nREWORK\t{rework}\n"
            )
            .into_bytes(),
            Self::Fail { message } => {
                let mut out = b"LOOM-MERGE-ROUTE FAIL\n".to_vec();
                out.extend_from_slice(message);
                out.push(b'\n');
                out
            }
        }
    }
}

#[cfg(test)]
mod tests;
