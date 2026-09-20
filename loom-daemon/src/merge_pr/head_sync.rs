//! The self-sync head-SHA attribution guard (#8164, slice 4 of #8191).
//!
//! # The incident
//!
//! `merge-pr.sh <n>` on a Judge-approved, mergeable PR failed with:
//!
//! ```text
//! Details: PR #<n>: {"message":"Head branch was modified. Review and try the merge again.","status":"409"}
//! ```
//!
//! The PR's timeline named the culprit: `merge-pr.sh` itself. Both of its
//! merge-retry loops answer a `Base branch was modified` rejection by calling
//! `forge_update_branch` (`PUT /repos/{nwo}/pulls/{n}/update-branch`), which
//! lands a `Merge branch 'main' into <branch>` commit **on the head branch** —
//! and then retry the merge with `$MERGE_PRECONDITION_SHA` as it was read
//! *before* that push. The forge compares the stale precondition against its
//! own current head, refuses with 409, and the script's `#5579` classifier
//! reads that as "the head moved under us" and exits 3. Re-running a minute
//! later succeeds, because the fresh read observes the sync commit.
//!
//! So the script lost a merge to a race **it created itself** and then blamed
//! the outside world for it.
//!
//! # Why "just retry on 409" is the wrong fix
//!
//! `#5579` made a moved head a hard stop for a reason: between Judge's
//! approval and the merge, a session can push new commits to the head branch,
//! and squash-merging the new head ships a diff nobody reviewed — invisibly,
//! because a squash leaves no ancestry to notice afterwards. A blanket
//! re-read-and-retry would reintroduce exactly that.
//!
//! The distinction this module draws is therefore **not** "did we retry
//! recently" but **what is the new head made of**:
//!
//! > The head may be re-read and re-merged only when the new head is a
//! > two-parent merge commit whose **first parent is the exact head we were
//! > about to merge** and whose **second parent is already contained in the
//! > base branch**.
//!
//! Under that shape the new head's content is (approved head ∪ base) and
//! nothing else — which is precisely the tree the merge would have produced
//! anyway. Any other shape (a rebase, an extra commit on top, a merge of some
//! unrelated branch) fails at least one clause and is refused as foreign.
//!
//! # Attribution is structural, never textual
//!
//! The tempting signal is the commit message — GitHub writes
//! `Merge branch 'main' into <branch>` — and it is worthless here: a commit
//! message is attacker-supplied text, and anyone who can push to the head
//! branch can write that message on a commit containing anything at all. The
//! parent SHAs cannot be forged in the same way: to satisfy clause one an
//! attacker would have to make their commit's first parent be the approved
//! head (fine, that is just "on top of it") **and** its second parent already
//! reachable from base (so it carries no unreviewed content). This module
//! therefore never looks at the message.
//!
//! # The one-shot budget
//!
//! Attribution is not enough on its own: `update-branch` is asynchronous, so
//! a second sync could land while the retry is in flight, and an unbounded
//! "refresh and retry" would chase a moving head forever. The caller spends a
//! single retry (`retry_used`), after which a mismatch is foreign by
//! definition — a hard stop, per #8164's own acceptance criterion that the
//! fix "must not mask a genuine concurrent-push race".

/// The only stdout a caller may treat as "re-read the head and retry once".
///
/// Emitted as `LOOM-HEAD-SELF-SYNC-RETRY <sha>`: a sentinel for the same
/// reason [`super::labels::CLEAN`] is one — the authorization must be a
/// POSITIVE signal, so a missing, old, or substituted binary cannot produce
/// one by falling over quietly. See `cli::merge_pr_head_sync`.
pub const RETRY: &str = "LOOM-HEAD-SELF-SYNC-RETRY";

/// Does this merge-API response say "your head-SHA precondition is stale"?
///
/// A port of `merge-pr.sh`'s `_is_head_mismatch_response`, kept byte-equivalent
/// to it by `tests/merge_pr_head_sync_differential.rs`. String provenance is
/// documented on `forge_merge_pr` / `forge_auto_merge` in
/// `lib/forge-helpers.sh`: the GitHub REST and Gitea forms are verified
/// against each forge's own source, the GraphQL one is best-effort.
///
/// Deliberately NOT matched: `Base branch was modified`. That one means the
/// PR's *base* fell behind and a sync-and-retry is correct; conflating the two
/// is how a head-mismatch would get eaten by the sync path and merged against
/// a moving target.
#[must_use]
pub fn is_head_mismatch(response: &str) -> bool {
    let hay = response.to_ascii_lowercase();
    hay.contains("head branch was modified.")
        || hay.contains("head out of date")
        || hay.contains("expectedheadoid")
}

/// What the caller knows when a merge attempt has just been refused.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Evidence {
    /// The forge's refusal text.
    pub response: String,
    /// The caller established the refusal is a head-SHA mismatch by a channel
    /// other than the text — `loom-daemon forge auto-merge`'s exit 4. Without
    /// it the text must say so itself.
    pub mismatch_confirmed: bool,
    /// Did THIS invocation push to the head branch (i.e. call
    /// `forge_update_branch`) before the refused attempt? Attribution is
    /// impossible without it: an unexplained head move is foreign.
    pub self_synced: bool,
    /// Has the single re-read-and-retry already been spent?
    pub retry_used: bool,
    /// The head SHA the refused merge was gated on.
    pub precondition_sha: String,
    /// The head SHA the forge reports now.
    pub current_head_sha: String,
    /// The current head commit's parents, in order.
    pub head_parents: Vec<String>,
    /// Is `head_parents[1]` already contained in the base branch? `None` when
    /// it could not be determined (a lookup failure, or no second parent) —
    /// which is not the same as `Some(false)` and is never a retry.
    pub second_parent_in_base: Option<bool>,
}

/// The guard's verdict over one refused merge attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The head moved only by this invocation's own base-sync. Re-read the
    /// head SHA (carried here) and retry the merge exactly once.
    SelfSyncRetry { new_head: String },
    /// Do not retry: the head move is not attributable to our own sync, or
    /// the one-shot budget is spent. The caller re-queues (exit 3).
    Foreign(String),
}

/// Decide whether a refused merge may be retried against a freshly-read head.
///
/// Every clause is a refusal by default; `SelfSyncRetry` is reached only when
/// all of them are satisfied, and each `Foreign` names which one was not, so
/// an operator reading the log learns what actually moved.
#[must_use]
pub fn classify(ev: &Evidence) -> Verdict {
    let foreign = |s: String| Verdict::Foreign(s);

    if !ev.mismatch_confirmed && !is_head_mismatch(&ev.response) {
        return foreign(
            "the merge was not refused for a stale head-SHA precondition, so re-reading the \
             head cannot be the remedy"
                .to_string(),
        );
    }
    if !ev.self_synced {
        return foreign(
            "this merge run never pushed to the head branch, so the head moved for a reason \
             outside this run — most likely a session pushing new commits, which #5579 \
             deliberately refuses to merge"
                .to_string(),
        );
    }
    if ev.retry_used {
        return foreign(
            "the single re-read-and-retry is already spent; a head that is still moving is a \
             live race, not this run's own base-sync"
                .to_string(),
        );
    }
    if ev.precondition_sha.is_empty() || ev.current_head_sha.is_empty() {
        return foreign(
            "the merge precondition or the current head SHA could not be determined, and an \
             unknown head is never retried"
                .to_string(),
        );
    }
    if ev.current_head_sha == ev.precondition_sha {
        return foreign(format!(
            "the head is still {}, unchanged from the merge precondition — the refusal has some \
             other cause and re-reading would change nothing",
            short(&ev.precondition_sha)
        ));
    }
    if ev.head_parents.len() != 2 {
        return foreign(format!(
            "the current head {} has {} parent(s), not the two of a base-sync merge commit \
             (a rebase-style update rewrites the branch, and a rewritten branch is not the \
             approved head plus base)",
            short(&ev.current_head_sha),
            ev.head_parents.len()
        ));
    }
    if ev.head_parents[0] != ev.precondition_sha {
        return foreign(format!(
            "the current head {}'s first parent is {}, not the approved head {} — commits \
             landed on this branch that the merge precondition never covered",
            short(&ev.current_head_sha),
            short(&ev.head_parents[0]),
            short(&ev.precondition_sha)
        ));
    }
    match ev.second_parent_in_base {
        Some(true) => Verdict::SelfSyncRetry {
            new_head: ev.current_head_sha.clone(),
        },
        Some(false) => foreign(format!(
            "the current head {} merges {}, which is not contained in the base branch — a \
             base-sync merges the base in, so this brings content from somewhere else",
            short(&ev.current_head_sha),
            short(&ev.head_parents[1])
        )),
        None => foreign(format!(
            "could not determine whether {} is contained in the base branch, and an \
             undetermined second parent is never retried",
            short(&ev.head_parents[1])
        )),
    }
}

/// First 8 characters of a SHA, for messages. Short of 8, printed whole.
fn short(sha: &str) -> String {
    sha.chars().take(8).collect()
}

/// The log line for an authorized retry — says what moved and why it is safe.
#[must_use]
pub fn retry_message(pr: &str, old_head: &str, new_head: &str) -> String {
    format!(
        "PR #{pr}: the head moved {} → {} because this run's own base-sync landed a merge of \
the base branch into it (first parent is the approved head, second parent is already in the \
base). Re-reading the head SHA and retrying the merge once (#8164).",
        short(old_head),
        short(new_head)
    )
}

/// The refusal text for a head move this run cannot claim as its own.
#[must_use]
pub fn foreign_message(pr: &str, why: &str) -> String {
    format!(
        "PR #{pr}: not retrying the head-SHA mismatch — {why}. Re-queueing instead, so the next \
pass re-reads the head and re-checks the verdict against it (#5579/#8164)."
    )
}

pub mod fetch;
pub use fetch::LiveInputs;

#[cfg(test)]
mod tests;
