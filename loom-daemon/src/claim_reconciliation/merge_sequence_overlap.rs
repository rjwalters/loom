//! Releasing recorded holds between PRs that share no changed file (#10077).
//!
//! Before #10060 the planner chained every consecutive pair of a transitive
//! overlap component, so many `loom:sequenced` holds already on the forge
//! order two PRs whose changed-file sets are disjoint. #10060's direct-overlap
//! DAG stops planning new ones, but Phase 2 never touches a follower that
//! already holds, so the old edges would stay until a predecessor landed, a
//! head moved, or the 72 h expiry fired. This module lets Phase 1 release
//! them ([`HoldAction::ReleaseNoOverlap`]).
//!
//! A hold is released only on a positive signal, and every unknown keeps it:
//!
//! - the marker is SOFT (`source=pass`) and is not a consolidation
//!   reservation (`cons-` plan, ADR-0023 §1);
//! - [`evaluate`] says `Keep(InFlight)` — both pinned heads are still current,
//!   so the file sets read now are the sets the order was recorded against;
//! - the predecessor is in this tick's open listing (its `head_ref` is
//!   known), and the follower is not stacked on it (`base_ref != head_ref`);
//! - BOTH changed-file reads succeeded and the sets are disjoint. A failed
//!   read is never "no files".
//!
//! [`TickFiles`] is the one per-tick changed-files cache Phase 1 and Phase 2
//! share, so no PR's files are read twice in a tick.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::stall::{hold_action_with_stall, stall_cause, CONSOLIDATION_PLAN_PREFIX};
use super::{
    evaluate, HoldAction, KeepReason, PredecessorState, SequenceMarker, SequencePr, Verdict,
    SEQUENCE_LABEL, SOURCE_PASS,
};
use crate::merge_pr::sequence::{fetch_predecessor, fetch_trusted_bodies, parse};

/// The changed-file sets read this tick, by PR number. `Some(None)` records
/// a failed read, so it is not retried within the tick and never counts as
/// disjoint.
#[derive(Debug, Default)]
pub struct TickFiles {
    map: BTreeMap<u32, Option<BTreeSet<String>>>,
}

impl TickFiles {
    /// `pr`'s changed files, reading them with `fetch` only on the first
    /// request this tick.
    pub fn get_or_fetch(
        &mut self,
        pr: &SequencePr,
        fetch: impl FnOnce(&SequencePr) -> Option<BTreeSet<String>>,
    ) -> Option<&BTreeSet<String>> {
        self.map
            .entry(pr.number)
            .or_insert_with(|| fetch(pr))
            .as_ref()
    }

    /// The successfully read sets for `numbers` — the shape the planner
    /// takes (a PR without an entry is a singleton there).
    #[must_use]
    pub fn known(&self, numbers: impl IntoIterator<Item = u32>) -> BTreeMap<u32, BTreeSet<String>> {
        numbers
            .into_iter()
            .filter_map(|n| {
                self.map
                    .get(&n)
                    .and_then(Option::as_ref)
                    .map(|f| (n, f.clone()))
            })
            .collect()
    }
}

/// Every release condition that does not need a files read: a soft,
/// non-consolidation marker naming `pred`, which is known from the open
/// listing, and no stacking between the two branches. An empty branch name
/// is unknown, so it can never prove the pair is not stacked.
#[must_use]
pub fn release_candidate(
    marker: &SequenceMarker,
    follower: &SequencePr,
    pred: Option<&SequencePr>,
) -> bool {
    let Some(pred) = pred else {
        return false;
    };
    marker.source.as_deref() == Some(SOURCE_PASS)
        && !marker.plan.starts_with(CONSOLIDATION_PLAN_PREFIX)
        && pred.number == marker.after
        && pred.number != follower.number
        && !pred.head_ref.is_empty()
        && !follower.base_ref.is_empty()
        && follower.base_ref != pred.head_ref
}

/// The pure release rule: [`release_candidate`] plus two successful,
/// disjoint changed-file reads. The caller also requires `Keep(InFlight)`.
#[must_use]
pub fn no_overlap_release(
    marker: &SequenceMarker,
    follower: &SequencePr,
    pred: Option<&SequencePr>,
    follower_files: Option<&BTreeSet<String>>,
    pred_files: Option<&BTreeSet<String>>,
) -> bool {
    let (Some(ff), Some(pf)) = (follower_files, pred_files) else {
        return false;
    };
    release_candidate(marker, follower, pred) && ff.is_disjoint(pf)
}

/// Phase 1's decision with the no-overlap release applied on top of
/// `action` (the [`hold_action_with_stall`] result). Only `HoldSoft` and
/// `ReleaseStalled` can become [`HoldAction::ReleaseNoOverlap`] — when both
/// the stall and the no-overlap release apply, the no-overlap reason is the
/// accurate one. Every other action keeps precedence unchanged. Files are
/// read through `files` only when every other condition already holds.
pub fn with_no_overlap(
    action: HoldAction,
    marker: &SequenceMarker,
    pred_state: Option<&PredecessorState>,
    follower: &SequencePr,
    open: &[SequencePr],
    files: &mut TickFiles,
    mut fetch: impl FnMut(&SequencePr) -> Option<BTreeSet<String>>,
) -> HoldAction {
    if !matches!(action, HoldAction::HoldSoft | HoldAction::ReleaseStalled) {
        return action;
    }
    let in_flight = pred_state
        .zip(follower.head_sha.as_deref())
        .is_some_and(|(p, fh)| {
            matches!(evaluate(marker, p, fh), Verdict::Keep(KeepReason::InFlight))
        });
    let pred = open.iter().find(|p| p.number == marker.after);
    if !in_flight || !release_candidate(marker, follower, pred) {
        return action;
    }
    let Some(pred) = pred else {
        return action;
    };
    let follower_files = files.get_or_fetch(follower, &mut fetch).cloned();
    let pred_files = files.get_or_fetch(pred, &mut fetch);
    if no_overlap_release(marker, follower, Some(pred), follower_files.as_ref(), pred_files) {
        HoldAction::ReleaseNoOverlap
    } else {
        action
    }
}

/// The release comment's reason for [`HoldAction::ReleaseNoOverlap`].
#[must_use]
pub fn release_reason(marker: &SequenceMarker) -> String {
    format!(
        "this PR and #{} share no changed files — the recorded order was transitive-only (it \
         came through a third PR's files, #10077), so no landing order is needed between them",
        marker.after
    )
}

/// The dry run's view of Phase 1: each `(follower, predecessor)` hold the
/// pass would release as no-overlap this tick, oldest follower first. Reads
/// only; writes nothing. Uses the same rule and cache as the live pass.
pub(super) fn would_release(
    gh_bin: &Path,
    root: &Path,
    open: &[SequencePr],
    files: &mut TickFiles,
) -> Vec<(u32, u32)> {
    let bin = gh_bin.to_string_lossy().to_string();
    let (now, bound, max_age) = (chrono::Utc::now(), super::stall_hours(), super::max_age_hours());
    let mut out = Vec::new();
    for pr in open.iter().filter(|p| p.has(SEQUENCE_LABEL)) {
        let Some(marker) =
            fetch_trusted_bodies(&bin, root, "{owner}/{repo}", pr.number).and_then(|b| parse(&b))
        else {
            continue;
        };
        let pred_state = fetch_predecessor(&bin, root, "{owner}/{repo}", marker.after);
        let head = open.iter().find(|p| p.number == marker.after);
        let cause = head.and_then(|h| stall_cause(h, now, bound));
        let action = hold_action_with_stall(
            &marker,
            pred_state.as_ref(),
            pr.head_sha.as_deref(),
            pr.has("loom:pr"),
            max_age,
            cause.as_ref(),
        );
        let fetch = |p: &SequencePr| super::changed_files(gh_bin, root, p);
        let decided = with_no_overlap(action, &marker, pred_state.as_ref(), pr, open, files, fetch);
        if decided == HoldAction::ReleaseNoOverlap {
            out.push((pr.number, marker.after));
        }
    }
    out
}

#[cfg(test)]
#[path = "merge_sequence_overlap_tests.rs"]
mod tests;
