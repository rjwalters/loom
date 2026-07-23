//! Epic supervisor loop — dispatch scheduling for `loom:epic` issues
//! (Phase 3 of epic #3842).
//!
//! The daemon-native epic supervisor drives every open `loom:epic` issue
//! through its fork-join lifecycle autonomously. This module composes the two
//! primitives that already shipped:
//!
//! - **Phase 1** ([`crate::epic_state`]) — the read-only [`EpicState`]
//!   classification ([`derive_epic_state`]).
//! - **Phase 2** ([`crate::issue_creation_mutex`] + [`crate::phase_join`]) —
//!   the global #3707 issue-creation mutex and the [`epic_join_ready`] /
//!   [`barrier_admits`] phase-join barrier gate.
//!
//! Per tick, the supervisor iterates open `loom:epic` issues, calls
//! [`derive_epic_state`] on each, and fires the **one** enabled transition by
//! dispatching the appropriate existing role. The daemon owns **scheduling +
//! synchronization only**; roles own every content mutation.
//!
//! # Derived-state → transition → dispatch
//!
//! | [`EpicState`] | [`EpicTransition`] | Dispatch shape |
//! |---|---|---|
//! | [`NeedsDecomp`] | [`DesignInPlace`] | **Architect-on-epic** — enrich the epic body in place with `### Phase` structure, **no PR** |
//! | [`Designed`] | [`ExpandFirstPhase`] | **Champion** expand phase-1 children (under the #3707 mutex) |
//! | [`PhaseJoin`] | [`AdvancePhase`] | **Champion** expand phase N+1 children (under the #3707 mutex, barrier-gated by [`epic_join_ready`]) |
//! | [`Active`] | [`BuildChildren`] | `dispatch_sweep` per open `loom:issue` child |
//! | [`Done`] | [`CloseEpic`] | **Champion** close-epic |
//!
//! [`NeedsDecomp`]: EpicState::NeedsDecomp
//! [`Designed`]: EpicState::Designed
//! [`Active`]: EpicState::Active
//! [`PhaseJoin`]: EpicState::PhaseJoin
//! [`Done`]: EpicState::Done
//! [`DesignInPlace`]: EpicTransition::DesignInPlace
//! [`ExpandFirstPhase`]: EpicTransition::ExpandFirstPhase
//! [`AdvancePhase`]: EpicTransition::AdvancePhase
//! [`BuildChildren`]: EpicTransition::BuildChildren
//! [`CloseEpic`]: EpicTransition::CloseEpic
//! [`derive_epic_state`]: crate::epic_state::derive_epic_state
//! [`epic_join_ready`]: crate::phase_join::epic_join_ready
//! [`barrier_admits`]: crate::phase_join::barrier_admits
//!
//! # Idempotency & recovery
//!
//! Two layers keep re-ticking safe (the acceptance-critical "a tick on an
//! unchanged epic is a no-op; a crash mid-transition recovers without duplicate
//! dispatch or duplicate issue creation"):
//!
//! 1. **Monotone derived state** — the *forge* is the source of truth. Once a
//!    role's content mutation lands (Architect adds `### Phase` sections;
//!    Champion mints phase children; a child closes), [`derive_epic_state`]
//!    classifies the epic into a *different* state, so the completed transition
//!    stops being enabled and never re-fires. A daemon restart loses all
//!    in-memory bookkeeping but re-derives the same forge truth, so recovery is
//!    automatic. Duplicate issue creation across a crash is additionally
//!    prevented by the #3707 mutex serializing the whole burst.
//! 2. **In-flight ledger** — within a running daemon a role dispatch is not
//!    instantaneous: the Architect/Champion process takes time to land its
//!    mutation, during which the derived state is unchanged and the same
//!    transition would be re-selected every tick. The [`EpicSupervisor`]
//!    records the transition it dispatched per epic and treats a re-selection
//!    of the *same* transition as a no-op until the state advances (the ledger
//!    entry is cleared) or a TTL elapses (so a crashed role dispatch cannot
//!    wedge an epic forever). Sweeps dispatched for `active` children are
//!    deduplicated per child issue number.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::epic_state::{count_phase_sections, derive_epic_state, EpicState, PhaseChild};
use crate::issue_creation_mutex::{IssueCreationMutex, CHAMPION_EPIC_DECOMP};
use crate::phase_join::{barrier_admits, PhaseBoundary};

// ============================================================================
// Constants
// ============================================================================

/// Default time-to-live for an in-flight singleton transition before the
/// supervisor is willing to re-dispatch it. A role dispatch that lands its
/// mutation advances the derived state (clearing the ledger) well within this
/// window; the TTL only ever fires when a dispatched role *crashed* without
/// landing its mutation, so re-dispatch is the correct recovery.
pub const DEFAULT_INFLIGHT_TTL_SECS: u64 = 900;

/// Environment variable overriding [`DEFAULT_INFLIGHT_TTL_SECS`]. Follows the
/// `LOOM_*` convention used elsewhere in the daemon.
pub const INFLIGHT_TTL_ENV: &str = "LOOM_EPIC_INFLIGHT_TTL_SECS";

/// Environment variable enabling the epic supervisor loop (Phase 4, #3872).
///
/// The supervisor is **opt-in** — unset or a false-y value keeps it OFF —
/// because the loop autonomously dispatches roles that spawn Architect /
/// Champion processes and create GitHub issues. Set to `1` / `true` / `yes` /
/// `on` to enable.
pub const SUPERVISOR_ENABLE_ENV: &str = "LOOM_EPIC_SUPERVISOR";

/// Environment variable overriding the supervisor tick interval (seconds).
pub const SUPERVISOR_INTERVAL_ENV: &str = "LOOM_EPIC_SUPERVISOR_INTERVAL_SECS";

/// Default supervisor tick interval. Epics advance on the order of minutes
/// (each transition spawns a role process), so a 5-minute cadence is ample and
/// keeps forge query volume low.
pub const DEFAULT_SUPERVISOR_INTERVAL_SECS: u64 = 300;

// ============================================================================
// Fetched epic facts
// ============================================================================

/// The facts about one open `loom:epic` issue the supervisor needs to schedule
/// its next transition, already fetched from the forge.
///
/// Keeping this a plain data struct (no forge I/O) makes the scheduling logic —
/// [`plan_epic_transition`] and [`EpicSupervisor::tick`] — a pure function of
/// already-fetched data, exactly mirroring the Phase 1 / Phase 2 design. An
/// [`EpicSource`] is responsible for materializing these from the forge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpicSnapshot {
    /// The epic issue number.
    pub number: u32,
    /// The epic body (scanned for `### Phase` sections by [`count_phase_sections`]).
    pub body: String,
    /// The epic's `loom:epic-phase` children, reduced to phase + open status —
    /// the Phase 1 input to [`derive_epic_state`].
    pub phase_children: Vec<PhaseChild>,
    /// The issue numbers of the epic's currently-open, build-ready
    /// `loom:issue` children (the wave the `active` transition sweeps).
    pub open_issue_children: Vec<u32>,
}

impl EpicSnapshot {
    /// Convenience constructor.
    #[must_use]
    pub fn new(
        number: u32,
        body: impl Into<String>,
        phase_children: Vec<PhaseChild>,
        open_issue_children: Vec<u32>,
    ) -> Self {
        Self {
            number,
            body: body.into(),
            phase_children,
            open_issue_children,
        }
    }

    /// The Phase 1 derived state of this epic.
    #[must_use]
    pub fn state(&self) -> EpicState {
        derive_epic_state(count_phase_sections(&self.body), &self.phase_children)
    }
}

// ============================================================================
// Transitions
// ============================================================================

/// Which phase a Champion expand burst materializes. Distinguishes the two
/// issue-creating expand dispatches so the concrete dispatcher can craft the
/// right prompt, while both share the #3707 mutex and the same
/// [`CHAMPION_EPIC_DECOMP`] serialization identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExpandKind {
    /// `designed` → materialize the **first** phase's children.
    FirstPhase,
    /// `phase_join` → materialize the **next** (N+1) phase's children.
    NextPhase,
}

/// The single transition the supervisor fires for an epic on a given tick.
///
/// Exactly one is enabled per derived state (see the module table); when none
/// is (an `active` epic with no build-ready children, or a barrier that is not
/// yet satisfied) the plan is [`EpicTransition::Noop`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EpicTransition {
    /// `needs_decomp`: dispatch the Architect-on-epic **enrich-body** shape —
    /// edit the epic body in place to add `### Phase` structure, producing no
    /// PR. This is the new dispatch shape distinct from a build→PR dispatch.
    DesignInPlace,
    /// `designed`: dispatch Champion to expand the first phase's children,
    /// serialized under the #3707 issue-creation mutex.
    ExpandFirstPhase,
    /// `phase_join`: dispatch Champion to expand phase N+1's children,
    /// serialized under the #3707 mutex **and** gated by the phase-join
    /// barrier ([`epic_join_ready`](crate::phase_join::epic_join_ready)).
    AdvancePhase,
    /// `active`: dispatch one `/loom:sweep` per open `loom:issue` child. The
    /// payload is the child issue numbers to build.
    BuildChildren(Vec<u32>),
    /// `done`: dispatch Champion to close the epic.
    CloseEpic,
    /// No transition is enabled this tick.
    Noop,
}

/// A stable, payload-free identity for an [`EpicTransition`]. Used by the
/// in-flight ledger to recognise when a re-planned tick selected the *same*
/// transition (a no-op) versus the epic advancing to a new one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransitionKind {
    /// [`EpicTransition::DesignInPlace`].
    DesignInPlace,
    /// [`EpicTransition::ExpandFirstPhase`].
    ExpandFirstPhase,
    /// [`EpicTransition::AdvancePhase`].
    AdvancePhase,
    /// [`EpicTransition::BuildChildren`].
    BuildChildren,
    /// [`EpicTransition::CloseEpic`].
    CloseEpic,
    /// [`EpicTransition::Noop`].
    Noop,
}

impl EpicTransition {
    /// The payload-free identity of this transition.
    #[must_use]
    pub fn kind(&self) -> TransitionKind {
        match self {
            EpicTransition::DesignInPlace => TransitionKind::DesignInPlace,
            EpicTransition::ExpandFirstPhase => TransitionKind::ExpandFirstPhase,
            EpicTransition::AdvancePhase => TransitionKind::AdvancePhase,
            EpicTransition::BuildChildren(_) => TransitionKind::BuildChildren,
            EpicTransition::CloseEpic => TransitionKind::CloseEpic,
            EpicTransition::Noop => TransitionKind::Noop,
        }
    }

    /// True when this transition performs no work.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        matches!(self, EpicTransition::Noop)
    }
}

/// Compute the **one** enabled transition for an epic from its already-fetched
/// [`EpicSnapshot`]. Pure and total: composes [`derive_epic_state`] (Phase 1)
/// with [`barrier_admits`] (Phase 2), performing no forge I/O and no dispatch.
///
/// The phase-boundary states route through the barrier so phase N+1 (or close)
/// never fires while a current-phase child is still open — if the barrier is
/// held the plan degrades to [`EpicTransition::Noop`], and `active` with no
/// build-ready children is likewise a no-op.
#[must_use]
pub fn plan_epic_transition(snapshot: &EpicSnapshot) -> EpicTransition {
    let state = snapshot.state();
    match state {
        EpicState::NeedsDecomp => EpicTransition::DesignInPlace,
        EpicState::Designed => EpicTransition::ExpandFirstPhase,
        EpicState::Active => {
            if snapshot.open_issue_children.is_empty() {
                // The phase is in flight but its build-ready children are not
                // yet materialized (or all already dispatched upstream); the
                // supervisor waits rather than inventing work.
                EpicTransition::Noop
            } else {
                EpicTransition::BuildChildren(snapshot.open_issue_children.clone())
            }
        }
        EpicState::PhaseJoin => match barrier_admits(state, &snapshot.phase_children) {
            Some(PhaseBoundary::AdvanceToNextPhase) => EpicTransition::AdvancePhase,
            // Barrier holds (a current-phase child is still open) — do not
            // advance. (Defence in depth; derive_epic_state would not have
            // returned PhaseJoin in that case.)
            _ => EpicTransition::Noop,
        },
        EpicState::Done => match barrier_admits(state, &snapshot.phase_children) {
            Some(PhaseBoundary::CloseEpic) => EpicTransition::CloseEpic,
            _ => EpicTransition::Noop,
        },
    }
}

// ============================================================================
// Dispatch shapes
// ============================================================================

/// A concrete role-dispatch the supervisor hands to an [`EpicDispatcher`].
///
/// Modelling the dispatch as a value (rather than an opaque method call) makes
/// the **new Architect-on-epic enrich-body shape** a first-class,
/// test-assertable thing: it carries `produces_pr = false` to mark it distinct
/// from the build→PR dispatch shape, and `creates_issues` flags the two expand
/// bursts that must run under the #3707 mutex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchShape {
    /// The firing role's canonical name (e.g. `"Architect"`, `"Champion"`).
    pub role: &'static str,
    /// The `claude -p` prompt the role process is launched with.
    pub prompt: String,
    /// Whether this dispatch is expected to open a PR. **`false`** for the
    /// enrich-body / expand / close shapes — they mutate issues in place and
    /// never branch a worktree or open a PR.
    pub produces_pr: bool,
    /// Whether this dispatch runs `gh issue create` (an issue-creating burst
    /// that must be serialized under the #3707 mutex).
    pub creates_issues: bool,
}

/// The Architect-on-epic **enrich-body** dispatch shape for `needs_decomp`.
///
/// Instructs the Architect to edit epic `epic`'s body in place, adding the
/// `### Phase` structure that decomposes it — **producing no PR** (the
/// distinguishing feature versus a normal build→PR Architect dispatch).
#[must_use]
pub fn architect_enrich_body_shape(epic: u32) -> DispatchShape {
    DispatchShape {
        role: "Architect",
        prompt: format!(
            "/architect Decompose epic #{epic} IN PLACE: edit the epic issue body to add \
             `### Phase` sections describing the phased plan. Do NOT open a pull request and \
             do NOT create child issues — only enrich the epic body."
        ),
        produces_pr: false,
        creates_issues: false,
    }
}

/// The Champion expand dispatch shape for `designed` / `phase_join`.
///
/// Materializes a wave of `loom:epic-phase` children for the epic. Both kinds
/// carry `creates_issues = true` (they run under the #3707 mutex) and
/// `produces_pr = false`.
#[must_use]
pub fn champion_expand_shape(epic: u32, kind: ExpandKind) -> DispatchShape {
    let which = match kind {
        ExpandKind::FirstPhase => "the first phase",
        ExpandKind::NextPhase => "the next (N+1) phase",
    };
    DispatchShape {
        role: "Champion",
        prompt: format!(
            "/champion Expand epic #{epic}: materialize the child issues for {which} from the \
             epic's `### Phase` plan. Create the phase's `loom:epic-phase` children only."
        ),
        produces_pr: false,
        creates_issues: true,
    }
}

/// The Champion close-epic dispatch shape for `done`.
///
/// Closes the epic once every phase's children are closed. Mutates the epic in
/// place; no PR, no issue creation.
#[must_use]
pub fn champion_close_epic_shape(epic: u32) -> DispatchShape {
    DispatchShape {
        role: "Champion",
        prompt: format!(
            "/champion Close epic #{epic}: all phases are complete (every phase child is \
             closed). Close the epic issue."
        ),
        produces_pr: false,
        creates_issues: false,
    }
}

// ============================================================================
// Source + dispatcher traits
// ============================================================================

/// Fetches the open `loom:epic` issues (and their scheduling-relevant facts)
/// the supervisor iterates each tick.
///
/// Abstracting the forge read behind a trait keeps [`EpicSupervisor`] testable
/// with fixtures and lets the concrete forge query evolve independently.
pub trait EpicSource {
    /// Return one [`EpicSnapshot`] per open `loom:epic` issue.
    fn list_open_epics(&mut self) -> Result<Vec<EpicSnapshot>>;
}

/// Performs the actual role dispatches the supervisor schedules.
///
/// The supervisor owns *when* and *whether* (scheduling + the #3707 mutex +
/// the phase-join barrier); the dispatcher owns *how* (spawning the role
/// process / dispatching the sweep). Methods are synchronous — the supervisor
/// holds the async #3707 guard across the (returning) `dispatch_role` call so
/// the mutex genuinely serializes the burst; a concrete issue-creating
/// dispatcher must therefore not return until its `gh issue create` burst has
/// completed.
pub trait EpicDispatcher {
    /// Dispatch a role with the given [`DispatchShape`] for epic `epic`.
    fn dispatch_role(&mut self, epic: u32, shape: &DispatchShape) -> Result<()>;

    /// Dispatch a `/loom:sweep` for one open `loom:issue` child of `epic`.
    fn dispatch_sweep(&mut self, epic: u32, child: u32) -> Result<()>;
}

// ============================================================================
// Supervisor engine
// ============================================================================

/// An in-flight singleton transition (design / expand / advance / close) the
/// supervisor has already dispatched for an epic and is waiting to land.
#[derive(Debug, Clone, Copy)]
struct InFlight {
    kind: TransitionKind,
    since: Instant,
}

/// Per-tick outcome counts, for observability and test assertions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Epics inspected this tick.
    pub epics_seen: usize,
    /// Singleton role transitions actually dispatched.
    pub roles_dispatched: usize,
    /// Individual `/loom:sweep` dispatches issued for `active` children.
    pub sweeps_dispatched: usize,
    /// Transitions skipped because the same transition was already in flight.
    pub skipped_in_flight: usize,
    /// Epics with no enabled transition this tick.
    pub noop: usize,
    /// Dispatch attempts that returned an error (logged, non-fatal).
    pub errors: usize,
}

/// The Phase 3 supervisor engine: iterates open epics, plans the one enabled
/// transition per epic, and fires it — serializing issue-creating expands under
/// the #3707 mutex and deduplicating in-flight dispatches.
pub struct EpicSupervisor<S: EpicSource, D: EpicDispatcher> {
    source: S,
    dispatcher: D,
    mutex: IssueCreationMutex,
    /// Singleton transition currently in flight per epic number.
    in_flight: HashMap<u32, InFlight>,
    /// Child issue numbers for which a sweep has already been dispatched this
    /// process lifetime (globally unique across epics).
    dispatched_children: HashSet<u32>,
    inflight_ttl: Duration,
}

impl<S: EpicSource, D: EpicDispatcher> EpicSupervisor<S, D> {
    /// Construct a supervisor over `source` and `dispatcher`, sharing the
    /// daemon-global #3707 `mutex`.
    #[must_use]
    pub fn new(source: S, dispatcher: D, mutex: IssueCreationMutex) -> Self {
        Self {
            source,
            dispatcher,
            mutex,
            in_flight: HashMap::new(),
            dispatched_children: HashSet::new(),
            inflight_ttl: resolve_inflight_ttl(),
        }
    }

    /// Override the in-flight TTL (primarily for tests).
    #[must_use]
    pub fn with_inflight_ttl(mut self, ttl: Duration) -> Self {
        self.inflight_ttl = ttl;
        self
    }

    /// Run one supervisor tick: fetch open epics, and for each fire the single
    /// enabled transition (or skip it as a no-op / already-in-flight).
    ///
    /// Errors from an individual dispatch are logged and counted in
    /// [`TickReport::errors`] rather than aborting the tick — one wedged epic
    /// must not starve the rest. A failure to *list* epics aborts the tick.
    pub async fn tick(&mut self) -> Result<TickReport> {
        let epics = self.source.list_open_epics()?;
        let mut report = TickReport {
            epics_seen: epics.len(),
            ..TickReport::default()
        };
        for epic in &epics {
            let transition = plan_epic_transition(epic);
            self.fire(epic.number, transition, &mut report).await;
        }
        Ok(report)
    }

    /// Fire one epic's planned transition, applying in-flight dedup + the
    /// #3707 mutex where required.
    async fn fire(&mut self, epic: u32, transition: EpicTransition, report: &mut TickReport) {
        let kind = transition.kind();

        // Advancing to a *different* transition means the previous one's
        // content mutation landed (the derived state changed) — clear any stale
        // in-flight marker so the new transition is admitted.
        if let Some(existing) = self.in_flight.get(&epic) {
            if existing.kind != kind {
                self.in_flight.remove(&epic);
            }
        }

        match transition {
            EpicTransition::Noop => {
                report.noop += 1;
            }
            EpicTransition::BuildChildren(children) => {
                self.fire_build_children(epic, children, report);
            }
            EpicTransition::DesignInPlace => {
                if self.singleton_in_flight(epic, kind) {
                    report.skipped_in_flight += 1;
                    return;
                }
                let shape = architect_enrich_body_shape(epic);
                self.dispatch_singleton(epic, kind, &shape, report);
            }
            EpicTransition::CloseEpic => {
                if self.singleton_in_flight(epic, kind) {
                    report.skipped_in_flight += 1;
                    return;
                }
                let shape = champion_close_epic_shape(epic);
                self.dispatch_singleton(epic, kind, &shape, report);
            }
            EpicTransition::ExpandFirstPhase => {
                self.fire_expand(epic, kind, ExpandKind::FirstPhase, report)
                    .await;
            }
            EpicTransition::AdvancePhase => {
                self.fire_expand(epic, kind, ExpandKind::NextPhase, report)
                    .await;
            }
        }
    }

    /// Dispatch `/loom:sweep` for each not-yet-dispatched open child, recording
    /// each so repeated ticks over an unchanged `active` epic are no-ops.
    fn fire_build_children(&mut self, epic: u32, children: Vec<u32>, report: &mut TickReport) {
        let mut any_new = false;
        for child in children {
            if self.dispatched_children.contains(&child) {
                continue;
            }
            match self.dispatcher.dispatch_sweep(epic, child) {
                Ok(()) => {
                    self.dispatched_children.insert(child);
                    report.sweeps_dispatched += 1;
                    any_new = true;
                }
                Err(e) => {
                    report.errors += 1;
                    log::warn!(
                        "epic_supervisor: sweep dispatch for child #{child} of epic #{epic} \
                         failed: {e}"
                    );
                }
            }
        }
        if !any_new {
            // Every open child was already dispatched — an unchanged tick.
            report.skipped_in_flight += 1;
        }
    }

    /// Fire a Champion expand transition (`designed` / `phase_join`) under the
    /// #3707 mutex. The async guard is acquired around the (synchronous)
    /// dispatch so the burst is genuinely serialized against every other
    /// issue-creating transition.
    async fn fire_expand(
        &mut self,
        epic: u32,
        kind: TransitionKind,
        expand: ExpandKind,
        report: &mut TickReport,
    ) {
        if self.singleton_in_flight(epic, kind) {
            report.skipped_in_flight += 1;
            return;
        }
        let shape = champion_expand_shape(epic, expand);
        // Hold the #3707 issue-creation mutex across the whole dispatch: both
        // epic-supervisor expand bursts are Champion `creates_issues` edges and
        // must not interleave with any other issue-creating burst. The single
        // CHAMPION_EPIC_DECOMP identity serializes both — the mutex is global,
        // so the identity is only observability metadata.
        let _guard = self.mutex.acquire(CHAMPION_EPIC_DECOMP).await;
        self.dispatch_singleton(epic, kind, &shape, report);
        // _guard dropped here → mutex released once the burst returns.
    }

    /// Dispatch one singleton role transition and record it in flight.
    fn dispatch_singleton(
        &mut self,
        epic: u32,
        kind: TransitionKind,
        shape: &DispatchShape,
        report: &mut TickReport,
    ) {
        match self.dispatcher.dispatch_role(epic, shape) {
            Ok(()) => {
                self.in_flight.insert(
                    epic,
                    InFlight {
                        kind,
                        since: Instant::now(),
                    },
                );
                report.roles_dispatched += 1;
            }
            Err(e) => {
                report.errors += 1;
                log::warn!("epic_supervisor: {} dispatch for epic #{epic} failed: {e}", shape.role);
            }
        }
    }

    /// True when the same singleton transition is already in flight for `epic`
    /// and has not exceeded the TTL. An expired marker is cleared so a crashed
    /// role dispatch can be re-dispatched.
    fn singleton_in_flight(&mut self, epic: u32, kind: TransitionKind) -> bool {
        if let Some(existing) = self.in_flight.get(&epic) {
            if existing.kind == kind {
                if existing.since.elapsed() < self.inflight_ttl {
                    return true;
                }
                // TTL expired — the dispatched role never landed its mutation
                // (a crash); clear so we re-dispatch.
                self.in_flight.remove(&epic);
            }
        }
        false
    }

    // ---- test / observability accessors -----------------------------------

    /// Number of epics currently tracked with an in-flight singleton transition.
    #[must_use]
    pub fn in_flight_len(&self) -> usize {
        self.in_flight.len()
    }

    /// Number of distinct child issues a sweep has been dispatched for.
    #[must_use]
    pub fn dispatched_children_len(&self) -> usize {
        self.dispatched_children.len()
    }

    /// Shared #3707 mutex handle (cloned).
    #[must_use]
    pub fn mutex(&self) -> IssueCreationMutex {
        self.mutex.clone()
    }
}

/// Resolve the in-flight TTL from [`INFLIGHT_TTL_ENV`], falling back to
/// [`DEFAULT_INFLIGHT_TTL_SECS`].
fn resolve_inflight_ttl() -> Duration {
    std::env::var(INFLIGHT_TTL_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or_else(|| Duration::from_secs(DEFAULT_INFLIGHT_TTL_SECS), Duration::from_secs)
}

// ============================================================================
// Runtime wiring — the supervisor loop runs OFF the async runtime (#3872)
// ============================================================================
//
// # Why a dedicated OS thread (Phase 4, #3872)
//
// [`EpicSupervisor::tick`] is an `async fn` for one reason: the two
// issue-creating expand paths acquire the #3707 [`IssueCreationMutex`]
// (a `tokio::sync::Mutex`) and hold the guard across the dispatch so the
// `gh issue create` burst is serialized. But the concrete
// [`forge::SpawnDispatcher::dispatch_role`] is **spawn-and-wait**: it calls
// `Command::status()`, blocking the calling thread until the Architect /
// Champion process exits — potentially minutes. That spawn-and-wait is
// deliberate and correct (the mutex *must* stay held until the burst finishes),
// so the only question is *where* the blocking call runs.
//
// A naive `tokio::spawn(async move { loop { sup.tick().await; ... } })` on the
// shared daemon runtime would park a worker thread inside that blocking
// `Command::status()` for the whole role-process lifetime. On the daemon's
// multi-threaded runtime that starves a worker; on a current-thread runtime it
// would freeze the event bus, reaper, sweep registry, and IPC listener for
// minutes.
//
// So the loop runs on its **own dedicated OS thread** with a private
// current-thread Tokio runtime. `tick()` still `.await`s the tokio mutex there
// (tokio mutexes are runtime-agnostic — the shared daemon-global handle works
// across runtimes), but every blocking `Command::status()` happens on this one
// thread. The shared daemon runtime never sees the block and stays fully
// responsive.

/// Whether the epic supervisor loop is enabled, per [`SUPERVISOR_ENABLE_ENV`].
///
/// Off by default (opt-in) — see [`SUPERVISOR_ENABLE_ENV`].
#[must_use]
pub fn supervisor_enabled() -> bool {
    std::env::var(SUPERVISOR_ENABLE_ENV).is_ok_and(|v| {
        matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    })
}

/// Resolve the supervisor tick interval from [`SUPERVISOR_INTERVAL_ENV`],
/// falling back to [`DEFAULT_SUPERVISOR_INTERVAL_SECS`]. A zero or unparseable
/// value falls back to the default (a zero-interval busy loop is never useful).
#[must_use]
pub fn resolve_supervisor_interval() -> Duration {
    std::env::var(SUPERVISOR_INTERVAL_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .map_or_else(|| Duration::from_secs(DEFAULT_SUPERVISOR_INTERVAL_SECS), Duration::from_secs)
}

/// Handle to a running supervisor loop thread.
///
/// The loop runs on a dedicated OS thread (see the module-level rationale). The
/// handle owns a shared shutdown flag: [`shutdown`](Self::shutdown) signals the
/// loop to stop at the next interval boundary and joins the thread. Dropping
/// the handle without calling `shutdown` signals the flag but does **not** join
/// (a tick may be blocked in a minutes-long role process; the OS reaps the
/// thread on process exit, matching how every other daemon subsystem — reaper,
/// event bus, IPC — is torn down by `process::exit`).
pub struct SupervisorHandle {
    shutdown: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl SupervisorHandle {
    /// A clone of the shutdown flag, so another subsystem (e.g. the daemon's
    /// signal handler) can request a graceful stop without owning the handle.
    #[must_use]
    pub fn shutdown_token(&self) -> Arc<AtomicBool> {
        self.shutdown.clone()
    }

    /// True until a stop has been requested (via [`shutdown`](Self::shutdown)
    /// or the shared [`shutdown_token`](Self::shutdown_token)).
    #[must_use]
    pub fn is_running(&self) -> bool {
        !self.shutdown.load(Ordering::Relaxed)
    }

    /// Signal the loop to stop and join the thread. Idempotent. Blocks until
    /// the current in-flight tick returns, so a caller on the async runtime
    /// should avoid calling this while a role dispatch may be in flight.
    pub fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for SupervisorHandle {
    fn drop(&mut self) {
        // Request stop but do NOT join here — a tick may be blocked in a
        // long-running role process, and process exit reaps the thread anyway.
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

/// Sleep for `total`, waking early (within `CHUNK`) once `shutdown` is set, so
/// a stop request between ticks is honored promptly rather than after a full
/// interval.
fn sleep_interruptible(total: Duration, shutdown: &AtomicBool) {
    const CHUNK: Duration = Duration::from_millis(250);
    let mut remaining = total;
    while remaining > Duration::ZERO {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        let nap = remaining.min(CHUNK);
        std::thread::sleep(nap);
        remaining = remaining.saturating_sub(nap);
    }
}

/// Spawn the supervisor loop on a dedicated OS thread and return its
/// [`SupervisorHandle`].
///
/// The thread builds a private current-thread Tokio runtime and, every
/// `interval`, `block_on`s [`EpicSupervisor::tick`]. Because the blocking
/// spawn-and-wait dispatch executes on this thread's runtime — never the shared
/// daemon runtime — a minutes-long role process cannot starve the daemon's
/// event bus, reaper, sweep registry, or IPC listener (#3872).
///
/// # Errors
///
/// Returns an error if the OS thread cannot be spawned.
pub fn spawn_supervisor_thread<S, D>(
    mut supervisor: EpicSupervisor<S, D>,
    interval: Duration,
) -> Result<SupervisorHandle>
where
    S: EpicSource + Send + 'static,
    D: EpicDispatcher + Send + 'static,
{
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_thread = shutdown.clone();
    let join = std::thread::Builder::new()
        .name("loom-epic-supervisor".to_string())
        .spawn(move || {
            // Private current-thread runtime: `tick()` awaits the tokio
            // IssueCreationMutex, but the blocking Command::status() inside a
            // dispatch runs here, off the shared daemon runtime (#3872).
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    log::error!("epic_supervisor: failed to build loop runtime: {e}");
                    return;
                }
            };
            log::info!("epic_supervisor: loop started (interval={}s)", interval.as_secs());
            while !shutdown_thread.load(Ordering::Relaxed) {
                match rt.block_on(supervisor.tick()) {
                    Ok(report) => {
                        if report.roles_dispatched > 0
                            || report.sweeps_dispatched > 0
                            || report.errors > 0
                        {
                            log::info!(
                                "epic_supervisor: tick — {} epic(s) seen, {} role(s), \
                                 {} sweep(s), {} skipped, {} error(s)",
                                report.epics_seen,
                                report.roles_dispatched,
                                report.sweeps_dispatched,
                                report.skipped_in_flight,
                                report.errors
                            );
                        }
                    }
                    Err(e) => {
                        log::warn!("epic_supervisor: tick failed to list epics: {e}");
                    }
                }
                sleep_interruptible(interval, &shutdown_thread);
            }
            log::info!("epic_supervisor: loop stopped");
        })
        .context("failed to spawn epic supervisor thread")?;
    Ok(SupervisorHandle {
        shutdown,
        join: Some(join),
    })
}

// ============================================================================
// Concrete runtime adapters (forge-backed source + spawn dispatcher)
// ============================================================================

/// Concrete [`EpicSource`] / [`EpicDispatcher`] implementations that wire the
/// supervisor to the live forge (`gh`) and the daemon's [`SweepRegistry`].
///
/// The pure scheduling logic above is exercised in tests via mocks; these
/// adapters are the runtime glue. The only unit-tested piece here is
/// [`parse_epic_phase_marker`] — the rest shells out to `gh` and cannot be
/// exercised without forge credentials.
pub mod forge {
    use super::{DispatchShape, EpicDispatcher, EpicSnapshot, EpicSource, PhaseChild};
    use crate::sweep_registry::SweepRegistry;
    use crate::types::SweepKind;
    use anyhow::{anyhow, Context, Result};
    use serde::Deserialize;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};

    /// The `loom:epic-phase` child marker embedded in a child issue body,
    /// e.g. `<!-- loom:epic:3842:phase:3 -->`. Parsed into `(parent, phase)`.
    ///
    /// Returns `None` when the body carries no such marker. Faithful to the
    /// Champion-authored convention (`<!-- loom:epic:<parent>:phase:<n> -->`).
    #[must_use]
    pub fn parse_epic_phase_marker(body: &str) -> Option<(u32, u32)> {
        // Locate the `loom:epic:` token and read `<parent>:phase:<n>` after it.
        let start = body.find("loom:epic:")? + "loom:epic:".len();
        let rest = &body[start..];
        let (parent_str, after) = rest.split_once(":phase:")?;
        let parent: u32 = parent_str.trim().parse().ok()?;
        // The phase number is the leading digit run of `after`.
        let digits: String = after
            .trim_start()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        let phase: u32 = digits.parse().ok()?;
        Some((parent, phase))
    }

    /// Minimal `gh issue list --json` row.
    #[derive(Debug, Deserialize)]
    struct GhIssue {
        number: u32,
        #[serde(default)]
        body: String,
        #[serde(default)]
        state: String,
        #[serde(default)]
        labels: Vec<GhLabel>,
    }

    #[derive(Debug, Deserialize)]
    struct GhLabel {
        name: String,
    }

    impl GhIssue {
        fn is_open(&self) -> bool {
            self.state.eq_ignore_ascii_case("open")
        }
        fn has_label(&self, name: &str) -> bool {
            self.labels.iter().any(|l| l.name == name)
        }
    }

    /// A forge-backed [`EpicSource`] that lists open `loom:epic` issues and
    /// their `loom:epic-phase` children via `gh`.
    pub struct GhEpicSource {
        gh_bin: PathBuf,
        repo: Option<String>,
    }

    impl GhEpicSource {
        /// Construct a source using `gh` from `PATH`, honoring `LOOM_REPO` for
        /// the `--repo` flag when set.
        #[must_use]
        pub fn new() -> Self {
            Self {
                gh_bin: PathBuf::from("gh"),
                repo: std::env::var("LOOM_REPO").ok(),
            }
        }

        /// Override the `gh` binary path (for tests / non-standard installs).
        #[must_use]
        pub fn with_gh_bin(mut self, bin: PathBuf) -> Self {
            self.gh_bin = bin;
            self
        }

        fn gh_json(&self, label: &str, state: &str) -> Result<Vec<GhIssue>> {
            let mut cmd = Command::new(&self.gh_bin);
            cmd.arg("issue")
                .arg("list")
                .arg("--label")
                .arg(label)
                .arg("--state")
                .arg(state)
                .arg("--limit")
                .arg("200")
                .arg("--json")
                .arg("number,body,state,labels");
            if let Some(ref repo) = self.repo {
                cmd.arg("--repo").arg(repo);
            }
            cmd.stderr(Stdio::piped());
            let out = cmd
                .output()
                .with_context(|| format!("failed to invoke {}", self.gh_bin.display()))?;
            if !out.status.success() {
                return Err(anyhow!(
                    "gh issue list --label {label} failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            serde_json::from_slice(&out.stdout).context("parse gh issue list JSON")
        }
    }

    impl Default for GhEpicSource {
        fn default() -> Self {
            Self::new()
        }
    }

    impl EpicSource for GhEpicSource {
        fn list_open_epics(&mut self) -> Result<Vec<EpicSnapshot>> {
            let epics = self.gh_json("loom:epic", "open")?;
            if epics.is_empty() {
                return Ok(vec![]);
            }
            // All epic-phase children (any state) so closed children still
            // count toward the fork-join barrier.
            let children = self.gh_json("loom:epic-phase", "all")?;

            let snapshots = epics
                .into_iter()
                .map(|epic| {
                    let mut phase_children = Vec::new();
                    let mut open_issue_children = Vec::new();
                    for child in &children {
                        let Some((parent, phase)) = parse_epic_phase_marker(&child.body) else {
                            continue;
                        };
                        if parent != epic.number {
                            continue;
                        }
                        phase_children.push(PhaseChild::new(phase, child.is_open()));
                        // Build-ready children are open and carry loom:issue.
                        if child.is_open() && child.has_label("loom:issue") {
                            open_issue_children.push(child.number);
                        }
                    }
                    EpicSnapshot::new(epic.number, epic.body, phase_children, open_issue_children)
                })
                .collect();
            Ok(snapshots)
        }
    }

    /// A concrete [`EpicDispatcher`] that spawns role processes for singleton
    /// transitions and dispatches sweeps through the daemon [`SweepRegistry`].
    ///
    /// Singleton role dispatches (`dispatch_role`) **spawn-and-wait**: the
    /// child role process runs to completion before the method returns. This is
    /// deliberate — the supervisor holds the #3707 issue-creation mutex across
    /// the call, so an issue-creating expand burst must finish before the guard
    /// is dropped. Sweeps are fire-and-forget via the registry (long-running
    /// builds), deduplicated by the registry's own claim locks.
    pub struct SpawnDispatcher {
        spawn_bin: PathBuf,
        registry: Arc<Mutex<SweepRegistry>>,
    }

    impl SpawnDispatcher {
        /// Construct a dispatcher spawning role processes via `spawn_bin`
        /// (typically `spawn-claude.sh`) and dispatching sweeps through
        /// `registry`.
        #[must_use]
        pub fn new(spawn_bin: PathBuf, registry: Arc<Mutex<SweepRegistry>>) -> Self {
            Self {
                spawn_bin,
                registry,
            }
        }
    }

    impl EpicDispatcher for SpawnDispatcher {
        fn dispatch_role(&mut self, epic: u32, shape: &DispatchShape) -> Result<()> {
            // Spawn-and-wait: the burst must complete before we return so the
            // supervisor's #3707 guard genuinely serializes it.
            let mut cmd = Command::new(&self.spawn_bin);
            cmd.arg("-p")
                .arg(&shape.prompt)
                .arg("--dangerously-skip-permissions")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let status = cmd.status().with_context(|| {
                format!(
                    "failed to spawn {} for {} on epic #{epic}",
                    self.spawn_bin.display(),
                    shape.role
                )
            })?;
            if !status.success() {
                return Err(anyhow!(
                    "{} dispatch for epic #{epic} exited with {status}",
                    shape.role
                ));
            }
            Ok(())
        }

        fn dispatch_sweep(&mut self, _epic: u32, child: u32) -> Result<()> {
            let mut reg = self
                .registry
                .lock()
                .map_err(|e| anyhow!("sweep registry mutex poisoned: {e}"))?;
            // Idempotency key + the registry's claim lock make a re-dispatch of
            // an already-running child a no-op.
            let key = format!("epic-child-{child}");
            reg.dispatch(&SweepKind::Issue(child), Some(key), None, None, None)
                .map(|_| ())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn test_parse_epic_phase_marker_basic() {
            assert_eq!(parse_epic_phase_marker("<!-- loom:epic:3842:phase:3 -->"), Some((3842, 3)));
        }

        #[test]
        fn test_parse_epic_phase_marker_in_larger_body() {
            let body = "# Title\n\n<!-- loom:epic:100:phase:2 -->\n\nsome prose";
            assert_eq!(parse_epic_phase_marker(body), Some((100, 2)));
        }

        #[test]
        fn test_parse_epic_phase_marker_absent() {
            assert_eq!(parse_epic_phase_marker("no marker here"), None);
            assert_eq!(parse_epic_phase_marker("loom:epic:abc:phase:1"), None);
            assert_eq!(parse_epic_phase_marker("loom:epic:5:phase:x"), None);
        }

        #[test]
        fn test_parse_epic_phase_marker_multidigit() {
            assert_eq!(parse_epic_phase_marker("loom:epic:12345:phase:10"), Some((12345, 10)));
        }
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

    /// A body with `n` `### Phase` sections.
    fn body_with_phases(n: usize) -> String {
        let mut s = String::from("# Epic\n\nprose\n\n");
        for i in 1..=n {
            s.push_str(&format!("### Phase {i}: section {i}\ndetails\n\n"));
        }
        s
    }

    // ===================================================================
    // plan_epic_transition — one enabled transition per derived state
    // ===================================================================

    #[test]
    fn test_plan_needs_decomp() {
        let snap = EpicSnapshot::new(1, "no phases here", vec![], vec![]);
        assert_eq!(snap.state(), EpicState::NeedsDecomp);
        assert_eq!(plan_epic_transition(&snap), EpicTransition::DesignInPlace);
    }

    #[test]
    fn test_plan_designed() {
        let snap = EpicSnapshot::new(2, body_with_phases(3), vec![], vec![]);
        assert_eq!(snap.state(), EpicState::Designed);
        assert_eq!(plan_epic_transition(&snap), EpicTransition::ExpandFirstPhase);
    }

    #[test]
    fn test_plan_active_with_build_ready_children() {
        let snap = EpicSnapshot::new(3, body_with_phases(3), vec![open(1)], vec![101, 102]);
        assert_eq!(snap.state(), EpicState::Active);
        assert_eq!(plan_epic_transition(&snap), EpicTransition::BuildChildren(vec![101, 102]));
    }

    #[test]
    fn test_plan_active_without_build_ready_children_is_noop() {
        // Phase in flight (an open epic-phase marker) but no build-ready
        // loom:issue children yet.
        let snap = EpicSnapshot::new(4, body_with_phases(3), vec![open(1)], vec![]);
        assert_eq!(snap.state(), EpicState::Active);
        assert_eq!(plan_epic_transition(&snap), EpicTransition::Noop);
    }

    #[test]
    fn test_plan_phase_join_advances() {
        // Phase 1 closed, body has 3 phases ⇒ phase_join, barrier ready.
        let snap = EpicSnapshot::new(5, body_with_phases(3), vec![closed(1)], vec![]);
        assert_eq!(snap.state(), EpicState::PhaseJoin);
        assert_eq!(plan_epic_transition(&snap), EpicTransition::AdvancePhase);
    }

    #[test]
    fn test_plan_done_closes() {
        // Body has 2 phases; both materialized and closed ⇒ done.
        let snap = EpicSnapshot::new(6, body_with_phases(2), vec![closed(1), closed(2)], vec![]);
        assert_eq!(snap.state(), EpicState::Done);
        assert_eq!(plan_epic_transition(&snap), EpicTransition::CloseEpic);
    }

    #[test]
    fn test_transition_kind_mapping() {
        assert_eq!(EpicTransition::DesignInPlace.kind(), TransitionKind::DesignInPlace);
        assert_eq!(EpicTransition::ExpandFirstPhase.kind(), TransitionKind::ExpandFirstPhase);
        assert_eq!(EpicTransition::AdvancePhase.kind(), TransitionKind::AdvancePhase);
        assert_eq!(EpicTransition::BuildChildren(vec![1]).kind(), TransitionKind::BuildChildren);
        assert_eq!(EpicTransition::CloseEpic.kind(), TransitionKind::CloseEpic);
        assert_eq!(EpicTransition::Noop.kind(), TransitionKind::Noop);
        assert!(EpicTransition::Noop.is_noop());
        assert!(!EpicTransition::DesignInPlace.is_noop());
    }

    // ===================================================================
    // Dispatch shapes — the new Architect-on-epic enrich-body shape
    // ===================================================================

    #[test]
    fn test_architect_enrich_body_shape_produces_no_pr() {
        let shape = architect_enrich_body_shape(42);
        assert_eq!(shape.role, "Architect");
        assert!(!shape.produces_pr, "enrich-body shape must not open a PR");
        assert!(!shape.creates_issues, "enrich-body only edits the epic body");
        assert!(shape.prompt.contains("#42"));
        assert!(shape.prompt.contains("IN PLACE"));
        assert!(shape.prompt.contains("### Phase"));
    }

    #[test]
    fn test_champion_expand_shapes_create_issues_no_pr() {
        for kind in [ExpandKind::FirstPhase, ExpandKind::NextPhase] {
            let shape = champion_expand_shape(7, kind);
            assert_eq!(shape.role, "Champion");
            assert!(shape.creates_issues, "expand is an issue-creating burst");
            assert!(!shape.produces_pr);
        }
        assert_ne!(
            champion_expand_shape(7, ExpandKind::FirstPhase).prompt,
            champion_expand_shape(7, ExpandKind::NextPhase).prompt
        );
    }

    #[test]
    fn test_champion_close_shape() {
        let shape = champion_close_epic_shape(9);
        assert_eq!(shape.role, "Champion");
        assert!(!shape.produces_pr);
        assert!(!shape.creates_issues);
        assert!(shape.prompt.contains("#9"));
    }

    // ===================================================================
    // Recording mocks for the engine
    // ===================================================================

    #[derive(Default)]
    struct RecordingDispatcher {
        roles: Vec<(u32, DispatchShape)>,
        sweeps: Vec<(u32, u32)>,
        fail_role: bool,
        fail_sweep_child: Option<u32>,
    }

    impl EpicDispatcher for RecordingDispatcher {
        fn dispatch_role(&mut self, epic: u32, shape: &DispatchShape) -> Result<()> {
            if self.fail_role {
                anyhow::bail!("forced role failure");
            }
            self.roles.push((epic, shape.clone()));
            Ok(())
        }
        fn dispatch_sweep(&mut self, epic: u32, child: u32) -> Result<()> {
            if self.fail_sweep_child == Some(child) {
                anyhow::bail!("forced sweep failure for #{child}");
            }
            self.sweeps.push((epic, child));
            Ok(())
        }
    }

    struct FixedSource {
        epics: Vec<EpicSnapshot>,
    }
    impl EpicSource for FixedSource {
        fn list_open_epics(&mut self) -> Result<Vec<EpicSnapshot>> {
            Ok(self.epics.clone())
        }
    }

    fn supervisor(epics: Vec<EpicSnapshot>) -> EpicSupervisor<FixedSource, RecordingDispatcher> {
        EpicSupervisor::new(
            FixedSource { epics },
            RecordingDispatcher::default(),
            IssueCreationMutex::new(),
        )
    }

    // ===================================================================
    // tick() — each transition edge dispatches the right thing
    // ===================================================================

    #[tokio::test]
    async fn test_tick_needs_decomp_dispatches_architect_enrich() {
        let mut sup = supervisor(vec![EpicSnapshot::new(1, "flat body", vec![], vec![])]);
        let report = sup.tick().await.unwrap();
        assert_eq!(report.roles_dispatched, 1);
        assert_eq!(sup.dispatcher.roles.len(), 1);
        let (epic, shape) = &sup.dispatcher.roles[0];
        assert_eq!(*epic, 1);
        assert_eq!(shape.role, "Architect");
        assert!(!shape.produces_pr);
    }

    #[tokio::test]
    async fn test_tick_designed_dispatches_champion_expand_under_mutex() {
        let mut sup = supervisor(vec![EpicSnapshot::new(2, body_with_phases(3), vec![], vec![])]);
        let mutex = sup.mutex();
        let report = sup.tick().await.unwrap();
        assert_eq!(report.roles_dispatched, 1);
        assert_eq!(sup.dispatcher.roles[0].1.role, "Champion");
        assert!(sup.dispatcher.roles[0].1.creates_issues);
        // The expand burst ran under the #3707 mutex (one completed burst).
        assert_eq!(mutex.completed_bursts().await, 1);
    }

    #[tokio::test]
    async fn test_tick_active_dispatches_sweep_per_child() {
        let mut sup = supervisor(vec![EpicSnapshot::new(
            3,
            body_with_phases(3),
            vec![open(1)],
            vec![301, 302, 303],
        )]);
        let report = sup.tick().await.unwrap();
        assert_eq!(report.sweeps_dispatched, 3);
        assert_eq!(sup.dispatcher.sweeps, vec![(3, 301), (3, 302), (3, 303)]);
    }

    #[tokio::test]
    async fn test_tick_phase_join_dispatches_advance_under_mutex() {
        let mut sup = supervisor(vec![EpicSnapshot::new(
            5,
            body_with_phases(3),
            vec![closed(1)],
            vec![],
        )]);
        let mutex = sup.mutex();
        let report = sup.tick().await.unwrap();
        assert_eq!(report.roles_dispatched, 1);
        assert_eq!(sup.dispatcher.roles[0].1.role, "Champion");
        assert!(sup.dispatcher.roles[0].1.creates_issues);
        assert_eq!(mutex.completed_bursts().await, 1);
    }

    #[tokio::test]
    async fn test_tick_done_dispatches_close() {
        let mut sup = supervisor(vec![EpicSnapshot::new(
            6,
            body_with_phases(2),
            vec![closed(1), closed(2)],
            vec![],
        )]);
        let report = sup.tick().await.unwrap();
        assert_eq!(report.roles_dispatched, 1);
        let (_epic, shape) = &sup.dispatcher.roles[0];
        assert_eq!(shape.role, "Champion");
        assert!(shape.prompt.contains("Close epic"));
    }

    // ===================================================================
    // Idempotency: an unchanged epic re-ticks to a no-op
    // ===================================================================

    #[tokio::test]
    async fn test_singleton_transition_dedups_across_ticks() {
        // needs_decomp: first tick dispatches Architect, second tick (same
        // unchanged forge state) must NOT re-dispatch.
        let snap = EpicSnapshot::new(1, "flat body", vec![], vec![]);
        let mut sup = supervisor(vec![snap]);

        let r1 = sup.tick().await.unwrap();
        assert_eq!(r1.roles_dispatched, 1);

        let r2 = sup.tick().await.unwrap();
        assert_eq!(r2.roles_dispatched, 0, "unchanged epic must not re-dispatch");
        assert_eq!(r2.skipped_in_flight, 1);
        assert_eq!(sup.dispatcher.roles.len(), 1, "still exactly one dispatch");
    }

    #[tokio::test]
    async fn test_active_sweeps_dedup_per_child() {
        let mut sup = supervisor(vec![EpicSnapshot::new(
            3,
            body_with_phases(3),
            vec![open(1)],
            vec![301, 302],
        )]);
        let r1 = sup.tick().await.unwrap();
        assert_eq!(r1.sweeps_dispatched, 2);
        // Second identical tick: both children already dispatched ⇒ no new
        // sweeps, counted as an in-flight skip.
        let r2 = sup.tick().await.unwrap();
        assert_eq!(r2.sweeps_dispatched, 0);
        assert_eq!(r2.skipped_in_flight, 1);
        assert_eq!(sup.dispatcher.sweeps.len(), 2);
    }

    #[tokio::test]
    async fn test_active_new_child_appears_next_tick() {
        // First tick has one build-ready child; a later wave adds another.
        let first = EpicSnapshot::new(3, body_with_phases(3), vec![open(1)], vec![301]);
        let mut sup = supervisor(vec![first]);
        let r1 = sup.tick().await.unwrap();
        assert_eq!(r1.sweeps_dispatched, 1);

        // Swap the source's epic for one with an additional open child.
        sup.source.epics = vec![EpicSnapshot::new(
            3,
            body_with_phases(3),
            vec![open(1)],
            vec![301, 350],
        )];
        let r2 = sup.tick().await.unwrap();
        assert_eq!(r2.sweeps_dispatched, 1, "only the new child #350 dispatches");
        assert_eq!(sup.dispatcher.sweeps, vec![(3, 301), (3, 350)]);
    }

    // ===================================================================
    // Recovery: state advances → the ledger clears → next transition fires
    // ===================================================================

    #[tokio::test]
    async fn test_state_advance_clears_ledger_and_fires_next_transition() {
        // Tick 1: needs_decomp → Architect enrich (in flight).
        let mut sup = supervisor(vec![EpicSnapshot::new(1, "flat", vec![], vec![])]);
        let r1 = sup.tick().await.unwrap();
        assert_eq!(r1.roles_dispatched, 1);
        assert_eq!(sup.in_flight_len(), 1);

        // The Architect landed its body edit: the epic is now designed. The
        // supervisor must recognise the advanced state, clear the stale
        // in-flight marker, and dispatch the Champion expand.
        sup.source.epics = vec![EpicSnapshot::new(1, body_with_phases(3), vec![], vec![])];
        let r2 = sup.tick().await.unwrap();
        assert_eq!(r2.roles_dispatched, 1);
        assert_eq!(sup.dispatcher.roles.len(), 2);
        assert_eq!(sup.dispatcher.roles[1].1.role, "Champion");
    }

    #[tokio::test]
    async fn test_ttl_expiry_allows_redispatch_after_crash() {
        // A dispatched role that never lands its mutation (crash) must be
        // re-dispatched once the TTL elapses. Use a zero TTL to force it.
        let mut sup = supervisor(vec![EpicSnapshot::new(1, "flat", vec![], vec![])])
            .with_inflight_ttl(Duration::from_secs(0));
        let r1 = sup.tick().await.unwrap();
        assert_eq!(r1.roles_dispatched, 1);
        // TTL is zero, so the in-flight marker is already stale next tick.
        let r2 = sup.tick().await.unwrap();
        assert_eq!(r2.roles_dispatched, 1, "expired in-flight allows re-dispatch");
        assert_eq!(sup.dispatcher.roles.len(), 2);
    }

    // ===================================================================
    // Multiple epics in one tick; errors are isolated
    // ===================================================================

    #[tokio::test]
    async fn test_tick_over_many_epics_fires_each() {
        let mut sup = supervisor(vec![
            EpicSnapshot::new(1, "flat", vec![], vec![]), // needs_decomp
            EpicSnapshot::new(2, body_with_phases(2), vec![], vec![]), // designed
            EpicSnapshot::new(3, body_with_phases(2), vec![open(1)], vec![301]), // active
            EpicSnapshot::new(4, body_with_phases(2), vec![closed(1), closed(2)], vec![]), // done
        ]);
        let report = sup.tick().await.unwrap();
        assert_eq!(report.epics_seen, 4);
        assert_eq!(report.roles_dispatched, 3); // architect, champion-expand, champion-close
        assert_eq!(report.sweeps_dispatched, 1);
    }

    #[tokio::test]
    async fn test_dispatch_error_is_isolated_and_counted() {
        let mut sup = EpicSupervisor::new(
            FixedSource {
                epics: vec![EpicSnapshot::new(1, "flat", vec![], vec![])],
            },
            RecordingDispatcher {
                fail_role: true,
                ..RecordingDispatcher::default()
            },
            IssueCreationMutex::new(),
        );
        let report = sup.tick().await.unwrap();
        assert_eq!(report.errors, 1);
        assert_eq!(report.roles_dispatched, 0);
        // A failed dispatch is NOT recorded in flight, so the next tick retries.
        assert_eq!(sup.in_flight_len(), 0);
    }

    #[tokio::test]
    async fn test_partial_sweep_failure_still_dispatches_others() {
        let mut sup = EpicSupervisor::new(
            FixedSource {
                epics: vec![EpicSnapshot::new(
                    3,
                    body_with_phases(2),
                    vec![open(1)],
                    vec![301, 302, 303],
                )],
            },
            RecordingDispatcher {
                fail_sweep_child: Some(302),
                ..RecordingDispatcher::default()
            },
            IssueCreationMutex::new(),
        );
        let report = sup.tick().await.unwrap();
        assert_eq!(report.sweeps_dispatched, 2);
        assert_eq!(report.errors, 1);
        // The failed child is not recorded, so a later tick retries just it.
        assert!(!sup.dispatched_children.contains(&302));
        assert!(sup.dispatched_children.contains(&301));
        assert!(sup.dispatched_children.contains(&303));
    }

    // ===================================================================
    // Barrier gating: phase_join with an open current-phase child holds
    // ===================================================================

    #[test]
    fn test_barrier_holds_when_current_phase_child_open() {
        // derive_epic_state returns Active (open child), so plan is
        // BuildChildren/Noop — never AdvancePhase. Confirm the phase-boundary
        // is not taken while a child is open.
        let snap = EpicSnapshot::new(5, body_with_phases(3), vec![open(2), closed(1)], vec![]);
        assert_eq!(snap.state(), EpicState::Active);
        assert_eq!(plan_epic_transition(&snap), EpicTransition::Noop);
    }

    // ===================================================================
    // TTL env resolution
    // ===================================================================

    #[test]
    fn test_resolve_inflight_ttl_default() {
        // Not asserting on the process env (tests share it); just confirm the
        // default constant is what we fall back to.
        assert_eq!(DEFAULT_INFLIGHT_TTL_SECS, 900);
        // A fresh supervisor with an explicit override honors it.
        let sup = supervisor(vec![]).with_inflight_ttl(Duration::from_secs(1234));
        assert_eq!(sup.inflight_ttl, Duration::from_secs(1234));
    }

    // ===================================================================
    // Phase 4 (#3872): runtime wiring — off-runtime supervisor loop thread
    // ===================================================================

    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};

    use serial_test::serial;

    /// A `Send` source that always yields the same fixed epics — usable from
    /// the dedicated supervisor thread (unlike the non-`Send`-shared
    /// `FixedSource`, this one is self-contained).
    struct SharedSource {
        epics: Vec<EpicSnapshot>,
    }
    impl EpicSource for SharedSource {
        fn list_open_epics(&mut self) -> Result<Vec<EpicSnapshot>> {
            Ok(self.epics.clone())
        }
    }

    /// A `Send` dispatcher recording dispatch counts into shared atomics so a
    /// test can observe progress of the loop running on another thread.
    struct CountingDispatcher {
        roles: Arc<AtomicUsize>,
        sweeps: Arc<AtomicUsize>,
        /// The thread id the blocking dispatch actually ran on — proves the
        /// dispatch executes off the caller's thread.
        dispatch_thread: Arc<Mutex<Option<std::thread::ThreadId>>>,
    }
    impl EpicDispatcher for CountingDispatcher {
        fn dispatch_role(&mut self, _epic: u32, _shape: &DispatchShape) -> Result<()> {
            *self.dispatch_thread.lock().unwrap() = Some(std::thread::current().id());
            self.roles.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn dispatch_sweep(&mut self, _epic: u32, _child: u32) -> Result<()> {
            self.sweeps.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// Block up to `deadline` for `cond` to hold, polling briefly.
    fn wait_until(deadline: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        cond()
    }

    #[test]
    fn test_spawn_supervisor_thread_ticks_then_shuts_down() {
        let roles = Arc::new(AtomicUsize::new(0));
        let sweeps = Arc::new(AtomicUsize::new(0));
        let dispatch_thread = Arc::new(Mutex::new(None));
        let dispatcher = CountingDispatcher {
            roles: roles.clone(),
            sweeps: sweeps.clone(),
            dispatch_thread: dispatch_thread.clone(),
        };
        // A single needs_decomp epic: the first tick dispatches the Architect
        // enrich, subsequent ticks dedup (in-flight), so `roles` settles at 1.
        let source = SharedSource {
            epics: vec![EpicSnapshot::new(1, "flat body", vec![], vec![])],
        };
        let sup = EpicSupervisor::new(source, dispatcher, IssueCreationMutex::new());

        let caller_thread = std::thread::current().id();
        let mut handle = spawn_supervisor_thread(sup, Duration::from_millis(20)).unwrap();
        assert!(handle.is_running());

        // The loop should dispatch the singleton architect transition promptly.
        assert!(
            wait_until(Duration::from_secs(5), || roles.load(Ordering::SeqCst) >= 1),
            "supervisor loop never dispatched"
        );

        handle.shutdown();
        assert!(!handle.is_running(), "handle reports stopped after shutdown");

        // Exactly one role dispatched (idempotent dedup across the many ticks
        // that ran before shutdown); no sweeps for a needs_decomp epic.
        assert_eq!(roles.load(Ordering::SeqCst), 1, "singleton dedup held across ticks");
        assert_eq!(sweeps.load(Ordering::SeqCst), 0);

        // The blocking dispatch ran on the dedicated supervisor thread, NOT the
        // test's (caller's) thread — the core #3872 guarantee.
        let ran_on = dispatch_thread
            .lock()
            .unwrap()
            .expect("dispatch recorded a thread");
        assert_ne!(ran_on, caller_thread, "dispatch must run off the caller thread");
    }

    #[test]
    fn test_supervisor_handle_shutdown_is_idempotent() {
        let source = SharedSource { epics: vec![] };
        let sup =
            EpicSupervisor::new(source, RecordingDispatcher::default(), IssueCreationMutex::new());
        let mut handle = spawn_supervisor_thread(sup, Duration::from_millis(20)).unwrap();
        let token = handle.shutdown_token();
        // An external subsystem (e.g. the signal handler) can request stop via
        // the shared token.
        token.store(true, Ordering::Relaxed);
        assert!(!handle.is_running());
        handle.shutdown();
        handle.shutdown(); // second call is a no-op (already joined)
        assert!(!handle.is_running());
    }

    #[test]
    #[serial]
    fn test_supervisor_enable_gating() {
        std::env::remove_var(SUPERVISOR_ENABLE_ENV);
        assert!(!supervisor_enabled(), "off by default (opt-in)");
        for truthy in ["1", "true", "TRUE", "yes", "on", " on "] {
            std::env::set_var(SUPERVISOR_ENABLE_ENV, truthy);
            assert!(supervisor_enabled(), "'{truthy}' enables");
        }
        for falsy in ["0", "false", "no", "off", "nonsense", ""] {
            std::env::set_var(SUPERVISOR_ENABLE_ENV, falsy);
            assert!(!supervisor_enabled(), "'{falsy}' stays disabled");
        }
        std::env::remove_var(SUPERVISOR_ENABLE_ENV);
    }

    #[test]
    #[serial]
    fn test_resolve_supervisor_interval_default_and_override() {
        assert_eq!(DEFAULT_SUPERVISOR_INTERVAL_SECS, 300);
        std::env::remove_var(SUPERVISOR_INTERVAL_ENV);
        assert_eq!(
            resolve_supervisor_interval(),
            Duration::from_secs(DEFAULT_SUPERVISOR_INTERVAL_SECS)
        );
        std::env::set_var(SUPERVISOR_INTERVAL_ENV, "45");
        assert_eq!(resolve_supervisor_interval(), Duration::from_secs(45));
        // Zero / garbage fall back to the default (no busy loop).
        std::env::set_var(SUPERVISOR_INTERVAL_ENV, "0");
        assert_eq!(
            resolve_supervisor_interval(),
            Duration::from_secs(DEFAULT_SUPERVISOR_INTERVAL_SECS)
        );
        std::env::set_var(SUPERVISOR_INTERVAL_ENV, "notanumber");
        assert_eq!(
            resolve_supervisor_interval(),
            Duration::from_secs(DEFAULT_SUPERVISOR_INTERVAL_SECS)
        );
        std::env::remove_var(SUPERVISOR_INTERVAL_ENV);
    }
}
