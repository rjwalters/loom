//! Structural invariant tests for the epic-supervisor state machine
//! (`loom_daemon::epic_state`).
//!
//! # History
//!
//! This file replaces the former `epic_conformance.rs`, which derived its
//! expectations by shelling out to a Python subprocess invoking the
//! now-deleted Python state-machine model's JSON exporter, and asserting the
//! Rust [`epic_transition_table`] matched it edge-for-edge, skipping silently
//! when the subprocess/model were unavailable.
//!
//! Per issue #4310: once `loom_daemon::epic_state` is the sole, authoritative
//! representation of the epic-supervisor lane (the Python model modeled all
//! four label lanes — issue, PR, proposal, epic — but only the epic lane ever
//! had a Rust mirror), a conformance test against a *deleted* second
//! implementation is meaningless, and a checked-in frozen snapshot would just
//! be a hardcoded mirror with no second source of truth behind it (exactly
//! the anti-pattern #3867 warned about). What remains semantically meaningful
//! independent of any second implementation are the table's own structural
//! invariants, asserted here directly — no subprocess, no skip path, always
//! run.

use loom_daemon::epic_state::{epic_state_ids, epic_transition_table, EpicState};

/// Invariant 1: exactly five derived epic states.
#[test]
fn exactly_five_epic_states() {
    let ids = epic_state_ids();
    assert_eq!(ids.len(), 5, "expected exactly five derived epic states, got {ids:?}");
}

/// Invariant 2: `epic:done` is the sole terminal state.
#[test]
fn epic_done_is_sole_terminal_state() {
    let all_states = [
        EpicState::NeedsDecomp,
        EpicState::Designed,
        EpicState::Active,
        EpicState::PhaseJoin,
        EpicState::Done,
    ];

    let terminals: Vec<&'static str> = all_states
        .into_iter()
        .filter(|s| s.is_terminal())
        .map(|s| s.as_state_id())
        .collect();

    assert_eq!(
        terminals,
        vec!["epic:done"],
        "epic:done must be the sole terminal derived state"
    );
}

/// Invariant 3: exactly five intra-lane edges in the transition table.
#[test]
fn exactly_five_intra_lane_edges() {
    let table = epic_transition_table();
    assert_eq!(table.len(), 5, "expected exactly five epic transition-table edges");
}

/// Invariant 4: barrier hygiene — every edge touching `epic:phase_join` (all
/// three: active->phase_join, phase_join->active, phase_join->done) carries a
/// non-empty barrier string.
#[test]
fn phase_join_edges_carry_a_barrier() {
    let table = epic_transition_table();

    let boundary: Vec<_> = table
        .iter()
        .filter(|e| e.src == "epic:phase_join" || e.dst == "epic:phase_join")
        .collect();

    assert_eq!(
        boundary.len(),
        3,
        "expected exactly three phase-join boundary edges (active->phase_join, \
         phase_join->active, phase_join->done), found {boundary:?}"
    );

    for edge in boundary {
        assert!(
            !edge.barrier.is_empty(),
            "phase-boundary edge {}->{} must declare a non-empty barrier",
            edge.src,
            edge.dst
        );
    }
}
