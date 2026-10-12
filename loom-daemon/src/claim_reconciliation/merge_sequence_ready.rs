//! Ready-first sequencing (#10371): approved work never convoys behind a
//! predecessor that is not about to land.
//!
//! Before #10371 the merge-sequencing pass ordered every overlap component
//! oldest-first, so a Judge-approved PR waited behind an older overlapping PR
//! that had no verdict, or a `loom:changes-requested` one. The order only
//! saves rebases when the predecessor is about to land; an unapproved
//! predecessor is not, so the hold cost throughput and saved nothing. On
//! 2026-10-05 five approved PRs sat in one chain behind a single
//! changes-requested head.
//!
//! Three rules, all keyed on one predicate, [`ready`]:
//!
//! 1. **Ready-first ordering** ([`tier`]): within a component, ready members
//!    are placed ahead of non-ready ones. Inside each tier the #10060 rank
//!    still applies (stalled last, stars first, then oldest-first), and a
//!    constraint edge (trusted marker, stacked base) still beats any rank.
//! 2. **No new holds behind a non-ready head** ([`drop_edges_behind_unready`]):
//!    the planner never emits a shared-files `source=pass` edge whose
//!    predecessor is not [`ready`].
//! 3. **Release when the head stops being ready** ([`with_readiness`]): an
//!    existing SOFT hold whose predecessor, as this tick's listing shows it,
//!    is no longer ready is released ([`HoldAction::ReleaseNotReady`]). This
//!    is the verdict-change re-plan: the release happens on the first tick
//!    that sees the flip, and the next tick's plan re-derives the order from
//!    the new readiness (rule 1), so the follower is not put back behind it.
//!
//! **Stacked bases are exempt from rules 2 and 3.** A follower whose base
//! branch is the predecessor's head branch ([`EdgeReason::StackedBase`])
//! depends on it, and the hold is what stops it merging into an unready
//! branch; readiness only governs rebase-saving (shared-files) order. Rule 3
//! mirrors the #10077 no-overlap check: it releases only when the pair is
//! provably not stacked, and an empty branch name is unknown, so it keeps the
//! hold (fail closed).
//!
//! **One-tick gap for non-ready followers.** When a follower's nearest
//! overlapping predecessor is not ready, its edge is dropped and the follower
//! is not re-attached to a farther ready member. That is harmless while the
//! follower itself is not ready (nothing is about to land), and once it gets
//! `loom:pr` the next tick re-plans it against the ready members.
//!
//! What it never touches: HARD holds (no `source=`, the human-authored shape)
//! stay [`HoldAction::HoldHard`] and never auto-expire; consolidation
//! reservations (`cons-` plans, ADR-0023 §1) keep their own contract; a
//! predecessor missing from the listing is never judged (fail closed); and
//! every other Phase-1 decision (release, void, expiry, stall) is returned
//! unchanged.
//!
//! **Transitive readiness (#10465).** An approved predecessor is not
//! necessarily *landable*: `loom:pr` can sit on a PR that is itself
//! `loom:sequenced` behind a non-ready PR, or that is `loom:operator` (which
//! is also where an exhausted re-date budget ends up). Holding a ready PR
//! behind it saves no rebase and costs hours, so [`demote_unlandable`]
//! derives, from the listing and the already-read markers, which PRs sit
//! behind a non-ready chain and tags them in memory with
//! [`NOT_LANDABLE_LABEL`]; [`ready`] then treats them as not ready, so rules
//! 1-3 above all become transitive. Hard (human) holds are untouched, as
//! before: only the predicate for *predecessors* changes.
//!
//! "Ready" is read from labels only: CI state and mergeability are not in the
//! open-PR listing, and fetching them per PR per tick would spend the API
//! budget #10332 is reducing. `loom:ci-failure` is the CI proxy the fleet
//! already maintains.

use std::collections::BTreeMap;

use super::stall::CONSOLIDATION_PLAN_PREFIX;
use super::{
    evaluate, EdgeReason, HoldAction, KeepReason, PredecessorState, SequenceGroup, SequenceMarker,
    SequencePr, Verdict, SOURCE_PASS,
};

/// The verdict label an approved PR carries.
pub const APPROVED_LABEL: &str = "loom:pr";

/// Labels that make an approved PR not ready to land: a review finding, a
/// red CI run, or a block.
pub const NOT_READY_LABELS: [&str; 5] = [
    "loom:changes-requested",
    "loom:ci-failure",
    "loom:blocked",
    // Parked for a human; a re-date budget that ran out lands here (#10465).
    "loom:operator",
    NOT_LANDABLE_LABEL,
];

/// In-memory only (never written to the forge): set by [`demote_unlandable`]
/// on an approved PR that is `loom:sequenced` behind a PR that is not ready
/// (#10465).
pub const NOT_LANDABLE_LABEL: &str = "loom-internal:behind-unready-chain";

/// Tag every PR that sits behind a non-ready chain with
/// [`NOT_LANDABLE_LABEL`]. A PR is demoted when it carries the hold label,
/// has a trusted marker in `markers`, and its marker's predecessor is open in
/// `prs` and is not (transitively) ready. A hold with no marker (manual), a
/// predecessor outside the listing, and a cycle all keep today's reading
/// (fail toward the pre-#10465 behavior). Non-mutating; returns the tagged
/// copy.
#[must_use]
pub fn demote_unlandable(
    prs: &[SequencePr],
    markers: &BTreeMap<u32, SequenceMarker>,
) -> Vec<SequencePr> {
    let by_number: BTreeMap<u32, &SequencePr> = prs.iter().map(|p| (p.number, p)).collect();
    fn landable(
        n: u32,
        by_number: &BTreeMap<u32, &SequencePr>,
        markers: &BTreeMap<u32, SequenceMarker>,
        seen: &mut Vec<u32>,
    ) -> bool {
        let Some(pr) = by_number.get(&n) else {
            return true;
        };
        if !ready(pr) {
            return false;
        }
        if seen.contains(&n) || !pr.has(super::SEQUENCE_LABEL) {
            return true;
        }
        let Some(m) = markers.get(&n) else {
            return true;
        };
        seen.push(n);
        landable(m.after, by_number, markers, seen)
    }
    prs.iter()
        .map(|p| {
            let mut out = p.clone();
            if ready(p)
                && p.has(super::SEQUENCE_LABEL)
                && !landable(p.number, &by_number, markers, &mut Vec::new())
            {
                out.labels.push(NOT_LANDABLE_LABEL.to_string());
            }
            out
        })
        .collect()
}

/// Is `pr` ready to land: Judge-approved, with no label saying otherwise?
#[must_use]
pub fn ready(pr: &SequencePr) -> bool {
    pr.has(APPROVED_LABEL) && !NOT_READY_LABELS.iter().any(|l| pr.has(l))
}

/// The ordering tier: `0` for ready members, `1` for the rest. Sorts ready
/// work first; the caller's rank breaks ties inside a tier.
#[must_use]
pub fn tier(pr: &SequencePr) -> u8 {
    u8::from(!ready(pr))
}

/// Remove every planned shared-files edge whose predecessor is not
/// [`ready`]. A predecessor absent from `by_number` is treated as not ready:
/// no such hold is ever written against a PR whose state this tick did not
/// read. [`EdgeReason::StackedBase`] edges are always kept — a stacked
/// follower depends on its base, whatever the base's readiness.
pub fn drop_edges_behind_unready(group: &mut SequenceGroup, by_number: &BTreeMap<u32, SequencePr>) {
    group.edges.retain(|e| {
        e.reason == EdgeReason::StackedBase || by_number.get(&e.after).is_some_and(ready)
    });
}

/// Is `follower` provably not stacked on `head`? Both branch names must be
/// known (non-empty) and differ — the #10077 `release_candidate` rule.
fn provably_unstacked(follower: &SequencePr, head: &SequencePr) -> bool {
    !follower.base_ref.is_empty() && !head.head_ref.is_empty() && follower.base_ref != head.head_ref
}

/// Phase 1's decision for one hold, plus the not-ready release: a SOFT hold
/// the other rules would keep ([`HoldAction::HoldSoft`]) is released when
/// its predecessor was read open at the recorded head (the ordinary
/// in-flight state) AND its row in this tick's listing is not [`ready`].
///
/// `head` is the predecessor's row in this tick's listing; `None` (outside
/// the listing) keeps the decision unchanged, as do an unreadable
/// predecessor (`pred: None`), an unknown follower head, and a follower that
/// is not provably unstacked from `head` (stacked, or either branch name
/// unknown) — every release needs a positive signal (the #9378 asymmetry).
/// Hard holds and consolidation reservations are never touched.
#[must_use]
pub fn with_readiness(
    action: HoldAction,
    marker: &SequenceMarker,
    pred: Option<&PredecessorState>,
    follower: &SequencePr,
    head: Option<&SequencePr>,
) -> HoldAction {
    let soft = marker.source.as_deref() == Some(SOURCE_PASS)
        && !marker.plan.starts_with(CONSOLIDATION_PLAN_PREFIX);
    let in_flight = pred
        .zip(follower.head_sha.as_deref())
        .is_some_and(|(p, fh)| {
            matches!(evaluate(marker, p, fh), Verdict::Keep(KeepReason::InFlight))
        });
    match head {
        Some(h)
            if action == HoldAction::HoldSoft
                && soft
                && in_flight
                && provably_unstacked(follower, h)
                && !ready(h) =>
        {
            HoldAction::ReleaseNotReady
        }
        _ => action,
    }
}

/// The release reason for [`HoldAction::ReleaseNotReady`].
#[must_use]
pub fn release_reason(marker: &SequenceMarker) -> String {
    format!(
        "#{} is not ready to land (no `loom:pr` verdict, it carries `loom:changes-requested`, \
         `loom:ci-failure`, `loom:blocked` or `loom:operator`, or it is itself sequenced behind a \
         PR that is not ready, #10465) — ready work is not held behind it (#10371), and \
         the order is re-derived from current readiness on the next tick",
        marker.after
    )
}

#[cfg(test)]
#[path = "merge_sequence_ready_tests.rs"]
mod tests;
