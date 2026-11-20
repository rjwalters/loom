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
//! Since #12775 the pass plans only stacked-base edges, so what remains of
//! this module is on the release side, keyed on one predicate, [`ready`]:
//! **release when the head stops being ready** ([`with_readiness`]): an
//! existing SOFT hold whose predecessor, as this tick's listing shows it,
//! is no longer ready is released ([`HoldAction::ReleaseNotReady`]).
//!
//! **Stacked bases are exempt.** A follower whose base branch is the
//! predecessor's head branch depends on it, and the hold is what stops it
//! merging into an unready branch. The rule mirrors the #10077 no-overlap
//! check: it releases only when the pair is provably not stacked, and an
//! empty branch name is unknown, so it keeps the hold (fail closed).
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
//! **Landable now (#12010).** Labels alone miss an approved PR that later
//! goes red or conflicting (the Judge's `loom:ci-failure` is only applied
//! during a review). [`super::landable::tag_unlandable`] reads each ready
//! PR's CI on its current head and its mergeability (conditional, ETag'd
//! reads) and tags a definite failure or conflict in memory
//! ([`super::landable::CI_RED_LABEL`], [`super::landable::CONFLICTING_LABEL`]),
//! so every rule above treats it as not ready. Pending or unknown CI and
//! unknown mergeability change nothing, and [`release_reason`] names the
//! concrete cause.

use std::collections::BTreeMap;

use super::stall::CONSOLIDATION_PLAN_PREFIX;
use super::{
    evaluate, HoldAction, KeepReason, PredecessorState, SequenceMarker, SequencePr, Verdict,
    SOURCE_PASS,
};

/// The verdict label an approved PR carries.
pub const APPROVED_LABEL: &str = "loom:pr";

/// Labels that make an approved PR not ready to land: a review finding, a
/// red CI run, or a block, plus the in-memory "not landable now" tags.
pub const NOT_READY_LABELS: [&str; 7] = [
    "loom:changes-requested",
    "loom:ci-failure",
    "loom:blocked",
    // Parked for a human; a re-date budget that ran out lands here (#10465).
    "loom:operator",
    NOT_LANDABLE_LABEL,
    super::landable::CI_RED_LABEL,
    super::landable::CONFLICTING_LABEL,
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

/// Why an existing hold is still kept, for the read-only `sequence-plan`
/// output (#12595): the first [`with_readiness`] condition that fails, or
/// "predecessor ready". Mirrors that function's order; writes nothing.
#[must_use]
pub fn hold_reason(
    marker: &SequenceMarker,
    pred: Option<&PredecessorState>,
    follower: &SequencePr,
    head: Option<&SequencePr>,
) -> String {
    if marker.source.as_deref() != Some(SOURCE_PASS) {
        return "human hold (no `source=pass`): never auto-released".into();
    }
    if marker.plan.starts_with(CONSOLIDATION_PLAN_PREFIX) {
        return "consolidation reservation: keeps its own contract".into();
    }
    let Some(h) = head else {
        return format!("predecessor #{} is outside this tick's listing", marker.after);
    };
    let in_flight = pred
        .zip(follower.head_sha.as_deref())
        .is_some_and(|(p, fh)| {
            matches!(evaluate(marker, p, fh), Verdict::Keep(KeepReason::InFlight))
        });
    if !in_flight {
        return format!(
            "predecessor #{} is not read open at the recorded head (moved, closed, unreadable, \
             or the follower head moved): the other hold rules decide",
            marker.after
        );
    }
    if !provably_unstacked(follower, h) {
        return format!(
            "stacked on #{} (or a branch name is unknown): readiness does not release it",
            marker.after
        );
    }
    if ready(h) {
        return format!("predecessor #{} is ready", marker.after);
    }
    format!("predecessor #{} is not ready: released on the next tick", marker.after)
}

/// The release reason for [`HoldAction::ReleaseNotReady`]. `head` is the
/// predecessor's row as this tick read it: a live red or conflicting head is
/// named concretely (#12010).
#[must_use]
pub fn release_reason(marker: &SequenceMarker, head: Option<&SequencePr>) -> String {
    if let Some(cause) = head.and_then(super::landable::cause) {
        return format!(
            "#{} is not landable now: {cause} — ready work is not held behind it (#12010), \
             and the order is re-derived from current readiness on the next tick",
            marker.after
        );
    }
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
