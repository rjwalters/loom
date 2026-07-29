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
//!    (Phase B, #3811; CPU/load term added in #3978): `min(token-pool size,
//!    disk headroom, cpu/load headroom, configured max)`. `dispatch()` already
//!    flips `loom:issue → loom:building`, acquires the per-issue `mkdir`-atomic
//!    claim lock, and spawns the rotated-token child.
//!
//! # Concurrency scaling (Phase B, #3811; CPU/load term #3978)
//!
//! Phase A resolved a single fixed cap once at daemon startup. Phase B replaces
//! it with a cap **recomputed every tick** by
//! [`resolve_dynamic_max_concurrent`] from live inputs — the token-pool size
//! ([`crate::tokens::token_pool_size`]), the worktree-root disk headroom
//! ([`crate::disk_headroom::disk_headroom_limit`]), the host's CPU/load
//! headroom ([`crate::cpu_headroom::cpu_headroom_limit`], #3978 — added because
//! the token/disk axes alone let a batch of resetting accounts push the cap up
//! regardless of how many concurrent `cargo build`s were already saturating the
//! host), and the operator ceiling (`LOOM_WORK_FINDER_MAX_CONCURRENT`,
//! repurposed from Phase A's fixed target into a *ceiling*). The effective
//! per-tick concurrency is then `min(dynamic_cap, backlog_depth)`: [`tick`]
//! iterates the ready `loom:issue` rows and stops at the cap, so concurrency
//! scales **up** as the backlog grows and drains to **zero** dispatches when
//! the queue is empty — all without a daemon restart, since
//! pool/disk/cpu/backlog are read fresh each tick.
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
use crate::cpu_headroom::{
    cpu_headroom_limit, resolve_est_cores_per_sweep, resolve_utilization_target,
};
use crate::disk_headroom::disk_headroom_limit;
use crate::event_bus::EventBus;
use crate::main_health_gate::{MainHealthState, WorkspaceHealthStates};
use crate::sweep_registry::OpenPrDispatchError;
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

/// Environment variable setting the **per-token concurrency factor** (#3947).
///
/// The dynamic cap's token axis is `healthy_tokens × per_token_concurrency`, not
/// the old implicit `healthy_tokens × 1`. A plan limit is a utilization-window
/// token bucket (not a session count), so a single healthy account can run
/// several concurrent sessions; this factor is how many. Precedence is the
/// standard **env > config (`autonomous.perTokenConcurrency`) > default**. A
/// zero / unparseable value is ignored (falls through to config/default).
pub const PER_TOKEN_CONCURRENCY_ENV: &str = "LOOM_PER_TOKEN_CONCURRENCY";

/// Default per-token concurrency factor (#3947). `2` — a conservative amount of
/// session-window stacking that roughly doubles throughput off a small healthy
/// set without pushing a single account near its concurrent-session ceiling. The
/// #3909 rotating spread still fills distinct accounts first, so stacking only
/// kicks in when concurrency demand exceeds the healthy-account count.
pub const DEFAULT_PER_TOKEN_CONCURRENCY: usize = 2;

/// Environment variable setting the **per-tick admission cap** (#4234, Gap 3 of
/// the #4231 decomposition).
///
/// [`resolve_dynamic_max_concurrent`] is a **live** ceiling — recomputed every
/// tick from the token pool, disk, and CPU/load axes — so it can *jump*
/// tick-to-tick (e.g. several exhausted token accounts resetting at once
/// raises the token axis from ~2 to ~14). Before #4234 a jump like that let a
/// single tick admit every newly-eligible candidate up to the new, larger cap
/// in one shot: `loadavg`/idle-fraction is a **lagging** signal sampled at
/// wave-*start*, so a burst admitted together all ramp their builds minutes
/// later, well after the tick that "safely" admitted them observed a
/// still-quiet host. This is the exact ramp-lag failure mode from the #4231
/// incident's second wave (host re-spiked at 01:41 after load had already
/// dropped to 8 — the admission had already happened by the time load caught
/// up). This knob bounds **how many *new* sweeps one tick may admit**,
/// independent of how large `max_concurrent` computes to that tick, forcing a
/// large jump to ramp up over several ticks instead of one — each subsequent
/// tick re-samples CPU/disk/token headroom fresh, so a ramp that turns out to
/// be too aggressive self-corrects within one interval
/// ([`DEFAULT_WORK_FINDER_INTERVAL_SECS`], default 60s) rather than in one
/// uncontrolled burst.
///
/// Precedence is the standard **env > config
/// (`autonomous.workFinder.maxAdmissionsPerTick`) > default**, resolved once at
/// daemon startup via [`read_work_finder_config`] → `config_resolver` — the
/// same startup-capture pattern as `cpuUtilizationTarget` / `estCoresPerSweep`
/// (#4032): the *inputs* (occupancy, live headroom) are re-read every tick, but
/// this *knob* takes effect only on daemon restart unless a future change
/// moves knob resolution into the tick loop itself. A zero/unparseable value is
/// dropped (falls through to config/default) — a zero cap would silently
/// freeze the loop at its current occupancy forever, which is a footgun, not a
/// deliberate "pause" (use the main-health-gate halt or a scheduled drain for
/// that instead).
pub const WORK_FINDER_MAX_ADMISSIONS_PER_TICK_ENV: &str =
    "LOOM_WORK_FINDER_MAX_ADMISSIONS_PER_TICK";

/// Default per-tick admission cap (#4234). `3` mirrors
/// [`DEFAULT_WORK_FINDER_MAX_CONCURRENT`] — the same conservative magnitude
/// that would have kept the #4231 6-way fan-out from admitting more than half
/// its sweeps in a single tick even if every other axis (token/disk/cpu) had
/// momentarily computed room for all six.
pub const DEFAULT_MAX_ADMISSIONS_PER_TICK: usize = 3;

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

    /// The set of issue numbers currently **quarantined** for repeated
    /// insta-crashing (Issue #3939). The finder skips these entirely — they are
    /// filtered out of the candidate list *before* the concurrency budget is
    /// filled, so a workspace whose only candidates are quarantined never
    /// reserves a shared dispatch slot (no cross-repo starvation).
    ///
    /// Defaults to empty so a dispatcher that does not model quarantine (e.g. a
    /// test fake) opts out with zero boilerplate.
    fn quarantined(&self) -> HashSet<u32> {
        HashSet::new()
    }

    /// Cumulative count of cross-host dispatch collisions this dispatcher's
    /// registry has observed (Issue #4085, Phase 0 of #4028) — dispatches whose
    /// pre-flip label read showed a peer host claimed the issue first. Read once
    /// per tick and surfaced on the per-tick summary line so an operator can
    /// watch the baseline collision rate. Defaults to `0` so a dispatcher that
    /// does not model collision detection (e.g. a test fake, or a registry with
    /// detection disabled) opts out with zero boilerplate.
    fn collisions(&self) -> u64 {
        0
    }

    /// The set of issue numbers a **peer host** has advertised as in-flight over
    /// the shared safehouse room and not yet expired (Issue #4028, Phase 1 soft
    /// claim). The finder skips these — treating a peer's soft claim as an
    /// additional TTL-bounded skip reason alongside [`SKIP_LABELS`] — so two
    /// daemons on a shared backlog collide far less than the non-atomic forge
    /// label flip alone permits. Defaults to empty so a dispatcher that does not
    /// model peer claims (a test fake, or a registry with `safehouse.enabled`
    /// false) opts out with zero boilerplate and **zero behavior change**.
    fn peer_claimed(&self) -> HashSet<u32> {
        HashSet::new()
    }

    /// Dispatch a build sweep for `issue`. Returns `true` when a **new** sweep
    /// was started, `false` when the dispatch was an idempotency no-op (a sweep
    /// with the same key was already running).
    ///
    /// # Errors
    ///
    /// Returns an error when the dispatch fails (e.g. a claim-lock collision).
    /// The caller logs and counts it; it is never fatal.
    fn dispatch(&mut self, issue: u32) -> Result<bool>;

    /// Count of in-flight sweeps that occupy the work-finder's concurrency
    /// budget (Issue #4003).
    ///
    /// Defaults to `in_flight().len()` — the pre-#4003 behavior — for
    /// dispatchers that don't model a startup-proof discount (e.g. test
    /// fakes). [`RegistryDispatcher`] overrides this to exclude a sweep that
    /// has been dispatched longer than its registry's startup-proof grace
    /// window with zero observed startup signal (no worktree, no checkpoint,
    /// no log output past the spawn header), so a wedged child frees its slot
    /// for a healthy queued sweep well before the (unchanged, 300s) startup
    /// watchdog would act. `in_flight()` itself — the dedup set used to skip
    /// an issue that already has a live sweep — is deliberately UNCHANGED by
    /// this: dedup safety comes from the registry's claim lock and the forge
    /// `loom:building` label, not from occupancy accounting, so discounting a
    /// wedged sweep here only ever lets a *different* queued issue take its
    /// slot.
    fn occupancy(&self) -> usize {
        self.in_flight().len()
    }
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
    /// Issues deferred to a future tick because the **per-tick admission cap**
    /// (#4234, `max_admissions_per_tick`) was reached, independent of
    /// `deferred_capacity` — this fires even when `max_concurrent` computes
    /// large enough to admit them (e.g. a token-axis jump), because the ramp
    /// cap deliberately smooths *how fast* new sweeps are admitted rather than
    /// how many may run concurrently. See [`WORK_FINDER_MAX_ADMISSIONS_PER_TICK_ENV`].
    pub deferred_ramp_cap: usize,
    /// Issues skipped because they are quarantined for repeated insta-crashing
    /// (Issue #3939). Filtered out before the concurrency budget is allocated, so
    /// a quarantined candidate never consumes a shared dispatch slot.
    pub skipped_quarantined: usize,
    /// Issues skipped because they already have an **open** linked PR (Issue
    /// #4123 open-PR dispatch guard). `dispatch()` refuses these with the typed
    /// [`OpenPrDispatchError`]; the finder attributes that refusal here rather
    /// than to [`errors`](Self::errors) so a duplicate-work skip is visible and
    /// distinct from a real dispatch failure. Every in-memory dedup signal dies
    /// with the parent sweep, so without this guard an issue whose approved PR is
    /// still open would be re-dispatched the moment its sweep exits.
    pub skipped_pr_open: usize,
    /// Issues skipped because a **peer host** advertised a live soft claim over
    /// the safehouse room (Issue #4028, Phase 1). Counted under its **own**
    /// distinct reason — never folded into [`collisions`](Self::collisions)
    /// (#4085's post-hoc collision *count*) or the label/in-flight skips — so an
    /// operator can see how many dispatches the soft claim actively prevented,
    /// separate from the collisions it did not. Always `0` when
    /// `safehouse.enabled` is false (the dispatcher's `peer_claimed()` is empty).
    pub skipped_peer_claim: usize,
    /// Dispatch attempts that returned an error (logged, non-fatal).
    pub errors: usize,
    /// Cumulative cross-host dispatch collisions observed (Issue #4085, Phase 0
    /// of #4028). Unlike the other counters — which are per-tick tallies — this
    /// is a **monotonic total** read from the dispatcher(s) at tick end, so an
    /// operator watching successive summary lines sees the baseline collision
    /// rate accumulate. Always `0` unless collision detection is enabled
    /// (`LOOM_DETECT_COLLISIONS` / `autonomous.collisionDetection.enabled`).
    pub collisions: u64,
    /// True when at least one workspace was gated this tick because the
    /// main-health gate (Phase C, #3812) had halted its dispatch (`main` was
    /// **verified** red — see [`crate::main_health_gate::GateOutcome`]). `seen`
    /// still reflects the backlog depth of the halted repo(s).
    ///
    /// Derived directly from the shared
    /// [`WorkspaceHealthStates`](crate::main_health_gate::WorkspaceHealthStates)
    /// flags the gate writes (#3974 AC3), so this can never disagree with what
    /// the gate loop reports — including when a repo's forge query fails.
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
///
/// Unlimited-admission convenience wrapper over
/// [`tick_with_admission_cap`] — callers that don't need the #4234 per-tick
/// ramp cap (most existing tests, and any caller predating #4234) get
/// byte-for-byte the pre-#4234 behavior.
pub fn tick(
    source: &mut impl WorkSource,
    dispatcher: &mut impl WorkDispatcher,
    max_concurrent: usize,
    halted: bool,
) -> Result<TickReport> {
    tick_with_admission_cap(source, dispatcher, max_concurrent, halted, usize::MAX)
}

/// Like [`tick`], but additionally bounds how many **new** sweeps this single
/// tick may admit to `max_admissions_per_tick`, independent of
/// `max_concurrent` (#4234, Gap 3 of the #4231 decomposition — see
/// [`WORK_FINDER_MAX_ADMISSIONS_PER_TICK_ENV`] for the full ramp-lag
/// rationale). `usize::MAX` (what [`tick`] passes) disables the ramp cap
/// entirely, reducing to the pre-#4234 behavior.
///
/// The two caps are independent and both apply: `occupancy >= max_concurrent`
/// defers to [`TickReport::deferred_capacity`] (the existing concurrency
/// ceiling); a *separate* `admitted_this_tick >= max_admissions_per_tick`
/// defers to [`TickReport::deferred_ramp_cap`] (the new ramp limiter) even
/// when `max_concurrent` still has room. Both checks run every candidate, so a
/// tick can produce both kinds of deferral in the same pass.
///
/// # Errors
///
/// Same as [`tick`].
pub fn tick_with_admission_cap(
    source: &mut impl WorkSource,
    dispatcher: &mut impl WorkDispatcher,
    max_concurrent: usize,
    halted: bool,
    max_admissions_per_tick: usize,
) -> Result<TickReport> {
    let ready = source.list_ready_issues()?;
    let mut report = TickReport {
        seen: ready.len(),
        ..TickReport::default()
    };

    // Reactive backstop: a red `main` halts all new dispatch this tick.
    if halted {
        report.halted = true;
        // Surface the running collision baseline even on a halted tick (#4085) —
        // no dispatch happens, so the total is just carried forward.
        report.collisions = dispatcher.collisions();
        return Ok(report);
    }

    let in_flight = dispatcher.in_flight();
    let quarantined = dispatcher.quarantined();
    let peer_claimed = dispatcher.peer_claimed();
    // Occupancy (Issue #4003) is a distinct, possibly-smaller count than
    // `in_flight.len()`: a dispatcher may discount a spawned-but-unproven
    // sweep past its startup-proof grace window. `in_flight` itself stays the
    // full dedup set for the `contains()` check below.
    let mut occupancy = dispatcher.occupancy();
    // Ramp-admission counter (#4234): distinct from `occupancy` — this counts
    // only sweeps admitted *this tick*, reset every call, whereas `occupancy`
    // carries forward prior ticks' still-running sweeps.
    let mut admitted_this_tick: usize = 0;

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
        // 2b. Insta-crash quarantine (#3939): skip a repeatedly-insta-crashing
        //     issue rather than re-dispatching it every tick. Checked before the
        //     capacity gate so a quarantined issue never consumes a slot.
        if quarantined.contains(&item.number) {
            report.skipped_quarantined += 1;
            continue;
        }
        // 2c. Peer soft claim (#4028): a peer host advertised a live claim over
        //     the safehouse room. Back off — treated like a skip label, before
        //     the capacity gate so a peer-claimed issue never reserves a slot.
        if peer_claimed.contains(&item.number) {
            report.skipped_peer_claim += 1;
            log::info!(
                "work_finder: skipping issue #{} — a peer host advertised a soft claim \
                 over safehouse (#4028)",
                item.number
            );
            continue;
        }
        // 3. Fixed concurrency cap — defer the rest to a future tick.
        if occupancy >= max_concurrent {
            report.deferred_capacity += 1;
            continue;
        }
        // 3b. Per-tick admission (ramp) cap (#4234) — independent of the
        //     concurrency cap above: even when `max_concurrent` has room, this
        //     tick may not admit more than `max_admissions_per_tick` *new*
        //     sweeps, so a sudden jump in the concurrency cap ramps up over
        //     several ticks instead of bursting in one.
        if admitted_this_tick >= max_admissions_per_tick {
            report.deferred_ramp_cap += 1;
            continue;
        }
        // 4. Dispatch. The registry's idempotency key + claim lock make a
        //    double-dispatch of an already-running issue a no-op / loud error.
        match dispatcher.dispatch(item.number) {
            Ok(true) => {
                report.dispatched += 1;
                occupancy += 1;
                admitted_this_tick += 1;
            }
            Ok(false) => {
                // Idempotency no-op: a sweep with the same key was already
                // running (label-flip lag). Count as in-flight, not a new
                // dispatch, and do not consume a capacity slot.
                report.skipped_in_flight += 1;
            }
            Err(e) => {
                // Open-PR guard refusal (#4123) is a *skip*, not a failure:
                // attribute it to its own counter so it stays visible and
                // distinct from a real dispatch error. Typed downcast, never a
                // string match.
                if e.downcast_ref::<OpenPrDispatchError>().is_some() {
                    report.skipped_pr_open += 1;
                    log::info!(
                        "work_finder: skipping issue #{} — it already has an open linked PR \
                         (#4123 open-PR guard)",
                        item.number
                    );
                } else {
                    report.errors += 1;
                    log::warn!("work_finder: dispatch for issue #{} failed: {e}", item.number);
                }
            }
        }
    }

    // Read the cumulative cross-host collision total AFTER the dispatch loop so
    // any collision recorded during this tick's dispatches is included (#4085).
    report.collisions = dispatcher.collisions();

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
///    dispatcher's own [`WorkDispatcher::occupancy`] count (Issue #4003: this
///    may discount a spawned-but-not-yet-proven-started sweep, so it can be
///    smaller than the sum of `in_flight().len()`), and `occupancy` is
///    incremented across workspace boundaries, so the combined dispatches of
///    all workspaces in one tick never exceed `max_concurrent`. The token pool
///    and scratch volume the cap protects are machine-level, so the budget
///    must be shared, not
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
/// Compute, per root (parallel to `roots`), whether the work-finder should hold
/// new dispatch off that root this tick — the `halted` slice `tick_multi`
/// consumes.
///
/// A root is held when its `main` is verified-red (`is_halted`, #3930) **or**
/// (#4084) a build-gate run against it is currently in flight and the
/// `suppress_dispatch_during_gate` knob is on — the latter keeps a fresh sweep
/// build from racing the gate's own build for cores. The suppression is strictly
/// **per root**: a sibling root with no gate in flight is never held on account
/// of another root's gate run, preserving the #3930 per-repo isolation contract.
/// With `suppress_dispatch_during_gate = false` the in-flight term drops out
/// entirely, so the result is byte-for-byte the pre-#4084 `is_halted`-only vector.
#[must_use]
pub fn dispatch_held_per_root(
    health_states: &WorkspaceHealthStates,
    roots: &[std::path::PathBuf],
    suppress_dispatch_during_gate: bool,
) -> Vec<bool> {
    roots
        .iter()
        .map(|r| {
            health_states.is_halted(r)
                || (suppress_dispatch_during_gate && health_states.is_gate_in_flight(r))
        })
        .collect()
}

/// Fold the daemon-global scheduled-drain flag (#4090) on top of the per-root
/// dispatch holds computed by [`dispatch_held_per_root`] (#3930 verified-red +
/// #4084 gate-in-flight).
///
/// A scheduled drain is daemon-global: it pauses new dispatch in EVERY repo at
/// once, so `draining` is OR'd onto every root's per-root hold. The two terms
/// are fully independent: a drain holds every root regardless of its gate
/// state, and a gate in flight holds its own root regardless of drain state.
/// With `draining = false` the result is byte-for-byte `dispatch_held_per_root`.
#[must_use]
pub fn dispatch_held_per_root_with_drain(
    health_states: &WorkspaceHealthStates,
    roots: &[std::path::PathBuf],
    suppress_dispatch_during_gate: bool,
    draining: bool,
) -> Vec<bool> {
    dispatch_held_per_root(health_states, roots, suppress_dispatch_during_gate)
        .into_iter()
        .map(|h| h || draining)
        .collect()
}

/// Unlimited-admission convenience wrapper over
/// [`tick_multi_with_admission_cap`] — see [`tick`] / [`tick_with_admission_cap`]
/// for the single-workspace analogue and the #4234 rationale.
pub fn tick_multi<S: WorkSource, D: WorkDispatcher>(
    workspaces: &mut [(S, D)],
    priorities: &[u32],
    max_concurrent: usize,
    halted: &[bool],
) -> TickReport {
    tick_multi_with_admission_cap(workspaces, priorities, max_concurrent, halted, usize::MAX)
}

/// Like [`tick_multi`], but additionally bounds how many **new** sweeps this
/// single tick may admit — **across every workspace, one shared counter** —
/// to `max_admissions_per_tick` (#4234). Mirrors
/// [`tick_with_admission_cap`]'s two-independent-caps design: the existing
/// shared `max_concurrent` budget and the new ramp cap both apply, and either
/// alone can defer a candidate. `usize::MAX` (what [`tick_multi`] passes)
/// disables the ramp cap, reducing to the pre-#4234 behavior.
pub fn tick_multi_with_admission_cap<S: WorkSource, D: WorkDispatcher>(
    workspaces: &mut [(S, D)],
    priorities: &[u32],
    max_concurrent: usize,
    halted: &[bool],
    max_admissions_per_tick: usize,
) -> TickReport {
    use crate::workspace_registry::DEFAULT_WORKSPACE_PRIORITY;

    let mut report = TickReport::default();

    // Snapshot per-workspace in-flight sets *first* (immutable borrow) so the
    // dedup filtering below always has the full in-flight view.
    let in_flights: Vec<HashSet<u32>> = workspaces.iter().map(|(_, d)| d.in_flight()).collect();
    // The global occupancy seed (Issue #4003) is the sum of each dispatcher's
    // OWN occupancy count, which may discount a spawned-but-unproven sweep —
    // distinct from (and never larger than) the in-flight dedup sets above.
    let mut occupancy: usize = workspaces.iter().map(|(_, d)| d.occupancy()).sum();

    // Snapshot each workspace's quarantined set (#3939) alongside its in-flight
    // set. Quarantined candidates are dropped in pass 1 *before* the global sort
    // and slot fill, so a workspace whose only candidates are quarantined never
    // reserves a shared dispatch slot — its slots go to healthy sibling work.
    let quarantined_sets: Vec<HashSet<u32>> =
        workspaces.iter().map(|(_, d)| d.quarantined()).collect();

    // Snapshot each workspace's peer-claim set (#4028) alongside its quarantined
    // set. A peer's live soft claim drops the candidate in pass 1, before the
    // global sort and slot fill, so a peer-claimed issue never reserves a shared
    // dispatch slot.
    let peer_claimed_sets: Vec<HashSet<u32>> =
        workspaces.iter().map(|(_, d)| d.peer_claimed()).collect();

    // Whether any workspace was gated this tick, derived **directly from the
    // shared per-repo halt flags** rather than accumulated as a side effect of
    // the candidate-gathering loop (#3974 AC3).
    //
    // The loop below `continue`s on a `list_ready_issues` error *before* it
    // reaches the halt check, so the old accumulate-in-loop form reported
    // `halted = false` for a repo that was in fact halted whenever the forge
    // query failed. During the 2026-07-26 incident `gh` was dead in the
    // daemon's process tree, so the listing failed every tick and the finder
    // logged "main-health gate cleared — resuming dispatch" in the same window
    // the gate was logging "still RED". Reading the flags directly means the
    // two loops can never disagree: this is the same `WorkspaceHealthStates`
    // the gate writes.
    let any_halted = halted.iter().take(workspaces.len()).any(|&h| h);

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
            continue;
        }

        let in_flight = &in_flights[idx];
        let workspace_priority = priorities
            .get(idx)
            .copied()
            .unwrap_or(DEFAULT_WORKSPACE_PRIORITY);

        for item in ready {
            if item.is_skipped() {
                report.skipped_labeled += 1;
                continue;
            }
            if in_flight.contains(&item.number) {
                report.skipped_in_flight += 1;
                continue;
            }
            // Insta-crash quarantine (#3939): drop before the candidate ever
            // enters the global queue, so it consumes no shared slot.
            if quarantined_sets[idx].contains(&item.number) {
                report.skipped_quarantined += 1;
                continue;
            }
            // Peer soft claim (#4028): a peer host is already building it — drop
            // before the global queue so it consumes no shared slot.
            if peer_claimed_sets[idx].contains(&item.number) {
                report.skipped_peer_claim += 1;
                log::info!(
                    "work_finder: skipping issue #{} — a peer host advertised a soft claim \
                     over safehouse (#4028)",
                    item.number
                );
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

    // Ramp-admission counter (#4234), shared across every workspace exactly
    // like `occupancy` — see `tick_with_admission_cap`'s single-workspace
    // analogue for the full rationale.
    let mut admitted_this_tick: usize = 0;

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
        // Shared global ramp cap (#4234) — independent of the concurrency cap
        // above; see `tick_with_admission_cap`.
        if admitted_this_tick >= max_admissions_per_tick {
            report.deferred_ramp_cap += 1;
            continue;
        }
        let dispatcher = &mut workspaces[cand.workspace_idx].1;
        match dispatcher.dispatch(cand.number) {
            Ok(true) => {
                report.dispatched += 1;
                occupancy += 1;
                admitted_this_tick += 1;
            }
            Ok(false) => {
                report.skipped_in_flight += 1;
            }
            Err(e) => {
                // Open-PR guard refusal (#4123) — see the single-workspace
                // `tick` for the rationale. A skip, not a failure.
                if e.downcast_ref::<OpenPrDispatchError>().is_some() {
                    report.skipped_pr_open += 1;
                    log::info!(
                        "work_finder: skipping issue #{} — it already has an open linked PR \
                         (#4123 open-PR guard)",
                        cand.number
                    );
                } else {
                    report.errors += 1;
                    log::warn!("work_finder: dispatch for issue #{} failed: {e}", cand.number);
                }
            }
        }
    }

    report.halted = any_halted;
    // Sum the cumulative cross-host collision totals across every workspace's
    // dispatcher (#4085), read after pass 2 so this tick's collisions count.
    report.collisions = workspaces.iter().map(|(_, d)| d.collisions()).sum();
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
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorkFinderConfig {
    /// `autonomous.workFinder.enabled` — whether to run the loop at all.
    pub enabled: Option<bool>,
    /// `autonomous.workFinder.intervalSecs` — tick interval in seconds
    /// (a zero/invalid value is dropped to `None`).
    pub interval_secs: Option<u64>,
    /// `autonomous.workFinder.maxConcurrent` — the operator concurrency ceiling
    /// (a zero/invalid value is dropped to `None`).
    pub max_concurrent: Option<usize>,
    /// `autonomous.workFinder.maxAdmissionsPerTick` — the per-tick ramp
    /// admission cap (#4234; a zero/invalid value is dropped to `None`). See
    /// [`WORK_FINDER_MAX_ADMISSIONS_PER_TICK_ENV`] for the full rationale.
    pub max_admissions_per_tick: Option<usize>,
    /// `autonomous.perTokenConcurrency` — how many concurrent sweeps to allow per
    /// *healthy* token in the dynamic cap (#3947). Note this lives at the
    /// `autonomous` level (not under `workFinder`), so it is read even when no
    /// `workFinder` block is present. A zero/invalid value is dropped to `None`.
    pub per_token_concurrency: Option<usize>,
    /// `autonomous.cpuUtilizationTarget` — the fraction of logical CPUs the CPU
    /// headroom term (#3978) is willing to dedicate to sweep work (#4032). Also
    /// lives at the `autonomous` level, alongside `perTokenConcurrency`. A
    /// value outside `(0, 1]` (or the wrong JSON type) is dropped to `None`, so
    /// it falls through to [`crate::cpu_headroom::DEFAULT_UTILIZATION_TARGET`].
    pub cpu_utilization_target: Option<f64>,
    /// `autonomous.estCoresPerSweep` — the estimated CPU cores a single
    /// concurrent sweep consumes while building/testing (#3978, #4032). A
    /// value `<= 0` (or the wrong JSON type) is dropped to `None`, so it falls
    /// through to [`crate::cpu_headroom::DEFAULT_EST_CORES_PER_SWEEP`].
    pub est_cores_per_sweep: Option<f64>,
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
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(autonomous) = crate::config_resolver::get_path(&effective, "autonomous") else {
        return WorkFinderConfig::default();
    };

    // `perTokenConcurrency` lives at the `autonomous` level (#3947), so it is
    // read even when the `workFinder` block is absent.
    let per_token_concurrency = autonomous
        .get("perTokenConcurrency")
        .and_then(serde_json::Value::as_u64)
        .filter(|&n| n > 0)
        .and_then(|n| usize::try_from(n).ok());

    // `cpuUtilizationTarget` / `estCoresPerSweep` (#4032) also live at the
    // `autonomous` level, mirroring `perTokenConcurrency`. `Value::as_f64`
    // accepts both integer and float JSON (`2` and `2.0`); range filters match
    // the env-var resolvers in `cpu_headroom.rs` exactly so a config value
    // that would be rejected there is dropped to `None` here too, rather than
    // being clamped or silently applied out of range.
    let cpu_utilization_target = autonomous
        .get("cpuUtilizationTarget")
        .and_then(serde_json::Value::as_f64)
        .filter(|&f| f > 0.0 && f <= 1.0);
    let est_cores_per_sweep = autonomous
        .get("estCoresPerSweep")
        .and_then(serde_json::Value::as_f64)
        .filter(|&f| f > 0.0);

    // The `workFinder` sub-block is optional; each field independently falls
    // through to `None` (env/default resolution) when absent.
    let wf = autonomous.get("workFinder");

    WorkFinderConfig {
        enabled: wf
            .and_then(|w| w.get("enabled"))
            .and_then(serde_json::Value::as_bool),
        interval_secs: wf
            .and_then(|w| w.get("intervalSecs"))
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
        max_concurrent: wf
            .and_then(|w| w.get("maxConcurrent"))
            .and_then(serde_json::Value::as_u64)
            .filter(|&n| n > 0)
            .and_then(|n| usize::try_from(n).ok()),
        max_admissions_per_tick: wf
            .and_then(|w| w.get("maxAdmissionsPerTick"))
            .and_then(serde_json::Value::as_u64)
            .filter(|&n| n > 0)
            .and_then(|n| usize::try_from(n).ok()),
        per_token_concurrency,
        cpu_utilization_target,
        est_cores_per_sweep,
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

/// Env override for the per-tick admission (ramp) cap — `None` when unset,
/// zero, or unparseable (a zero cap would freeze the loop, see
/// [`WORK_FINDER_MAX_ADMISSIONS_PER_TICK_ENV`]).
fn env_max_admissions_per_tick() -> Option<usize> {
    std::env::var(WORK_FINDER_MAX_ADMISSIONS_PER_TICK_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
}

/// Resolve the per-tick admission (ramp) cap with precedence **env > config
/// (`autonomous.workFinder.maxAdmissionsPerTick`) > default** (#4234).
/// Resolved once at daemon startup — the same startup-capture pattern as
/// [`resolve_cpu_utilization_target`] / [`resolve_cpu_est_cores_per_sweep`]
/// (#4032) — and threaded through to [`spawn_work_finder_task`] /
/// [`spawn_multi_work_finder_task`] as a plain `usize`, mirroring
/// `per_token_concurrency`. See [`WORK_FINDER_MAX_ADMISSIONS_PER_TICK_ENV`] for
/// why this is a deliberate startup-capture, not a per-tick re-read: the ramp
/// cap's whole purpose is to smooth admission *within* the live per-tick
/// re-computation of `max_concurrent`, so it does not itself need to be live —
/// an operator retuning it takes effect on the next daemon restart, exactly
/// like `configured_max` / `per_token_concurrency` today.
#[must_use]
pub fn resolve_max_admissions_per_tick_with_config(config: &WorkFinderConfig) -> usize {
    env_max_admissions_per_tick()
        .or(config.max_admissions_per_tick)
        .unwrap_or(DEFAULT_MAX_ADMISSIONS_PER_TICK)
}

/// Env override for the per-token concurrency factor — `None` when unset, zero,
/// or unparseable (a zero factor would multiply the token axis to nothing).
fn env_per_token_concurrency() -> Option<usize> {
    std::env::var(PER_TOKEN_CONCURRENCY_ENV)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
}

/// Resolve the per-token concurrency factor with precedence **env > config >
/// default** (#3947). Mirrors [`resolve_max_concurrent_with_config`]: the env
/// var ([`PER_TOKEN_CONCURRENCY_ENV`]) wins, then
/// `autonomous.perTokenConcurrency`, then [`DEFAULT_PER_TOKEN_CONCURRENCY`]. A
/// zero/unparseable value at any layer is treated as absent so it never
/// collapses the token axis to zero.
#[must_use]
pub fn resolve_per_token_concurrency(config: &WorkFinderConfig) -> usize {
    env_per_token_concurrency()
        .or(config.per_token_concurrency)
        .unwrap_or(DEFAULT_PER_TOKEN_CONCURRENCY)
}

/// Resolve the CPU headroom term's utilization-target knob with precedence
/// **env > config > default** (#4032). Thin wrapper over
/// [`crate::cpu_headroom::resolve_utilization_target`] that reads the config
/// half from this module's already-parsed [`WorkFinderConfig`], mirroring how
/// [`resolve_per_token_concurrency`] reads `config.per_token_concurrency`.
#[must_use]
pub fn resolve_cpu_utilization_target(config: &WorkFinderConfig) -> f64 {
    resolve_utilization_target(config.cpu_utilization_target)
}

/// Resolve the CPU headroom term's est-cores-per-sweep knob with precedence
/// **env > config > default** (#4032). Thin wrapper over
/// [`crate::cpu_headroom::resolve_est_cores_per_sweep`]; see
/// [`resolve_cpu_utilization_target`].
#[must_use]
pub fn resolve_cpu_est_cores_per_sweep(config: &WorkFinderConfig) -> f64 {
    resolve_est_cores_per_sweep(config.est_cores_per_sweep)
}

/// Compute the **work-driven dynamic concurrency cap** (Phase B, #3811;
/// per-token concurrency factor added in #3947; CPU/load term added in
/// #3978): `min(token_limit × per_token_concurrency, disk_headroom,
/// cpu_headroom, configured_max)`.
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
/// the pool/disk/cpu ceiling). Keeping the cap as `min(token×factor, disk, cpu,
/// configured)` and letting `tick` apply the backlog bound is what makes
/// concurrency scale up with the backlog and drain to zero when it empties.
///
/// The bounds map directly to the resource each protects:
/// - `token_limit` — the count of *healthy* accounts (or the raw pool when no
///   ranking exists). **Multiplied by `per_token_concurrency`** (#3947): a plan
///   limit is a utilization-window token bucket, not a session count, so one
///   healthy account can comfortably run several concurrent sessions. Before
///   #3947 the implicit factor was `1` (one sweep per account), which collapsed
///   the whole fleet to cap 1 when 6/7 accounts were at their weekly ceiling
///   even though the single healthy account had ample session-window headroom.
/// - `disk_headroom` — never provision more worktrees than the scratch volume
///   can hold at `LOOM_PER_WORKTREE_GB` each.
/// - `cpu_headroom` (#3978) — never start more concurrent sweeps than the host
///   has CPU/load headroom for, at `LOOM_EST_CORES_PER_SWEEP` estimated cores
///   each. This is the term the pre-#3978 formula lacked: a token-axis jump
///   (e.g. several exhausted accounts resetting at once) used to raise the
///   cap regardless of how many concurrent `cargo build`s were already
///   running in worktrees, which could starve the main-health gate's own
///   build of CPU badly enough to false-time-out. See
///   [`crate::cpu_headroom::cpu_headroom_limit`].
/// - `configured_max` — the operator ceiling
///   (`LOOM_WORK_FINDER_MAX_CONCURRENT`), a hard upper bound regardless of how
///   much token/disk/cpu headroom exists.
///
/// `per_token_concurrency` is clamped to a floor of `1` so a mis-set `0`
/// degrades to the pre-#3947 one-sweep-per-account behavior rather than
/// dispatching nothing. `token_limit × factor` uses a saturating multiply so a
/// pathological product can never wrap.
#[must_use]
pub fn resolve_dynamic_max_concurrent(
    token_limit: usize,
    per_token_concurrency: usize,
    disk_headroom: usize,
    cpu_headroom: usize,
    configured_max: usize,
) -> usize {
    token_limit
        .saturating_mul(per_token_concurrency.max(1))
        .min(disk_headroom)
        .min(cpu_headroom)
        .min(configured_max)
}

// ============================================================================
// Runtime wiring — the loop runs on the shared daemon runtime
// ============================================================================

/// Spawn the work-finder loop on the shared daemon runtime and return its task
/// handle so the daemon can keep it alive for the process lifetime.
///
/// Every `interval`, the task recomputes the **dynamic** concurrency cap
/// (Phase B, #3811; CPU/load term #3978) — `min(token-pool size, disk
/// headroom, cpu/load headroom, configured_max)` via
/// [`resolve_dynamic_max_concurrent`] — from live inputs read fresh under
/// `workspace_root`, then runs one [`tick`] with it. The cap is **not** captured
/// once at startup, so a pool that grows/shrinks (`loom-tokens bootstrap`), a
/// scratch volume that fills/frees, current host load, or a draining backlog
/// are all honored without a daemon restart. `configured_max` is the operator
/// ceiling (`LOOM_WORK_FINDER_MAX_CONCURRENT`).
///
/// Unlike the epic supervisor, no dedicated OS thread is needed:
/// [`SweepRegistry::dispatch`] returns promptly (fire-and-forget child spawn),
/// so the finder never parks a runtime worker in a minutes-long blocking call —
/// the same footing as the reaper task
/// ([`crate::sweep_registry::spawn_reaper_task`]). The per-tick disk probe shells
/// out to `df` briefly, which is negligible on the 60s default interval.
#[allow(clippy::too_many_arguments)] // dynamic-cap inputs + shared state; the
                                     // multi-workspace variant ([`spawn_multi_work_finder_task`]) is the production
                                     // path, this single-workspace form is retained for reference/tests.
pub fn spawn_work_finder_task<S, D>(
    mut source: S,
    mut dispatcher: D,
    interval: Duration,
    workspace_root: PathBuf,
    configured_max: usize,
    per_token_concurrency: usize,
    cpu_utilization_target: f64,
    cpu_est_cores_per_sweep: f64,
    max_admissions_per_tick: usize,
    health_state: Arc<MainHealthState>,
    suppress_dispatch_during_gate: bool,
    event_bus: Arc<EventBus>,
) -> tokio::task::JoinHandle<()>
where
    S: WorkSource + Send + 'static,
    D: WorkDispatcher + Send + 'static,
{
    log::info!(
        "work_finder: starting loop (interval={}s, configured_max={configured_max}, \
         per_token_concurrency={per_token_concurrency}, \
         max_admissions_per_tick={max_admissions_per_tick}, \
         dynamic cap = min(healthy tokens × per-token, disk, cpu, configured_max))",
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
        // Axis-visibility state (#4234): promote the per-tick axis line from
        // `debug!` to `info!` only when the computed cap actually **changes**
        // value tick-to-tick — mirrors the state-change-dedup discipline
        // `was_pressured` already applies to the capacity advisory, so an
        // operator watching the log at default level sees every meaningful cap
        // move (e.g. the token axis jumping from a batch of account resets)
        // without a steady-state stream of identical lines every interval.
        let mut was_max_concurrent: Option<usize> = None;
        loop {
            ticker.tick().await;
            // Reactive main-health backstop (Phase C, #3812): skip all dispatch
            // while the gate reports a red `main`. Also (#4084) hold dispatch
            // while a gate *run* is in flight, so a fresh sweep build is not
            // dispatched into the same root the gate's own build is competing
            // with for cores — the `suppress_dispatch_during_gate` knob (default
            // on) gates this so the pre-#4084 behavior is exactly recoverable.
            // Host-distress circuit breaker (#4235): sample load-per-core, fold
            // it into the breaker, and consult it as a second dispatch
            // suppressor alongside the main-health halt flag and the gate
            // in-flight hold. See the multi-workspace loop for the full
            // rationale; when no breaker is registered these are no-ops.
            if let Some(breaker) = crate::host_breaker::global() {
                let load_per_core = crate::cpu_headroom::load_per_core();
                if let Some(transition) = breaker.observe(load_per_core, chrono::Utc::now()) {
                    crate::host_breaker::emit_transition_event(&event_bus, &transition);
                }
            }
            let halted = health_state.is_halted()
                || (suppress_dispatch_during_gate && health_state.is_gate_in_flight())
                || crate::host_breaker::global_is_suppressed();
            // Recompute the dynamic cap from live inputs every tick (Phase B),
            // now with token-capacity backpressure (#3902): the token axis is the
            // count of *healthy* accounts from the ranking, not the flat pool.
            let pool_size = token_pool_size(&workspace_root);
            let ranking = capacity::read_ranking(&workspace_root);
            let token_limit = ranking.as_ref().map_or(pool_size, |r| r.available);
            let disk = disk_headroom_limit(&workspace_root);
            // CPU headroom (#3978; measured-idle signal #4031): the term the
            // pre-#3978 formula lacked. `cpu_headroom_limit()` refreshes the
            // memoized idle sample, which sleeps ~1s on macOS (`iostat`), so it
            // is moved off the runtime via `spawn_blocking`; a join error falls
            // back to the policy floor of 1 (soft backoff, never a hard halt).
            // `cpu_utilization_target` / `cpu_est_cores_per_sweep` are resolved
            // once at startup (env > config > default, #4032) and captured by
            // this task, mirroring `per_token_concurrency`. NOTE (#4234): a
            // release-build-heavy repo should raise `estCoresPerSweep` well
            // above the shipped default of 2.0 — a `cargo build --release`
            // parallelizes across every core via rustc codegen units, so 6
            // concurrent release builds demand far closer to `6 × ncpu` than
            // `6 × 2`. The per-tick `max_admissions_per_tick` ramp cap below is
            // a second, independent backstop for exactly this under-estimate:
            // even if `estCoresPerSweep` is miscalibrated too low and the cpu
            // axis over-admits, the ramp cap still bounds how many of those
            // over-admitted sweeps land in any single tick.
            let cpu = tokio::task::spawn_blocking(move || {
                cpu_headroom_limit(cpu_utilization_target, cpu_est_cores_per_sweep)
            })
            .await
            .unwrap_or(1);
            let max_concurrent = resolve_dynamic_max_concurrent(
                token_limit,
                per_token_concurrency,
                disk,
                cpu,
                configured_max,
            );
            let axis_line = format!(
                "work_finder: dynamic cap = {max_concurrent} (pool={pool_size}, \
                 healthy_tokens={token_limit}, per_token={per_token_concurrency}, disk={disk}, \
                 cpu={cpu}, configured_max={configured_max}, \
                 max_admissions_per_tick={max_admissions_per_tick}, halted={halted})"
            );
            if was_max_concurrent != Some(max_concurrent) {
                log::info!("{axis_line}");
                was_max_concurrent = Some(max_concurrent);
            } else {
                log::debug!("{axis_line}");
            }
            match tick_with_admission_cap(
                &mut source,
                &mut dispatcher,
                max_concurrent,
                halted,
                max_admissions_per_tick,
            ) {
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
                    if report.dispatched > 0
                        || report.errors > 0
                        || report.skipped_quarantined > 0
                        || report.skipped_pr_open > 0
                        || report.skipped_peer_claim > 0
                        || report.deferred_ramp_cap > 0
                    {
                        log::info!(
                            "work_finder: tick — cap {max_concurrent} (pool={pool_size}, \
                             healthy={token_limit}, per_token={per_token_concurrency}, disk={disk}, \
                             cpu={cpu}, ceiling={configured_max}, ramp_cap={max_admissions_per_tick}); \
                             {} seen, {} dispatched, {} labeled-skip, {} in-flight-skip, \
                             {} quarantine-skip, {} pr-open-skip, {} peer-claim-skip, \
                             {} deferred (capacity), {} deferred (ramp), {} error(s), \
                             {} cross-host-collision(s)",
                            report.seen,
                            report.dispatched,
                            report.skipped_labeled,
                            report.skipped_in_flight,
                            report.skipped_quarantined,
                            report.skipped_pr_open,
                            report.skipped_peer_claim,
                            report.deferred_capacity,
                            report.deferred_ramp_cap,
                            report.errors,
                            report.collisions
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
                            cpu,
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
#[allow(clippy::too_many_arguments)] // dynamic-cap inputs (#4032 adds two more
                                     // resolved-once-at-startup f64 knobs, mirroring
                                     // per_token_concurrency) + shared state.
pub fn spawn_multi_work_finder_task(
    pool: Arc<WorkspacePool>,
    fallback_root: PathBuf,
    interval: Duration,
    configured_max: usize,
    per_token_concurrency: usize,
    cpu_utilization_target: f64,
    cpu_est_cores_per_sweep: f64,
    max_admissions_per_tick: usize,
    health_states: Arc<WorkspaceHealthStates>,
    suppress_dispatch_during_gate: bool,
    event_bus: Arc<EventBus>,
    drain: Arc<std::sync::atomic::AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    log::info!(
        "work_finder: starting multi-workspace loop (interval={}s, configured_max={configured_max}, \
         per_token_concurrency={per_token_concurrency}, max_admissions_per_tick={max_admissions_per_tick}, \
         dynamic cap = min(healthy tokens × per-token, disk, cpu, configured_max), global across workspaces)",
        interval.as_secs()
    );
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // First tick fires immediately; skip it so we don't churn at boot.
        ticker.tick().await;
        let mut was_halted = false;
        let mut was_pressured = false;
        // Axis-visibility state (#4234) — see the single-workspace loop above
        // for the full rationale.
        let mut was_max_concurrent: Option<usize> = None;
        // Missing-root hygiene (#4326): tracks which registered roots are
        // currently missing so `filter_missing_roots` logs a warning once per
        // transition rather than once per tick.
        let mut missing_roots_warned: HashSet<PathBuf> = HashSet::new();
        loop {
            ticker.tick().await;

            // Dynamic cap from live *machine-level* inputs (one token pool, one
            // scratch volume) probed from the daemon's primary workspace.
            let pool_size = token_pool_size(&fallback_root);
            let ranking = capacity::read_ranking(&fallback_root);
            let token_limit = ranking.as_ref().map_or(pool_size, |r| r.available);
            let disk = disk_headroom_limit(&fallback_root);
            // CPU headroom (#3978; measured-idle signal #4031) — also a
            // machine-level resource, probed once per tick and shared across
            // every workspace. Moved off the runtime via `spawn_blocking` since
            // the macOS `iostat` refresh sleeps ~1s; a join error falls back to
            // the policy floor of 1. `cpu_utilization_target` /
            // `cpu_est_cores_per_sweep` are resolved once at startup (env >
            // config > default, #4032) and captured by this task, mirroring
            // `per_token_concurrency`.
            let cpu = tokio::task::spawn_blocking(move || {
                cpu_headroom_limit(cpu_utilization_target, cpu_est_cores_per_sweep)
            })
            .await
            .unwrap_or(1);
            let max_concurrent = resolve_dynamic_max_concurrent(
                token_limit,
                per_token_concurrency,
                disk,
                cpu,
                configured_max,
            );

            // Resolve the current set of workspaces fresh each tick so registry
            // edits (add / remove / set-priority) are hot-applied.
            let registry = WorkspaceRegistry::load_default().unwrap_or_else(|e| {
                log::warn!("work_finder: could not load workspace registry ({e}); using cwd");
                WorkspaceRegistry::default()
            });
            let roots = registry.effective_roots(&fallback_root);
            // Skip registered roots whose directory no longer exists on disk
            // (#4326 — e.g. a leaked/stale registry entry) so a dangling entry
            // cannot occupy top dispatch priority or burn the tick. This is
            // warn-and-skip, never auto-remove: the entry stays registered
            // (`loom-daemon status` flags it, `workspace remove` clears it).
            let roots = filter_missing_roots(roots, &mut missing_roots_warned);

            // Per-repo priority tiers (#3946), parallel to `pairs`: lower = higher
            // priority. The empty-registry cwd fallback resolves to the default.
            let priorities: Vec<u32> = roots.iter().map(|r| registry.priority_of(r)).collect();

            // Per-repo main-health halt (#3930): look up each root's own gate
            // state, parallel to `pairs`. A red repo halts only its own dispatch.
            // A root whose gate *run* is in flight (#4084) is likewise held —
            // per-root, so a sibling with no gate in flight keeps dispatching
            // (the #3930 isolation contract). `suppress_dispatch_during_gate`
            // (default on) gates the in-flight term so the pre-#4084 behavior is
            // exactly recoverable.
            //
            // A scheduled drain (#4090) is daemon-global: it pauses new dispatch
            // in EVERY repo at once, so it is OR'd on top of every root's
            // per-root hold. Both terms are additive: drain holds every root
            // regardless of gate state, and a gate in flight holds its own root
            // regardless of drain state.
            let draining = drain.load(std::sync::atomic::Ordering::Relaxed);
            // Host-distress circuit breaker (#4235): sample the current
            // load-per-core and fold it into the breaker's state machine. A
            // tripped/cooling breaker is a *daemon-global* dispatch suppressor
            // (like the scheduled drain) — it holds new dispatch in EVERY repo
            // while running work drains — so it is OR'd onto every root's hold
            // below. The load sample uses the fast, non-sleeping loadavg read (no
            // `iostat`), safe to call inline. When no breaker is registered the
            // helpers are no-ops returning `false` (zero behavior change).
            if let Some(breaker) = crate::host_breaker::global() {
                let load_per_core = crate::cpu_headroom::load_per_core();
                if let Some(transition) = breaker.observe(load_per_core, chrono::Utc::now()) {
                    crate::host_breaker::emit_transition_event(&event_bus, &transition);
                }
            }
            let breaker_suppressed = crate::host_breaker::global_is_suppressed();
            let halted: Vec<bool> = dispatch_held_per_root_with_drain(
                &health_states,
                &roots,
                suppress_dispatch_during_gate,
                draining,
            )
            .into_iter()
            .map(|h| h || breaker_suppressed)
            .collect();
            let any_halted = halted.iter().any(|&h| h);

            let mut pairs: Vec<(GhWorkSource, RegistryDispatcher)> = roots
                .iter()
                .map(|root| {
                    let registry = pool.get_or_provision(root);
                    (GhWorkSource::for_root(root), RegistryDispatcher::new(registry))
                })
                .collect();

            let axis_line = format!(
                "work_finder: dynamic cap = {max_concurrent} (pool={pool_size}, \
                 healthy_tokens={token_limit}, per_token={per_token_concurrency}, disk={disk}, \
                 cpu={cpu}, configured_max={configured_max}, \
                 max_admissions_per_tick={max_admissions_per_tick}, any_halted={any_halted}, \
                 workspaces={}, priorities={priorities:?})",
                pairs.len()
            );
            if was_max_concurrent != Some(max_concurrent) {
                log::info!("{axis_line}");
                was_max_concurrent = Some(max_concurrent);
            } else {
                log::debug!("{axis_line}");
            }

            let report = tick_multi_with_admission_cap(
                &mut pairs,
                &priorities,
                max_concurrent,
                &halted,
                max_admissions_per_tick,
            );

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

            if report.dispatched > 0
                || report.errors > 0
                || report.skipped_quarantined > 0
                || report.skipped_pr_open > 0
                || report.skipped_peer_claim > 0
                || report.deferred_ramp_cap > 0
            {
                log::info!(
                    "work_finder: tick — cap {max_concurrent} (pool={pool_size}, \
                     healthy={token_limit}, per_token={per_token_concurrency}, disk={disk}, \
                     cpu={cpu}, ceiling={configured_max}, ramp_cap={max_admissions_per_tick}); \
                     {} workspace(s), \
                     {} seen, {} dispatched, {} labeled-skip, {} in-flight-skip, \
                     {} quarantine-skip, {} pr-open-skip, {} peer-claim-skip, \
                     {} deferred (capacity), {} deferred (ramp), {} error(s), \
                     {} cross-host-collision(s)",
                    pairs.len(),
                    report.seen,
                    report.dispatched,
                    report.skipped_labeled,
                    report.skipped_in_flight,
                    report.skipped_quarantined,
                    report.skipped_pr_open,
                    report.skipped_peer_claim,
                    report.deferred_capacity,
                    report.deferred_ramp_cap,
                    report.errors,
                    report.collisions
                );
            }

            if !report.halted {
                let assessment = capacity::assess_pressure(
                    ranking.as_ref(),
                    pool_size,
                    token_limit,
                    disk,
                    cpu,
                    configured_max,
                    report.deferred_capacity,
                    capacity::DEFAULT_ADVISORY_MIN_QUEUED,
                );
                was_pressured = emit_capacity_transition(&event_bus, was_pressured, &assessment);
            }
        }
    })
}

/// Filter a resolved root list down to directories that still exist on disk
/// (Issue #4326), warning once per missing/recovered transition rather than
/// every tick.
///
/// Deliberately **not** folded into
/// [`WorkspaceRegistry::effective_roots`](crate::workspace_registry::WorkspaceRegistry::effective_roots)
/// — that helper's empty-registry branch falls back to the daemon's cwd, and
/// dropping missing roots *inside* it would make an all-roots-missing
/// registry indistinguishable from an empty one, silently redirecting
/// dispatch into the daemon's own cwd. Filtering here instead means an
/// all-missing registry yields zero roots for this tick — no dispatch, no
/// cwd fallback — while the dangling entries stay registered so
/// `loom-daemon status` can flag them and an operator can `workspace remove`
/// them. This is warn-and-skip, **never auto-remove**: a root can be
/// transiently absent (an unmounted network volume, an in-progress restore),
/// and auto-deregistering would destroy operator state on a transient
/// condition.
///
/// `warned` carries the set of roots currently known-missing across ticks so
/// a long-dangling entry logs once on first-seen-missing (not once per tick),
/// and — if it later reappears and then disappears again — re-warns instead
/// of staying silent forever.
fn filter_missing_roots(roots: Vec<PathBuf>, warned: &mut HashSet<PathBuf>) -> Vec<PathBuf> {
    let mut existing = Vec::with_capacity(roots.len());
    let mut still_missing: HashSet<PathBuf> = HashSet::new();
    for root in roots {
        if root.is_dir() {
            existing.push(root);
        } else {
            if !warned.contains(&root) {
                log::warn!(
                    "work_finder: registered workspace root {} does not exist on disk; \
                     skipping it for this tick (entry stays registered — run \
                     `loom-daemon workspace remove {}` to deregister it if this is \
                     permanent, or `loom-daemon status` to confirm)",
                    root.display(),
                    root.display()
                );
            }
            still_missing.insert(root);
        }
    }
    *warned = still_missing;
    existing
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

        fn quarantined(&self) -> HashSet<u32> {
            match self.registry.lock() {
                Ok(reg) => reg.quarantined_issues(),
                Err(poisoned) => {
                    log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                    HashSet::new()
                }
            }
        }

        /// Discounted occupancy count (Issue #4003): a sweep dispatched longer
        /// than the registry's configured startup-proof grace window with zero
        /// observed startup signal does not count toward the budget — see
        /// `SweepRegistry::occupied_issues`. Reap-on-read first, mirroring
        /// `in_flight()`, so a child whose process already exited never
        /// over-counts either.
        fn occupancy(&self) -> usize {
            let mut reg = match self.registry.lock() {
                Ok(r) => r,
                Err(poisoned) => {
                    log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                    return 0;
                }
            };
            reg.reap_liveness();
            reg.occupied_issues().len()
        }

        fn collisions(&self) -> u64 {
            match self.registry.lock() {
                Ok(reg) => reg.collision_count(),
                Err(poisoned) => {
                    log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                    0
                }
            }
        }

        fn peer_claimed(&self) -> HashSet<u32> {
            match self.registry.lock() {
                Ok(reg) => reg.peer_claimed_issues(),
                Err(poisoned) => {
                    log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                    HashSet::new()
                }
            }
        }

        fn dispatch(&mut self, issue: u32) -> Result<bool> {
            let mut reg = self
                .registry
                .lock()
                .map_err(|e| anyhow!("sweep registry mutex poisoned: {e}"))?;
            // Autonomous dispatch model (issue #3944): resolve an EXPLICIT model
            // (`autonomous.model` config > shipped non-premium default) so the
            // spawned child never silently inherits the operator's interactive
            // CLI default (which may be a premium tier that burns usage credits).
            // No dispatch-param tier here — the work finder has no per-issue
            // override — so `explicit = None`.
            let repo_root = reg.config().workspace_root.clone();
            let (model, source) = crate::sweep_registry::resolve_dispatch_model(&repo_root, None);
            log::info!(
                "work_finder: dispatching issue #{issue} with model={model} (source={})",
                source.as_str()
            );
            // Idempotency key + the registry's claim lock make a re-dispatch of
            // an already-running issue a no-op (`was_new = false`) or a loud
            // lock-collision error.
            let key = format!("workfinder-{issue}");
            let outcome =
                reg.dispatch(&SweepKind::Issue(issue), Some(key), Some(&model), None, None)?;
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
        /// Issue numbers whose dispatch should be refused by the open-PR guard
        /// (#4123) — the dispatcher returns the typed [`OpenPrDispatchError`].
        pr_open_issues: HashSet<u32>,
        /// Issue numbers this dispatcher reports as quarantined (Issue #3939).
        quarantined: HashSet<u32>,
        /// Cumulative cross-host collision count this dispatcher reports (#4085).
        collisions: u64,
        /// Issue numbers a peer host has soft-claimed over safehouse (#4028).
        peer_claimed: HashSet<u32>,
    }

    impl WorkDispatcher for RecordingDispatcher {
        fn in_flight(&self) -> HashSet<u32> {
            self.in_flight.clone()
        }
        fn quarantined(&self) -> HashSet<u32> {
            self.quarantined.clone()
        }
        fn collisions(&self) -> u64 {
            self.collisions
        }
        fn peer_claimed(&self) -> HashSet<u32> {
            self.peer_claimed.clone()
        }
        fn dispatch(&mut self, issue: u32) -> Result<bool> {
            if self.pr_open_issues.contains(&issue) {
                // Mirror the production `SweepRegistry::dispatch` open-PR guard:
                // refuse with the typed, downcast-matchable error (#4123).
                return Err(OpenPrDispatchError { issue, pr: 9999 }.into());
            }
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
    // RegistryDispatcher — production dispatch-path regression (Issue #3967)
    // ===================================================================

    /// Build a real `SweepRegistry` (not the `RecordingDispatcher` test
    /// fake) backed by a fixture spawn binary that records its env to a
    /// sibling log and exits immediately — same pattern used by the
    /// `sweep_registry.rs` and `ipc.rs` fixtures.
    fn setup_registry_dispatcher_in_tempdir(
    ) -> (RegistryDispatcher, tempfile::TempDir, std::path::PathBuf) {
        use crate::sweep_registry::{SweepRegistry, SweepRegistryConfig};
        use std::os::unix::fs::PermissionsExt;
        use std::sync::Mutex;

        let dir = tempfile::tempdir().unwrap();
        let scripts_dir = dir.path().join(".loom").join("scripts");
        std::fs::create_dir_all(&scripts_dir).unwrap();
        let fake_bin = scripts_dir.join("spawn-claude.sh");
        let record_log = dir.path().join("workfinder-fake-spawn.log");
        let script = format!(
            r#"#!/usr/bin/env bash
printf 'LOOM_SWEEP_CLAIM_OWNED=%s\n' "${{LOOM_SWEEP_CLAIM_OWNED:-unset}}" >> "{rec}"
printf 'argv: %s\n' "$*" >> "{rec}"
exit 0
"#,
            rec = record_log.display()
        );
        std::fs::write(&fake_bin, script).unwrap();
        let mut perms = std::fs::metadata(&fake_bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_bin, perms).unwrap();

        let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
        config.spawn_bin = Some(fake_bin);
        config.skip_label_flip = true;
        config.journal_path = Some(dir.path().join("test-sweeps-journal.json"));
        let registry = Arc::new(Mutex::new(SweepRegistry::new(config)));
        (RegistryDispatcher::new(registry), dir, record_log)
    }

    /// Issue #3967 / #4111: the autonomous work finder's real dispatch path
    /// (`RegistryDispatcher`, the `WorkDispatcher` impl wired into
    /// production — as opposed to the `RecordingDispatcher` test fake used
    /// by every `tick`/`tick_multi` test above) must export
    /// `LOOM_SWEEP_CLAIM_OWNED=<issue>` into the spawned child's env, AND
    /// (#4111) append `--claim-owned <issue>` to the child's own argv, exactly
    /// like the IPC `DispatchSweep` path (`ipc.rs`) and the CLI `dispatch`
    /// subcommand (`main.rs`) do. `RegistryDispatcher::dispatch` forwards to
    /// `SweepRegistry::dispatch` → `spawn_child`, so this closes the
    /// dispatch-path-level regression coverage across all three daemon
    /// dispatch entry points — for both the env-var and the argv-flag signal.
    #[test]
    #[serial]
    fn test_registry_dispatcher_exports_claim_ownership_marker() {
        // Issue #4044: mirrors `sweep_registry::tests::FIXTURE_CHILD_WAIT_MS`
        // (that const is private to `sweep_registry`'s test module, so it
        // can't be reused here directly). A short fixed poll bound falsely
        // reddens this test under host exec-latency pressure (syspolicyd,
        // AV scanners delaying the spawned child's launch) — the bound is a
        // ceiling on a healthy-host-cheap poll, not a promptness assertion,
        // so widening it is free.
        const FIXTURE_CHILD_WAIT_MS: u128 = 120_000;

        let (mut dispatcher, _dir, record_log) = setup_registry_dispatcher_in_tempdir();

        let was_new = dispatcher.dispatch(3964).expect("dispatch should succeed");
        assert!(was_new, "expected a fresh dispatch, not an idempotency no-op");

        let start = std::time::Instant::now();
        let mut recorded = String::new();
        while start.elapsed().as_millis() < FIXTURE_CHILD_WAIT_MS {
            if let Ok(s) = std::fs::read_to_string(&record_log) {
                if s.contains("LOOM_SWEEP_CLAIM_OWNED=") {
                    recorded = s;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            recorded.contains("LOOM_SWEEP_CLAIM_OWNED=3964"),
            "expected the work-finder's production RegistryDispatcher to export \
             the daemon-owned-child self-claim marker; got: {recorded:?}"
        );
        // #4111: the positional argv flag must also be present on this same
        // dispatch (belt-and-suspenders — the env var alone was proven
        // insufficient for a `/loom:sweep` child to actually notice).
        assert!(
            recorded.contains("--claim-owned 3964"),
            "expected the work-finder's production RegistryDispatcher to append \
             --claim-owned 3964 to the child argv (#4111); got: {recorded:?}"
        );
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

    // ===================================================================
    // tick_with_admission_cap — per-tick ramp cap (#4234, Gap 3 of #4231)
    // ===================================================================

    #[test]
    fn test_tick_admission_cap_limits_new_dispatches_even_under_large_concurrency_cap() {
        // 6 ready candidates, plenty of concurrency room (max_concurrent=10),
        // but the ramp cap only allows 3 *new* admissions this tick — exactly
        // the #4231 6-way-fan-out scenario: a token-axis jump could make
        // max_concurrent look like it has room for all 6, but the ramp cap
        // still bounds the burst.
        let mut source = FakeSource::once((1..=6).map(issue).collect());
        let mut disp = RecordingDispatcher::default();
        let report = tick_with_admission_cap(&mut source, &mut disp, 10, false, 3).unwrap();

        assert_eq!(report.seen, 6);
        assert_eq!(report.dispatched, 3, "only the ramp cap's worth admitted");
        assert_eq!(report.deferred_ramp_cap, 3, "the rest deferred to the ramp cap, not capacity");
        assert_eq!(report.deferred_capacity, 0, "concurrency cap was never the binding constraint");
        assert_eq!(disp.dispatched, vec![1, 2, 3]);
    }

    #[test]
    fn test_tick_admission_cap_and_concurrency_cap_are_independent_and_both_apply() {
        // Concurrency cap (2) is smaller than the ramp cap (5) here, so the
        // concurrency cap is the one that actually binds — exercising that the
        // two checks compose rather than one silently overriding the other.
        let mut source = FakeSource::once((1..=6).map(issue).collect());
        let mut disp = RecordingDispatcher::default();
        let report = tick_with_admission_cap(&mut source, &mut disp, 2, false, 5).unwrap();

        assert_eq!(report.dispatched, 2);
        assert_eq!(report.deferred_capacity, 4, "concurrency cap bound first");
        assert_eq!(report.deferred_ramp_cap, 0, "ramp cap never reached — occupancy hit 2 first");
    }

    #[test]
    fn test_tick_admission_cap_unlimited_reduces_to_plain_tick() {
        // `tick()` is a thin wrapper passing `usize::MAX` — byte-for-byte the
        // pre-#4234 unlimited-admission behavior.
        let mut source_capped = FakeSource::once((1..=4).map(issue).collect());
        let mut disp_capped = RecordingDispatcher::default();
        let capped =
            tick_with_admission_cap(&mut source_capped, &mut disp_capped, 10, false, usize::MAX)
                .unwrap();

        let mut source_plain = FakeSource::once((1..=4).map(issue).collect());
        let mut disp_plain = RecordingDispatcher::default();
        let plain = tick(&mut source_plain, &mut disp_plain, 10, false).unwrap();

        assert_eq!(capped, plain);
        assert_eq!(disp_capped.dispatched, disp_plain.dispatched);
    }

    #[test]
    fn test_tick_multi_admission_cap_shared_across_workspaces() {
        // Two workspaces, 4 candidates total, ramp cap 2 — the cap is a single
        // shared counter across both workspaces (mirrors the concurrency cap's
        // existing shared-budget contract).
        let source_a = FakeSource::once(vec![issue(1), issue(2)]);
        let disp_a = RecordingDispatcher::default();
        let source_b = FakeSource::once(vec![issue(3), issue(4)]);
        let disp_b = RecordingDispatcher::default();
        let mut multi = vec![(source_a, disp_a), (source_b, disp_b)];

        let report = tick_multi_with_admission_cap(&mut multi, &[], 10, &[false, false], 2);

        assert_eq!(report.dispatched, 2);
        assert_eq!(report.deferred_ramp_cap, 2);
        assert_eq!(report.deferred_capacity, 0);
    }

    #[test]
    fn test_tick_admission_cap_zero_defers_everything() {
        // A ramp cap of 0 admits nothing this tick (still distinct from
        // `halted`: `seen` reflects the backlog, no main-health warning fires).
        let mut source = FakeSource::once((1..=3).map(issue).collect());
        let mut disp = RecordingDispatcher::default();
        let report = tick_with_admission_cap(&mut source, &mut disp, 10, false, 0).unwrap();

        assert_eq!(report.dispatched, 0);
        assert_eq!(report.deferred_ramp_cap, 3);
        assert!(disp.dispatched.is_empty());
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
    fn test_tick_skips_quarantined_issue() {
        // Insta-crash quarantine (#3939): a quarantined issue is skipped — never
        // dispatched — and counted in `skipped_quarantined`, while its healthy
        // siblings dispatch normally.
        let mut source = FakeSource::once(vec![issue(1), issue(2), issue(3)]);
        let mut disp = RecordingDispatcher {
            quarantined: HashSet::from([2]),
            ..Default::default()
        };
        let report = tick(&mut source, &mut disp, 10, false).unwrap();

        assert_eq!(report.skipped_quarantined, 1, "#2 is quarantined");
        assert_eq!(report.dispatched, 2, "#1 and #3 still dispatch");
        assert_eq!(disp.dispatched, vec![1, 3], "#2 never dispatched");
    }

    #[test]
    fn test_tick_quarantined_does_not_consume_capacity_slot() {
        // The quarantine skip happens BEFORE the capacity gate, so a quarantined
        // issue never reserves a slot: with cap 1 and #1 quarantined, #2 gets the
        // slot rather than the tick deferring #2 behind a wasted #1 dispatch.
        let mut source = FakeSource::once(vec![issue(1), issue(2)]);
        let mut disp = RecordingDispatcher {
            quarantined: HashSet::from([1]),
            ..Default::default()
        };
        let report = tick(&mut source, &mut disp, 1, false).unwrap();

        assert_eq!(report.skipped_quarantined, 1);
        assert_eq!(report.dispatched, 1);
        assert_eq!(disp.dispatched, vec![2], "the single slot goes to the healthy #2");
    }

    #[test]
    fn test_tick_multi_quarantined_workspace_does_not_starve_sibling() {
        // AC #3 (#3939): workspace A's only candidate is quarantined; workspace B
        // has a healthy candidate. With a shared cap of 1, B's issue MUST be
        // dispatched — a quarantined candidate never reserves the shared slot, so
        // healthy sibling work is not starved.
        let mut multi = vec![
            (
                FakeSource::once(vec![issue(1)]),
                RecordingDispatcher {
                    quarantined: HashSet::from([1]),
                    ..Default::default()
                },
            ),
            (FakeSource::once(vec![issue(10)]), RecordingDispatcher::default()),
        ];
        let report = tick_multi(&mut multi, &[], 1, &[false, false]);

        assert_eq!(report.skipped_quarantined, 1, "workspace A's #1 is quarantined");
        assert_eq!(report.dispatched, 1);
        assert!(multi[0].1.dispatched.is_empty(), "quarantined workspace dispatches nothing");
        assert_eq!(multi[1].1.dispatched, vec![10], "healthy sibling gets the shared slot");
    }

    #[test]
    fn test_tick_peer_claim_skipped_under_distinct_counter() {
        // A peer host's live soft claim (#4028) skips the issue under its OWN
        // distinct counter — never folded into labeled/in-flight/quarantine — and
        // does not consume a capacity slot (checked before the cap gate), so the
        // healthy sibling issue takes the slot.
        let mut source = FakeSource::once(vec![issue(1), issue(2)]);
        let mut disp = RecordingDispatcher {
            peer_claimed: HashSet::from([1]),
            ..Default::default()
        };
        let report = tick(&mut source, &mut disp, 1, false).unwrap();

        assert_eq!(report.skipped_peer_claim, 1, "#1 is peer-claimed");
        assert_eq!(report.skipped_labeled, 0, "peer-claim is NOT a label skip");
        assert_eq!(report.skipped_in_flight, 0, "peer-claim is NOT an in-flight skip");
        assert_eq!(report.dispatched, 1);
        assert_eq!(disp.dispatched, vec![2], "the slot goes to the un-claimed #2");
    }

    #[test]
    fn test_tick_stops_skipping_once_peer_claim_clears() {
        // Once the peer claim lapses (empty peer_claimed set, mirroring a TTL
        // expiry / retraction), the previously-skipped issue dispatches normally.
        let mut source = FakeSource::once(vec![issue(1)]);
        let mut disp = RecordingDispatcher::default(); // no peer claims now
        let report = tick(&mut source, &mut disp, 5, false).unwrap();

        assert_eq!(report.skipped_peer_claim, 0);
        assert_eq!(report.dispatched, 1);
        assert_eq!(disp.dispatched, vec![1]);
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
    fn test_tick_surfaces_collision_total() {
        // The dispatcher reports a cumulative cross-host collision count (#4085);
        // the tick surfaces it on the report so the per-tick summary line can log
        // the running baseline.
        let mut source = FakeSource::once(vec![issue(1)]);
        let mut disp = RecordingDispatcher {
            collisions: 3,
            ..Default::default()
        };
        let report = tick(&mut source, &mut disp, 5, false).unwrap();
        assert_eq!(report.collisions, 3, "collision total surfaced from dispatcher");
        assert_eq!(report.dispatched, 1);
    }

    #[test]
    fn test_tick_collision_total_surfaced_when_halted() {
        // Even on a halted tick (no dispatch), the running collision baseline is
        // carried forward onto the report.
        let mut source = FakeSource::once(vec![issue(1)]);
        let mut disp = RecordingDispatcher {
            collisions: 2,
            ..Default::default()
        };
        let report = tick(&mut source, &mut disp, 5, true).unwrap();
        assert!(report.halted);
        assert_eq!(report.dispatched, 0);
        assert_eq!(report.collisions, 2);
    }

    #[test]
    fn test_tick_multi_sums_collision_totals() {
        // tick_multi sums the collision totals across every workspace's
        // dispatcher (#4085).
        let mut multi = vec![
            (
                FakeSource::once(vec![issue(1)]),
                RecordingDispatcher {
                    collisions: 2,
                    ..Default::default()
                },
            ),
            (
                FakeSource::once(vec![issue(2)]),
                RecordingDispatcher {
                    collisions: 5,
                    ..Default::default()
                },
            ),
        ];
        let report = tick_multi(&mut multi, &[0, 0], 10, &[false, false]);
        assert_eq!(report.collisions, 7, "collision totals summed across workspaces");
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
    fn test_tick_open_pr_refusal_counts_as_pr_open_skip_not_error() {
        // #2 has an open linked PR: `dispatch()` refuses with the typed
        // OpenPrDispatchError, which the finder attributes to `skipped_pr_open`
        // — NOT `errors` — while its siblings dispatch normally (#4123).
        let mut source = FakeSource::once(vec![issue(1), issue(2), issue(3)]);
        let mut disp = RecordingDispatcher {
            pr_open_issues: HashSet::from([2]),
            ..Default::default()
        };
        let report = tick(&mut source, &mut disp, 10, false).unwrap();

        assert_eq!(report.skipped_pr_open, 1, "#2's open-PR refusal is a pr-open-skip");
        assert_eq!(report.errors, 0, "an open-PR skip is never a dispatch error");
        assert_eq!(report.dispatched, 2, "#1 and #3 still dispatch");
        assert_eq!(disp.dispatched, vec![1, 3], "#2 never dispatched");
    }

    #[test]
    fn test_tick_skip_only_pr_open_tick_is_reported() {
        // A tick whose ONLY outcome is a pr-open-skip must still surface a
        // non-empty report (dispatched == 0, errors == 0) — the counter carries
        // the visibility, and the tick-log gate includes `skipped_pr_open` so
        // such a tick is no longer silent (#4123).
        let mut source = FakeSource::once(vec![issue(5)]);
        let mut disp = RecordingDispatcher {
            pr_open_issues: HashSet::from([5]),
            ..Default::default()
        };
        let report = tick(&mut source, &mut disp, 10, false).unwrap();

        assert_eq!(report.dispatched, 0);
        assert_eq!(report.errors, 0);
        assert_eq!(report.skipped_pr_open, 1);
        assert!(disp.dispatched.is_empty(), "nothing dispatched on a skip-only tick");
    }

    #[test]
    fn test_tick_multi_open_pr_refusal_counts_as_pr_open_skip() {
        // The multi-workspace path attributes the open-PR refusal the same way
        // as the single-workspace `tick` (#4123): the epic supervisor and
        // watchdogs route through the same `dispatch()` seam, so this coverage
        // matches the guard's placement.
        let src_a = FakeSource::once(vec![issue(1), issue(2)]);
        let disp_a = RecordingDispatcher {
            pr_open_issues: HashSet::from([2]),
            ..Default::default()
        };
        let mut pairs: Vec<(FakeSource, RecordingDispatcher)> = vec![(src_a, disp_a)];
        let report = tick_multi(&mut pairs, &[], 10, &[false]);

        assert_eq!(report.skipped_pr_open, 1);
        assert_eq!(report.errors, 0);
        assert_eq!(report.dispatched, 1);
        assert_eq!(pairs[0].1.dispatched, vec![1]);
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
    fn test_tripped_host_breaker_suppresses_tick_without_aborting_running_work() {
        // The host-distress circuit breaker (#4235) is consulted at the tick
        // choke point by folding its `is_suppressed()` into the same `halted`
        // bool the main-health gate uses. This proves the two load-bearing
        // properties end-to-end: a *tripped* breaker dispatches ZERO new sweeps,
        // and the running (in-flight) sweeps are left untouched — drain, don't
        // abort.
        use crate::host_breaker::{BreakerPhase, HostBreakerConfig, SharedHostBreaker};
        let now = chrono::Utc::now();
        let breaker = SharedHostBreaker::new(HostBreakerConfig {
            enabled: true,
            load_per_core_threshold: 2.5,
            sustain_ticks: 3,
            cooldown_secs: 300,
        });
        // Not yet tripped: the breaker does not suppress, so a tick dispatches.
        assert!(!breaker.is_suppressed());

        // Three sustained over-threshold samples trip it to Open.
        breaker.observe(Some(4.0), now);
        breaker.observe(Some(4.0), now);
        breaker.observe(Some(4.0), now);
        assert_eq!(breaker.snapshot().phase, BreakerPhase::Open);
        assert!(breaker.is_suppressed(), "tripped breaker suppresses dispatch");

        // Feed the breaker's suppression into the tick's `halted` input, exactly
        // as the work-finder loop does.
        let mut source = FakeSource::once((1..=5).map(issue).collect());
        let mut disp = RecordingDispatcher {
            in_flight: HashSet::from([100, 101]),
            ..Default::default()
        };
        let halted = breaker.is_suppressed();
        let report = tick(&mut source, &mut disp, 10, halted).unwrap();

        assert!(report.halted, "a tripped breaker halts the tick");
        assert_eq!(report.seen, 5, "backlog is still observed");
        assert_eq!(report.dispatched, 0, "zero new dispatch while the breaker is open");
        assert!(disp.dispatched.is_empty(), "no new sweeps started");
        // Drain, don't abort: the two running sweeps are untouched.
        assert_eq!(disp.in_flight, HashSet::from([100, 101]));
    }

    #[test]
    fn test_host_breaker_cooldown_release_resumes_dispatch() {
        // After the breaker cools down and releases, the tick resumes dispatch —
        // the "cool-down release" half of the Test Plan, driven through the same
        // `halted`-composition path the loop uses.
        use crate::host_breaker::{BreakerPhase, HostBreakerConfig, SharedHostBreaker};
        let t0 = chrono::Utc::now();
        let breaker = SharedHostBreaker::new(HostBreakerConfig {
            enabled: true,
            load_per_core_threshold: 2.5,
            sustain_ticks: 3,
            cooldown_secs: 300,
        });
        // Trip → Open.
        for _ in 0..3 {
            breaker.observe(Some(4.0), t0);
        }
        assert!(breaker.is_suppressed());
        // Load drops → CoolDown (still suppressed).
        breaker.observe(Some(0.1), t0 + chrono::Duration::seconds(10));
        assert_eq!(breaker.snapshot().phase, BreakerPhase::CoolDown);
        assert!(breaker.is_suppressed(), "cool-down still suppresses");
        // Cool-down elapses with acceptable load → Closed, dispatch resumes.
        breaker.observe(Some(0.1), t0 + chrono::Duration::seconds(400));
        assert_eq!(breaker.snapshot().phase, BreakerPhase::Closed);
        assert!(!breaker.is_suppressed());

        let mut source = FakeSource::once((1..=3).map(issue).collect());
        let mut disp = RecordingDispatcher::default();
        let report = tick(&mut source, &mut disp, 10, breaker.is_suppressed()).unwrap();
        assert!(!report.halted);
        assert_eq!(report.dispatched, 3, "dispatch resumes once the breaker releases");
        assert_eq!(disp.dispatched, vec![1, 2, 3]);
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
        let report = tick_multi(&mut multi, &[], 10, &[false]);
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
        let report = tick_multi(&mut multi, &[], 10, &[false, false]);
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
        let report = tick_multi(&mut multi, &[], 3, &[false, false]);
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
        let report = tick_multi(&mut multi, &[], 4, &[false, false]);
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
        let report = tick_multi(&mut multi, &[], 10, &[false, false, false]);
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
        let report = tick_multi(&mut multi, &[], 10, &[true, true]);
        assert!(report.halted);
        assert_eq!(report.seen, 6, "backlog across both workspaces is still observed");
        assert_eq!(report.dispatched, 0, "zero dispatch while halted");
        assert!(multi.iter().all(|(_, d)| d.dispatched.is_empty()));
    }

    #[test]
    fn test_tick_multi_empty_workspace_set_is_noop() {
        let mut multi: Vec<(FakeSource, RecordingDispatcher)> = vec![];
        let report = tick_multi(&mut multi, &[], 10, &[]);
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
        let report = tick_multi(&mut multi, &[], 10, &[true, false]);
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
        let report = tick_multi(&mut multi, &[], 10, &[false, true]);
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
        let report = tick_multi(&mut multi, &[], 3, &[true, false]);
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
        let report = tick_multi(&mut multi, &[], 10, &[false, false]);
        assert!(!report.halted);
        assert_eq!(report.dispatched, 2);
    }

    #[test]
    fn test_tick_multi_halt_survives_a_failing_forge_query() {
        // #3974 AC3: `report.halted` must be derived from the shared halt flags
        // the gate writes, never accumulated as a side effect of the
        // candidate-gathering loop. In the incident `gh` was dead in the
        // daemon's process tree, so `list_ready_issues` errored *before* the
        // loop reached its halt check — the finder then logged "main-health gate
        // cleared — resuming dispatch" in the same window the gate was logging
        // "still RED".
        let mut multi =
            vec![(err_source("gh: No user exists for uid 501"), RecordingDispatcher::default())];
        let report = tick_multi(&mut multi, &[], 10, &[true]);
        assert_eq!(report.errors, 1, "the forge query failed");
        assert!(
            report.halted,
            "a halted repo must still report halted when its forge query fails — \
             otherwise work_finder and main_health_gate disagree"
        );
        assert!(multi[0].1.dispatched.is_empty());
    }

    #[test]
    fn test_tick_multi_halt_reported_even_when_repo_has_no_backlog() {
        // Same derivation property with an empty (rather than failing) source:
        // "halted" is a property of the gate state, not of what the tick saw.
        let mut multi = vec![(FakeSource::once(vec![]), RecordingDispatcher::default())];
        let report = tick_multi(&mut multi, &[], 10, &[true]);
        assert_eq!(report.seen, 0);
        assert!(report.halted, "halt is derived from the shared flag, not from the backlog");
    }

    #[test]
    fn test_tick_multi_extra_halt_entries_are_ignored() {
        // A `halted` slice longer than the workspace list (a stale snapshot)
        // must not manufacture a halt for workspaces that do not exist.
        let mut multi = vec![(FakeSource::once(vec![issue(1)]), RecordingDispatcher::default())];
        let report = tick_multi(&mut multi, &[], 10, &[false, true, true]);
        assert!(!report.halted, "only the present workspaces' flags count");
        assert_eq!(report.dispatched, 1);
    }

    #[test]
    fn test_tick_multi_missing_halt_entry_defaults_not_halted() {
        // A short `halted` slice (fewer entries than workspaces) treats the
        // unspecified workspaces as not halted rather than panicking.
        let mut multi = vec![
            (FakeSource::once(vec![issue(1)]), RecordingDispatcher::default()),
            (FakeSource::once(vec![issue(10)]), RecordingDispatcher::default()),
        ];
        let report = tick_multi(&mut multi, &[], 10, &[true]);
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
    // resolve_dynamic_max_concurrent — Phase B work-driven policy (#3811),
    // per-token concurrency factor (#3947), CPU/load headroom term (#3978)
    // ===================================================================

    #[test]
    fn test_dynamic_cap_is_min_of_four_inputs() {
        // Never exceeds any of the four bounds (factor 1 = pre-#3947 semantics).
        assert_eq!(resolve_dynamic_max_concurrent(10, 1, 10, 10, 10), 10);
        assert_eq!(resolve_dynamic_max_concurrent(2, 1, 9, 9, 9), 2, "token axis binds");
        assert_eq!(resolve_dynamic_max_concurrent(9, 1, 3, 9, 9), 3, "disk binds");
        assert_eq!(resolve_dynamic_max_concurrent(9, 1, 9, 4, 9), 4, "cpu binds");
        assert_eq!(resolve_dynamic_max_concurrent(9, 1, 9, 9, 4), 4, "ceiling binds");
    }

    #[test]
    fn test_dynamic_cap_pool_size_bound_never_over_subscribes() {
        // With a large disk headroom, cpu headroom, and ceiling and factor 1,
        // the token-pool size is the hard bound — the cap never exceeds the
        // number of accounts.
        for pool in 0..=5 {
            assert_eq!(
                resolve_dynamic_max_concurrent(pool, 1, 100, 100, 100),
                pool,
                "cap must equal pool size {pool} when disk/cpu/ceiling are larger"
            );
        }
    }

    #[test]
    fn test_dynamic_cap_disk_headroom_bound() {
        // A nearly-full scratch volume (disk headroom 1) caps concurrency at 1
        // even with a big pool, high ceiling, AND a big per-token factor (#3947):
        // stacking never provisions more worktrees than the disk can hold.
        assert_eq!(resolve_dynamic_max_concurrent(8, 4, 1, 8, 8), 1);
        // A full volume (0 headroom) drops the cap to 0 — dispatch nothing.
        assert_eq!(resolve_dynamic_max_concurrent(8, 4, 0, 8, 8), 0);
    }

    #[test]
    fn test_dynamic_cap_cpu_headroom_bound() {
        // #3978 core: a saturated host (cpu headroom 1) caps concurrency at 1
        // even with a big pool, ample disk, AND a big per-token factor — never
        // start more concurrent sweep builds than the host's CPU/load headroom
        // can currently absorb (this is what starved the build-gate's own
        // `cargo` invocation of CPU in the #3978 incident).
        assert_eq!(resolve_dynamic_max_concurrent(8, 4, 8, 1, 8), 1);
        // Unlike disk, the cpu_headroom *term itself* is policy-floored at 1
        // (see `crate::cpu_headroom::cpu_headroom`), so a caller can never pass
        // 0 for it from the real end-to-end path — but the raw `min` here still
        // honors whatever is passed, including a defensive 0.
        assert_eq!(resolve_dynamic_max_concurrent(8, 4, 8, 0, 8), 0);
    }

    #[test]
    fn test_dynamic_cap_zero_pool_dispatches_nothing() {
        // No usable tokens ⇒ cap 0 regardless of the factor (0 × N = 0) ⇒ a
        // subsequent tick dispatches nothing (the spawn path would hard-fail
        // EX_CONFIG anyway).
        let cap = resolve_dynamic_max_concurrent(0, 2, 10, 10, 10);
        assert_eq!(cap, 0);
        let mut source = FakeSource::once((1..=3).map(issue).collect());
        let mut disp = RecordingDispatcher::default();
        let report = tick(&mut source, &mut disp, cap, false).unwrap();
        assert_eq!(report.dispatched, 0);
        assert_eq!(report.deferred_capacity, 3);
        assert!(disp.dispatched.is_empty());
    }

    #[test]
    fn test_dynamic_cap_per_token_factor_multiplies_token_axis() {
        // #3947 core: the token axis is `healthy × per-token`, not `healthy × 1`.
        // 3 healthy accounts × factor 2 = 6, bounded only by disk/cpu/ceiling.
        assert_eq!(resolve_dynamic_max_concurrent(3, 2, 100, 100, 100), 6);
        // 2 healthy × factor 3 = 6.
        assert_eq!(resolve_dynamic_max_concurrent(2, 3, 100, 100, 100), 6);
        // The product is still clamped by disk, cpu, and the operator ceiling.
        assert_eq!(resolve_dynamic_max_concurrent(3, 2, 4, 100, 100), 4, "disk clamps the product");
        assert_eq!(resolve_dynamic_max_concurrent(3, 2, 100, 4, 100), 4, "cpu clamps the product");
        assert_eq!(
            resolve_dynamic_max_concurrent(3, 2, 100, 100, 5),
            5,
            "ceiling clamps the product"
        );
    }

    #[test]
    fn test_dynamic_cap_one_healthy_account_factor_two_dispatches_two() {
        // The load-bearing #3947 scenario (6/7 accounts at their weekly ceiling):
        // ONE healthy account with factor 2 must allow TWO concurrent sweeps —
        // the pre-#3947 implicit 1:1 cap would have collapsed this to 1.
        let cap = resolve_dynamic_max_concurrent(1, 2, 120, 120, 3);
        assert_eq!(cap, 2, "1 healthy × per-token 2 = 2 (disk 120, cpu 120, ceiling 3 don't bind)");

        // And that cap actually dispatches 2 (not 1) against a 2-deep backlog.
        let mut source = FakeSource::once((1..=2).map(issue).collect());
        let mut disp = RecordingDispatcher::default();
        let report = tick(&mut source, &mut disp, cap, false).unwrap();
        assert_eq!(report.dispatched, 2, "1-healthy/factor-2 dispatches 2 concurrent sweeps");
        assert_eq!(report.deferred_capacity, 0);
        assert_eq!(disp.dispatched.len(), 2);
    }

    #[test]
    fn test_dynamic_cap_zero_factor_degrades_to_one() {
        // A mis-set factor 0 must NOT collapse the cap to zero — it degrades to
        // the pre-#3947 one-sweep-per-account behavior (floor of 1).
        assert_eq!(resolve_dynamic_max_concurrent(4, 0, 100, 100, 100), 4);
    }

    #[test]
    fn test_dynamic_cap_token_axis_jump_bounded_by_cpu_the_3978_scenario() {
        // The exact incident this issue fixes: several exhausted token
        // accounts reset at once, jumping the healthy-token count from 2 to
        // 14 (a pre-#3978 formula would raise the cap to 14 × per-token
        // regardless of host load). With concurrent Rust builds already
        // saturating the host (cpu headroom backed off to 3), the cap must
        // stay bounded at 3, not jump to a CPU-starving 28.
        let cap = resolve_dynamic_max_concurrent(14, 2, 100, 3, 100);
        assert_eq!(cap, 3, "cpu headroom protects the host even as the token axis spikes");
    }

    // ===================================================================
    // Dynamic cap composed with tick — scale-up / scale-to-zero (#3811)
    // ===================================================================

    #[test]
    fn test_scale_up_with_growing_backlog_bounded_by_dynamic_cap() {
        // Fixed resources: pool=4, factor=1, disk=10, cpu=10, ceiling=10 ⇒
        // dynamic cap 4. As the backlog grows tick-over-tick, effective
        // concurrency scales up but is bounded by the cap (min(cap, backlog)).
        let cap = resolve_dynamic_max_concurrent(4, 1, 10, 10, 10);
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
        let cap = resolve_dynamic_max_concurrent(5, 1, 5, 5, 5);
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
            r#"{"autonomous": {"perTokenConcurrency": 4, "cpuUtilizationTarget": 0.6, "estCoresPerSweep": 3.5, "workFinder": {"enabled": true, "intervalSecs": 90, "maxConcurrent": 5, "maxAdmissionsPerTick": 4}}}"#,
        );
        assert_eq!(
            read_work_finder_config(tmp.path()),
            WorkFinderConfig {
                enabled: Some(true),
                interval_secs: Some(90),
                max_concurrent: Some(5),
                max_admissions_per_tick: Some(4),
                per_token_concurrency: Some(4),
                cpu_utilization_target: Some(0.6),
                est_cores_per_sweep: Some(3.5),
            }
        );
    }

    // ===================================================================
    // cpuUtilizationTarget / estCoresPerSweep config parsing (#4032)
    // ===================================================================

    #[test]
    fn test_config_cpu_knobs_read_without_work_finder_block() {
        // Both knobs live at the `autonomous` level (#4032), so they are read
        // even when the `workFinder` sub-block is absent — mirroring
        // `perTokenConcurrency`.
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous": {"cpuUtilizationTarget": 0.5, "estCoresPerSweep": 1.5}}"#,
        );
        let cfg = read_work_finder_config(tmp.path());
        assert_eq!(cfg.cpu_utilization_target, Some(0.5));
        assert_eq!(cfg.est_cores_per_sweep, Some(1.5));
        assert_eq!(cfg.enabled, None);
    }

    #[test]
    fn test_config_integer_cpu_knobs_parse_as_f64() {
        // `Value::as_f64` handles integer JSON as well as float — assert it
        // (#4032 AC: "estCoresPerSweep": 2 as well as 2.0).
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"estCoresPerSweep": 2}}"#);
        let cfg = read_work_finder_config(tmp.path());
        assert_eq!(cfg.est_cores_per_sweep, Some(2.0));
    }

    #[test]
    fn test_config_out_of_range_cpu_utilization_target_drops_to_none() {
        for bad in ["0", "-1", "1.5"] {
            let tmp = tempfile::tempdir().unwrap();
            write_config(
                tmp.path(),
                &format!(r#"{{"autonomous": {{"cpuUtilizationTarget": {bad}}}}}"#),
            );
            assert_eq!(
                read_work_finder_config(tmp.path()).cpu_utilization_target,
                None,
                "cpuUtilizationTarget={bad} must drop to None, not be clamped"
            );
        }
    }

    #[test]
    fn test_config_out_of_range_est_cores_per_sweep_drops_to_none() {
        for bad in ["0", "-2"] {
            let tmp = tempfile::tempdir().unwrap();
            write_config(
                tmp.path(),
                &format!(r#"{{"autonomous": {{"estCoresPerSweep": {bad}}}}}"#),
            );
            assert_eq!(
                read_work_finder_config(tmp.path()).est_cores_per_sweep,
                None,
                "estCoresPerSweep={bad} must drop to None, not be clamped"
            );
        }
    }

    #[test]
    fn test_config_wrong_json_type_cpu_knobs_drop_to_none() {
        // A string, bool, or null where a number is expected must not panic
        // and must resolve to None (soft-fail to env/default resolution).
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous": {"cpuUtilizationTarget": "0.8", "estCoresPerSweep": true}}"#,
        );
        let cfg = read_work_finder_config(tmp.path());
        assert_eq!(cfg.cpu_utilization_target, None);
        assert_eq!(cfg.est_cores_per_sweep, None);

        let tmp2 = tempfile::tempdir().unwrap();
        write_config(tmp2.path(), r#"{"autonomous": {"estCoresPerSweep": null}}"#);
        assert_eq!(read_work_finder_config(tmp2.path()).est_cores_per_sweep, None);
    }

    #[test]
    fn test_config_per_token_concurrency_read_without_work_finder_block() {
        // `perTokenConcurrency` lives at the `autonomous` level (#3947), so it is
        // read even when the `workFinder` sub-block is absent.
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"perTokenConcurrency": 3}}"#);
        let cfg = read_work_finder_config(tmp.path());
        assert_eq!(cfg.per_token_concurrency, Some(3));
        assert_eq!(cfg.enabled, None);
        assert_eq!(cfg.max_concurrent, None);
    }

    #[test]
    fn test_config_zero_per_token_concurrency_drops_to_none() {
        // A zero factor in config is treated as absent so it falls through to
        // env/default rather than collapsing the token axis to zero.
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"perTokenConcurrency": 0}}"#);
        assert_eq!(read_work_finder_config(tmp.path()).per_token_concurrency, None);
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
    // config_resolver migration (#4058) — tier precedence
    // ===================================================================

    fn write_project_config(dir: &Path, body: &str) {
        let full = dir.join(crate::config_resolver::PROJECT_CONFIG_REL);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, body).unwrap();
    }

    fn write_local_config(dir: &Path, body: &str) {
        let full = dir.join(crate::config_resolver::LOCAL_CONFIG_REL);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, body).unwrap();
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_project_tier_only_is_honored_like_legacy() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_project_config(
            tmp.path(),
            r#"{"autonomous": {"perTokenConcurrency": 4, "workFinder": {"enabled": true, "maxConcurrent": 5}}}"#,
        );
        let cfg = read_work_finder_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(cfg.enabled, Some(true));
        assert_eq!(cfg.max_concurrent, Some(5));
        assert_eq!(cfg.per_token_concurrency, Some(4));
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_project_tier_overrides_legacy_overlap_and_supplies_non_overlap() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous": {"workFinder": {"enabled": true, "maxConcurrent": 5, "intervalSecs": 60}}}"#,
        );
        write_project_config(tmp.path(), r#"{"autonomous": {"workFinder": {"maxConcurrent": 9}}}"#);
        let cfg = read_work_finder_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        // Overlapping `maxConcurrent` -> project tier wins.
        assert_eq!(cfg.max_concurrent, Some(9));
        // Non-overlapping keys still supplied by the legacy tier.
        assert_eq!(cfg.enabled, Some(true));
        assert_eq!(cfg.interval_secs, Some(60));
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_local_tier_overrides_legacy_and_project() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"workFinder": {"maxConcurrent": 5}}}"#);
        write_project_config(tmp.path(), r#"{"autonomous": {"workFinder": {"maxConcurrent": 9}}}"#);
        write_local_config(tmp.path(), r#"{"autonomous": {"workFinder": {"maxConcurrent": 2}}}"#);
        let cfg = read_work_finder_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(cfg.max_concurrent, Some(2));
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
    // resolve_max_admissions_per_tick_with_config — env > config > default
    // (#4234)
    // ===================================================================

    #[test]
    #[serial]
    fn test_resolve_max_admissions_per_tick_with_config_precedence() {
        std::env::remove_var(WORK_FINDER_MAX_ADMISSIONS_PER_TICK_ENV);

        // Default when neither env nor config set.
        assert_eq!(
            resolve_max_admissions_per_tick_with_config(&WorkFinderConfig::default()),
            DEFAULT_MAX_ADMISSIONS_PER_TICK
        );

        // Config used when env unset.
        let cfg = WorkFinderConfig {
            max_admissions_per_tick: Some(7),
            ..Default::default()
        };
        assert_eq!(resolve_max_admissions_per_tick_with_config(&cfg), 7);

        // Env overrides config.
        std::env::set_var(WORK_FINDER_MAX_ADMISSIONS_PER_TICK_ENV, "1");
        assert_eq!(resolve_max_admissions_per_tick_with_config(&cfg), 1);

        // A zero/garbage env value is ignored; config still wins over default.
        std::env::set_var(WORK_FINDER_MAX_ADMISSIONS_PER_TICK_ENV, "0");
        assert_eq!(resolve_max_admissions_per_tick_with_config(&cfg), 7);
        std::env::set_var(WORK_FINDER_MAX_ADMISSIONS_PER_TICK_ENV, "nope");
        assert_eq!(resolve_max_admissions_per_tick_with_config(&cfg), 7);
        std::env::remove_var(WORK_FINDER_MAX_ADMISSIONS_PER_TICK_ENV);
    }

    #[test]
    fn test_read_work_finder_config_parses_max_admissions_per_tick() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"workFinder": {"maxAdmissionsPerTick": 5}}}"#);
        let cfg = read_work_finder_config(tmp.path());
        assert_eq!(cfg.max_admissions_per_tick, Some(5));
    }

    #[test]
    fn test_read_work_finder_config_drops_zero_max_admissions_per_tick() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"workFinder": {"maxAdmissionsPerTick": 0}}}"#);
        let cfg = read_work_finder_config(tmp.path());
        assert_eq!(cfg.max_admissions_per_tick, None, "a zero cap is treated as absent");
    }

    #[test]
    #[serial]
    fn test_resolve_per_token_concurrency_precedence() {
        std::env::remove_var(PER_TOKEN_CONCURRENCY_ENV);

        // Default (2) when neither env nor config set.
        assert_eq!(
            resolve_per_token_concurrency(&WorkFinderConfig::default()),
            DEFAULT_PER_TOKEN_CONCURRENCY
        );
        assert_eq!(DEFAULT_PER_TOKEN_CONCURRENCY, 2);

        // Config used when env unset.
        let cfg = WorkFinderConfig {
            per_token_concurrency: Some(4),
            ..Default::default()
        };
        assert_eq!(resolve_per_token_concurrency(&cfg), 4);

        // Env overrides config.
        std::env::set_var(PER_TOKEN_CONCURRENCY_ENV, "3");
        assert_eq!(resolve_per_token_concurrency(&cfg), 3);

        // A zero/garbage env value is ignored; config still wins over default.
        std::env::set_var(PER_TOKEN_CONCURRENCY_ENV, "0");
        assert_eq!(resolve_per_token_concurrency(&cfg), 4);
        std::env::set_var(PER_TOKEN_CONCURRENCY_ENV, "nope");
        assert_eq!(resolve_per_token_concurrency(&cfg), 4);
        std::env::remove_var(PER_TOKEN_CONCURRENCY_ENV);
    }

    #[test]
    #[serial]
    fn test_resolve_cpu_utilization_target_precedence() {
        use crate::cpu_headroom::{DEFAULT_UTILIZATION_TARGET, UTILIZATION_TARGET_ENV};
        std::env::remove_var(UTILIZATION_TARGET_ENV);

        // Default when neither env nor config set.
        assert!(
            (resolve_cpu_utilization_target(&WorkFinderConfig::default())
                - DEFAULT_UTILIZATION_TARGET)
                .abs()
                < f64::EPSILON
        );

        // Config used when env unset.
        let cfg = WorkFinderConfig {
            cpu_utilization_target: Some(0.6),
            ..Default::default()
        };
        assert!((resolve_cpu_utilization_target(&cfg) - 0.6).abs() < f64::EPSILON);

        // Env overrides config.
        std::env::set_var(UTILIZATION_TARGET_ENV, "0.5");
        assert!((resolve_cpu_utilization_target(&cfg) - 0.5).abs() < f64::EPSILON);
        std::env::remove_var(UTILIZATION_TARGET_ENV);
    }

    #[test]
    #[serial]
    fn test_resolve_cpu_est_cores_per_sweep_precedence() {
        use crate::cpu_headroom::{DEFAULT_EST_CORES_PER_SWEEP, EST_CORES_PER_SWEEP_ENV};
        std::env::remove_var(EST_CORES_PER_SWEEP_ENV);

        // Default when neither env nor config set.
        assert!(
            (resolve_cpu_est_cores_per_sweep(&WorkFinderConfig::default())
                - DEFAULT_EST_CORES_PER_SWEEP)
                .abs()
                < f64::EPSILON
        );

        // Config used when env unset.
        let cfg = WorkFinderConfig {
            est_cores_per_sweep: Some(4.0),
            ..Default::default()
        };
        assert!((resolve_cpu_est_cores_per_sweep(&cfg) - 4.0).abs() < f64::EPSILON);

        // Env overrides config.
        std::env::set_var(EST_CORES_PER_SWEEP_ENV, "1.0");
        assert!((resolve_cpu_est_cores_per_sweep(&cfg) - 1.0).abs() < f64::EPSILON);
        std::env::remove_var(EST_CORES_PER_SWEEP_ENV);
    }

    // ===================================================================
    // Token-capacity advisory transitions (#3902)
    // ===================================================================

    fn pressured_assessment() -> capacity::PressureAssessment {
        // token_limit 1 < disk 10, cpu 10, ceiling 10; 12 deferred ⇒ token-bound
        // + pressured.
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

    // ===================================================================
    // Gate-in-flight dispatch suppressor (#4084)
    // ===================================================================

    #[test]
    fn test_dispatch_held_per_root_gate_in_flight_holds_only_its_own_root() {
        // A root whose gate run is in flight is held; a sibling with no gate in
        // flight is NOT — the #3930 per-repo isolation contract must survive.
        let states = WorkspaceHealthStates::new();
        let root_a = std::path::PathBuf::from("/tmp/repo-a");
        let root_b = std::path::PathBuf::from("/tmp/repo-b");
        states.get_or_create(&root_a).set_gate_in_flight(true);
        let held = dispatch_held_per_root(&states, &[root_a, root_b], true);
        assert_eq!(held, vec![true, false], "only the in-flight root is held");
    }

    #[test]
    fn test_dispatch_held_per_root_suppressor_disabled_is_is_halted_only() {
        // With the suppressor off, the in-flight term drops out entirely — the
        // result is byte-for-byte the pre-#4084 `is_halted`-only vector, even
        // for a root with a gate run in flight.
        let states = WorkspaceHealthStates::new();
        let root_a = std::path::PathBuf::from("/tmp/repo-a");
        let root_b = std::path::PathBuf::from("/tmp/repo-b");
        states.get_or_create(&root_a).set_gate_in_flight(true);
        states.get_or_create(&root_b).set_halted(true);
        let held = dispatch_held_per_root(&states, &[root_a.clone(), root_b.clone()], false);
        assert_eq!(
            held,
            vec![false, true],
            "suppressor off ⇒ gate-in-flight is ignored; only verified-red holds"
        );
        // Sanity: with the suppressor on, root_a is additionally held.
        let held_on = dispatch_held_per_root(&states, &[root_a, root_b], true);
        assert_eq!(held_on, vec![true, true]);
    }

    #[test]
    fn test_dispatch_held_per_root_with_drain_holds_every_root() {
        // A daemon-global scheduled drain (#4090) holds EVERY root at once,
        // regardless of each root's gate state — the merge with #4084 must not
        // let the per-root gate term shadow the global drain term.
        let states = WorkspaceHealthStates::new();
        let root_a = std::path::PathBuf::from("/tmp/repo-a");
        let root_b = std::path::PathBuf::from("/tmp/repo-b");
        // Neither root is verified-red nor has a gate in flight.
        let held = dispatch_held_per_root_with_drain(
            &states,
            &[root_a, root_b],
            true, // suppressor on
            true, // draining
        );
        assert_eq!(
            held,
            vec![true, true],
            "a scheduled drain holds every root regardless of gate state"
        );
    }

    #[test]
    fn test_dispatch_held_per_root_with_drain_gate_still_per_root_when_not_draining() {
        // With no drain in progress the gate-in-flight term stays strictly
        // per-root: only the root whose gate run is in flight is held, its
        // sibling keeps dispatching (#3930 isolation contract survives #4090).
        let states = WorkspaceHealthStates::new();
        let root_a = std::path::PathBuf::from("/tmp/repo-a");
        let root_b = std::path::PathBuf::from("/tmp/repo-b");
        states.get_or_create(&root_a).set_gate_in_flight(true);
        let held = dispatch_held_per_root_with_drain(
            &states,
            &[root_a, root_b],
            true,  // suppressor on
            false, // not draining
        );
        assert_eq!(
            held,
            vec![true, false],
            "gate-in-flight holds only its own root when not draining"
        );
    }

    #[test]
    fn test_dispatch_held_per_root_with_drain_terms_are_independent() {
        // Both terms compose additively: a drain holds a healthy root, and a
        // gate in flight holds its own root — with the drain on, every root is
        // held whether or not its gate is in flight.
        let states = WorkspaceHealthStates::new();
        let root_a = std::path::PathBuf::from("/tmp/repo-a"); // gate in flight
        let root_b = std::path::PathBuf::from("/tmp/repo-b"); // healthy
        states.get_or_create(&root_a).set_gate_in_flight(true);
        let held = dispatch_held_per_root_with_drain(
            &states,
            &[root_a, root_b],
            true, // suppressor on
            true, // draining
        );
        assert_eq!(held, vec![true, true], "drain OR gate-in-flight: both roots held");
    }

    #[test]
    fn test_dispatch_held_per_root_with_drain_no_drain_matches_per_root() {
        // With `draining = false` the result is byte-for-byte the plain
        // per-root vector — the drain fold is a pure superset, never a
        // regression of the #4084 / #3930 semantics.
        let states = WorkspaceHealthStates::new();
        let root_a = std::path::PathBuf::from("/tmp/repo-a");
        let root_b = std::path::PathBuf::from("/tmp/repo-b");
        states.get_or_create(&root_a).set_gate_in_flight(true);
        states.get_or_create(&root_b).set_halted(true);
        let roots = [root_a, root_b];
        let plain = dispatch_held_per_root(&states, &roots, true);
        let with_drain = dispatch_held_per_root_with_drain(&states, &roots, true, false);
        assert_eq!(plain, with_drain, "draining=false ⇒ identical to dispatch_held_per_root");
    }

    #[test]
    fn test_gate_in_flight_root_dispatches_zero_new_sweeps() {
        // End-to-end through `tick_multi`: a root marked held (as
        // `dispatch_held_per_root` would for a gate in flight) dispatches
        // nothing, while its healthy sibling gets the shared slot.
        let states = WorkspaceHealthStates::new();
        let root_a = std::path::PathBuf::from("/tmp/repo-a");
        let root_b = std::path::PathBuf::from("/tmp/repo-b");
        states.get_or_create(&root_a).set_gate_in_flight(true);
        let halted = dispatch_held_per_root(&states, &[root_a, root_b], true);

        let mut multi = vec![
            (FakeSource::once(vec![issue(1)]), RecordingDispatcher::default()),
            (FakeSource::once(vec![issue(10)]), RecordingDispatcher::default()),
        ];
        let report = tick_multi(&mut multi, &[], 10, &halted);

        assert!(report.halted, "the gated root marks the tick as halted");
        assert!(
            multi[0].1.dispatched.is_empty(),
            "root with a gate run in flight dispatches zero new sweeps"
        );
        assert_eq!(
            multi[1].1.dispatched,
            vec![10],
            "sibling root with no gate in flight is unaffected"
        );
    }

    // ===================================================================
    // Missing-root hygiene (Issue #4326)
    // ===================================================================

    #[test]
    fn test_filter_missing_roots_skips_only_the_missing_one() {
        // A registry with one existing and one missing root dispatches only
        // into the existing root — the missing one is dropped, not the whole
        // tick.
        let tmp = tempfile::tempdir().unwrap();
        let existing = tmp.path().to_path_buf();
        let missing = tmp.path().join("does-not-exist");
        let mut warned = HashSet::new();

        let filtered = filter_missing_roots(vec![existing.clone(), missing.clone()], &mut warned);

        assert_eq!(filtered, vec![existing], "only the existing root survives");
        assert!(warned.contains(&missing), "the missing root is tracked as warned");
    }

    #[test]
    fn test_filter_missing_roots_all_missing_does_not_fall_back_to_cwd() {
        // The all-missing case must yield an EMPTY root list, never a silent
        // fallback to the daemon's cwd (that fallback is
        // `effective_roots`'s exclusive, empty-*registry* behavior — a
        // non-empty registry whose entries all happen to be missing on disk
        // is a distinct case and must not reuse it).
        let tmp = tempfile::tempdir().unwrap();
        let missing_a = tmp.path().join("gone-a");
        let missing_b = tmp.path().join("gone-b");
        let mut warned = HashSet::new();

        let filtered = filter_missing_roots(vec![missing_a, missing_b], &mut warned);

        assert!(filtered.is_empty(), "an all-missing registry yields zero roots, not cwd");
        assert_eq!(warned.len(), 2);
    }

    #[test]
    fn test_filter_missing_roots_warns_once_then_recovers() {
        // The `warned` set only ever tracks roots missing on the *current*
        // call — a caller that re-checks each tick (as the work-finder loop
        // does) naturally re-warns if a root disappears again after
        // recovering, and stays silent tick-over-tick while it remains
        // missing (the loop only logs for roots newly inserted into
        // `warned`, exercised by the loop itself, not this pure-function
        // test — this test just verifies the recovery bookkeeping).
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("flaky");
        let mut warned = HashSet::new();

        // First call: missing.
        let filtered = filter_missing_roots(vec![root.clone()], &mut warned);
        assert!(filtered.is_empty());
        assert!(warned.contains(&root));

        // Root reappears (e.g. a remounted volume).
        std::fs::create_dir_all(&root).unwrap();
        let filtered = filter_missing_roots(vec![root.clone()], &mut warned);
        assert_eq!(filtered, vec![root.clone()]);
        assert!(warned.is_empty(), "a recovered root is no longer tracked as missing");

        // Root disappears again — should be treated as newly-missing (i.e.
        // would re-warn), not silently skipped as "already known".
        std::fs::remove_dir_all(&root).unwrap();
        let filtered = filter_missing_roots(vec![root.clone()], &mut warned);
        assert!(filtered.is_empty());
        assert!(warned.contains(&root));
    }

    #[test]
    fn test_filter_missing_roots_empty_input_returns_empty() {
        let mut warned = HashSet::new();
        assert!(filter_missing_roots(vec![], &mut warned).is_empty());
        assert!(warned.is_empty());
    }
}
