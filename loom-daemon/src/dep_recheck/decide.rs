//! The four-way decision (#7617) — the one comparison extracted from
//! `curator.md`'s prose (epic #7810, PR 4).
//!
//! Turns this pass's `CONCLUSION_HASH` plus the most recent prior marker into
//! an action, and says whether performing that action requires claiming
//! `loom:curating` first.
//!
//! **Pure.** No forge call, no label read. That is the property that makes it
//! usable: the claim question has to be answerable *before* claiming, or every
//! pass takes a claim just to discover it had nothing to say.

/// What this pass should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Nothing to report at all (an empty hash, e.g. `operator-premise` with
    /// `VERDICT=open`). Distinct from `Skip`: there was no conclusion, rather
    /// than a conclusion that has not changed.
    None,
    /// Same conclusion, still inside the staleness window. The no-op path: no
    /// comment, no label change, and — per #7617 — no claim either.
    Skip,
    /// A new or changed conclusion. Always reported, window irrelevant.
    Comment,
    /// Same conclusion, but the prior marker is stale: exactly one refresh.
    Heartbeat,
}

impl Action {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Action::None => "none",
            Action::Skip => "skip",
            Action::Comment => "comment",
            Action::Heartbeat => "heartbeat",
        }
    }

    /// Whether posting this action requires claiming `loom:curating` first.
    ///
    /// True means: claim immediately before posting, and release afterwards
    /// unless the same pass also transitions the issue to `loom:curated` (whose
    /// own label edit drops `loom:curating` in the same command). False means:
    /// do not touch `loom:curating` at all this pass.
    #[must_use]
    pub fn claims(self) -> bool {
        matches!(self, Action::Comment | Action::Heartbeat)
    }
}

/// `curator.md`'s documented default staleness window.
pub const DEFAULT_HEARTBEAT_HOURS: u64 = 24;

/// Decide.
///
/// The order is the contract, and each branch answers a different question:
///
/// 1. no hash → nothing was concluded, so nothing to compare or claim
/// 2. no prior hash, or a changed one → report it; the window is irrelevant
/// 3. unchanged, inside the window → skip, and take no claim
/// 4. unchanged, at or past the window → one heartbeat
#[must_use]
pub fn decide(hash: &str, prior_hash: &str, prior_age_hours: u64, heartbeat_hours: u64) -> Action {
    if hash.is_empty() {
        Action::None
    } else if prior_hash.is_empty() || hash != prior_hash {
        // Two readings of one branch, and the shell spells them as two: no
        // prior marker at all (the first-ever check on this issue), or a
        // conclusion that has changed. The `is_empty()` half is strictly
        // subsumed — `hash` is non-empty here, so it cannot equal an empty
        // prior — and is kept because it names the case a reader looks for.
        //
        // Either way the answer is the same and the window is irrelevant: a
        // changed conclusion is never suppressed. That is also what carries
        // the #6516 orthogonal-blocker escalation, which changes the hash by
        // construction.
        Action::Comment
    } else if prior_age_hours < heartbeat_hours {
        Action::Skip
    } else {
        Action::Heartbeat
    }
}

#[cfg(test)]
mod tests;
