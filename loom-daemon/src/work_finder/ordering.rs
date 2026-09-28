//! Dispatch-candidate ordering keys and the comparator (#3946, re-keyed by
//! #9244 for `loom:operator-priority`).
//!
//! Moved out of `work_finder.rs` (size-frozen, `.loom/docs/file-size-policy.md`)
//! when #9244 added the operator-priority and red-main-fix keys. The ready
//! queue ([`super::ready_queue`]) and both tick paths rank with the same
//! comparator, so the queue a human reads is the order dispatch uses.

use std::cmp::Ordering;

/// A dispatch candidate tagged with the cross-repo ordering keys: its
/// workspace's priority tier, the operator-priority ("starred") keys, the
/// red-main-fix key, age, and the workspace index used to route the eventual
/// `dispatch()` back to the owning workspace. Built by
/// [`super::ready_queue::key_of`], then globally sorted by [`candidate_cmp`]
/// before the shared concurrency budget is filled.
///
/// `loom:urgent` is **not** a key any more (#9244). The label is still
/// tolerated on an issue; it just no longer changes where the issue sorts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PriorityCandidate {
    /// The owning workspace's index in the `workspaces` slice (dispatch routing).
    pub workspace_idx: usize,
    /// The owning workspace's priority tier (lower = higher priority).
    pub workspace_priority: u32,
    /// Whether the issue carries `loom:operator-priority` (#9244): the
    /// operator starred it, so it sorts ahead of everything else, fleet-wide.
    pub operator_priority: bool,
    /// When the issue was starred (the `labeled` timeline event for
    /// `loom:operator-priority`), when known. Orders starred issues among
    /// themselves; `None` falls back to [`Self::created_at`].
    pub operator_priority_at: Option<String>,
    /// Whether the issue carries the `<!-- loom:main-red-fix -->` marker
    /// **and** its repo's `main` is verified red right now. Resolved before
    /// the key is built, so [`candidate_cmp`] stays a pure function.
    pub main_red_fix: bool,
    /// The issue's creation timestamp for age ordering (oldest-first).
    pub created_at: Option<String>,
    /// The issue number (dispatch target + final deterministic tiebreak).
    pub number: u32,
    /// The issue's `<!-- loom:complexity=<tier> -->` stratum (#4827), carried
    /// from its work item so pass 2's `dispatch()` can stratify the
    /// model-cost A/B arm assignment without re-fetching the body. Not part of
    /// the ordering keys; [`candidate_cmp`] ignores it.
    pub complexity: Option<String>,
}

/// Total ordering over dispatch candidates (#3946, #9244):
///
/// 1. starred (`loom:operator-priority`) first;
/// 2. among starred issues, starred-at ascending — the issue starred first
///    lands first — with a missing starred-at falling back to `createdAt`;
/// 3. red-main fixes first (set only while that repo's `main` is verified red);
/// 4. workspace priority ascending (a tool repo pinned to `0` drains before a
///    product repo at the default `100`);
/// 5. `createdAt` oldest first (a dated issue sorts before an undated one);
/// 6. issue number ascending, so the order is fully deterministic.
///
/// Keys 1-3 are [`lane_cmp`]; the single-workspace tick sorts by those alone
/// so its listing order is untouched when nothing is starred or red.
#[must_use]
pub fn candidate_cmp(a: &PriorityCandidate, b: &PriorityCandidate) -> Ordering {
    lane_cmp(a, b)
        .then_with(|| a.workspace_priority.cmp(&b.workspace_priority))
        .then_with(|| cmp_created_at(&a.created_at, &b.created_at))
        .then_with(|| a.number.cmp(&b.number))
}

/// Keys 1-3 of [`candidate_cmp`]: starred first, then starred-at among
/// starred issues, then red-main fixes. Two unstarred, non-fix candidates
/// compare equal, which is what lets a stable sort by this comparator leave
/// ordinary work in its existing order.
#[must_use]
pub fn lane_cmp(a: &PriorityCandidate, b: &PriorityCandidate) -> Ordering {
    // `true` sorts first: reverse the bool compare (true > false).
    b.operator_priority
        .cmp(&a.operator_priority)
        .then_with(|| {
            if a.operator_priority && b.operator_priority {
                cmp_created_at(starred_at(a), starred_at(b))
            } else {
                Ordering::Equal
            }
        })
        .then_with(|| b.main_red_fix.cmp(&a.main_red_fix))
}

/// The timestamp a starred candidate orders by: its starred-at, else its
/// `createdAt` (#9244 key 2's fallback).
fn starred_at(c: &PriorityCandidate) -> &Option<String> {
    if c.operator_priority_at.is_some() {
        &c.operator_priority_at
    } else {
        &c.created_at
    }
}

/// Oldest-first ordering over optional ISO-8601 timestamps: a dated issue
/// (`Some`) sorts before an undated one (`None`); two dated issues compare
/// lexically (ISO-8601 ⇒ chronological); two undated issues are equal (the
/// caller's number tiebreak decides).
fn cmp_created_at(a: &Option<String>, b: &Option<String>) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => x.cmp(y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}
