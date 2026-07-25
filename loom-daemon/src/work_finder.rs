//! Autonomous work-finder loop — forge-polling dispatch of `loom:issue` items
//! (Phase A of epic #3809).
//!
//! The daemon-native work finder is the **core missing brain**: the component
//! that turns a human-approved `loom:issue` into a dispatched build without an
//! operator. Before this loop the Rust `loom-daemon` had no forge poller — its
//! only sweep entry point was the explicit `DispatchSweep` IPC request. The
//! deleted v0.10.0 shepherd brain did this; this module restores it on the
//! daemon runtime.
//!
//! # Shape (mirrors [`crate::epic_supervisor`])
//!
//! Per tick, the finder:
//!
//! 1. Queries the forge for ready work — `gh issue list --label loom:issue
//!    --state open --json number,labels` via [`GhWorkSource`], the direct
//!    analogue of [`crate::epic_supervisor::forge::GhEpicSource`].
//! 2. Filters out issues that are **already in flight** (present in the
//!    [`SweepRegistry`](crate::sweep_registry::SweepRegistry) as a `Running` /
//!    `Pending` sweep) or that defensively carry any [`SKIP_LABELS`] entry
//!    (`loom:building` / `loom:blocked` / `loom:operator-only`).
//! 3. For each remaining issue, dispatches through the existing
//!    [`SweepRegistry::dispatch`](crate::sweep_registry::SweepRegistry::dispatch)
//!    path — up to a **work-driven** max-concurrency cap recomputed every tick
//!    (Phase B, #3811): `min(token-pool size, disk headroom, configured max)`.
//!    `dispatch()` already flips `loom:issue → loom:building`, acquires the
//!    per-issue `mkdir`-atomic claim lock, and spawns the rotated-token child.
//!
//! # Concurrency scaling (Phase B, #3811)
//!
//! Phase A resolved a single fixed cap once at daemon startup. Phase B replaces
//! it with a cap **recomputed every tick** by
//! [`resolve_dynamic_max_concurrent`] from three live inputs — the token-pool
//! size ([`crate::tokens::token_pool_size`]), the worktree-root disk headroom
//! ([`crate::disk_headroom::disk_headroom_limit`]), and the operator ceiling
//! (`LOOM_WORK_FINDER_MAX_CONCURRENT`, repurposed from Phase A's fixed target
//! into a *ceiling*). The effective per-tick concurrency is then
//! `min(dynamic_cap, backlog_depth)`: [`tick`] iterates the ready `loom:issue`
//! rows and stops at the cap, so concurrency scales **up** as the backlog grows
//! and drains to **zero** dispatches when the queue is empty — all without a
//! daemon restart, since pool/disk/backlog are read fresh each tick.
//!
//! # Idempotency & fail-safe
//!
//! The finder never reimplements the claim/label/dedup machinery — it reuses
//! the three layers `dispatch()` already provides:
//!
//! - **Idempotency key** — each dispatch uses `workfinder-<issue>` so a running
//!   sweep with the same key short-circuits to a no-op (`was_new = false`).
//! - **Claim lock** — `dispatch()` acquires `.loom/locks/issue-<N>` atomically;
//!   a collision (e.g. a concurrent epic-supervisor sweep for the same child)
//!   fails loudly and is logged, never double-dispatched.
//! - **Registry dedup** — the authoritative "already in-flight" check is the
//!   registry itself: an issue with a live `Running`/`Pending` entry is skipped
//!   even if the forge still shows `loom:issue` (label-flip lag).
//!
//! A forge-query error aborts *that* tick only; the caller logs it and the next
//! tick proceeds normally. A single dispatch error is logged and counted, never
//! fatal — one wedged issue must not starve the rest, and nothing propagates a
//! panic out of the detached loop task.
//!
//! # Why a plain `tokio::spawn` (not a dedicated OS thread)
//!
//! Unlike [`crate::epic_supervisor`], whose concrete dispatcher is
//! spawn-and-wait (`Command::status()` blocks for the whole Architect/Champion
//! process lifetime, holding the #3707 mutex), every call into
//! [`SweepRegistry::dispatch`](crate::sweep_registry::SweepRegistry::dispatch)
//! returns quickly: it spawns the child via `Command::spawn` and returns the
//! handle immediately for the reaper to reap later. The finder holds no mutex
//! across a long call, so a plain `tokio::spawn` interval task on the shared
//! daemon runtime is sufficient and correct — matching the reaper task
//! ([`crate::sweep_registry::spawn_reaper_task`]) rather than the epic
//! supervisor's OS-thread machinery.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use crate::capacity::{self, CapacityAdvisory};
use crate::disk_headroom::disk_headroom_limit;
use crate::event_bus::EventBus;
use crate::main_health_gate::{MainHealthState, WorkspaceHealthStates};
use crate::tokens::token_pool_size;
use crate::types::Event;
use crate::workspace_pool::WorkspacePool;
use crate::workspace_registry::WorkspaceRegistry;

// ============================================================================
// Constants
// ============================================================================

/// Environment variable enabling the work-finder loop.
///
/// The finder is **opt-in** — unset or a false-y value keeps it OFF, so the
/// daemon's behavior is byte-for-byte unchanged when the variable is absent —
/// because the loop autonomously dispatches build sweeps (spawning
/// rotated-token children and flipping `loom:issue → loom:building`). Set to
/// `1` / `true` / `yes` / `on` (case-insensitive) to enable.
pub const WORK_FINDER_ENABLE_ENV: &str = "LOOM_WORK_FINDER";

/// Environment variable overriding the work-finder tick interval (seconds).
pub const WORK_FINDER_INTERVAL_ENV: &str = "LOOM_WORK_FINDER_INTERVAL_SECS";

/// Default work-finder tick interval. Much tighter than the epic supervisor's
/// 300s default — the `loom:issue` backlog should drain promptly — while still
/// keeping forge query volume low.
pub const DEFAULT_WORK_FINDER_INTERVAL_SECS: u64 = 60;

/// Environment variable setting the max-concurrency **ceiling**.
///
/// In Phase A this was the fixed concurrency target; Phase B (#3811) repurposes
/// it as the operator ceiling in the dynamic policy
/// ([`resolve_dynamic_max_concurrent`]) — the cap never rises above this value
/// however large the token pool or disk headroom. The name is intentionally
/// kept (no new env var) so existing operator configuration keeps working.
pub const WORK_FINDER_MAX_CONCURRENT_ENV: &str = "LOOM_WORK_FINDER_MAX_CONCURRENT";

/// Default max-concurrency ceiling. The dynamic cap
/// ([`resolve_dynamic_max_concurrent`]) is bounded by the token-pool size and
/// disk headroom in addition to this ceiling, so this is an upper bound, not a
/// fixed target.
pub const DEFAULT_WORK_FINDER_MAX_CONCURRENT: usize = 3;

/// Labels that disqualify an issue from dispatch even if it still appears in
/// the `loom:issue`-filtered listing.
///
/// A `loom:issue` row should never itself carry these (they are mutually
/// exclusive states in the `.github/labels.yml` state machine), but `gh`'s
/// label cache can be briefly stale, so the finder checks defensively.
pub const SKIP_LABELS: &[&str] = &["loom:building", "loom:blocked", "loom:operator-only"];

/// Label that promotes an issue ahead of its non-urgent siblings **within the
/// same workspace-priority tier** (Issue #3946). Detection is best-effort: if no
/// issue in a deployment carries this label the ordering reduces to
/// (workspace priority, age) with no behavior change, so this never depends on
/// the label being defined in a given repo's `.github/labels.yml`.
pub const URGENT_LABEL: &str = "loom:urgent";

// ============================================================================
// Fetched work facts
// ============================================================================

/// One ready-work candidate fetched from the forge: its issue number and the
/// labels it currently carries (for defensive [`SKIP_LABELS`] filtering).
///
/// Keeping this a plain data struct (no forge I/O) makes [`tick`] a pure
/// function of already-fetched data, mirroring the [`crate::epic_supervisor`]
/// design. A [`WorkSource`] materializes these from the forge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItem {
    /// The issue number.
    pub number: u32,
    /// The labels currently on the issue.
    pub labels: Vec<String>,
    /// The issue's creation timestamp (`gh`'s `createdAt`, an ISO-8601 string),
    /// used for age ordering (#3946). ISO-8601 sorts chronologically as a plain
    /// string, so oldest-first is `created_at` ascending. `None` (older `gh`
    /// output / a synthetic item) sorts *after* any dated item and falls back to
    /// the issue number as a monotonic-with-creation age proxy.
    pub created_at: Option<String>,
}

impl WorkItem {
    /// Convenience constructor (no creation timestamp — the item sorts by number
    /// as its age proxy).
    #[must_use]
    pub fn new(number: u32, labels: Vec<String>) -> Self {
        Self {
            number,
            labels,
            created_at: None,
        }
    }

    /// Constructor carrying the issue's `createdAt` timestamp for age ordering.
    #[must_use]
    pub fn with_created_at(number: u32, labels: Vec<String>, created_at: Option<String>) -> Self {
        Self {
            number,
            labels,
            created_at,
        }
    }

    /// True when the issue carries any [`SKIP_LABELS`] entry.
    #[must_use]
    pub fn is_skipped(&self) -> bool {
        self.labels
            .iter()
            .any(|l| SKIP_LABELS.contains(&l.as_str()))
    }

    /// True when the issue carries the [`URGENT_LABEL`] (#3946) — it dispatches
    /// ahead of non-urgent siblings in the same workspace-priority tier.
    #[must_use]
    pub fn is_urgent(&self) -> bool {
        self.labels.iter().any(|l| l == URGENT_LABEL)
    }
}

/// A dispatch candidate tagged with the cross-repo ordering keys (#3946): its
/// workspace's priority tier, urgency, age, and the workspace index used to
/// route the eventual `dispatch()` back to the owning workspace. Built by
/// [`tick_multi`] after the per-workspace skip-label / in-flight filtering, then
/// globally sorted by [`candidate_cmp`] before the shared concurrency budget is
/// filled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriorityCandidate {
    /// The owning workspace's index in the `workspaces` slice (dispatch routing).
    pub workspace_idx: usize,
    /// The owning workspace's priority tier (lower = higher priority).
    pub workspace_priority: u32,
    /// Whether the issue carries [`URGENT_LABEL`].
    pub urgent: bool,
    /// The issue's creation timestamp for age ordering (oldest-first).
    pub created_at: Option<String>,
    /// The issue number (dispatch target + final deterministic tiebreak).
    pub number: u32,
}

/// Total ordering over dispatch candidates (#3946): **(workspace priority asc,
/// `loom:urgent` first, issue age asc/oldest-first, issue number asc)**.
///
/// - Workspace priority ascending puts higher-priority tiers (lower numbers)
///   first, so a tool repo pinned to `0` drains before a product repo at the
///   default `100` regardless of how deep or old the product backlog is.
/// - Within a tier, urgent issues (`loom:urgent`) come before non-urgent ones.
/// - Then oldest-first by `createdAt`: a dated issue sorts before an undated one
///   (`Some < None`); two undated issues fall through to the number tiebreak.
/// - The issue number is the final tiebreak so the order is fully deterministic
///   (and, since numbers are monotonic with creation, a sane age proxy when
///   `createdAt` is unavailable).
#[must_use]
pub fn candidate_cmp(a: &PriorityCandidate, b: &PriorityCandidate) -> std::cmp::Ordering {
    a.workspace_priority
        .cmp(&b.workspace_priority)
        // `urgent` true should sort first: reverse the bool compare (true > false).
        .then_with(|| b.urgent.cmp(&a.urgent))
        .then_with(|| cmp_created_at(&a.created_at, &b.created_at))
        .then_with(|| a.number.cmp(&b.number))
}

/// Oldest-first ordering over optional `createdAt` timestamps: a dated issue
/// (`Some`) sorts before an undated one (`None`); two dated issues compare
/// lexically (ISO-8601 ⇒ chronological); two undated issues are equal (the
/// caller's number tiebreak decides).
fn cmp_created_at(a: &Option<String>, b: &Option<String>) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Some(x), Some(y)) => x.cmp(y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

// ============================================================================
// Source + dispatcher traits
// ============================================================================

/// Fetches the ready-to-build `loom:issue` items the finder iterates each tick.
///
/// Abstracting the forge read behind a trait keeps [`tick`] testable with a
/// fake source and lets the concrete `gh` query evolve independently — exactly
/// as [`crate::epic_supervisor::EpicSource`] does.
pub trait WorkSource {
    /// Return one [`WorkItem`] per open `loom:issue`.
    ///
    /// # Errors
    ///
    /// Returns an error when the forge query fails. The caller logs it and
    /// retries on the next tick — the error is never fatal.
    fn list_ready_issues(&mut self) -> Result<Vec<WorkItem>>;
}

/// Performs the actual sweep dispatches the finder schedules and reports which
/// issues are already in flight.
///
/// The finder owns *when* and *whether* (scheduling + the concurrency cap); the
/// dispatcher owns *how* (the registry `dispatch()` call and the in-flight
/// query). Splitting it out keeps [`tick`] unit-testable without a real
/// registry or `gh` credentials.
pub trait WorkDispatcher {
    /// The set of issue numbers that currently have a live (`Running` /
    /// `Pending`) sweep — the authoritative "already in-flight" view.
    fn in_flight(&self) -> HashSet<u32>;

    /// Dispatch a build sweep for `issue`. Returns `true` when a **new** sweep
    /// was started, `false` when the dispatch was an idempotency no-op (a sweep
    /// with the same key was already running).
    ///
    /// # Errors
    ///
    /// Returns an error when the dispatch fails (e.g. a claim-lock collision).
    /// The caller logs and counts it; it is never fatal.
    fn dispatch(&mut self, issue: u32) -> Result<bool>;
}

// ============================================================================
// Tick
// ============================================================================

/// Per-tick outcome counts, for observability and test assertions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Ready `loom:issue` rows returned by the source this tick.
    pub seen: usize,
    /// Issues for which a **new** sweep was dispatched this tick.
    pub dispatched: usize,
    /// Issues skipped because they carried a [`SKIP_LABELS`] entry.
    pub skipped_labeled: usize,
    /// Issues skipped because a live sweep already exists for them (registry
    /// in-flight set, or an idempotency no-op from `dispatch()`).
    pub skipped_in_flight: usize,
    /// Issues deferred to a future tick because the concurrency cap was reached.
    pub deferred_capacity: usize,
    /// Dispatch attempts that returned an error (logged, non-fatal).
    pub errors: usize,
    /// True when this tick dispatched nothing because the main-health gate
    /// (Phase C, #3812) had halted dispatch (`main` was red). `seen` still
    /// reflects the backlog depth; `dispatched` is always 0 in this case.
    pub halted: bool,
}

/// Run one work-finder tick: fetch ready issues, filter, and dispatch up to the
/// fixed concurrency cap.
///
/// The count of live sweeps at tick start (`dispatcher.in_flight().len()`) is
/// treated as the current occupancy; the finder dispatches only while
/// `occupancy < max_concurrent`, incrementing occupancy per new dispatch so a
/// single tick never overshoots the cap.
///
/// # Reactive main-health halt (Phase C, #3812)
///
/// When `halted` is `true` the main-health gate has observed a red `main`, so
/// this tick dispatches **zero** new issues (existing in-flight sweeps are never
/// touched) and returns early with [`TickReport::halted`] set. `seen` still
/// reflects the backlog so the loop can log "backlog is N but halted." The
/// caller resumes normally once a green gate run clears the flag.
///
/// # Errors
///
/// Propagates a source (`list_ready_issues`) error so the caller can log it and
/// retry next tick. Individual dispatch errors are logged and counted in
/// [`TickReport::errors`] rather than aborting the tick.
pub fn tick(
    source: &mut impl WorkSource,
    dispatcher: &mut impl WorkDispatcher,
    max_concurrent: usize,
    halted: bool,
) -> Result<TickReport> {
    let ready = source.list_ready_issues()?;
    let mut report = TickReport {
        seen: ready.len(),
        ..TickReport::default()
    };

    // Reactive backstop: a red `main` halts all new dispatch this tick.
    if halted {
        report.halted = true;
        return Ok(report);
    }

    let in_flight = dispatcher.in_flight();
    let mut occupancy = in_flight.len();

    for item in ready {
        // 1. Defensive skip-label filter (stale forge cache).
        if item.is_skipped() {
            report.skipped_labeled += 1;
            continue;
        }
        // 2. Authoritative in-flight dedup against the registry.
        if in_flight.contains(&item.number) {
            report.skipped_in_flight += 1;
            continue;
        }
        // 3. Fixed concurrency cap — defer the rest to a future tick.
        if occupancy >= max_concurrent {
            report.deferred_capacity += 1;
            continue;
        }
        // 4. Dispatch. The registry's idempotency key + claim lock make a
        //    double-dispatch of an already-running issue a no-op / loud error.
        match dispatcher.dispatch(item.number) {
            Ok(true) => {
                report.dispatched += 1;
                occupancy += 1;
            }
            Ok(false) => {
                // Idempotency no-op: a sweep with the same key was already
                // running (label-flip lag). Count as in-flight, not a new
                // dispatch, and do not consume a capacity slot.
                report.skipped_in_flight += 1;
            }
            Err(e) => {
                report.errors += 1;
                log::warn!("work_finder: dispatch for issue #{} failed: {e}", item.number);
            }
        }
    }

    Ok(report)
}

/// Run one **multi-workspace** work-finder tick across N `(source, dispatcher)`
/// pairs — one per registered workspace (#3928) — sharing a **single global**
/// concurrency budget.
///
/// This is the multi-repo generalization of [`tick`]. Three properties are
/// load-bearing (and directly map to the issue's acceptance criteria):
///
/// 1. **Single global budget.** The occupancy seed is the *sum* of every
///    dispatcher's in-flight sweeps, and `occupancy` is incremented across
///    workspace boundaries, so the combined dispatches of all workspaces in one
///    tick never exceed `max_concurrent`. The token pool and scratch volume the
///    cap protects are machine-level, so the budget must be shared, not
///    replicated per repo.
/// 2. **Per-workspace error isolation.** A source (`list_ready_issues`) failure
///    for one workspace is logged and counted in [`TickReport::errors`], then the
///    loop **continues** to the next workspace — one repo's bad auth / deleted
///    remote / forge outage never blocks the others.
/// 3. **Empty-registry equivalence.** With a single workspace (the empty-registry
///    fallback), this reduces to the same schedule [`tick`] produces, so wiring
///    it in for N=1 preserves the pre-#3928 behavior.
///
/// # Per-repo main-health gate (#3930)
///
/// `halted` is a slice **parallel to `workspaces`**: `halted[i] == true` means
/// repo `i`'s `main` is currently red, so its own dispatch loop is skipped this
/// tick while sibling repos keep dispatching against the shared global budget. A
/// halted workspace's backlog is still polled and accumulated into
/// [`TickReport::seen`] (mirroring the pre-#3930 aggregate-log behavior), and its
/// in-flight sweeps still seed the shared occupancy (they are never touched). A
/// missing entry (`halted.len() < workspaces.len()`) defaults to *not halted*.
/// [`TickReport::halted`] is set when **any** workspace was gated this tick — in
/// the single-workspace (empty-registry) case this reduces to the pre-#3930
/// single-flag semantics byte-for-byte.
///
/// Unlike [`tick`], a source error is **not** propagated (there is no single
/// caller to retry — the other workspaces must still run); it is folded into the
/// returned [`TickReport`].
///
/// # Cross-repo priority ordering (#3946)
///
/// `priorities` is a slice **parallel to `workspaces`**: `priorities[i]` is
/// workspace `i`'s dispatch tier (lower = higher priority; a missing entry
/// defaults to [`crate::workspace_registry::DEFAULT_WORKSPACE_PRIORITY`]).
/// Rather than dispatching each workspace's backlog in registration order, this
/// gathers every eligible candidate across all workspaces into one queue, sorts
/// it by [`candidate_cmp`] — **(workspace priority asc, `loom:urgent` first,
/// issue age asc, number asc)** — and then fills the single shared concurrency
/// budget in that global order. So a deep, old product-repo backlog never
/// starves a small higher-priority tool repo: the tool repo's candidates are
/// dispatched first even though the product repo has older / more work. The
/// cap/budget mechanics are unchanged — this only orders the queue.
///
/// Strict priority is intentional (v1): a permanently-full higher tier starves
/// lower tiers. Fairness reservations are an explicit follow-up.
pub fn tick_multi<S: WorkSource, D: WorkDispatcher>(
    workspaces: &mut [(S, D)],
    priorities: &[u32],
    max_concurrent: usize,
    halted: &[bool],
) -> TickReport {
    use crate::workspace_registry::DEFAULT_WORKSPACE_PRIORITY;

    let mut report = TickReport::default();

    // Snapshot per-workspace in-flight sets *first* (immutable borrow) so the
    // global occupancy seed is the sum across all workspaces before any dispatch.
    let in_flights: Vec<HashSet<u32>> = workspaces.iter().map(|(_, d)| d.in_flight()).collect();
    let mut occupancy: usize = in_flights.iter().map(HashSet::len).sum();

    // Whether any workspace was gated this tick — folds down to the pre-#3930
    // single-flag semantics for a single (empty-registry) workspace.
    let mut any_halted = false;

    // Pass 1 (mutable source reads): gather every eligible candidate across all
    // workspaces, applying the per-workspace skip-label / in-flight filters and
    // the per-repo halt gate. Nothing is dispatched yet — ordering must be
    // decided globally, so dispatch happens in pass 2 after the sort.
    let mut candidates: Vec<PriorityCandidate> = Vec::new();
    for (idx, (source, _)) in workspaces.iter_mut().enumerate() {
        let ready = match source.list_ready_issues() {
            Ok(r) => r,
            Err(e) => {
                // Per-workspace isolation: log, count, and move on — the other
                // workspaces are still polled and dispatched this same tick.
                report.errors += 1;
                log::warn!("work_finder: listing ready issues for workspace #{idx} failed: {e}");
                continue;
            }
        };
        report.seen += ready.len();

        // Per-repo main-health gate (#3930): a red repo skips only its own
        // dispatch loop this tick. `seen` above still reflects its backlog so the
        // caller can log "backlog is N but halted"; its in-flight sweeps stay in
        // the global occupancy seed and are never touched.
        if halted.get(idx).copied().unwrap_or(false) {
            any_halted = true;
            continue;
        }

        let in_flight = &in_flights[idx];
        let workspace_priority = priorities.get(idx).copied().unwrap_or(DEFAULT_WORKSPACE_PRIORITY);

        for item in ready {
            if item.is_skipped() {
                report.skipped_labeled += 1;
                continue;
            }
            if in_flight.contains(&item.number) {
                report.skipped_in_flight += 1;
                continue;
            }
            candidates.push(PriorityCandidate {
                workspace_idx: idx,
                workspace_priority,
                urgent: item.is_urgent(),
                created_at: item.created_at,
                number: item.number,
            });
        }
    }

    // Global priority sort (#3946): (workspace priority, urgent, age, number).
    candidates.sort_by(candidate_cmp);

    // Pass 2 (mutable dispatcher calls): fill the single shared concurrency
    // budget in the sorted global order, routing each candidate back to its
    // owning workspace's dispatcher.
    for cand in candidates {
        // Shared global cap across all workspaces — defer once the combined
        // occupancy hits the budget, regardless of which workspace still has
        // ready items.
        if occupancy >= max_concurrent {
            report.deferred_capacity += 1;
            continue;
        }
        let dispatcher = &mut workspaces[cand.workspace_idx].1;
        match dispatcher.dispatch(cand.number) {
            Ok(true) => {
                report.dispatched += 1;
                occupancy += 1;
            }
            Ok(false) => {
                report.skipped_in_flight += 1;
            }
            Err(e) => {
                report.errors += 1;
                log::warn!("work_finder: dispatch for issue #{} failed: {e}", cand.number);
            }
        }
    }

    report.halted = any_halted;
    report
}

// ============================================================================
// Env-var configuration helpers
// ============================================================================

/// Whether the work-finder loop is enabled, per [`WORK_FINDER_ENABLE_ENV`].
///
/// Off by default (opt-in) — parsing mirrors
/// [`crate::epic_supervisor::supervisor_enabled`]. This is the **env-only**
/// primitive; the config-aware entry point the daemon actually uses is
/// [`resolve_enabled`] (precedence env > config > default).
#[must_use]
pub fn enabled() -> bool {
    std::env::var(WORK_FINDER_ENABLE_ENV).is_ok_and(|v| {
        matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    })
}

/// Env override for the tick interval — `None` when unset, zero, or
/// unparseable (a zero-interval busy loop is never useful).
fn env_interval_secs() -> Option<u64> {
    std::env::var(WORK_FINDER_INTERVAL_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
}

/// Env override for the max-concurrency ceiling — `None` when unset, zero, or
/// unparseable (a zero cap would dispatch nothing, defeating the loop).
fn env_max_concurrent() -> Option<usize> {
    std::env::var(WORK_FINDER_MAX_CONCURRENT_ENV)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
}

/// Resolve the tick interval from [`WORK_FINDER_INTERVAL_ENV`], falling back to
/// [`DEFAULT_WORK_FINDER_INTERVAL_SECS`]. A zero or unparseable value falls back
/// to the default (a zero-interval busy loop is never useful).
#[must_use]
pub fn resolve_interval() -> Duration {
    env_interval_secs()
        .map_or_else(|| Duration::from_secs(DEFAULT_WORK_FINDER_INTERVAL_SECS), Duration::from_secs)
}

/// Resolve the fixed max-concurrency cap from
/// [`WORK_FINDER_MAX_CONCURRENT_ENV`], falling back to
/// [`DEFAULT_WORK_FINDER_MAX_CONCURRENT`]. A zero or unparseable value falls
/// back to the default (a zero cap would dispatch nothing, defeating the loop).
#[must_use]
pub fn resolve_max_concurrent() -> usize {
    env_max_concurrent().unwrap_or(DEFAULT_WORK_FINDER_MAX_CONCURRENT)
}

// ============================================================================
// Config-file configuration (.loom/config.json → autonomous.workFinder)
// ============================================================================

/// The subset of `.loom/config.json → autonomous.workFinder` this module
/// consumes. Each field is `Option` so an absent key falls through to the
/// env-var / built-in-default resolution — the precedence is **env > config >
/// default** for every knob.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkFinderConfig {
    /// `autonomous.workFinder.enabled` — whether to run the loop at all.
    pub enabled: Option<bool>,
    /// `autonomous.workFinder.intervalSecs` — tick interval in seconds
    /// (a zero/invalid value is dropped to `None`).
    pub interval_secs: Option<u64>,
    /// `autonomous.workFinder.maxConcurrent` — the operator concurrency ceiling
    /// (a zero/invalid value is dropped to `None`).
    pub max_concurrent: Option<usize>,
}

/// Read `.loom/config.json → autonomous.workFinder`, soft-failing every field
/// to `None` (env/default resolution) on any of: missing file, malformed JSON,
/// or a missing `autonomous` / `workFinder` block.
///
/// Mirrors the soft-fail contract of
/// [`crate::main_health_gate::read_build_gate_config`] — a repo with no
/// `autonomous` block gets zero behavior change (env-only, exactly like today).
/// A zero or non-integer `intervalSecs` / `maxConcurrent` is treated as absent
/// so it falls through to the built-in default rather than a useless value.
#[must_use]
pub fn read_work_finder_config(repo_root: &Path) -> WorkFinderConfig {
    let config_path = repo_root.join(".loom").join("config.json");

    let config_str = match std::fs::read_to_string(&config_path) {
        Ok(s) => s,
        Err(e) => {
            log::debug!("work_finder: could not read config at {}: {e}", config_path.display());
            return WorkFinderConfig::default();
        }
    };

    let config: serde_json::Value = match serde_json::from_str(&config_str) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("work_finder: could not parse config at {}: {e}", config_path.display());
            return WorkFinderConfig::default();
        }
    };

    let Some(wf) = config.get("autonomous").and_then(|a| a.get("workFinder")) else {
        return WorkFinderConfig::default();
    };

    WorkFinderConfig {
        enabled: wf.get("enabled").and_then(serde_json::Value::as_bool),
        interval_secs: wf
            .get("intervalSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
        max_concurrent: wf
            .get("maxConcurrent")
            .and_then(serde_json::Value::as_u64)
            .filter(|&n| n > 0)
            .and_then(|n| usize::try_from(n).ok()),
    }
}

/// Resolve whether the loop is enabled with precedence **env > config >
/// default(false)**. When [`WORK_FINDER_ENABLE_ENV`] is *set* (to any value) it
/// decides (truthy enables, anything else disables); when unset the config
/// `enabled` flag decides; absent config leaves it off (opt-in, zero behavior
/// change).
#[must_use]
pub fn resolve_enabled(config: &WorkFinderConfig) -> bool {
    if let Ok(v) = std::env::var(WORK_FINDER_ENABLE_ENV) {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config.enabled.unwrap_or(false)
}

/// Resolve the tick interval with precedence **env > config > default**.
#[must_use]
pub fn resolve_interval_with_config(config: &WorkFinderConfig) -> Duration {
    env_interval_secs()
        .or(config.interval_secs)
        .map_or_else(|| Duration::from_secs(DEFAULT_WORK_FINDER_INTERVAL_SECS), Duration::from_secs)
}

/// Resolve the max-concurrency ceiling with precedence **env > config >
/// default**.
#[must_use]
pub fn resolve_max_concurrent_with_config(config: &WorkFinderConfig) -> usize {
    env_max_concurrent()
        .or(config.max_concurrent)
        .unwrap_or(DEFAULT_WORK_FINDER_MAX_CONCURRENT)
}

/// Compute the **work-driven dynamic concurrency cap** (Phase B, #3811):
/// `min(pool_size, disk_headroom, configured_max)`.
///
/// This is the total-concurrency ceiling for the loop, recomputed every tick
/// from live inputs. It deliberately does **not** fold in the backlog depth:
/// [`tick`] already bounds the *effective* per-tick concurrency to
/// `min(this_cap, backlog_depth)` by iterating the ready `loom:issue` rows and
/// deferring the remainder, and it compares the cap against the current live
/// sweep occupancy (`in_flight().len()`) — which counts already-dispatched
/// `loom:building` sweeps that are **not** in the ready backlog. Folding backlog
/// into the cap here would under-utilize the pool whenever prior-tick sweeps are
/// still running (a smaller "new work" number would cap total occupancy below
/// the pool/disk ceiling). Keeping the cap as `min(pool, disk, configured)` and
/// letting `tick` apply the backlog bound is what makes concurrency scale up
/// with the backlog and drain to zero when it empties.
///
/// The three bounds map directly to the resource each protects:
/// - `pool_size` — never over-subscribe a rotated OAuth account (one live sweep
///   per `.loom/tokens/*.token`).
/// - `disk_headroom` — never provision more worktrees than the scratch volume
///   can hold at `LOOM_PER_WORKTREE_GB` each.
/// - `configured_max` — the operator ceiling
///   (`LOOM_WORK_FINDER_MAX_CONCURRENT`), a hard upper bound regardless of how
///   much pool/disk headroom exists.
#[must_use]
pub fn resolve_dynamic_max_concurrent(
    pool_size: usize,
    disk_headroom: usize,
    configured_max: usize,
) -> usize {
    pool_size.min(disk_headroom).min(configured_max)
}

// ============================================================================
// Runtime wiring — the loop runs on the shared daemon runtime
// ============================================================================

/// Spawn the work-finder loop on the shared daemon runtime and return its task
/// handle so the daemon can keep it alive for the process lifetime.
///
/// Every `interval`, the task recomputes the **dynamic** concurrency cap
/// (Phase B, #3811) — `min(token-pool size, disk headroom, configured_max)` via
/// [`resolve_dynamic_max_concurrent`] — from live inputs read fresh under
/// `workspace_root`, then runs one [`tick`] with it. The cap is **not** captured
/// once at startup, so a pool that grows/shrinks (`loom-tokens bootstrap`), a
/// scratch volume that fills/frees, or a draining backlog are all honored
/// without a daemon restart. `configured_max` is the operator ceiling
/// (`LOOM_WORK_FINDER_MAX_CONCURRENT`).
///
/// Unlike the epic supervisor, no dedicated OS thread is needed:
/// [`SweepRegistry::dispatch`] returns promptly (fire-and-forget child spawn),
/// so the finder never parks a runtime worker in a minutes-long blocking call —
/// the same footing as the reaper task
/// ([`crate::sweep_registry::spawn_reaper_task`]). The per-tick disk probe shells
/// out to `df` briefly, which is negligible on the 60s default interval.
pub fn spawn_work_finder_task<S, D>(
    mut source: S,
    mut dispatcher: D,
    interval: Duration,
    workspace_root: PathBuf,
    configured_max: usize,
    health_state: Arc<MainHealthState>,
    event_bus: Arc<EventBus>,
) -> tokio::task::JoinHandle<()>
where
    S: WorkSource + Send + 'static,
    D: WorkDispatcher + Send + 'static,
{
    log::info!(
        "work_finder: starting loop (interval={}s, configured_max={configured_max}, \
         dynamic cap = min(healthy tokens, disk, configured_max))",
        interval.as_secs()
    );
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // If a tick's work (disk probe + dispatch) overruns the interval, measure
        // the next interval from when it finished rather than firing the missed
        // ticks back-to-back (#3885). Matches the main-health gate loop.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // First tick fires immediately; skip it so we don't churn at boot.
        ticker.tick().await;
        // Track the halt state across ticks so we log the halt/resume edges
        // once per halted period, not once per skipped tick.
        let mut was_halted = false;
        // Token-capacity pressure state (#3902), tracked across ticks so the
        // add-capacity advisory / recovery fires only on state change, never
        // every tick.
        let mut was_pressured = false;
        loop {
            ticker.tick().await;
            // Reactive main-health backstop (Phase C, #3812): skip all dispatch
            // while the gate reports a red `main`.
            let halted = health_state.is_halted();
            // Recompute the dynamic cap from live inputs every tick (Phase B),
            // now with token-capacity backpressure (#3902): the token axis is the
            // count of *healthy* accounts from the ranking, not the flat pool.
            let pool_size = token_pool_size(&workspace_root);
            let ranking = capacity::read_ranking(&workspace_root);
            let token_limit = ranking.as_ref().map_or(pool_size, |r| r.available);
            let disk = disk_headroom_limit(&workspace_root);
            let max_concurrent = resolve_dynamic_max_concurrent(token_limit, disk, configured_max);
            log::debug!(
                "work_finder: dynamic cap = {max_concurrent} (pool={pool_size}, \
                 healthy_tokens={token_limit}, disk={disk}, configured_max={configured_max}, \
                 halted={halted})"
            );
            match tick(&mut source, &mut dispatcher, max_concurrent, halted) {
                Ok(report) => {
                    if report.halted && !was_halted {
                        log::warn!(
                            "work_finder: main-health gate halted dispatch — {} ready issue(s) \
                             held until main is green again",
                            report.seen
                        );
                    } else if !report.halted && was_halted {
                        log::info!("work_finder: main-health gate cleared — resuming dispatch");
                    }
                    was_halted = report.halted;
                    if report.dispatched > 0 || report.errors > 0 {
                        log::info!(
                            "work_finder: tick — cap {max_concurrent} (pool={pool_size}, \
                             healthy={token_limit}, disk={disk}, ceiling={configured_max}); \
                             {} seen, {} dispatched, {} labeled-skip, {} in-flight-skip, \
                             {} deferred, {} error(s)",
                            report.seen,
                            report.dispatched,
                            report.skipped_labeled,
                            report.skipped_in_flight,
                            report.deferred_capacity,
                            report.errors
                        );
                    }
                    // Token-capacity advisory (#3902) — surface on state change.
                    // Skip while halted: a red-main halt defers everything, so the
                    // token axis is not the (relevant) bottleneck this tick.
                    if !report.halted {
                        let assessment = capacity::assess_pressure(
                            ranking.as_ref(),
                            pool_size,
                            token_limit,
                            disk,
                            configured_max,
                            report.deferred_capacity,
                            capacity::DEFAULT_ADVISORY_MIN_QUEUED,
                        );
                        was_pressured =
                            emit_capacity_transition(&event_bus, was_pressured, &assessment);
                    }
                }
                Err(e) => {
                    log::warn!("work_finder: tick failed to list ready issues: {e}");
                }
            }
        }
    })
}

/// Spawn the **multi-workspace** work-finder loop (#3928) on the shared daemon
/// runtime.
///
/// This is the multi-repo replacement for [`spawn_work_finder_task`]. Every
/// tick it:
///
/// 1. Re-reads the machine-level [`WorkspaceRegistry`] and resolves
///    [`effective_roots`](WorkspaceRegistry::effective_roots) against
///    `fallback_root` — an **empty** registry yields `vec![fallback_root]`
///    (today's single-workspace behavior); a populated one yields the registered
///    roots. Re-reading each tick means `loom-daemon workspace add|remove` is
///    hot-applied without a daemon restart.
/// 2. Builds one `(GhWorkSource, RegistryDispatcher)` pair per root — the source
///    scoped to that repo via [`GhWorkSource::for_root`], the dispatcher over
///    that root's own [`SweepRegistry`](crate::sweep_registry::SweepRegistry)
///    from the shared [`WorkspacePool`] (so each sweep spawns with `current_dir`
///    set to its own repo root, and `.loom/locks` / `.loom/logs` /
///    `.loom/sweep-checkpoint` are correctly scoped).
/// 3. Runs one [`tick_multi`] with the **single global** dynamic cap.
///
/// The dynamic cap inputs (token pool, disk headroom) are **machine-level**
/// resources, so they are probed once per tick from `fallback_root` (the
/// daemon's primary workspace) and the resulting cap is a single global budget
/// shared across every workspace — never replicated per repo.
///
/// # Known limitation (documented tradeoff, deferred to phase c #3929)
///
/// The event-bus `sweep.issue.{N}.*` topics are keyed by issue number only
/// (frozen taxonomy). Two repos that each have an open issue #N publish on the
/// same topic string. This is an accepted, documented limitation for phase b;
/// the `(repo, issue)` key that disambiguates them is phase c (#3929). No new
/// topic shape is introduced here (CLAUDE.md: "New topics require a follow-up
/// issue").
pub fn spawn_multi_work_finder_task(
    pool: Arc<WorkspacePool>,
    fallback_root: PathBuf,
    interval: Duration,
    configured_max: usize,
    health_states: Arc<WorkspaceHealthStates>,
    event_bus: Arc<EventBus>,
) -> tokio::task::JoinHandle<()> {
    log::info!(
        "work_finder: starting multi-workspace loop (interval={}s, configured_max={configured_max}, \
         dynamic cap = min(healthy tokens, disk, configured_max), global across workspaces)",
        interval.as_secs()
    );
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // First tick fires immediately; skip it so we don't churn at boot.
        ticker.tick().await;
        let mut was_halted = false;
        let mut was_pressured = false;
        loop {
            ticker.tick().await;

            // Dynamic cap from live *machine-level* inputs (one token pool, one
            // scratch volume) probed from the daemon's primary workspace.
            let pool_size = token_pool_size(&fallback_root);
            let ranking = capacity::read_ranking(&fallback_root);
            let token_limit = ranking.as_ref().map_or(pool_size, |r| r.available);
            let disk = disk_headroom_limit(&fallback_root);
            let max_concurrent = resolve_dynamic_max_concurrent(token_limit, disk, configured_max);

            // Resolve the current set of workspaces fresh each tick so registry
            // edits (add / remove / set-priority) are hot-applied.
            let registry = WorkspaceRegistry::load_default().unwrap_or_else(|e| {
                log::warn!("work_finder: could not load workspace registry ({e}); using cwd");
                WorkspaceRegistry::default()
            });
            let roots = registry.effective_roots(&fallback_root);

            // Per-repo priority tiers (#3946), parallel to `pairs`: lower = higher
            // priority. The empty-registry cwd fallback resolves to the default.
            let priorities: Vec<u32> = roots.iter().map(|r| registry.priority_of(r)).collect();

            // Per-repo main-health halt (#3930): look up each root's own gate
            // state, parallel to `pairs`. A red repo halts only its own dispatch.
            let halted: Vec<bool> = roots.iter().map(|r| health_states.is_halted(r)).collect();
            let any_halted = halted.iter().any(|&h| h);

            let mut pairs: Vec<(GhWorkSource, RegistryDispatcher)> = roots
                .iter()
                .map(|root| {
                    let registry = pool.get_or_provision(root);
                    (GhWorkSource::for_root(root), RegistryDispatcher::new(registry))
                })
                .collect();

            log::debug!(
                "work_finder: dynamic cap = {max_concurrent} (pool={pool_size}, \
                 healthy_tokens={token_limit}, disk={disk}, configured_max={configured_max}, \
                 any_halted={any_halted}, workspaces={}, priorities={priorities:?})",
                pairs.len()
            );

            let report = tick_multi(&mut pairs, &priorities, max_concurrent, &halted);

            if report.halted && !was_halted {
                log::warn!(
                    "work_finder: main-health gate halted dispatch for {} of {} repo(s) — \
                     their ready issues held until their main is green again",
                    halted.iter().filter(|&&h| h).count(),
                    halted.len()
                );
            } else if !report.halted && was_halted {
                log::info!("work_finder: main-health gate cleared — resuming dispatch");
            }
            was_halted = report.halted;

            if report.dispatched > 0 || report.errors > 0 {
                log::info!(
                    "work_finder: tick — cap {max_concurrent} (pool={pool_size}, \
                     healthy={token_limit}, disk={disk}, ceiling={configured_max}); {} workspace(s), \
                     {} seen, {} dispatched, {} labeled-skip, {} in-flight-skip, {} deferred, \
                     {} error(s)",
                    pairs.len(),
                    report.seen,
                    report.dispatched,
                    report.skipped_labeled,
                    report.skipped_in_flight,
                    report.deferred_capacity,
                    report.errors
                );
            }

            if !report.halted {
                let assessment = capacity::assess_pressure(
                    ranking.as_ref(),
                    pool_size,
                    token_limit,
                    disk,
                    configured_max,
                    report.deferred_capacity,
                    capacity::DEFAULT_ADVISORY_MIN_QUEUED,
                );
                was_pressured = emit_capacity_transition(&event_bus, was_pressured, &assessment);
            }
        }
    })
}

/// Emit the add-capacity advisory / recovery on a token-pressure **state
/// change** and return the new pressured state. A no-op (returns `was_pressured`
/// unchanged) when the state is stable, so the operator sees one advisory on the
/// way in and one recovery on the way out — never a per-tick stream (#3902).
///
/// Each transition is surfaced on all three operator channels required by the
/// issue: the daemon log, the `daemon.capacity.advisory` event-bus topic, and —
/// via the recomputed [`crate::types::CapacityReport`] — the daemon status view.
fn emit_capacity_transition(
    event_bus: &Arc<EventBus>,
    was_pressured: bool,
    assessment: &capacity::PressureAssessment,
) -> bool {
    if assessment.pressured && !was_pressured {
        let advisory = CapacityAdvisory::pressure(assessment);
        log::warn!("work_finder: {}", advisory.message);
        publish_capacity_advisory(event_bus, &advisory);
        true
    } else if !assessment.pressured && was_pressured {
        let advisory = CapacityAdvisory::recovery(assessment);
        log::info!("work_finder: {}", advisory.message);
        publish_capacity_advisory(event_bus, &advisory);
        false
    } else {
        was_pressured
    }
}

/// Publish a [`CapacityAdvisory`] on the `daemon.capacity.advisory` topic.
/// Fire-and-forget: a `NoSubscribers` result is logged at debug and ignored
/// (matching the daemon's other publish sites).
fn publish_capacity_advisory(event_bus: &Arc<EventBus>, advisory: &CapacityAdvisory) {
    let event = Event::CapacityAdvisory {
        pressured: advisory.pressured,
        queued: advisory.queued,
        healthy_accounts: advisory.healthy_accounts,
        exhausted_accounts: advisory.exhausted_accounts,
        total_accounts: advisory.total_accounts,
        estimated_drain_minutes: advisory.estimated_drain_minutes,
        message: advisory.message.clone(),
    };
    if let Err(e) = event_bus.publish(event) {
        log::debug!("work_finder: capacity advisory not delivered: {e}");
    }
}

// ============================================================================
// Concrete runtime adapters (forge-backed source + registry dispatcher)
// ============================================================================

/// Concrete [`WorkSource`] / [`WorkDispatcher`] implementations that wire the
/// finder to the live forge (`gh`) and the daemon's [`SweepRegistry`].
///
/// The pure [`tick`] logic above is exercised in tests via mocks; these
/// adapters are the runtime glue and shell out to `gh` / spawn children, so
/// they are not unit-tested directly (mirroring
/// [`crate::epic_supervisor::forge`]).
pub mod forge {
    use super::{WorkDispatcher, WorkItem, WorkSource};
    use crate::sweep_registry::SweepRegistry;
    use crate::types::{SweepKind, SweepState};
    use anyhow::{anyhow, Context, Result};
    use serde::Deserialize;
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};

    /// Minimal `gh issue list --json number,labels,createdAt` row.
    #[derive(Debug, Deserialize)]
    struct GhIssue {
        number: u32,
        #[serde(default)]
        labels: Vec<GhLabel>,
        /// Issue creation timestamp for age ordering (#3946). `#[serde(default)]`
        /// tolerates older `gh` output that omits it (the item then sorts by
        /// number as its age proxy).
        #[serde(rename = "createdAt", default)]
        created_at: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    struct GhLabel {
        name: String,
    }

    /// A forge-backed [`WorkSource`] that lists open `loom:issue` items via
    /// `gh`. Mirrors [`crate::epic_supervisor::forge::GhEpicSource`].
    pub struct GhWorkSource {
        gh_bin: PathBuf,
        repo: Option<String>,
        /// Working directory the `gh` query runs in. When set (multi-workspace
        /// fan-out, #3928) `gh` auto-detects the repo from that root's git
        /// remote, so each registered workspace is polled against its own repo
        /// without a single machine-global `LOOM_REPO`. `None` keeps today's
        /// behavior (inherit the daemon's cwd).
        cwd: Option<PathBuf>,
    }

    impl GhWorkSource {
        /// Construct a source using `gh` from `PATH`, honoring `LOOM_REPO` for
        /// the `--repo` flag when set.
        #[must_use]
        pub fn new() -> Self {
            Self {
                gh_bin: PathBuf::from("gh"),
                repo: std::env::var("LOOM_REPO").ok(),
                cwd: None,
            }
        }

        /// Construct a source scoped to a specific workspace `root` (#3928): the
        /// `gh` query runs with `current_dir(root)` so it targets that repo's own
        /// remote. `LOOM_REPO`, when set, is still honored as a machine-global
        /// `--repo` override (preserving the single-workspace behavior
        /// byte-for-byte); in a genuine multi-repo deployment it is left unset so
        /// each root's cwd selects its repo.
        #[must_use]
        pub fn for_root(root: &Path) -> Self {
            Self {
                gh_bin: PathBuf::from("gh"),
                repo: std::env::var("LOOM_REPO").ok(),
                cwd: Some(root.to_path_buf()),
            }
        }

        /// Override the `gh` binary path (for tests / non-standard installs).
        #[must_use]
        pub fn with_gh_bin(mut self, bin: PathBuf) -> Self {
            self.gh_bin = bin;
            self
        }
    }

    impl Default for GhWorkSource {
        fn default() -> Self {
            Self::new()
        }
    }

    impl WorkSource for GhWorkSource {
        fn list_ready_issues(&mut self) -> Result<Vec<WorkItem>> {
            let mut cmd = Command::new(&self.gh_bin);
            cmd.arg("issue")
                .arg("list")
                .arg("--label")
                .arg("loom:issue")
                .arg("--state")
                .arg("open")
                .arg("--limit")
                .arg("200")
                .arg("--json")
                .arg("number,labels,createdAt");
            if let Some(ref repo) = self.repo {
                cmd.arg("--repo").arg(repo);
            }
            if let Some(ref cwd) = self.cwd {
                cmd.current_dir(cwd);
            }
            cmd.stderr(Stdio::piped());
            let out = cmd
                .output()
                .with_context(|| format!("failed to invoke {}", self.gh_bin.display()))?;
            if !out.status.success() {
                return Err(anyhow!(
                    "gh issue list --label loom:issue failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            let rows: Vec<GhIssue> =
                serde_json::from_slice(&out.stdout).context("parse gh issue list JSON")?;
            Ok(rows
                .into_iter()
                .map(|r| {
                    WorkItem::with_created_at(
                        r.number,
                        r.labels.into_iter().map(|l| l.name).collect(),
                        r.created_at,
                    )
                })
                .collect())
        }
    }

    /// A concrete [`WorkDispatcher`] backed by the daemon [`SweepRegistry`].
    ///
    /// `dispatch()` calls the registry's own `dispatch()` — reusing its
    /// idempotency key, `mkdir`-atomic claim lock, and `loom:issue →
    /// loom:building` label flip — so the finder never reimplements the race
    /// guard. `in_flight()` reads the registry's `Running` / `Pending` entries.
    pub struct RegistryDispatcher {
        registry: Arc<Mutex<SweepRegistry>>,
    }

    impl RegistryDispatcher {
        /// Construct a dispatcher over the shared registry.
        #[must_use]
        pub fn new(registry: Arc<Mutex<SweepRegistry>>) -> Self {
            Self { registry }
        }
    }

    impl WorkDispatcher for RegistryDispatcher {
        fn in_flight(&self) -> HashSet<u32> {
            let mut reg = match self.registry.lock() {
                Ok(r) => r,
                Err(poisoned) => {
                    log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                    return HashSet::new();
                }
            };
            // Reap-on-read (Issue #3893): reconcile liveness before seeding
            // occupancy so a sweep whose child has exited does not over-count
            // against the concurrency budget and defer legitimate new dispatch.
            reg.reap_liveness();
            let mut set = HashSet::new();
            for state in [SweepState::Running, SweepState::Pending] {
                for info in reg.list(Some(&state)) {
                    if let SweepKind::Issue(n) = info.kind {
                        set.insert(n);
                    }
                }
            }
            set
        }

        fn dispatch(&mut self, issue: u32) -> Result<bool> {
            let mut reg = self
                .registry
                .lock()
                .map_err(|e| anyhow!("sweep registry mutex poisoned: {e}"))?;
            // Idempotency key + the registry's claim lock make a re-dispatch of
            // an already-running issue a no-op (`was_new = false`) or a loud
            // lock-collision error.
            let key = format!("workfinder-{issue}");
            let outcome = reg.dispatch(&SweepKind::Issue(issue), Some(key), None, None, None)?;
            Ok(outcome.was_new)
        }
    }
}

// Re-export the concrete adapters at the module root for ergonomic wiring.
pub use forge::{GhWorkSource, RegistryDispatcher};

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serial_test::serial;

    // ===================================================================
    // Mock source + dispatcher
    // ===================================================================

    /// A fake [`WorkSource`] returning a scripted sequence of results, one per
    /// `tick`. Each entry is either an `Ok(items)` or a forge `Err`.
    struct FakeSource {
        results: std::collections::VecDeque<Result<Vec<WorkItem>>>,
    }

    impl FakeSource {
        fn once(items: Vec<WorkItem>) -> Self {
            let mut results = std::collections::VecDeque::new();
            results.push_back(Ok(items));
            Self { results }
        }
    }

    impl WorkSource for FakeSource {
        fn list_ready_issues(&mut self) -> Result<Vec<WorkItem>> {
            self.results.pop_front().unwrap_or_else(|| Ok(Vec::new()))
        }
    }

    /// A recording [`WorkDispatcher`] with a configurable in-flight set.
    #[derive(Default)]
    struct RecordingDispatcher {
        dispatched: Vec<u32>,
        in_flight: HashSet<u32>,
        /// Issue numbers whose dispatch should report an idempotency no-op.
        noop_issues: HashSet<u32>,
        /// Issue numbers whose dispatch should error.
        fail_issues: HashSet<u32>,
    }

    impl WorkDispatcher for RecordingDispatcher {
        fn in_flight(&self) -> HashSet<u32> {
            self.in_flight.clone()
        }
        fn dispatch(&mut self, issue: u32) -> Result<bool> {
            if self.fail_issues.contains(&issue) {
                anyhow::bail!("forced dispatch failure for #{issue}");
            }
            self.dispatched.push(issue);
            Ok(!self.noop_issues.contains(&issue))
        }
    }

    fn issue(n: u32) -> WorkItem {
        WorkItem::new(n, vec!["loom:issue".to_string()])
    }

    // ===================================================================
    // tick — dispatch scheduling
    // ===================================================================

    #[test]
    fn test_tick_dispatches_up_to_cap() {
        // N=5 ready issues, cap K=2 → exactly 2 dispatched this tick, 3 deferred.
        let mut source = FakeSource::once((1..=5).map(issue).collect());
        let mut disp = RecordingDispatcher::default();
        let report = tick(&mut source, &mut disp, 2, false).unwrap();

        assert_eq!(report.seen, 5);
        assert_eq!(report.dispatched, 2);
        assert_eq!(report.deferred_capacity, 3);
        assert_eq!(report.errors, 0);
        assert_eq!(disp.dispatched, vec![1, 2]);
    }

    #[test]
    fn test_tick_all_dispatched_when_under_cap() {
        let mut source = FakeSource::once((1..=3).map(issue).collect());
        let mut disp = RecordingDispatcher::default();
        let report = tick(&mut source, &mut disp, 10, false).unwrap();

        assert_eq!(report.dispatched, 3);
        assert_eq!(report.deferred_capacity, 0);
        assert_eq!(disp.dispatched, vec![1, 2, 3]);
    }

    #[test]
    fn test_tick_existing_occupancy_counts_against_cap() {
        // 2 already in flight, cap 3 ⇒ only 1 slot free even though 4 ready.
        let mut source = FakeSource::once(vec![issue(10), issue(11), issue(12), issue(13)]);
        let mut disp = RecordingDispatcher {
            in_flight: HashSet::from([100, 101]),
            ..Default::default()
        };
        let report = tick(&mut source, &mut disp, 3, false).unwrap();

        assert_eq!(report.dispatched, 1);
        assert_eq!(report.deferred_capacity, 3);
        assert_eq!(disp.dispatched, vec![10]);
    }

    #[test]
    fn test_tick_skips_issue_already_in_registry() {
        // #7 is already in flight in the registry even though the source still
        // reports it as loom:issue (label-flip lag) — it must be skipped.
        let mut source = FakeSource::once(vec![issue(7), issue(8)]);
        let mut disp = RecordingDispatcher {
            in_flight: HashSet::from([7]),
            ..Default::default()
        };
        let report = tick(&mut source, &mut disp, 10, false).unwrap();

        assert_eq!(report.skipped_in_flight, 1);
        assert_eq!(report.dispatched, 1);
        assert_eq!(disp.dispatched, vec![8]);
    }

    #[test]
    fn test_tick_skips_skip_labeled_issues() {
        // Each SKIP_LABELS entry disqualifies a row even in the loom:issue list.
        let mut source = FakeSource::once(vec![
            WorkItem::new(1, vec!["loom:issue".into(), "loom:building".into()]),
            WorkItem::new(2, vec!["loom:issue".into(), "loom:blocked".into()]),
            WorkItem::new(3, vec!["loom:issue".into(), "loom:operator-only".into()]),
            issue(4),
        ]);
        let mut disp = RecordingDispatcher::default();
        let report = tick(&mut source, &mut disp, 10, false).unwrap();

        assert_eq!(report.skipped_labeled, 3);
        assert_eq!(report.dispatched, 1);
        assert_eq!(disp.dispatched, vec![4]);
    }

    #[test]
    fn test_tick_idempotency_noop_not_counted_as_dispatch() {
        // dispatch() returns Ok(false) (a sweep with the same key was already
        // running) — it must not count as a new dispatch nor consume a slot.
        let mut source = FakeSource::once(vec![issue(1), issue(2), issue(3)]);
        let mut disp = RecordingDispatcher {
            noop_issues: HashSet::from([1]),
            ..Default::default()
        };
        // Cap of 2: #1 is a no-op (frees its slot), so #2 AND #3 still dispatch.
        let report = tick(&mut source, &mut disp, 2, false).unwrap();

        assert_eq!(report.dispatched, 2, "only #2 and #3 are new dispatches");
        assert_eq!(report.skipped_in_flight, 1, "#1 was an idempotency no-op");
        assert_eq!(report.deferred_capacity, 0);
        assert_eq!(disp.dispatched, vec![1, 2, 3]);
    }

    #[test]
    fn test_tick_dispatch_error_is_non_fatal() {
        // #2 errors; the tick still dispatches #1 and #3 and reports 1 error.
        let mut source = FakeSource::once(vec![issue(1), issue(2), issue(3)]);
        let mut disp = RecordingDispatcher {
            fail_issues: HashSet::from([2]),
            ..Default::default()
        };
        let report = tick(&mut source, &mut disp, 10, false).unwrap();

        assert_eq!(report.dispatched, 2);
        assert_eq!(report.errors, 1);
        assert_eq!(disp.dispatched, vec![1, 3]);
    }

    #[test]
    fn test_tick_source_error_propagates_then_next_tick_succeeds() {
        // First tick's source errors; tick() returns Err (the loop logs it,
        // non-fatal). The second tick succeeds and dispatches normally.
        let mut results = std::collections::VecDeque::new();
        results.push_back(Err(anyhow::anyhow!("gh unavailable")));
        results.push_back(Ok(vec![issue(1), issue(2)]));
        let mut source = FakeSource { results };
        let mut disp = RecordingDispatcher::default();

        let first = tick(&mut source, &mut disp, 10, false);
        assert!(first.is_err(), "source error propagates out of the tick");
        assert_eq!(disp.dispatched.len(), 0, "no dispatch on the erroring tick");

        let second = tick(&mut source, &mut disp, 10, false).unwrap();
        assert_eq!(second.dispatched, 2, "the next tick proceeds normally");
        assert_eq!(disp.dispatched, vec![1, 2]);
    }

    #[test]
    fn test_tick_empty_ready_is_noop() {
        let mut source = FakeSource::once(vec![]);
        let mut disp = RecordingDispatcher::default();
        let report = tick(&mut source, &mut disp, 10, false).unwrap();
        assert_eq!(report, TickReport::default());
        assert!(disp.dispatched.is_empty());
    }

    // ===================================================================
    // tick — reactive main-health halt (Phase C, #3812)
    // ===================================================================

    #[test]
    fn test_tick_halted_dispatches_zero_with_backlog() {
        // A red `main` (halted=true) dispatches nothing even with ample capacity
        // and a full backlog; existing in-flight sweeps are untouched.
        let mut source = FakeSource::once((1..=5).map(issue).collect());
        let mut disp = RecordingDispatcher {
            in_flight: HashSet::from([100, 101]),
            ..Default::default()
        };
        let report = tick(&mut source, &mut disp, 10, true).unwrap();

        assert!(report.halted, "report must flag the halt");
        assert_eq!(report.seen, 5, "backlog is still observed");
        assert_eq!(report.dispatched, 0, "zero dispatch while halted");
        assert_eq!(report.deferred_capacity, 0);
        assert!(disp.dispatched.is_empty(), "no sweeps started while halted");
    }

    #[test]
    fn test_tick_resumes_dispatch_once_halt_cleared() {
        // Same source shape: halted ⇒ zero, then not halted ⇒ dispatches.
        let mut source = FakeSource::once((1..=3).map(issue).collect());
        let mut disp = RecordingDispatcher::default();
        let halted = tick(&mut source, &mut disp, 10, true).unwrap();
        assert!(halted.halted);
        assert_eq!(halted.dispatched, 0);
        assert!(disp.dispatched.is_empty());

        // Next tick with the halt cleared dispatches normally.
        let mut source = FakeSource::once((1..=3).map(issue).collect());
        let resumed = tick(&mut source, &mut disp, 10, false).unwrap();
        assert!(!resumed.halted);
        assert_eq!(resumed.dispatched, 3);
        assert_eq!(disp.dispatched, vec![1, 2, 3]);
    }

    // ===================================================================
    // tick_multi — multi-workspace fan-out (#3928)
    // ===================================================================

    fn err_source(msg: &'static str) -> FakeSource {
        let mut results = std::collections::VecDeque::new();
        results.push_back(Err(anyhow::anyhow!(msg)));
        FakeSource { results }
    }

    #[test]
    fn test_tick_multi_single_workspace_matches_single_tick() {
        // Empty-registry equivalence: one workspace behaves exactly like tick().
        let mut multi =
            vec![(FakeSource::once((1..=3).map(issue).collect()), RecordingDispatcher::default())];
        let report = tick_multi(&mut multi, &[],10, &[false]);
        assert_eq!(report.seen, 3);
        assert_eq!(report.dispatched, 3);
        assert_eq!(report.deferred_capacity, 0);
        assert_eq!(multi[0].1.dispatched, vec![1, 2, 3]);
    }

    #[test]
    fn test_tick_multi_routes_to_correct_workspace() {
        // Two workspaces, each with its own ready set — dispatch must route to
        // the matching dispatcher, not aggregate onto one.
        let mut multi = vec![
            (FakeSource::once(vec![issue(1), issue(2)]), RecordingDispatcher::default()),
            (FakeSource::once(vec![issue(10), issue(11)]), RecordingDispatcher::default()),
        ];
        let report = tick_multi(&mut multi, &[],10, &[false, false]);
        assert_eq!(report.seen, 4);
        assert_eq!(report.dispatched, 4);
        assert_eq!(multi[0].1.dispatched, vec![1, 2], "workspace 0 dispatches its own issues");
        assert_eq!(multi[1].1.dispatched, vec![10, 11], "workspace 1 dispatches its own issues");
    }

    #[test]
    fn test_tick_multi_shared_global_cap_across_workspaces() {
        // Cap 3 shared across TWO workspaces each holding 5 ready issues: the
        // COMBINED dispatch count is exactly 3, never 3-per-workspace (the token
        // pool / scratch volume are machine-level, so the budget is global).
        let mut multi = vec![
            (FakeSource::once((1..=5).map(issue).collect()), RecordingDispatcher::default()),
            (FakeSource::once((10..=14).map(issue).collect()), RecordingDispatcher::default()),
        ];
        let report = tick_multi(&mut multi, &[],3, &[false, false]);
        let total: usize = multi.iter().map(|(_, d)| d.dispatched.len()).sum();
        assert_eq!(report.dispatched, 3, "combined dispatch never exceeds the global cap");
        assert_eq!(total, 3, "sum of per-workspace dispatches equals the global cap");
        assert_eq!(report.deferred_capacity, 7, "the remaining 7 are deferred");
    }

    #[test]
    fn test_tick_multi_existing_occupancy_is_summed_globally() {
        // Workspace 0 already has 2 in-flight, workspace 1 has 1 in-flight;
        // global occupancy is 3, cap is 4 ⇒ only 1 free slot across both.
        let mut multi = vec![
            (
                FakeSource::once(vec![issue(1), issue(2)]),
                RecordingDispatcher {
                    in_flight: HashSet::from([100, 101]),
                    ..Default::default()
                },
            ),
            (
                FakeSource::once(vec![issue(10), issue(11)]),
                RecordingDispatcher {
                    in_flight: HashSet::from([200]),
                    ..Default::default()
                },
            ),
        ];
        let report = tick_multi(&mut multi, &[],4, &[false, false]);
        assert_eq!(report.dispatched, 1, "3 in-flight + cap 4 ⇒ 1 free slot globally");
        assert_eq!(report.deferred_capacity, 3);
        // The single free slot goes to the first workspace's first ready issue.
        assert_eq!(multi[0].1.dispatched, vec![1]);
        assert!(multi[1].1.dispatched.is_empty());
    }

    #[test]
    fn test_tick_multi_one_workspace_errors_others_proceed() {
        // Workspace 1's forge query fails; workspaces 0 and 2 must still be
        // polled and dispatched in the same tick (per-repo error isolation).
        let mut multi = vec![
            (FakeSource::once(vec![issue(1), issue(2)]), RecordingDispatcher::default()),
            (err_source("bad auth for repo B"), RecordingDispatcher::default()),
            (FakeSource::once(vec![issue(30)]), RecordingDispatcher::default()),
        ];
        let report = tick_multi(&mut multi, &[],10, &[false, false, false]);
        assert_eq!(report.errors, 1, "the failing workspace is counted, not fatal");
        assert_eq!(report.dispatched, 3, "the two healthy workspaces still dispatch");
        assert_eq!(multi[0].1.dispatched, vec![1, 2]);
        assert!(multi[1].1.dispatched.is_empty(), "the erroring workspace dispatched nothing");
        assert_eq!(multi[2].1.dispatched, vec![30]);
    }

    #[test]
    fn test_tick_multi_halted_dispatches_zero_across_workspaces() {
        let mut multi = vec![
            (FakeSource::once((1..=3).map(issue).collect()), RecordingDispatcher::default()),
            (FakeSource::once((10..=12).map(issue).collect()), RecordingDispatcher::default()),
        ];
        let report = tick_multi(&mut multi, &[],10, &[true, true]);
        assert!(report.halted);
        assert_eq!(report.seen, 6, "backlog across both workspaces is still observed");
        assert_eq!(report.dispatched, 0, "zero dispatch while halted");
        assert!(multi.iter().all(|(_, d)| d.dispatched.is_empty()));
    }

    #[test]
    fn test_tick_multi_empty_workspace_set_is_noop() {
        let mut multi: Vec<(FakeSource, RecordingDispatcher)> = vec![];
        let report = tick_multi(&mut multi, &[],10, &[]);
        assert_eq!(report, TickReport::default());
    }

    #[test]
    fn test_tick_multi_per_repo_gate_red_repo_halts_only_itself() {
        // Per-repo main-health gate (#3930): repo A (index 0) is red, repo B
        // (index 1) is green. A dispatches NOTHING; B still dispatches its full
        // backlog. A's backlog is still counted in `seen` for logging.
        let mut multi = vec![
            (FakeSource::once(vec![issue(1), issue(2)]), RecordingDispatcher::default()),
            (FakeSource::once(vec![issue(10), issue(11)]), RecordingDispatcher::default()),
        ];
        let report = tick_multi(&mut multi, &[],10, &[true, false]);
        assert!(report.halted, "report.halted is set when any repo was gated");
        assert_eq!(report.seen, 4, "both repos' backlogs are observed");
        assert_eq!(report.dispatched, 2, "only repo B dispatched");
        assert!(multi[0].1.dispatched.is_empty(), "red repo A dispatched nothing");
        assert_eq!(multi[1].1.dispatched, vec![10, 11], "green repo B dispatched its backlog");
    }

    #[test]
    fn test_tick_multi_per_repo_gate_other_repo_red_does_not_halt_us() {
        // Mirror image: repo A (index 0) is green, repo B (index 1) is red. A
        // dispatches; B does not. A red repo never halts a sibling.
        let mut multi = vec![
            (FakeSource::once(vec![issue(1), issue(2)]), RecordingDispatcher::default()),
            (FakeSource::once(vec![issue(10), issue(11)]), RecordingDispatcher::default()),
        ];
        let report = tick_multi(&mut multi, &[],10, &[false, true]);
        assert!(report.halted, "a gated sibling still sets report.halted");
        assert_eq!(report.dispatched, 2, "only repo A dispatched");
        assert_eq!(multi[0].1.dispatched, vec![1, 2], "green repo A dispatched its backlog");
        assert!(multi[1].1.dispatched.is_empty(), "red repo B dispatched nothing");
    }

    #[test]
    fn test_tick_multi_red_repo_in_flight_still_seeds_global_occupancy() {
        // A red repo's in-flight sweeps are never touched and still count toward
        // the shared global budget: repo A (red) has 3 in-flight, cap is 3, so the
        // green repo B gets ZERO free slots this tick even though A itself skips.
        let mut multi = vec![
            (
                FakeSource::once(vec![issue(1)]),
                RecordingDispatcher {
                    in_flight: HashSet::from([100, 101, 102]),
                    ..Default::default()
                },
            ),
            (FakeSource::once(vec![issue(10), issue(11)]), RecordingDispatcher::default()),
        ];
        let report = tick_multi(&mut multi, &[],3, &[true, false]);
        assert!(report.halted);
        assert_eq!(report.dispatched, 0, "occupancy already at cap from red repo's in-flight");
        assert_eq!(report.deferred_capacity, 2, "repo B's 2 ready issues deferred by the budget");
        assert!(multi.iter().all(|(_, d)| d.dispatched.is_empty()));
    }

    #[test]
    fn test_tick_multi_none_halted_is_not_reported_halted() {
        // No repo gated ⇒ report.halted is false (reduces to pre-#3930 semantics).
        let mut multi = vec![
            (FakeSource::once(vec![issue(1)]), RecordingDispatcher::default()),
            (FakeSource::once(vec![issue(10)]), RecordingDispatcher::default()),
        ];
        let report = tick_multi(&mut multi, &[],10, &[false, false]);
        assert!(!report.halted);
        assert_eq!(report.dispatched, 2);
    }

    #[test]
    fn test_tick_multi_missing_halt_entry_defaults_not_halted() {
        // A short `halted` slice (fewer entries than workspaces) treats the
        // unspecified workspaces as not halted rather than panicking.
        let mut multi = vec![
            (FakeSource::once(vec![issue(1)]), RecordingDispatcher::default()),
            (FakeSource::once(vec![issue(10)]), RecordingDispatcher::default()),
        ];
        let report = tick_multi(&mut multi, &[],10, &[true]);
        assert!(report.halted);
        assert_eq!(report.dispatched, 1, "workspace 1 (no halt entry) still dispatches");
        assert!(multi[0].1.dispatched.is_empty());
        assert_eq!(multi[1].1.dispatched, vec![10]);
    }

    // ===================================================================
    // tick_multi — cross-repo priority ordering (#3946)
    // ===================================================================

    fn issue_at(n: u32, created_at: &str) -> WorkItem {
        WorkItem::with_created_at(n, vec!["loom:issue".into()], Some(created_at.to_string()))
    }

    fn urgent_issue(n: u32) -> WorkItem {
        WorkItem::new(n, vec!["loom:issue".into(), URGENT_LABEL.into()])
    }

    #[test]
    fn test_tick_multi_higher_priority_repo_dispatches_first_under_cap() {
        // ACCEPTANCE (#3946): the LOWER-priority repo (index 0, priority 100) has
        // OLDER and MORE candidates than the HIGHER-priority repo (index 1,
        // priority 0). Under a global cap of 2, the higher-priority repo's
        // candidates MUST dispatch first anyway — a deep/old product backlog
        // never starves a small high-priority tool repo.
        let mut multi = vec![
            (
                // Low-priority repo: 4 candidates, all OLDER (2023 timestamps).
                FakeSource::once(vec![
                    issue_at(1, "2023-01-01T00:00:00Z"),
                    issue_at(2, "2023-01-02T00:00:00Z"),
                    issue_at(3, "2023-01-03T00:00:00Z"),
                    issue_at(4, "2023-01-04T00:00:00Z"),
                ]),
                RecordingDispatcher::default(),
            ),
            (
                // High-priority repo: 2 candidates, both NEWER (2025 timestamps).
                FakeSource::once(vec![
                    issue_at(50, "2025-01-01T00:00:00Z"),
                    issue_at(51, "2025-01-02T00:00:00Z"),
                ]),
                RecordingDispatcher::default(),
            ),
        ];
        // priorities parallel to workspaces: repo 0 = 100 (low), repo 1 = 0 (high).
        let report = tick_multi(&mut multi, &[100, 0], 2, &[false, false]);

        assert_eq!(report.dispatched, 2, "the global cap of 2 is filled");
        assert_eq!(report.deferred_capacity, 4, "the low-priority repo's 4 are deferred");
        assert!(
            multi[0].1.dispatched.is_empty(),
            "the low-priority repo dispatches NOTHING despite older/more candidates"
        );
        assert_eq!(
            multi[1].1.dispatched,
            vec![50, 51],
            "the high-priority repo's candidates dispatch first"
        );
    }

    #[test]
    fn test_tick_multi_urgent_beats_older_within_same_tier() {
        // Within one workspace-priority tier, `loom:urgent` sorts ahead of an
        // older / lower-numbered non-urgent sibling. Cap 1 ⇒ only the urgent one.
        let mut multi = vec![(
            FakeSource::once(vec![
                issue_at(1, "2023-01-01T00:00:00Z"), // older, non-urgent
                urgent_issue(9),                     // newer, urgent
            ]),
            RecordingDispatcher::default(),
        )];
        let report = tick_multi(&mut multi, &[100], 1, &[false]);
        assert_eq!(report.dispatched, 1);
        assert_eq!(
            multi[0].1.dispatched,
            vec![9],
            "the urgent issue outranks the older non-urgent one in the same tier"
        );
    }

    #[test]
    fn test_tick_multi_oldest_first_within_same_tier_and_urgency() {
        // Same tier, neither urgent: oldest-first by createdAt. Cap 1 ⇒ the
        // oldest (#7, 2022) dispatches before the newer (#2, 2024).
        let mut multi = vec![(
            FakeSource::once(vec![
                issue_at(2, "2024-06-01T00:00:00Z"),
                issue_at(7, "2022-06-01T00:00:00Z"),
            ]),
            RecordingDispatcher::default(),
        )];
        let report = tick_multi(&mut multi, &[100], 1, &[false]);
        assert_eq!(report.dispatched, 1);
        assert_eq!(multi[0].1.dispatched, vec![7], "the older issue dispatches first");
    }

    #[test]
    fn test_tick_multi_missing_priority_entry_defaults() {
        // A short `priorities` slice (fewer entries than workspaces) treats the
        // unspecified workspaces as the default tier rather than panicking. With
        // both at the default, ordering reduces to age/number.
        let mut multi = vec![
            (FakeSource::once(vec![issue(1)]), RecordingDispatcher::default()),
            (FakeSource::once(vec![issue(10)]), RecordingDispatcher::default()),
        ];
        let report = tick_multi(&mut multi, &[], 10, &[false, false]);
        assert_eq!(report.dispatched, 2);
        assert_eq!(multi[0].1.dispatched, vec![1]);
        assert_eq!(multi[1].1.dispatched, vec![10]);
    }

    #[test]
    fn test_candidate_cmp_ordering() {
        use std::cmp::Ordering;
        let mk = |idx, prio, urgent, created: Option<&str>, num| PriorityCandidate {
            workspace_idx: idx,
            workspace_priority: prio,
            urgent,
            created_at: created.map(str::to_string),
            number: num,
        };

        // Priority dominates everything else: prio 0 (urgent=false, newer) still
        // beats prio 100 (urgent=true, older).
        let high = mk(0, 0, false, Some("2025-01-01T00:00:00Z"), 999);
        let low = mk(1, 100, true, Some("2000-01-01T00:00:00Z"), 1);
        assert_eq!(candidate_cmp(&high, &low), Ordering::Less);

        // Same tier: urgent before non-urgent.
        let u = mk(0, 100, true, Some("2025-01-01T00:00:00Z"), 50);
        let n = mk(0, 100, false, Some("2000-01-01T00:00:00Z"), 1);
        assert_eq!(candidate_cmp(&u, &n), Ordering::Less);

        // Same tier + same urgency: oldest-first.
        let old = mk(0, 100, false, Some("2020-01-01T00:00:00Z"), 80);
        let new = mk(0, 100, false, Some("2024-01-01T00:00:00Z"), 2);
        assert_eq!(candidate_cmp(&old, &new), Ordering::Less);

        // A dated issue sorts before an undated one (Some < None).
        let dated = mk(0, 100, false, Some("2024-01-01T00:00:00Z"), 5);
        let undated = mk(0, 100, false, None, 4);
        assert_eq!(candidate_cmp(&dated, &undated), Ordering::Less);

        // Fully-tied keys fall through to the number tiebreak (lower first).
        let a = mk(0, 100, false, None, 3);
        let b = mk(0, 100, false, None, 8);
        assert_eq!(candidate_cmp(&a, &b), Ordering::Less);
    }

    // ===================================================================
    // WorkItem
    // ===================================================================

    #[test]
    fn test_work_item_is_urgent() {
        assert!(!issue(1).is_urgent());
        assert!(urgent_issue(1).is_urgent());
        assert!(WorkItem::new(1, vec!["loom:issue".into(), "loom:urgent".into()]).is_urgent());
    }

    #[test]
    fn test_work_item_is_skipped() {
        assert!(!issue(1).is_skipped());
        assert!(WorkItem::new(1, vec!["loom:building".into()]).is_skipped());
        assert!(WorkItem::new(1, vec!["loom:blocked".into()]).is_skipped());
        assert!(WorkItem::new(1, vec!["loom:operator-only".into()]).is_skipped());
        assert!(!WorkItem::new(1, vec!["loom:curated".into()]).is_skipped());
    }

    // ===================================================================
    // Env-var configuration
    // ===================================================================

    #[test]
    #[serial]
    fn test_enabled_off_by_default() {
        std::env::remove_var(WORK_FINDER_ENABLE_ENV);
        assert!(!enabled(), "unset ⇒ disabled (zero behavior change)");
    }

    #[test]
    #[serial]
    fn test_enabled_truthy_values() {
        for v in ["1", "true", "yes", "on", "TRUE", "On", " Yes "] {
            std::env::set_var(WORK_FINDER_ENABLE_ENV, v);
            assert!(enabled(), "{v:?} should enable");
        }
        std::env::remove_var(WORK_FINDER_ENABLE_ENV);
    }

    #[test]
    #[serial]
    fn test_enabled_falsy_values() {
        for v in ["0", "false", "no", "off", "", "maybe"] {
            std::env::set_var(WORK_FINDER_ENABLE_ENV, v);
            assert!(!enabled(), "{v:?} should not enable");
        }
        std::env::remove_var(WORK_FINDER_ENABLE_ENV);
    }

    #[test]
    #[serial]
    fn test_resolve_interval_default_and_override() {
        std::env::remove_var(WORK_FINDER_INTERVAL_ENV);
        assert_eq!(resolve_interval(), Duration::from_secs(DEFAULT_WORK_FINDER_INTERVAL_SECS));

        std::env::set_var(WORK_FINDER_INTERVAL_ENV, "120");
        assert_eq!(resolve_interval(), Duration::from_secs(120));

        // Zero and unparseable fall back to the default.
        std::env::set_var(WORK_FINDER_INTERVAL_ENV, "0");
        assert_eq!(resolve_interval(), Duration::from_secs(DEFAULT_WORK_FINDER_INTERVAL_SECS));
        std::env::set_var(WORK_FINDER_INTERVAL_ENV, "garbage");
        assert_eq!(resolve_interval(), Duration::from_secs(DEFAULT_WORK_FINDER_INTERVAL_SECS));
        std::env::remove_var(WORK_FINDER_INTERVAL_ENV);
    }

    #[test]
    #[serial]
    fn test_resolve_max_concurrent_default_and_override() {
        std::env::remove_var(WORK_FINDER_MAX_CONCURRENT_ENV);
        assert_eq!(resolve_max_concurrent(), DEFAULT_WORK_FINDER_MAX_CONCURRENT);

        std::env::set_var(WORK_FINDER_MAX_CONCURRENT_ENV, "7");
        assert_eq!(resolve_max_concurrent(), 7);

        // Zero and unparseable fall back to the default.
        std::env::set_var(WORK_FINDER_MAX_CONCURRENT_ENV, "0");
        assert_eq!(resolve_max_concurrent(), DEFAULT_WORK_FINDER_MAX_CONCURRENT);
        std::env::set_var(WORK_FINDER_MAX_CONCURRENT_ENV, "nope");
        assert_eq!(resolve_max_concurrent(), DEFAULT_WORK_FINDER_MAX_CONCURRENT);
        std::env::remove_var(WORK_FINDER_MAX_CONCURRENT_ENV);
    }

    // ===================================================================
    // resolve_dynamic_max_concurrent — Phase B work-driven policy (#3811)
    // ===================================================================

    #[test]
    fn test_dynamic_cap_is_min_of_three_inputs() {
        // Never exceeds any of the three bounds.
        assert_eq!(resolve_dynamic_max_concurrent(10, 10, 10), 10);
        assert_eq!(resolve_dynamic_max_concurrent(2, 9, 9), 2, "pool binds");
        assert_eq!(resolve_dynamic_max_concurrent(9, 3, 9), 3, "disk binds");
        assert_eq!(resolve_dynamic_max_concurrent(9, 9, 4), 4, "ceiling binds");
    }

    #[test]
    fn test_dynamic_cap_pool_size_bound_never_over_subscribes() {
        // With a large disk headroom and ceiling, the token-pool size is the
        // hard bound — the cap never exceeds the number of usable accounts.
        for pool in 0..=5 {
            assert_eq!(
                resolve_dynamic_max_concurrent(pool, 100, 100),
                pool,
                "cap must equal pool size {pool} when disk/ceiling are larger"
            );
        }
    }

    #[test]
    fn test_dynamic_cap_disk_headroom_bound() {
        // A nearly-full scratch volume (disk headroom 1) caps concurrency at 1
        // even with a big pool and high ceiling.
        assert_eq!(resolve_dynamic_max_concurrent(8, 1, 8), 1);
        // A full volume (0 headroom) drops the cap to 0 — dispatch nothing.
        assert_eq!(resolve_dynamic_max_concurrent(8, 0, 8), 0);
    }

    #[test]
    fn test_dynamic_cap_zero_pool_dispatches_nothing() {
        // No usable tokens ⇒ cap 0 ⇒ a subsequent tick dispatches nothing (the
        // spawn path would hard-fail EX_CONFIG anyway).
        let cap = resolve_dynamic_max_concurrent(0, 10, 10);
        assert_eq!(cap, 0);
        let mut source = FakeSource::once((1..=3).map(issue).collect());
        let mut disp = RecordingDispatcher::default();
        let report = tick(&mut source, &mut disp, cap, false).unwrap();
        assert_eq!(report.dispatched, 0);
        assert_eq!(report.deferred_capacity, 3);
        assert!(disp.dispatched.is_empty());
    }

    // ===================================================================
    // Dynamic cap composed with tick — scale-up / scale-to-zero (#3811)
    // ===================================================================

    #[test]
    fn test_scale_up_with_growing_backlog_bounded_by_dynamic_cap() {
        // Fixed resources: pool=4, disk=10, ceiling=10 ⇒ dynamic cap 4. As the
        // backlog grows tick-over-tick, effective concurrency scales up but is
        // bounded by the cap (min(cap, backlog)).
        let cap = resolve_dynamic_max_concurrent(4, 10, 10);
        assert_eq!(cap, 4);

        // Backlog 2 (< cap): all 2 dispatch, nothing deferred.
        let mut source = FakeSource::once((1..=2).map(issue).collect());
        let mut disp = RecordingDispatcher::default();
        let report = tick(&mut source, &mut disp, cap, false).unwrap();
        assert_eq!(report.dispatched, 2, "backlog 2 < cap 4 ⇒ 2 dispatched");
        assert_eq!(report.deferred_capacity, 0);

        // Backlog 6 (> cap): scales up to the cap (4), defers the surplus (2).
        let mut source = FakeSource::once((10..=15).map(issue).collect());
        let mut disp = RecordingDispatcher::default();
        let report = tick(&mut source, &mut disp, cap, false).unwrap();
        assert_eq!(report.dispatched, 4, "backlog 6 > cap 4 ⇒ scaled up to cap");
        assert_eq!(report.deferred_capacity, 2);
    }

    #[test]
    fn test_scale_to_zero_on_empty_backlog() {
        // Even with ample resources (cap 5), an empty backlog dispatches nothing
        // — no capacity is pre-reserved and no idle workers are spawned.
        let cap = resolve_dynamic_max_concurrent(5, 5, 5);
        assert_eq!(cap, 5);
        let mut source = FakeSource::once(vec![]);
        let mut disp = RecordingDispatcher::default();
        let report = tick(&mut source, &mut disp, cap, false).unwrap();
        assert_eq!(report, TickReport::default(), "empty backlog ⇒ zero activity");
        assert!(disp.dispatched.is_empty());
    }

    // ===================================================================
    // Config-file surface — read_work_finder_config soft-fail (#3813)
    // ===================================================================

    fn write_config(dir: &Path, body: &str) {
        let loom_dir = dir.join(".loom");
        std::fs::create_dir_all(&loom_dir).unwrap();
        std::fs::write(loom_dir.join("config.json"), body).unwrap();
    }

    #[test]
    fn test_config_missing_file_is_all_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_work_finder_config(tmp.path()), WorkFinderConfig::default());
    }

    #[test]
    fn test_config_malformed_json_is_all_none() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), "{not valid json");
        assert_eq!(read_work_finder_config(tmp.path()), WorkFinderConfig::default());
    }

    #[test]
    fn test_config_missing_autonomous_block_is_all_none() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"terminals": []}"#);
        assert_eq!(read_work_finder_config(tmp.path()), WorkFinderConfig::default());
    }

    #[test]
    fn test_config_missing_work_finder_block_is_all_none() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"mainHealthGate": {"enabled": true}}}"#);
        assert_eq!(read_work_finder_config(tmp.path()), WorkFinderConfig::default());
    }

    #[test]
    fn test_config_full_block_is_parsed() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous": {"workFinder": {"enabled": true, "intervalSecs": 90, "maxConcurrent": 5}}}"#,
        );
        assert_eq!(
            read_work_finder_config(tmp.path()),
            WorkFinderConfig {
                enabled: Some(true),
                interval_secs: Some(90),
                max_concurrent: Some(5),
            }
        );
    }

    #[test]
    fn test_config_enabled_false_is_disabled_flag() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"workFinder": {"enabled": false}}}"#);
        let cfg = read_work_finder_config(tmp.path());
        assert_eq!(cfg.enabled, Some(false));
        assert_eq!(cfg.interval_secs, None);
        assert_eq!(cfg.max_concurrent, None);
    }

    #[test]
    fn test_config_zero_interval_and_max_drop_to_none() {
        // A zero interval/max in config is treated as absent so it falls through
        // to the built-in default rather than a useless value.
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous": {"workFinder": {"enabled": true, "intervalSecs": 0, "maxConcurrent": 0}}}"#,
        );
        let cfg = read_work_finder_config(tmp.path());
        assert_eq!(cfg.enabled, Some(true));
        assert_eq!(cfg.interval_secs, None);
        assert_eq!(cfg.max_concurrent, None);
    }

    // ===================================================================
    // Config-file surface — resolve_* precedence env > config > default (#3813)
    // ===================================================================

    #[test]
    #[serial]
    fn test_resolve_enabled_precedence() {
        std::env::remove_var(WORK_FINDER_ENABLE_ENV);

        // Absent config + unset env ⇒ default off (zero behavior change).
        assert!(!resolve_enabled(&WorkFinderConfig::default()));

        // Config alone enables when env is unset.
        let on = WorkFinderConfig {
            enabled: Some(true),
            ..Default::default()
        };
        assert!(resolve_enabled(&on));
        let off = WorkFinderConfig {
            enabled: Some(false),
            ..Default::default()
        };
        assert!(!resolve_enabled(&off));

        // Env overrides config in both directions.
        std::env::set_var(WORK_FINDER_ENABLE_ENV, "1");
        assert!(resolve_enabled(&off), "env truthy overrides config=false");
        std::env::set_var(WORK_FINDER_ENABLE_ENV, "0");
        assert!(!resolve_enabled(&on), "env falsy overrides config=true");
        std::env::remove_var(WORK_FINDER_ENABLE_ENV);
    }

    #[test]
    #[serial]
    fn test_resolve_interval_with_config_precedence() {
        std::env::remove_var(WORK_FINDER_INTERVAL_ENV);

        // Default when neither env nor config set.
        assert_eq!(
            resolve_interval_with_config(&WorkFinderConfig::default()),
            Duration::from_secs(DEFAULT_WORK_FINDER_INTERVAL_SECS)
        );

        // Config used when env unset.
        let cfg = WorkFinderConfig {
            interval_secs: Some(120),
            ..Default::default()
        };
        assert_eq!(resolve_interval_with_config(&cfg), Duration::from_secs(120));

        // Env overrides config.
        std::env::set_var(WORK_FINDER_INTERVAL_ENV, "45");
        assert_eq!(resolve_interval_with_config(&cfg), Duration::from_secs(45));

        // A zero/garbage env value is ignored; config still wins over default.
        std::env::set_var(WORK_FINDER_INTERVAL_ENV, "0");
        assert_eq!(resolve_interval_with_config(&cfg), Duration::from_secs(120));
        std::env::set_var(WORK_FINDER_INTERVAL_ENV, "nope");
        assert_eq!(resolve_interval_with_config(&cfg), Duration::from_secs(120));
        std::env::remove_var(WORK_FINDER_INTERVAL_ENV);
    }

    #[test]
    #[serial]
    fn test_resolve_max_concurrent_with_config_precedence() {
        std::env::remove_var(WORK_FINDER_MAX_CONCURRENT_ENV);

        // Default when neither env nor config set.
        assert_eq!(
            resolve_max_concurrent_with_config(&WorkFinderConfig::default()),
            DEFAULT_WORK_FINDER_MAX_CONCURRENT
        );

        // Config used when env unset.
        let cfg = WorkFinderConfig {
            max_concurrent: Some(8),
            ..Default::default()
        };
        assert_eq!(resolve_max_concurrent_with_config(&cfg), 8);

        // Env overrides config.
        std::env::set_var(WORK_FINDER_MAX_CONCURRENT_ENV, "2");
        assert_eq!(resolve_max_concurrent_with_config(&cfg), 2);

        // A zero/garbage env value is ignored; config still wins over default.
        std::env::set_var(WORK_FINDER_MAX_CONCURRENT_ENV, "0");
        assert_eq!(resolve_max_concurrent_with_config(&cfg), 8);
        std::env::set_var(WORK_FINDER_MAX_CONCURRENT_ENV, "nope");
        assert_eq!(resolve_max_concurrent_with_config(&cfg), 8);
        std::env::remove_var(WORK_FINDER_MAX_CONCURRENT_ENV);
    }

    // ===================================================================
    // Token-capacity advisory transitions (#3902)
    // ===================================================================

    fn pressured_assessment() -> capacity::PressureAssessment {
        // token_limit 1 < disk 10, ceiling 10; 12 deferred ⇒ token-bound + pressured.
        let snap = capacity::RankingSnapshot {
            total: 7,
            available: 1,
            exhausted: 6,
            ..capacity::RankingSnapshot::default()
        };
        capacity::assess_pressure(
            Some(&snap),
            7,
            1,
            10,
            10,
            12,
            capacity::DEFAULT_ADVISORY_MIN_QUEUED,
        )
    }

    fn calm_assessment() -> capacity::PressureAssessment {
        // Nothing deferred ⇒ not pressured (healthy pool).
        let snap = capacity::RankingSnapshot {
            total: 7,
            available: 7,
            ..capacity::RankingSnapshot::default()
        };
        capacity::assess_pressure(
            Some(&snap),
            7,
            7,
            10,
            10,
            0,
            capacity::DEFAULT_ADVISORY_MIN_QUEUED,
        )
    }

    #[test]
    fn transition_enters_pressure_and_publishes_advisory() {
        let bus = Arc::new(EventBus::new());
        let mut sub = bus.subscribe(["daemon.capacity.advisory"]);
        let a = pressured_assessment();
        assert!(a.pressured);

        // Not previously pressured ⇒ transition fires, returns true.
        let now = emit_capacity_transition(&bus, false, &a);
        assert!(now, "entered pressured state");

        match sub.try_recv().expect("an advisory event was published") {
            Event::CapacityAdvisory {
                pressured,
                queued,
                healthy_accounts,
                message,
                ..
            } => {
                assert!(pressured);
                assert_eq!(queued, 12);
                assert_eq!(healthy_accounts, 1);
                assert!(message.contains("loom-tokens bootstrap"));
            }
            other => panic!("expected CapacityAdvisory, got {other:?}"),
        }
    }

    #[test]
    fn transition_is_deduplicated_while_pressure_persists() {
        let bus = Arc::new(EventBus::new());
        let mut sub = bus.subscribe(["daemon.capacity.advisory"]);
        let a = pressured_assessment();

        // Already pressured ⇒ no new event, state stays true.
        let now = emit_capacity_transition(&bus, true, &a);
        assert!(now);
        assert!(
            matches!(sub.try_recv(), Err(crate::event_bus::RecvError::Empty)),
            "no duplicate advisory while pressure persists"
        );
    }

    #[test]
    fn transition_recovers_and_publishes_symmetric_event() {
        let bus = Arc::new(EventBus::new());
        let mut sub = bus.subscribe(["daemon.capacity.advisory"]);
        let calm = calm_assessment();

        // Was pressured, now calm ⇒ recovery event, state returns to false.
        let now = emit_capacity_transition(&bus, true, &calm);
        assert!(!now, "left pressured state");

        match sub.try_recv().expect("a recovery event was published") {
            Event::CapacityAdvisory {
                pressured, message, ..
            } => {
                assert!(!pressured);
                assert!(message.contains("restored"));
            }
            other => panic!("expected CapacityAdvisory recovery, got {other:?}"),
        }
    }

    #[test]
    fn transition_stays_calm_when_never_pressured() {
        let bus = Arc::new(EventBus::new());
        let mut sub = bus.subscribe(["daemon.capacity.advisory"]);
        let calm = calm_assessment();

        let now = emit_capacity_transition(&bus, false, &calm);
        assert!(!now);
        assert!(
            matches!(sub.try_recv(), Err(crate::event_bus::RecvError::Empty)),
            "no event when staying calm"
        );
    }

    #[test]
    fn capacity_advisory_event_topic() {
        let ev = Event::CapacityAdvisory {
            pressured: true,
            queued: 3,
            healthy_accounts: 1,
            exhausted_accounts: 6,
            total_accounts: 7,
            estimated_drain_minutes: Some(90),
            message: "x".to_string(),
        };
        assert_eq!(ev.topic(), "daemon.capacity.advisory");
    }
}
