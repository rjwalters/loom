//! Per-repo pending-human queue depth on `fleet.state` (#11955): the
//! eligible `loom:triage` / `loom:curated` counts the collector pass read, plus
//! the operator-held items derived from the view's rows and PR census.

use std::collections::BTreeMap;

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
