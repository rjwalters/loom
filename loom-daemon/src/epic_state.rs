//! Derived-state computation for `loom:epic` issues (read-only).
//!
//! The daemon-native epic supervisor (epic #3842) drives a `loom:epic` issue
//! through a fork-join lifecycle. Rather than mint new GitHub labels for each
//! phase of that lifecycle, every epic-supervisor state rides the single
//! `loom:epic` label and is **derived** — computed on demand from data already
//! visible on the forge:
//!
//! 1. the number of `### Phase` sections in the epic body, and
//! 2. the open/closed status of the epic's `loom:epic-phase` children.
//!
//! This module implements Phase 1 of #3842: the pure, **read-only**
//! classification. It performs zero forge mutation — [`derive_epic_state`] is a
//! total function over already-fetched data, so any forge I/O (fetching the
//! body, listing children) is a separate, read-only concern for later phases.
//!
//! # Conformance
//!
//! The authoritative model is the executable state machine in
//! `loom-tools/src/loom_tools/state_machine.py` (issue #3841, merged in #3844).
//! Its epic-supervisor lane defines exactly five derived states, all backed by
//! `loom:epic`:
//!
//! ```text
//! epic:needs_decomp → epic:designed → epic:active ⇄ epic:phase_join → epic:done
//! ```
//!
//! [`EpicState`] mirrors those five states 1:1 (see [`EpicState::as_state_id`]),
//! and the classification conditions below correspond to the model's transition
//! guards:
//!
//! | [`EpicState`] | Condition | Model transition it enables |
//! |---|---|---|
//! | [`NeedsDecomp`] | body has &lt;2 `### Phase` sections | `needs_decomp → designed` (Champion decomposes) |
//! | [`Designed`] | ≥2 `### Phase` sections, no `epic-phase` children yet | `designed → active` (first phase dispatched) |
//! | [`Active`] | a current-phase child is open | `active → phase_join` (current phase completes) |
//! | [`PhaseJoin`] | current phase's children all closed, more phases remain | `phase_join → active` (advance: dispatch next phase) |
//! | [`Done`] | all phases' children closed, no phases remain | `phase_join → done` (join: all phases complete) |
//!
//! [`NeedsDecomp`]: EpicState::NeedsDecomp
//! [`Designed`]: EpicState::Designed
//! [`Active`]: EpicState::Active
//! [`PhaseJoin`]: EpicState::PhaseJoin
//! [`Done`]: EpicState::Done

/// The minimum number of `### Phase` sections an epic body must contain before
/// it is considered decomposed. Below this the epic is [`EpicState::NeedsDecomp`].
///
/// Mirrors the epic #3842 rule "body has &lt;2 `### Phase`".
pub const MIN_PHASE_SECTIONS: usize = 2;

/// A derived state of an open `loom:epic` issue.
///
/// All five variants ride the single `loom:epic` GitHub label — they are
/// *computed*, never stored as distinct labels. This mirrors the `derived=True`
/// epic-supervisor lane in the Python state-machine model (#3841).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EpicState {
    /// Body has fewer than [`MIN_PHASE_SECTIONS`] `### Phase` sections; the epic
    /// has no phase structure yet. Enables the Architect/Champion decomposition
    /// edge (`epic:needs_decomp → epic:designed`).
    NeedsDecomp,
    /// Body is decomposed (≥[`MIN_PHASE_SECTIONS`] `### Phase` sections) but no
    /// `loom:epic-phase` children have been materialized yet. Enables
    /// `epic:designed → epic:active` (first phase dispatched by Champion).
    Designed,
    /// At least one child of the current phase is still open — the phase is in
    /// flight. The supervisor builds children (`epic:active`).
    Active,
    /// Every child of the current phase is closed, but more phases remain in the
    /// body. The fork-join barrier is satisfied for this phase; the next phase
    /// must be materialized (`epic:phase_join`, barrier-gated).
    PhaseJoin,
    /// Every phase's children are closed and no phases remain — the epic is
    /// complete. Terminal state (`epic:done`).
    Done,
}

impl EpicState {
    /// The canonical state id from the Python model
    /// (`loom-tools/src/loom_tools/state_machine.py`), e.g. `"epic:needs_decomp"`.
    ///
    /// Provided for conformance cross-checks and observability so the Rust
    /// supervisor can name states identically to the authoritative graph.
    #[must_use]
    pub fn as_state_id(self) -> &'static str {
        match self {
            EpicState::NeedsDecomp => "epic:needs_decomp",
            EpicState::Designed => "epic:designed",
            EpicState::Active => "epic:active",
            EpicState::PhaseJoin => "epic:phase_join",
            EpicState::Done => "epic:done",
        }
    }

    /// True for the terminal state ([`EpicState::Done`]), matching the
    /// `terminal=True` flag on `epic:done` in the model.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, EpicState::Done)
    }
}

impl std::fmt::Display for EpicState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_state_id())
    }
}

/// A directed edge in the epic-supervisor transition table.
///
/// This is the **Rust side** of the epic sub-graph — the transitions among the
/// five derived [`EpicState`]s that the supervisor loop drives. It exists as an
/// explicit, inspectable artifact (rather than only implicitly inside
/// `plan_epic_transition`) so it can be documented and, crucially,
/// **conformance-checked** against the authoritative Python model in
/// `loom-tools/src/loom_tools/state_machine.py` (epic #3842 Phase 4, #3873).
///
/// Only intra-lane edges are modelled — both [`src`](Self::src) and
/// [`dst`](Self::dst) are `epic:*` derived state ids. The lane-*entry* edge
/// (`new → epic:needs_decomp`, the Architect filing a `loom:epic` proposal) is
/// deliberately excluded: it is not a supervisor transition, and the supervisor
/// begins its lifecycle at [`EpicState::NeedsDecomp`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpicEdge {
    /// Source state id (an `epic:*` derived state).
    pub src: &'static str,
    /// Destination state id (an `epic:*` derived state).
    pub dst: &'static str,
    /// The role that fires the edge, matching the Python model's `Role` value
    /// (e.g. `"Champion"`, `"Supervisor"`).
    pub role: &'static str,
    /// The fork-join barrier description on phase-boundary edges (any edge
    /// touching `epic:phase_join`), or `""` for non-boundary edges. Matches the
    /// Python model's `Transition.barrier` string exactly.
    pub barrier: &'static str,
    /// Whether firing this edge runs `gh issue create` — the set the #3707
    /// issue-creation mutex serializes. Matches the Python `creates_issues` flag.
    pub creates_issues: bool,
}

/// The epic-supervisor transition table: the five edges among the five derived
/// [`EpicState`]s.
///
/// Faithful to the epic sub-graph of the Python state-machine model (#3841) —
/// same edges, roles, barriers, and `creates_issues` flags. The conformance
/// test (`loom-daemon/tests/epic_conformance.rs`) derives its expectation by
/// parsing the Python model and asserts this table matches it, so drift between
/// the two representations is caught mechanically rather than by hand.
///
/// The edges (matching `plan_epic_transition` + `barrier_admits`):
///
/// ```text
/// epic:needs_decomp → epic:designed    (Champion, creates_issues)   [decompose]
/// epic:designed     → epic:active      (Champion)                   [expand]
/// epic:active       → epic:phase_join  (Supervisor, barrier)        [fork-join]
/// epic:phase_join   → epic:active      (Supervisor, barrier)        [join/advance]
/// epic:phase_join   → epic:done        (Supervisor, barrier)        [close]
/// ```
#[must_use]
pub fn epic_transition_table() -> [EpicEdge; 5] {
    [
        EpicEdge {
            src: "epic:needs_decomp",
            dst: "epic:designed",
            role: "Champion",
            barrier: "",
            creates_issues: true,
        },
        EpicEdge {
            src: "epic:designed",
            dst: "epic:active",
            role: "Champion",
            barrier: "",
            creates_issues: false,
        },
        EpicEdge {
            src: "epic:active",
            dst: "epic:phase_join",
            role: "Supervisor",
            barrier: "fork-join: current phase complete",
            creates_issues: false,
        },
        EpicEdge {
            src: "epic:phase_join",
            dst: "epic:active",
            role: "Supervisor",
            barrier: "advance: dispatch next phase",
            creates_issues: false,
        },
        EpicEdge {
            src: "epic:phase_join",
            dst: "epic:done",
            role: "Supervisor",
            barrier: "join: all phases complete",
            creates_issues: false,
        },
    ]
}

/// All five derived epic state ids, in lifecycle order. Mirrors the `EpicState`
/// variants and the Python model's `lane == epic` states.
#[must_use]
pub fn epic_state_ids() -> [&'static str; 5] {
    [
        EpicState::NeedsDecomp.as_state_id(),
        EpicState::Designed.as_state_id(),
        EpicState::Active.as_state_id(),
        EpicState::PhaseJoin.as_state_id(),
        EpicState::Done.as_state_id(),
    ]
}

/// A materialized `loom:epic-phase` child of an epic, reduced to the two facts
/// the derived-state computation needs: which phase it belongs to, and whether
/// it is still open.
///
/// Extracting these two facts from a forge issue (title parsing, open/closed
/// status) is a separate, read-only concern — this struct is the pure input to
/// [`derive_epic_state`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhaseChild {
    /// 1-based phase number this child belongs to (e.g. `1` for "Phase 1").
    pub phase: u32,
    /// True if the child issue is still open.
    pub open: bool,
}

impl PhaseChild {
    /// Convenience constructor.
    #[must_use]
    pub fn new(phase: u32, open: bool) -> Self {
        Self { phase, open }
    }
}

/// Count the `### Phase` sections in an epic body.
///
/// Faithful to the epic #3842 definition (`grep -c "### Phase"`): counts the
/// number of lines containing the `### Phase` substring. Using line-containment
/// rather than a strict heading anchor matches the authoritative `grep`
/// semantics quoted in the epic.
#[must_use]
pub fn count_phase_sections(body: &str) -> usize {
    body.lines()
        .filter(|line| line.contains("### Phase"))
        .count()
}

/// Classify an open `loom:epic` issue into its [`EpicState`].
///
/// Pure and read-only: a total function of the `### Phase` count and the
/// per-child phase/open status. Performs no forge I/O and no mutation.
///
/// `phase_count` is typically [`count_phase_sections`] applied to the epic body;
/// `children` are the epic's `loom:epic-phase` children (see [`PhaseChild`]).
///
/// Classification (see the module-level conformance table):
///
/// 1. `phase_count < `[`MIN_PHASE_SECTIONS`] ⇒ [`EpicState::NeedsDecomp`].
/// 2. no children ⇒ [`EpicState::Designed`].
/// 3. any child of the current (highest-numbered materialized) phase open ⇒
///    [`EpicState::Active`].
/// 4. current phase fully closed, more phases remain ⇒ [`EpicState::PhaseJoin`].
/// 5. current phase fully closed, no phases remain ⇒ [`EpicState::Done`].
#[must_use]
pub fn derive_epic_state(phase_count: usize, children: &[PhaseChild]) -> EpicState {
    // 1. needs_decomp is purely body-driven and takes precedence: an epic with
    //    no phase structure has nothing to build regardless of stray children.
    if phase_count < MIN_PHASE_SECTIONS {
        return EpicState::NeedsDecomp;
    }

    // 2. Decomposed but nothing dispatched yet.
    if children.is_empty() {
        return EpicState::Designed;
    }

    // The "current phase" is the highest phase number that has materialized
    // children — the supervisor expands phases in order, so the newest wave of
    // children marks the phase in flight.
    let current_phase = children.iter().map(|c| c.phase).max().unwrap_or(0);

    // 3. Any current-phase child still open ⇒ the phase is in flight.
    let current_phase_open = children.iter().any(|c| c.phase == current_phase && c.open);
    if current_phase_open {
        return EpicState::Active;
    }

    // Current phase is fully closed. Whether we join-and-advance or finish
    // depends on whether the body declares phases beyond the current one.
    let more_phases_remain = (current_phase as usize) < phase_count;
    if more_phases_remain {
        // 4. Fork-join barrier satisfied for this phase; next phase pending.
        EpicState::PhaseJoin
    } else {
        // 5. All phases materialized and closed.
        EpicState::Done
    }
}

/// Convenience wrapper: derive the state directly from the raw epic body and
/// children, counting `### Phase` sections internally via [`count_phase_sections`].
#[must_use]
pub fn derive_epic_state_from_body(body: &str, children: &[PhaseChild]) -> EpicState {
    derive_epic_state(count_phase_sections(body), children)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // ===== count_phase_sections =====

    #[test]
    fn test_count_phase_sections_none() {
        let body = "# Epic\n\nSome prose with no phase headings.\n";
        assert_eq!(count_phase_sections(body), 0);
    }

    #[test]
    fn test_count_phase_sections_multiple() {
        let body = "\
# Epic: something

### Phase 1: Foo
details

### Phase 2: Bar
details

### Phase 3: Baz
details
";
        assert_eq!(count_phase_sections(body), 3);
    }

    #[test]
    fn test_count_phase_sections_ignores_non_phase_headings() {
        let body = "### Overview\n### Goal\n### Phase 1: Only this one\n";
        assert_eq!(count_phase_sections(body), 1);
    }

    // ===== state: needs_decomp =====

    #[test]
    fn test_needs_decomp_empty_body() {
        assert_eq!(derive_epic_state(0, &[]), EpicState::NeedsDecomp);
    }

    #[test]
    fn test_needs_decomp_single_phase() {
        // One phase section is below the MIN_PHASE_SECTIONS threshold.
        assert_eq!(derive_epic_state(1, &[]), EpicState::NeedsDecomp);
    }

    #[test]
    fn test_needs_decomp_precedence_over_stray_children() {
        // Body drives needs_decomp; stray children do not promote it.
        let children = [PhaseChild::new(1, true)];
        assert_eq!(derive_epic_state(1, &children), EpicState::NeedsDecomp);
    }

    #[test]
    fn test_needs_decomp_from_body() {
        let body = "# Epic\n\nNo phases yet.\n";
        assert_eq!(derive_epic_state_from_body(body, &[]), EpicState::NeedsDecomp);
    }

    // ===== state: designed =====

    #[test]
    fn test_designed_two_phases_no_children() {
        assert_eq!(derive_epic_state(2, &[]), EpicState::Designed);
    }

    #[test]
    fn test_designed_from_body() {
        let body = "\
### Phase 1: A
### Phase 2: B
### Phase 3: C
";
        assert_eq!(derive_epic_state_from_body(body, &[]), EpicState::Designed);
    }

    // ===== state: active =====

    #[test]
    fn test_active_open_child_in_current_phase() {
        let children = [PhaseChild::new(1, true), PhaseChild::new(1, false)];
        assert_eq!(derive_epic_state(3, &children), EpicState::Active);
    }

    #[test]
    fn test_active_later_phase_open() {
        // Phase 1 closed, phase 2 (current) has an open child ⇒ active.
        let children = [
            PhaseChild::new(1, false),
            PhaseChild::new(2, false),
            PhaseChild::new(2, true),
        ];
        assert_eq!(derive_epic_state(3, &children), EpicState::Active);
    }

    #[test]
    fn test_active_single_phase_epic_in_flight() {
        // Body declares exactly 2 phases; only phase 1 materialized and open.
        let children = [PhaseChild::new(1, true)];
        assert_eq!(derive_epic_state(2, &children), EpicState::Active);
    }

    // ===== state: phase_join =====

    #[test]
    fn test_phase_join_current_phase_closed_more_remain() {
        // Phase 1 children all closed, body has 3 phases ⇒ join before phase 2.
        let children = [PhaseChild::new(1, false), PhaseChild::new(1, false)];
        assert_eq!(derive_epic_state(3, &children), EpicState::PhaseJoin);
    }

    #[test]
    fn test_phase_join_at_intermediate_phase() {
        // Phases 1 and 2 closed, phase 2 is current, body has 4 phases ⇒ join.
        let children = [
            PhaseChild::new(1, false),
            PhaseChild::new(2, false),
            PhaseChild::new(2, false),
        ];
        assert_eq!(derive_epic_state(4, &children), EpicState::PhaseJoin);
    }

    // ===== state: done =====

    #[test]
    fn test_done_all_phases_closed() {
        // Body has 2 phases; both phases materialized and all children closed.
        let children = [PhaseChild::new(1, false), PhaseChild::new(2, false)];
        assert_eq!(derive_epic_state(2, &children), EpicState::Done);
    }

    #[test]
    fn test_done_current_phase_equals_phase_count() {
        // Highest materialized phase (3) equals phase_count (3), all closed.
        let children = [
            PhaseChild::new(1, false),
            PhaseChild::new(2, false),
            PhaseChild::new(3, false),
        ];
        let state = derive_epic_state(3, &children);
        assert_eq!(state, EpicState::Done);
        assert!(state.is_terminal());
    }

    // ===== boundary: phase_join vs done hinges on more_phases_remain =====

    #[test]
    fn test_join_vs_done_boundary() {
        // Same closed current-phase children; only phase_count differs.
        let children = [PhaseChild::new(2, false)];
        assert_eq!(derive_epic_state(3, &children), EpicState::PhaseJoin);
        assert_eq!(derive_epic_state(2, &children), EpicState::Done);
    }

    // ===== conformance: state ids match the Python model =====

    #[test]
    fn test_state_ids_match_python_model() {
        assert_eq!(EpicState::NeedsDecomp.as_state_id(), "epic:needs_decomp");
        assert_eq!(EpicState::Designed.as_state_id(), "epic:designed");
        assert_eq!(EpicState::Active.as_state_id(), "epic:active");
        assert_eq!(EpicState::PhaseJoin.as_state_id(), "epic:phase_join");
        assert_eq!(EpicState::Done.as_state_id(), "epic:done");
    }

    // ===== epic transition table (conformance artifact, #3873) =====

    #[test]
    fn test_epic_state_ids_matches_variants() {
        assert_eq!(
            epic_state_ids(),
            [
                "epic:needs_decomp",
                "epic:designed",
                "epic:active",
                "epic:phase_join",
                "epic:done",
            ]
        );
    }

    #[test]
    fn test_epic_transition_table_shape() {
        let table = epic_transition_table();
        assert_eq!(table.len(), 5);

        // Every edge's endpoints are known derived epic states.
        let ids: std::collections::HashSet<&str> = epic_state_ids().into_iter().collect();
        for e in &table {
            assert!(ids.contains(e.src), "unknown src {}", e.src);
            assert!(ids.contains(e.dst), "unknown dst {}", e.dst);
        }

        // Exactly one issue-creating edge (needs_decomp → designed).
        let creators: Vec<_> = table.iter().filter(|e| e.creates_issues).collect();
        assert_eq!(creators.len(), 1);
        assert_eq!(creators[0].src, "epic:needs_decomp");
        assert_eq!(creators[0].dst, "epic:designed");

        // Exactly three phase-boundary (barrier) edges — every edge touching
        // epic:phase_join declares a non-empty barrier.
        let boundary: Vec<_> = table
            .iter()
            .filter(|e| e.src == "epic:phase_join" || e.dst == "epic:phase_join")
            .collect();
        assert_eq!(boundary.len(), 3);
        for e in boundary {
            assert!(!e.barrier.is_empty(), "boundary edge {}->{} needs a barrier", e.src, e.dst);
        }

        // Non-boundary edges carry no barrier.
        for e in &table {
            let touches_join = e.src == "epic:phase_join" || e.dst == "epic:phase_join";
            if !touches_join {
                assert!(
                    e.barrier.is_empty(),
                    "non-boundary edge {}->{} must have no barrier",
                    e.src,
                    e.dst
                );
            }
        }
    }

    #[test]
    fn test_only_done_is_terminal() {
        assert!(EpicState::Done.is_terminal());
        for s in [
            EpicState::NeedsDecomp,
            EpicState::Designed,
            EpicState::Active,
            EpicState::PhaseJoin,
        ] {
            assert!(!s.is_terminal(), "{s} should not be terminal");
        }
    }

    // ===== end-to-end over a realistic epic body (mirrors epic #3842) =====

    #[test]
    fn test_realistic_epic_body_lifecycle() {
        let body = "\
# Epic: Daemon-native epic supervisor

Some framing prose.

### Phase 1: Derived-state computation (read-only)
details

### Phase 2: Synchronization primitives
details

### Phase 3: Supervisor loop
details

### Phase 4: Conformance + observability
details
";
        assert_eq!(count_phase_sections(body), 4);

        // Designed: decomposed, nothing dispatched.
        assert_eq!(derive_epic_state_from_body(body, &[]), EpicState::Designed);

        // Active: phase 1 in flight.
        let active = [PhaseChild::new(1, true)];
        assert_eq!(derive_epic_state_from_body(body, &active), EpicState::Active);

        // PhaseJoin: phase 1 done, three phases remain.
        let joined = [PhaseChild::new(1, false)];
        assert_eq!(derive_epic_state_from_body(body, &joined), EpicState::PhaseJoin);

        // Done: all four phases materialized and closed.
        let finished = [
            PhaseChild::new(1, false),
            PhaseChild::new(2, false),
            PhaseChild::new(3, false),
            PhaseChild::new(4, false),
        ];
        assert_eq!(derive_epic_state_from_body(body, &finished), EpicState::Done);
    }
}
