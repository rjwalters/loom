"""Executable specification of the Loom label state machine.

Loom coordinates its AI agents entirely through GitHub/Gitea *labels*: an issue
walks ``loom:triage → loom:curating → loom:curated → loom:issue → loom:building``,
a PR walks ``loom:review-requested ↔ loom:changes-requested → loom:pr → merged``,
proposals originate as ``loom:architect|hermit|auditor``, and epics run a
fork-join supervisor loop. Until now that graph existed **only implicitly**,
scattered across role prompts (``curator.md``, ``champion-epic.md``,
``sweep.md``, …). There was no single source of truth and no mechanical way to
catch a structural regression — an unreachable state, a dead-end, two logical
states conflated onto one label, or a missing fork-join barrier.

This module is that single source of truth: a pure-stdlib model of the graph
plus a :func:`StateMachine.validate` pass that returns ``(errors, warnings,
notes)``. It also renders the graph as a Mermaid ``stateDiagram-v2`` (the
``--mermaid`` flag / :func:`StateMachine.render_mermaid`) so the README diagram
can be kept in lockstep with the model by a CI test.

Modeling the graph immediately surfaced a real, live bug: ``epic:needs_decomp``
was a **dead-end** — an epic with no phase structure had no autonomous
transition forward, so the whole epic-supervisor sub-graph was unreachable.
That is exactly why epics did not advance autonomously. The canonical graph
here includes the *fix* (the Champion decomposition edge
``epic:needs_decomp → epic:designed``) so it validates clean; the deliberately
broken variants used in the tests are where the validators fire.

Design notes
------------
* **Only existing GitHub labels are used.** The five epic-supervisor states all
  ride the single ``loom:epic`` label and are marked ``derived=True`` — they are
  computed by the daemon-native supervisor rather than distinguished by a
  dedicated label. The label-conflation validator special-cases this: >1 state
  sharing a label is an ERROR *unless* every such state is ``derived`` (then a
  NOTE). See issue about not minting new labels for epic phases.
* **#3707 mutex coverage.** Transitions that call ``gh issue create`` carry
  ``creates_issues=True``; the validator enumerates them as the set the global
  issue-filing mutex (#3707) must serialize (Architect/Hermit/Auditor proposal
  filing + Champion epic decomposition; Curator oversized-issue decomposition is
  serialized under the same invariant even though it does not change label
  state).

Run ``python -m loom_tools.state_machine`` to validate, or
``python -m loom_tools.state_machine --mermaid`` to emit the diagram.
"""

from __future__ import annotations

import argparse
import re
import sys
from collections import defaultdict
from dataclasses import dataclass, field
from enum import Enum
from typing import Iterator, Optional


# ---------------------------------------------------------------------------
# Lanes and roles
# ---------------------------------------------------------------------------


class Lane(str, Enum):
    """The four coordination lanes the graph decomposes into."""

    ISSUE = "issue"
    PR = "pr"
    PROPOSAL = "proposal"
    EPIC = "epic"


class Role(str, Enum):
    """The actor that fires a transition.

    ``daemon_dispatchable`` marks roles the loom-daemon / GitHub Actions cron /
    ``/loom:sweep`` can drive autonomously. ``HUMAN`` is the sole
    non-dispatchable role: an edge it fires is an *autonomy gap* (a point where
    the pipeline stalls waiting on a person), which the validator reports as a
    WARNING.
    """

    HUMAN = "Human"
    CURATOR = "Curator"
    BUILDER = "Builder"
    JUDGE = "Judge"
    DOCTOR = "Doctor"
    CHAMPION = "Champion"
    ARCHITECT = "Architect"
    HERMIT = "Hermit"
    AUDITOR = "Auditor"
    SUPERVISOR = "Supervisor"  # daemon-native epic supervisor

    @property
    def daemon_dispatchable(self) -> bool:
        return self is not Role.HUMAN


# The fork-join barrier state for the epic supervisor loop. Any epic transition
# entering or leaving this state is a phase-boundary edge and MUST declare a
# ``barrier`` (see the barrier-hygiene validator).
PHASE_JOIN_STATE = "epic:phase_join"


# ---------------------------------------------------------------------------
# Model dataclasses
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class State:
    """A logical state in the graph.

    Attributes:
        id: Unique identifier. For label-backed states this equals the label
            (e.g. ``loom:issue``); for lane-boundary states it is a bare word
            (``new``, ``closed``, ``merged``); for derived epic states it is an
            ``epic:*`` pseudo-label.
        lane: Which coordination lane the state belongs to.
        label: The backing GitHub label, or ``None`` for states with no label
            (``new``/``closed``/``merged`` — these represent forge lifecycle,
            not a Loom label).
        entry: True if the machine may start in this state.
        terminal: True if the state has no outgoing transitions (an end state).
        derived: True if the state is *computed* by the daemon rather than
            distinguished by a dedicated label (legitimizes label reuse).
    """

    id: str
    lane: Lane
    label: Optional[str] = None
    entry: bool = False
    terminal: bool = False
    derived: bool = False


@dataclass(frozen=True)
class Transition:
    """A directed, role-fired edge between two states.

    Attributes:
        src: Source state id.
        dst: Destination state id.
        role: The :class:`Role` that fires the edge.
        guard: Human-readable firing condition.
        creates_issues: True if firing this edge runs ``gh issue create`` — the
            set the #3707 global issue-filing mutex must serialize.
        barrier: Non-empty on epic phase-boundary edges, describing the
            fork-join condition; empty otherwise.
    """

    src: str
    dst: str
    role: Role
    guard: str = ""
    creates_issues: bool = False
    barrier: str = ""


@dataclass
class ValidationResult:
    """Outcome of :func:`StateMachine.validate`.

    Unpacks as ``(errors, warnings, notes)`` and exposes :meth:`ok`.
    """

    errors: list[str] = field(default_factory=list)
    warnings: list[str] = field(default_factory=list)
    notes: list[str] = field(default_factory=list)

    def ok(self) -> bool:
        """True when there are no structural ERRORs (warnings/notes are fine)."""
        return not self.errors

    def __iter__(self) -> Iterator[list[str]]:
        # Allow: errors, warnings, notes = machine.validate()
        return iter((self.errors, self.warnings, self.notes))


# ---------------------------------------------------------------------------
# The machine
# ---------------------------------------------------------------------------


@dataclass
class StateMachine:
    """A set of states plus the transitions between them, with a validator."""

    states: list[State]
    transitions: list[Transition]

    # -- lookups -----------------------------------------------------------

    def state_ids(self) -> set[str]:
        return {s.id for s in self.states}

    def by_id(self) -> dict[str, State]:
        return {s.id: s for s in self.states}

    def outgoing(self) -> dict[str, list[Transition]]:
        """Adjacency map over *known* endpoints only (unknown ids are dropped
        here so reachability/dead-end checks operate on a well-formed graph;
        the endpoint validator reports the unknown ids separately)."""
        ids = self.state_ids()
        adj: dict[str, list[Transition]] = {sid: [] for sid in ids}
        for t in self.transitions:
            if t.src in ids and t.dst in ids:
                adj[t.src].append(t)
        return adj

    @staticmethod
    def is_phase_boundary(t: Transition) -> bool:
        """True for epic fork-join edges (any edge touching the join state)."""
        return t.src == PHASE_JOIN_STATE or t.dst == PHASE_JOIN_STATE

    # -- validation --------------------------------------------------------

    def validate(self) -> ValidationResult:
        """Run every structural check and return the collected findings."""
        res = ValidationResult()
        ids = self.state_ids()

        # 1. every transition endpoint is a known state.
        for t in self.transitions:
            if t.src not in ids:
                res.errors.append(
                    f"transition {t.src}->{t.dst}: unknown source state '{t.src}'"
                )
            if t.dst not in ids:
                res.errors.append(
                    f"transition {t.src}->{t.dst}: unknown destination state '{t.dst}'"
                )

        adj = self.outgoing()

        # 2. reachability — every state reachable from an entry state.
        entries = [s.id for s in self.states if s.entry]
        if not entries:
            res.errors.append("no entry state defined (nothing is reachable)")
        reachable: set[str] = set()
        stack = list(entries)
        while stack:
            cur = stack.pop()
            if cur in reachable:
                continue
            reachable.add(cur)
            for t in adj.get(cur, []):
                stack.append(t.dst)
        for s in self.states:
            if s.id not in reachable:
                res.errors.append(
                    f"state '{s.id}' is unreachable from any entry state"
                )

        # 3. dead-end / terminal hygiene.
        for s in self.states:
            n_out = len(adj.get(s.id, []))
            if s.terminal and n_out > 0:
                res.errors.append(
                    f"terminal state '{s.id}' must have 0 outgoing edges, has {n_out}"
                )
            if not s.terminal and n_out == 0:
                res.errors.append(
                    f"non-terminal state '{s.id}' is a dead-end (no outgoing edges)"
                )

        # 4. label conflation — one label backing >1 logical state is an ERROR
        #    unless every such state is derived (then a NOTE).
        label_states: dict[str, list[State]] = defaultdict(list)
        for s in self.states:
            if s.label is not None:
                label_states[s.label].append(s)
        for label, sts in sorted(label_states.items()):
            if len(sts) <= 1:
                continue
            names = ", ".join(s.id for s in sts)
            if all(s.derived for s in sts):
                res.notes.append(
                    f"label '{label}' backs {len(sts)} derived states ({names}); "
                    "legitimate iff the daemon computes the state"
                )
            else:
                non_derived = ", ".join(s.id for s in sts if not s.derived)
                res.errors.append(
                    f"label '{label}' conflates {len(sts)} logical states ({names}); "
                    f"non-derived offenders: {non_derived}"
                )

        # 5. autonomy gaps — edges fired by a non-daemon-dispatchable role.
        for t in self.transitions:
            if not t.role.daemon_dispatchable:
                res.warnings.append(
                    f"autonomy gap: edge {t.src}->{t.dst} is fired by "
                    f"non-daemon-dispatchable role {t.role.value}"
                )

        # 6. barrier hygiene — epic phase-boundary edges must declare a barrier.
        for t in self.transitions:
            if self.is_phase_boundary(t) and not t.barrier:
                res.errors.append(
                    f"epic phase-boundary edge {t.src}->{t.dst} must declare a barrier"
                )

        # 7. #3707 coverage — enumerate the issue-creating edges the global
        #    mutex must serialize.
        creators = [t for t in self.transitions if t.creates_issues]
        if creators:
            listed = "; ".join(
                f"{t.src}->{t.dst} [{t.role.value}]" for t in creators
            )
            res.notes.append(
                f"#3707 issue-filing mutex must serialize {len(creators)} "
                f"creates_issues edge(s): {listed}"
            )

        return res

    # -- rendering ---------------------------------------------------------

    def render_mermaid(self) -> str:
        """Render the graph as a Mermaid ``stateDiagram-v2`` grouped by lane."""
        by_id = self.by_id()

        def alias(state_id: str) -> str:
            return "s_" + re.sub(r"[^0-9a-zA-Z]+", "_", state_id).strip("_")

        def display(s: State) -> str:
            if s.label is not None and not s.derived:
                return s.label
            return s.id

        lane_titles = {
            Lane.ISSUE: "Issue lane",
            Lane.PR: "PR lane",
            Lane.PROPOSAL: "Proposal lane",
            Lane.EPIC: "Epic supervisor lane (derived — loom:epic)",
        }

        lines: list[str] = ["stateDiagram-v2"]

        # Composite state per lane, states declared inside.
        for lane in Lane:
            members = [s for s in self.states if s.lane is lane]
            if not members:
                continue
            lines.append(f'    state "{lane_titles[lane]}" as lane_{lane.value} {{')
            for s in members:
                lines.append(f'        {alias(s.id)} : {display(s)}')
            lines.append("    }")

        # Entry / terminal pseudo-state edges.
        for s in self.states:
            if s.entry:
                lines.append(f"    [*] --> {alias(s.id)}")

        # Transitions (declared after the composites so cross-lane edges work).
        for t in self.transitions:
            parts = [t.role.value]
            if t.barrier:
                parts.append(f"barrier: {t.barrier}")
            if t.creates_issues:
                parts.append("creates issues")
            label = ": " + " · ".join(parts) if parts else ""
            lines.append(f"    {alias(t.src)} --> {alias(t.dst)} {label}".rstrip())

        for s in self.states:
            if s.terminal:
                lines.append(f"    {alias(s.id)} --> [*]")

        return "\n".join(lines)


# ---------------------------------------------------------------------------
# The canonical Loom graph
# ---------------------------------------------------------------------------


def canonical() -> StateMachine:
    """Build a fresh instance of the canonical (documented, correct) graph.

    A fresh instance every call so callers/tests can mutate copies to
    construct deliberately-broken variants.
    """
    states = [
        # --- issue lane ---------------------------------------------------
        State("new", Lane.ISSUE, label=None, entry=True),
        State("loom:triage", Lane.ISSUE, label="loom:triage"),
        State("loom:curating", Lane.ISSUE, label="loom:curating"),
        State("loom:curated", Lane.ISSUE, label="loom:curated"),
        State("loom:issue", Lane.ISSUE, label="loom:issue"),
        State("loom:building", Lane.ISSUE, label="loom:building"),
        State("closed", Lane.ISSUE, label=None, terminal=True),
        # --- PR lane ------------------------------------------------------
        State("loom:review-requested", Lane.PR, label="loom:review-requested"),
        State("loom:changes-requested", Lane.PR, label="loom:changes-requested"),
        State("loom:pr", Lane.PR, label="loom:pr"),
        State("merged", Lane.PR, label=None, terminal=True),
        # --- proposal lane ------------------------------------------------
        State("loom:architect", Lane.PROPOSAL, label="loom:architect"),
        State("loom:hermit", Lane.PROPOSAL, label="loom:hermit"),
        State("loom:auditor", Lane.PROPOSAL, label="loom:auditor"),
        # --- epic supervisor lane (all derived, all backed by loom:epic) --
        State("epic:needs_decomp", Lane.EPIC, label="loom:epic", derived=True),
        State("epic:designed", Lane.EPIC, label="loom:epic", derived=True),
        State("epic:active", Lane.EPIC, label="loom:epic", derived=True),
        State("epic:phase_join", Lane.EPIC, label="loom:epic", derived=True),
        State("epic:done", Lane.EPIC, label="loom:epic", derived=True, terminal=True),
    ]

    transitions = [
        # --- issue lane ---------------------------------------------------
        Transition("new", "loom:triage", Role.HUMAN, "issue filed for triage"),
        Transition("loom:triage", "loom:curating", Role.CURATOR, "Curator claims"),
        Transition("loom:curating", "loom:curated", Role.CURATOR, "enrichment complete"),
        Transition("loom:curated", "loom:issue", Role.HUMAN, "human approval to build"),
        Transition("loom:issue", "loom:building", Role.BUILDER, "Builder claims"),
        Transition("loom:building", "loom:review-requested", Role.BUILDER, "Builder opens PR"),
        Transition("loom:building", "closed", Role.CHAMPION, "linked PR merged (Closes #N)"),
        # --- PR lane ------------------------------------------------------
        Transition("loom:review-requested", "loom:pr", Role.JUDGE, "Judge approves"),
        Transition("loom:review-requested", "loom:changes-requested", Role.JUDGE, "Judge requests changes"),
        Transition("loom:changes-requested", "loom:review-requested", Role.DOCTOR, "Doctor addresses feedback"),
        Transition("loom:pr", "merged", Role.CHAMPION, "auto-merge criteria met"),
        # --- proposal lane (creation edges are #3707-serialized) ----------
        Transition("new", "loom:architect", Role.ARCHITECT, "Architect files proposal", creates_issues=True),
        Transition("new", "loom:hermit", Role.HERMIT, "Hermit files simplification proposal", creates_issues=True),
        Transition("new", "loom:auditor", Role.AUDITOR, "Auditor files runtime bug", creates_issues=True),
        Transition("loom:architect", "loom:issue", Role.CHAMPION, "Champion approves proposal"),
        Transition("loom:architect", "closed", Role.CHAMPION, "Champion rejects proposal"),
        Transition("loom:hermit", "loom:issue", Role.CHAMPION, "Champion approves proposal"),
        Transition("loom:hermit", "closed", Role.CHAMPION, "Champion rejects proposal"),
        Transition("loom:auditor", "loom:issue", Role.CHAMPION, "Champion approves proposal"),
        Transition("loom:auditor", "closed", Role.CHAMPION, "Champion rejects proposal"),
        # --- epic supervisor lane -----------------------------------------
        Transition("new", "epic:needs_decomp", Role.ARCHITECT, "epic proposal filed (loom:epic)"),
        # THE FIX: without this decomposition edge, epic:needs_decomp is a
        # dead-end and the whole supervisor sub-graph is unreachable.
        Transition(
            "epic:needs_decomp", "epic:designed", Role.CHAMPION,
            "Champion decomposes epic into phase issues", creates_issues=True,
        ),
        Transition("epic:designed", "epic:active", Role.CHAMPION, "first phase dispatched"),
        Transition(
            "epic:active", "epic:phase_join", Role.SUPERVISOR,
            "all in-flight phase PRs merged", barrier="fork-join: current phase complete",
        ),
        Transition(
            "epic:phase_join", "epic:active", Role.SUPERVISOR,
            "phases remain", barrier="advance: dispatch next phase",
        ),
        Transition(
            "epic:phase_join", "epic:done", Role.SUPERVISOR,
            "no phases remain", barrier="join: all phases complete",
        ),
    ]

    return StateMachine(states=states, transitions=transitions)


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def _format_report(res: ValidationResult) -> str:
    lines: list[str] = []
    lines.append(f"errors:   {len(res.errors)}")
    for e in res.errors:
        lines.append(f"  ERROR   {e}")
    lines.append(f"warnings: {len(res.warnings)}")
    for w in res.warnings:
        lines.append(f"  WARN    {w}")
    lines.append(f"notes:    {len(res.notes)}")
    for n in res.notes:
        lines.append(f"  NOTE    {n}")
    lines.append("")
    lines.append("OK" if res.ok() else "FAILED (structural errors present)")
    return "\n".join(lines)


def main(argv: Optional[list[str]] = None) -> int:
    parser = argparse.ArgumentParser(
        prog="loom-state-machine",
        description="Executable spec of the Loom label state machine.",
    )
    parser.add_argument(
        "--mermaid",
        action="store_true",
        help="emit the graph as a Mermaid stateDiagram-v2 grouped by lane",
    )
    args = parser.parse_args(argv)

    machine = canonical()

    if args.mermaid:
        print(machine.render_mermaid())
        return 0

    res = machine.validate()
    print(_format_report(res))
    return 0 if res.ok() else 1


if __name__ == "__main__":
    sys.exit(main())
