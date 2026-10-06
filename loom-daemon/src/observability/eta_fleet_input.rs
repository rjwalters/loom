//! The fleet-state emitter's read of the ETA tracker (#10196), split out of
//! [`super`] to keep it under the file-size budget.

use super::lock;

/// What [`super::super::fleet_state`] needs (#10196): the tracker's live items,
/// the last pass's open-PR census and plan slots, and this host's id. `None`
/// when ETA is disabled. It is cloned out under the one tracker lock, like
/// [`super::snapshot_input`], so the emitter runs no tracker code.
pub(in crate::observability) fn fleet_state_input(
) -> Option<crate::observability::fleet_state::FleetInput> {
    let guard = lock();
    let state = guard.as_ref()?;
    Some(crate::observability::fleet_state::FleetInput {
        host_id: state.host_id.clone(),
        items: state.tracker.live_items(),
        census: state.tracker.pr_census(),
        slots: state.tracker.plan_slots(),
    })
}
