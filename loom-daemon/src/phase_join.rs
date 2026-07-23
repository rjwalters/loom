//! Phase-join barrier gate for the daemon-native epic supervisor (#3842).
//!
//! An epic decomposes into ordered phases; each phase materializes a wave of
//! `loom:epic-phase` children. The fork-join barrier keeps phases strictly
//! ordered: the supervisor must **never** fire phase N+1's materialization
//! (or the epic-close transition) while any child of the current phase is
//! still open.
//!
//! This module implements Phase 2's barrier half in isolation:
//! [`epic_join_ready`] is the pure predicate the supervisor consults, and
//! [`barrier_admits`] gates the two phase-boundary transitions against it.
//! Neither touches the forge — the Phase 3 supervisor loop feeds them the
//! already-fetched [`PhaseChild`] set from [`crate::epic_state`].
//!
//! # Conformance
//!
//! The authoritative model is `loom-tools/src/loom_tools/state_machine.py`
//! (#3841). Its epic-supervisor lane routes both phase-boundary edges through
//! `epic:phase_join`, and each such edge declares a `barrier` (enforced by the
//! model's `barrier hygiene` validator):
//!
//! ```text
//! epic:phase_join → epic:active  (barrier: "advance: dispatch next phase")
//! epic:phase_join → epic:done    (barrier: "join: all phases complete")
//! ```
//!
//! Both edges are admissible only when the barrier below is satisfied — i.e.
//! every current-phase child is closed. The predicate here is deliberately
//! consistent with [`crate::epic_state::derive_epic_state`]: the "current
//! phase" is the highest materialized phase number, and the barrier is
//! satisfied exactly when that phase has no open child (the same condition
//! that lifts the epic out of [`EpicState::Active`]).

use crate::epic_state::{EpicState, PhaseChild};

/// The two fork-join phase-boundary transitions the barrier gates.
///
/// Both are fired by the `Supervisor` role from the `epic:phase_join` state in
/// the Python model; each carries a non-empty `barrier` string there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PhaseBoundary {
    /// `epic:phase_join → epic:active`: the current phase joined and more
    /// phases remain, so materialize the next phase's children.
    /// (model barrier: *advance: dispatch next phase*).
    AdvanceToNextPhase,
    /// `epic:phase_join → epic:done`: the current (final) phase joined and no
    /// phases remain, so close the epic.
    /// (model barrier: *join: all phases complete*).
    CloseEpic,
}

impl PhaseBoundary {
    /// The destination epic-state id this boundary transitions into.
    #[must_use]
    pub fn dst_state_id(self) -> &'static str {
        match self {
            PhaseBoundary::AdvanceToNextPhase => "epic:active",
            PhaseBoundary::CloseEpic => "epic:done",
        }
    }

    /// The model's `barrier` string for this phase-boundary edge.
    #[must_use]
    pub fn barrier_label(self) -> &'static str {
        match self {
            PhaseBoundary::AdvanceToNextPhase => "advance: dispatch next phase",
            PhaseBoundary::CloseEpic => "join: all phases complete",
        }
    }
}

/// The highest phase number among the epic's materialized children — the
/// "current phase" in flight. Returns `None` when there are no children.
#[must_use]
fn current_phase(children: &[PhaseChild]) -> Option<u32> {
    children.iter().map(|c| c.phase).max()
}

/// The fork-join barrier predicate: `true` iff every child of the **current
/// phase** is closed.
///
/// The current phase is the highest-numbered materialized phase (the supervisor
/// expands phases in order, so the newest wave marks the phase in flight). The
/// barrier is satisfied when that phase has no open child — the exact condition
/// under which the epic leaves [`EpicState::Active`].
///
/// Returns `false` when there are no children at all: a `designed` epic that
/// has dispatched nothing is not at a join point.
///
/// This is the predicate the supervisor MUST check before firing either
/// phase-boundary transition; while it is `false`, the barrier holds and no
/// phase advance or epic-close may occur.
#[must_use]
pub fn epic_join_ready(children: &[PhaseChild]) -> bool {
    let Some(cur) = current_phase(children) else {
        // No children materialized → nothing has forked, so nothing can join.
        return false;
    };
    // Ready iff no child of the current phase is still open.
    !children.iter().any(|c| c.phase == cur && c.open)
}

/// True if *any* materialized child (of any phase) is still open.
///
/// A stricter companion to [`epic_join_ready`]: under the ordering invariant
/// (phase N+1 is never materialized until phase N joins) earlier phases are
/// always closed, so this agrees with [`epic_join_ready`]. Exposed so the
/// supervisor can assert the invariant defensively.
#[must_use]
pub fn any_child_open(children: &[PhaseChild]) -> bool {
    children.iter().any(|c| c.open)
}

/// The barrier gate: given an epic's derived `state` and its `children`, return
/// the enabled phase-boundary transition **only** if the barrier admits it,
/// else `None`.
///
/// This is the single choke point the supervisor calls to decide whether it may
/// fire a phase-boundary edge:
///
/// * [`EpicState::PhaseJoin`] → [`PhaseBoundary::AdvanceToNextPhase`], but only
///   when [`epic_join_ready`] holds.
/// * [`EpicState::Done`] → [`PhaseBoundary::CloseEpic`], but only when
///   [`epic_join_ready`] holds.
/// * Any other state (`NeedsDecomp` / `Designed` / `Active`) → `None`: these are
///   not phase-boundary points, and in particular `Active` means the current
///   phase still has an open child, so the barrier explicitly forbids advancing.
///
/// Returning `None` is the barrier holding: the supervisor cannot fire phase
/// N+1 materialization or epic-close while it is `None`.
#[must_use]
pub fn barrier_admits(state: EpicState, children: &[PhaseChild]) -> Option<PhaseBoundary> {
    // Defence in depth: the barrier is only ever satisfied when the current
    // phase is fully closed, regardless of how `state` was derived. If the two
    // disagree (they should not), the conservative choice is to hold the
    // barrier and return `None`.
    if !epic_join_ready(children) {
        return None;
    }
    match state {
        EpicState::PhaseJoin => Some(PhaseBoundary::AdvanceToNextPhase),
        EpicState::Done => Some(PhaseBoundary::CloseEpic),
        // Not a phase-boundary state.
        EpicState::NeedsDecomp | EpicState::Designed | EpicState::Active => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn open(phase: u32) -> PhaseChild {
        PhaseChild::new(phase, true)
    }
    fn closed(phase: u32) -> PhaseChild {
        PhaseChild::new(phase, false)
    }

    // ===== epic_join_ready =====

    #[test]
    fn test_join_not_ready_with_no_children() {
        // Designed epic: nothing forked, nothing to join.
        assert!(!epic_join_ready(&[]));
    }

    #[test]
    fn test_join_not_ready_current_phase_open() {
        // Phase 1 has an open child ⇒ barrier holds.
        let children = [open(1), closed(1)];
        assert!(!epic_join_ready(&children));
    }

    #[test]
    fn test_join_ready_current_phase_all_closed() {
        let children = [closed(1), closed(1)];
        assert!(epic_join_ready(&children));
    }

    #[test]
    fn test_join_ready_ignores_lower_phase_when_current_closed() {
        // Current phase is 2 (max); both its children closed ⇒ ready, even
        // though we also check only the current phase per the spec.
        let children = [closed(1), closed(2), closed(2)];
        assert!(epic_join_ready(&children));
    }

    #[test]
    fn test_join_not_ready_when_current_phase_2_open() {
        // Current phase 2 has an open child ⇒ not ready.
        let children = [closed(1), open(2)];
        assert!(!epic_join_ready(&children));
    }

    // ===== any_child_open =====

    #[test]
    fn test_any_child_open() {
        assert!(!any_child_open(&[]));
        assert!(!any_child_open(&[closed(1), closed(2)]));
        assert!(any_child_open(&[closed(1), open(2)]));
    }

    // ===== barrier_admits: gates the two boundary transitions =====

    #[test]
    fn test_barrier_admits_advance_on_phase_join_ready() {
        let children = [closed(1)];
        assert_eq!(
            barrier_admits(EpicState::PhaseJoin, &children),
            Some(PhaseBoundary::AdvanceToNextPhase)
        );
    }

    #[test]
    fn test_barrier_admits_close_on_done_ready() {
        let children = [closed(1), closed(2)];
        assert_eq!(barrier_admits(EpicState::Done, &children), Some(PhaseBoundary::CloseEpic));
    }

    #[test]
    fn test_barrier_holds_when_current_phase_open_even_if_state_says_join() {
        // Contrived disagreement: a caller asks whether it may advance while the
        // current phase still has an open child. The barrier must refuse.
        let children = [open(1)];
        assert_eq!(barrier_admits(EpicState::PhaseJoin, &children), None);
        assert_eq!(barrier_admits(EpicState::Done, &children), None);
    }

    #[test]
    fn test_barrier_denies_non_boundary_states() {
        let closed_children = [closed(1)];
        // Active is never a boundary point.
        assert_eq!(barrier_admits(EpicState::Active, &closed_children), None);
        // Designed / NeedsDecomp likewise.
        assert_eq!(barrier_admits(EpicState::Designed, &closed_children), None);
        assert_eq!(barrier_admits(EpicState::NeedsDecomp, &[]), None);
    }

    #[test]
    fn test_active_state_with_open_child_never_advances() {
        // The canonical "barrier holds" case: an in-flight phase.
        let children = [open(2), closed(2), closed(1)];
        assert!(!epic_join_ready(&children));
        assert_eq!(barrier_admits(EpicState::Active, &children), None);
    }

    // ===== boundary-metadata conformance with the Python model =====

    #[test]
    fn test_phase_boundary_metadata_matches_model() {
        assert_eq!(PhaseBoundary::AdvanceToNextPhase.dst_state_id(), "epic:active");
        assert_eq!(PhaseBoundary::CloseEpic.dst_state_id(), "epic:done");
        assert_eq!(
            PhaseBoundary::AdvanceToNextPhase.barrier_label(),
            "advance: dispatch next phase"
        );
        assert_eq!(PhaseBoundary::CloseEpic.barrier_label(), "join: all phases complete");
    }

    // ===== interleaved child-close events against the barrier =====

    /// Simulate the children of the current phase closing one at a time, in
    /// several interleavings, and assert the barrier flips to ready exactly when
    /// the last one closes — never before.
    #[test]
    fn test_barrier_holds_until_last_child_closes() {
        // Three children in the current phase (phase 1).
        let close_orders: [[usize; 3]; 3] = [[0, 1, 2], [2, 0, 1], [1, 2, 0]];

        for order in close_orders {
            let mut children = vec![open(1), open(1), open(1)];
            for (step, &idx) in order.iter().enumerate() {
                children[idx] = closed(1);
                let closed_count = children.iter().filter(|c| !c.open).count();
                let ready = epic_join_ready(&children);
                if step < 2 {
                    // Still at least one open ⇒ barrier holds.
                    assert!(
                        !ready,
                        "barrier lifted early after closing {closed_count}/3 (order {order:?})"
                    );
                    // And no boundary transition is admitted.
                    assert_eq!(barrier_admits(EpicState::PhaseJoin, &children), None);
                } else {
                    // Last child closed ⇒ barrier ready.
                    assert!(ready, "barrier did not lift after all closed (order {order:?})");
                    assert_eq!(
                        barrier_admits(EpicState::PhaseJoin, &children),
                        Some(PhaseBoundary::AdvanceToNextPhase)
                    );
                }
            }
        }
    }

    /// Two phases interleaving: phase 2's children must all close before the
    /// join can fire, and an open phase-2 child while phase 1 is closed still
    /// holds the barrier.
    #[test]
    fn test_two_phase_interleaving() {
        // Phase 1 fully closed, phase 2 forked with two children.
        let mut children = vec![closed(1), open(2), open(2)];
        assert!(!epic_join_ready(&children)); // phase 2 in flight

        children[1] = closed(2);
        assert!(!epic_join_ready(&children)); // one phase-2 child still open

        children[2] = closed(2);
        assert!(epic_join_ready(&children)); // phase 2 joined
        assert_eq!(barrier_admits(EpicState::Done, &children), Some(PhaseBoundary::CloseEpic));
    }
}
