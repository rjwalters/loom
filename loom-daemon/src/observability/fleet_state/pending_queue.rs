//! Per-repo pending-human queue depth on `fleet.state` (#11955): the
//! eligible `loom:triage` / `loom:curated` counts the collector pass read, plus
//! the operator-held items derived from the view's rows and PR census.

use std::collections::BTreeMap;
use std::sync::Mutex;

use super::{FleetView, RepoView};
use crate::telemetry::kinds::fleet_state::{FleetHoldKind, FleetQueueDepth, FleetStage};

/// Distinct items under an operator hold in `repo` (#11955): hold rows of an
/// operator kind plus `merge_hold` rows, and any `merge_hold` PRs the census
/// counts that have no issue row (so a PR seen both ways counts once). `None`
/// when the repo's PR census was not read whole. Pure.
pub(super) fn operator_held(repo: &RepoView) -> Option<u32> {
    let census = repo.census.as_ref()?;
    let is_operator = |kind: Option<FleetHoldKind>| {
        matches!(
            kind,
            Some(
                FleetHoldKind::Operator
                    | FleetHoldKind::OperatorOnly
                    | FleetHoldKind::OperatorDecision
            )
        )
    };
    let merge_hold_rows = repo
        .rows
        .values()
        .filter(|row| row.stage == FleetStage::MergeHold)
        .count();
    let held_rows = repo
        .rows
        .values()
        .filter(|row| row.stage == FleetStage::MergeHold || is_operator(row.hold_kind))
        .count();
    let census_merge_hold = census
        .by_stage
        .get(FleetStage::MergeHold.as_str())
        .copied()
        .unwrap_or(0);
    let unrowed = usize::try_from(census_merge_hold)
        .unwrap_or(usize::MAX)
        .saturating_sub(merge_hold_rows);
    Some(u32::try_from(held_rows + unrowed).unwrap_or(u32::MAX))
}

/// The last complete collector-pass `(awaiting_curation, awaiting_approval)`
/// eligible counts per repo (#11955), carried here between passes so a tick
/// pass re-stamps the same `pending_queue` a reader saw (never a stale guess
/// from a partial listing). Owned by this sibling module rather than
/// `PassState` per the file-size policy; the collector/tick entry points hold
/// the pass lock, so access is already serialised.
static LAST_ELIGIBLE: Mutex<BTreeMap<String, (u32, u32)>> = Mutex::new(BTreeMap::new());

/// Collector pass (also diffing operator recency, #11956): record this pass's eligible counts — a repo whose triage or
/// curated listing was incomplete drops out (unknown, never zero) — then stamp
/// the view.
pub(super) fn observe(
    view: &mut FleetView,
    eligible: BTreeMap<String, (u32, u32)>,
    now: chrono::DateTime<chrono::Utc>,
) {
    let mut last = LAST_ELIGIBLE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *last = eligible;
    annotate_pending(view, &last);
    super::operator_recency::observe(view, now);
}

/// Tick pass: re-stamp from the last collector-pass counts (no forge read).
pub(super) fn restamp(view: &mut FleetView, now: chrono::DateTime<chrono::Utc>) {
    let last = LAST_ELIGIBLE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    annotate_pending(view, &last);
    super::operator_recency::restamp(view, now);
}

/// Stamp each repo with its pending-human queue depth (#11955) from the last
/// complete collector-pass eligible counts; a repo with no entry in
/// `eligible` (an incomplete or unread listing) is left `None`, never zero.
/// Both passes call this so the fact is stable between forge reads.
pub(super) fn annotate_pending(view: &mut FleetView, eligible: &BTreeMap<String, (u32, u32)>) {
    for (repo, (curation, approval)) in eligible {
        let state = view.repos.entry(repo.clone()).or_default();
        state.pending_queue = Some(FleetQueueDepth {
            awaiting_curation: *curation,
            awaiting_approval: *approval,
            operator_held: operator_held(state),
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "pending_queue_tests.rs"]
mod tests;
