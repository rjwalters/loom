"""Tests for the executable Loom state-machine spec.

Two responsibilities:

1. The **canonical** graph validates clean (no structural ERRORs).
2. Each structural validator *fires* on a deliberately-broken variant — a
   guard is worthless if it never trips.

Also guards the README Mermaid diagram against drift from ``render_mermaid()``.
"""

from __future__ import annotations

import dataclasses
import re
from pathlib import Path

import pytest

from loom_tools.state_machine import (
    Lane,
    PHASE_JOIN_STATE,
    Role,
    State,
    StateMachine,
    Transition,
    canonical,
)


# ---------------------------------------------------------------------------
# Canonical graph
# ---------------------------------------------------------------------------


def test_canonical_validates_clean():
    """The shipped canonical graph must have zero structural errors."""
    res = canonical().validate()
    assert res.ok(), f"canonical graph has errors: {res.errors}"
    assert res.errors == []


def test_validate_unpacks_as_tuple():
    """validate() unpacks as (errors, warnings, notes)."""
    errors, warnings, notes = canonical().validate()
    assert errors == []
    assert isinstance(warnings, list)
    assert isinstance(notes, list)


def test_canonical_has_four_lanes():
    machine = canonical()
    lanes = {s.lane for s in machine.states}
    assert lanes == {Lane.ISSUE, Lane.PR, Lane.PROPOSAL, Lane.EPIC}


def test_epic_states_all_derived_on_loom_epic():
    """All epic-supervisor states ride the single loom:epic label."""
    machine = canonical()
    epic_states = [s for s in machine.states if s.lane is Lane.EPIC]
    assert epic_states, "expected epic states"
    for s in epic_states:
        assert s.label == "loom:epic", s
        assert s.derived is True, s


def test_decomposition_edge_present():
    """The fix for the epic:needs_decomp dead-end must be in the canonical graph."""
    machine = canonical()
    assert any(
        t.src == "epic:needs_decomp" and t.dst == "epic:designed"
        for t in machine.transitions
    ), "canonical graph is missing the Champion decomposition edge"


def test_single_entry_state():
    machine = canonical()
    entries = [s.id for s in machine.states if s.entry]
    assert entries == ["new"]


# ---------------------------------------------------------------------------
# Canonical warnings / notes are present (the checks *do* run)
# ---------------------------------------------------------------------------


def test_canonical_reports_autonomy_gaps():
    """Human-fired edges surface as autonomy-gap warnings."""
    _, warnings, _ = canonical().validate()
    assert any("curated->loom:issue" in w or "curated" in w for w in warnings)
    assert all("autonomy gap" in w for w in warnings)


def test_canonical_label_conflation_is_a_note_not_error():
    """loom:epic reuse across derived states is a NOTE, never an ERROR."""
    res = canonical().validate()
    assert any("loom:epic" in n and "derived" in n for n in res.notes)
    assert not any("conflates" in e for e in res.errors)


def test_canonical_enumerates_3707_creates_issues_edges():
    res = canonical().validate()
    mutex_notes = [n for n in res.notes if "#3707" in n]
    assert len(mutex_notes) == 1
    note = mutex_notes[0]
    # All four issue-creating edges must be enumerated.
    for frag in [
        "new->loom:architect",
        "new->loom:hermit",
        "new->loom:auditor",
        "epic:needs_decomp->epic:designed",
    ]:
        assert frag in note, f"missing {frag} in #3707 note"


# ---------------------------------------------------------------------------
# Validator 1: unknown endpoints
# ---------------------------------------------------------------------------


def test_unknown_endpoint_fires():
    machine = canonical()
    machine.transitions.append(
        Transition("loom:building", "nonexistent:state", Role.BUILDER, "bogus")
    )
    res = machine.validate()
    assert not res.ok()
    assert any("unknown destination state 'nonexistent:state'" in e for e in res.errors)


# ---------------------------------------------------------------------------
# Validator 2 + 3: remove the decomposition edge -> unreachable + dead-end
# ---------------------------------------------------------------------------


def _drop_transition(machine: StateMachine, src: str, dst: str) -> None:
    machine.transitions = [
        t for t in machine.transitions if not (t.src == src and t.dst == dst)
    ]


def test_removing_decomposition_edge_fires_deadend_and_unreachable():
    machine = canonical()
    _drop_transition(machine, "epic:needs_decomp", "epic:designed")
    res = machine.validate()
    assert not res.ok()

    # dead-end: epic:needs_decomp now has no outgoing edge.
    assert any(
        "epic:needs_decomp" in e and "dead-end" in e for e in res.errors
    ), res.errors

    # unreachable: the whole downstream supervisor sub-graph.
    for dead in ("epic:designed", "epic:active", "epic:phase_join", "epic:done"):
        assert any(
            f"'{dead}'" in e and "unreachable" in e for e in res.errors
        ), f"expected {dead} unreachable; errors={res.errors}"


def test_no_entry_state_fires():
    machine = canonical()
    machine.states = [
        dataclasses.replace(s, entry=False) if s.entry else s
        for s in machine.states
    ]
    res = machine.validate()
    assert not res.ok()
    assert any("no entry state" in e for e in res.errors)


def test_terminal_with_outgoing_edge_fires():
    machine = canonical()
    machine.transitions.append(
        Transition("closed", "loom:issue", Role.HUMAN, "reopen (illegal)")
    )
    res = machine.validate()
    assert not res.ok()
    assert any("terminal state 'closed'" in e for e in res.errors)


# ---------------------------------------------------------------------------
# Validator 4: label conflation on non-derived states
# ---------------------------------------------------------------------------


def test_label_conflation_on_non_derived_fires():
    machine = canonical()
    # Two non-derived states sharing one real label.
    machine.states.append(State("loom:issue-dup", Lane.ISSUE, label="loom:issue"))
    machine.transitions.append(
        Transition("loom:curated", "loom:issue-dup", Role.HUMAN, "dup")
    )
    machine.transitions.append(
        Transition("loom:issue-dup", "loom:building", Role.BUILDER, "dup")
    )
    res = machine.validate()
    assert not res.ok()
    assert any("conflates" in e and "loom:issue" in e for e in res.errors)


# ---------------------------------------------------------------------------
# Validator 6: barrier hygiene
# ---------------------------------------------------------------------------


def test_dropping_a_barrier_fires_missing_barrier_error():
    machine = canonical()
    new_transitions = []
    dropped = False
    for t in machine.transitions:
        if StateMachine.is_phase_boundary(t) and t.barrier and not dropped:
            new_transitions.append(dataclasses.replace(t, barrier=""))
            dropped = True
        else:
            new_transitions.append(t)
    assert dropped, "expected at least one phase-boundary edge to strip"
    machine.transitions = new_transitions
    res = machine.validate()
    assert not res.ok()
    assert any("phase-boundary edge" in e and "must declare a barrier" in e for e in res.errors)


def test_every_phase_boundary_edge_has_a_barrier():
    machine = canonical()
    boundary = [t for t in machine.transitions if StateMachine.is_phase_boundary(t)]
    assert boundary, "expected phase-boundary edges around the join state"
    assert all(t.barrier for t in boundary)
    # Sanity: the join state is actually present.
    assert PHASE_JOIN_STATE in machine.state_ids()


# ---------------------------------------------------------------------------
# Validator 5: autonomy gaps fire on a fresh human edge
# ---------------------------------------------------------------------------


def test_autonomy_gap_warning_for_human_role():
    assert Role.HUMAN.daemon_dispatchable is False
    for r in Role:
        if r is not Role.HUMAN:
            assert r.daemon_dispatchable is True


# ---------------------------------------------------------------------------
# Mermaid rendering + README sync
# ---------------------------------------------------------------------------


def test_render_mermaid_is_state_diagram_grouped_by_lane():
    out = canonical().render_mermaid()
    assert out.startswith("stateDiagram-v2")
    for title in ("Issue lane", "PR lane", "Proposal lane", "Epic supervisor lane"):
        assert title in out, f"missing lane group {title!r}"
    # Entry and terminal pseudo-states.
    assert "[*] --> s_new" in out
    assert "s_merged --> [*]" in out


def _repo_root() -> Path:
    # test file: <root>/loom-tools/tests/test_state_machine.py
    return Path(__file__).resolve().parents[2]


def _extract_readme_mermaid() -> str | None:
    readme = _repo_root() / "README.md"
    if not readme.exists():
        return None
    text = readme.read_text(encoding="utf-8")
    # Find the ```mermaid fenced block inside the "Loom State Machine" section.
    m = re.search(r"## Loom State Machine.*?```mermaid\n(.*?)```", text, re.DOTALL)
    if not m:
        return None
    return m.group(1).rstrip("\n")


def test_readme_mermaid_matches_generated():
    """The committed README diagram must match render_mermaid() exactly.

    This is the CI guard that keeps the documentation diagram from drifting
    away from the model. If this fails, regenerate with:
        python -m loom_tools.state_machine --mermaid
    and paste the output into the README "Loom State Machine" section.
    """
    embedded = _extract_readme_mermaid()
    assert embedded is not None, (
        "Could not find a ```mermaid block under a '## Loom State Machine' "
        "heading in README.md"
    )
    generated = canonical().render_mermaid().rstrip("\n")
    assert embedded == generated, (
        "README Mermaid diagram is out of sync with render_mermaid().\n"
        "Regenerate: python -m loom_tools.state_machine --mermaid"
    )
