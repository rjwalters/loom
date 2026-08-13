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
//!    (Phase B, #3811; simplified in #4512; token axis removed / RAM headroom
//!    added in #5270): `min(disk headroom, ram headroom, configured max)`.
//!    `dispatch()` already flips `loom:issue → loom:building`, acquires the
//!    per-issue `mkdir`-atomic claim lock, and spawns the rotated-token child.
//!
//! # Concurrency scaling (Phase B, #3811; CPU term removed in #4512; token axis removed / RAM added in #5270)
//!
//! Phase A resolved a single fixed cap once at daemon startup. Phase B replaces
//! it with a cap **recomputed every tick** by
//! [`resolve_dynamic_max_concurrent`] from live inputs — the worktree-root
//! disk headroom ([`crate::disk_headroom::disk_headroom_limit`]), the host's
//! available-RAM headroom (#5270, [`crate::ram_headroom::ram_headroom_limit`]),
//! and the per-machine operator ceiling (`LOOM_WORK_FINDER_MAX_CONCURRENT` /
//! `autonomous.workFinder.maxConcurrent`). The effective per-tick concurrency
//! is then `min(dynamic_cap, backlog_depth)`: [`tick`] iterates the ready
//! `loom:issue` rows and stops at the cap, so concurrency scales **up** as the
//! backlog grows and drains to **zero** dispatches when the queue is empty —
//! all without a daemon restart, since disk/RAM/backlog are read fresh each
//! tick. Token-pool health ([`crate::tokens::token_pool_size`] /
//! [`crate::capacity::read_ranking`]) is still read every tick but, since
//! #5270, feeds spawn-time **selection** only (prefer fresher/healthier
//! accounts) — it is no longer a term in this `min(...)`.
//!
//! **#4512 removed the CPU term from this formula; #5270 removed the token
//! axis too, unconditionally on every auth path.** A fourth axis
//! (`cpu_headroom = (logical_cpus × cpuUtilizationTarget − consumed_cores) /
//! estCoresPerSweep`, #3978/#4031) used to be part of the `min(...)`. It priced
//! every sweep as a build, so it throttled the API-wait-dominated majority to
//! defend against the heavy-build minority — an 8-core worker measured **95%
//! idle** was capped at 2. The hard floors that meter genuinely *exhaustible*
//! resources stayed after that removal (the token axis, disk headroom), but
//! #5270 dropped the token axis too: operator direction was "we should only
//! ever limit parallelism based on the machine disk/RAM/CPU" — a metered API
//! key has no subscription window, and overage means even a subscription pool
//! no longer hard-stops at one either, so counting *healthy accounts* was
//! never really a proxy for this host's own capacity. RAM headroom
//! ([`crate::ram_headroom`]) joined disk headroom as the replacement machine
//! axis. The heavy stages still serialize where they actually occur, on the
//! machine-wide build slot ([`crate::build_slot`]). The host breaker (#4235,
//! [`crate::host_breaker`]) remains the load safety net that makes a
//! hand-tuned ceiling safe: a mis-set knob trips a **measured** breaker
//! instead of melting the host.
//!
//! **#4903 added a saturation brake on *admission* (not on the cap); #5270
//! retuned it into the primary CPU gate.** Removing the CPU term left the cap
//! with no term that reads the host at all, so a CPU-heavy workload (analog
//! simulation: minutes of sustained `ngspice`, not API-wait) could drive an
//! 8-core worker to 12× overcommit while the daemon still believed it had nine
//! free slots. [`crate::admission_brake`] closes that without resurrecting the
//! formula: once per tick it asks [`crate::cpu_headroom::is_host_saturated`]
//! and, when the answer is yes, holds **new** admissions
//! ([`TickReport::deferred_saturation`]) until the host recovers. Its default
//! threshold started generous (`4.0` load/core, a rarely-tripped backstop) and
//! was retuned by #5270 to `0.95` — the operator's literal "dumb mode" ask,
//! now the primary CPU admission gate rather than a backstop above the (now
//! nonexistent) token axis. In-flight sweeps are never touched.
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
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::Result;

use crate::capacity::{self, CapacityAdvisory};
use crate::disk_headroom::disk_headroom_limit;
use crate::event_bus::EventBus;
use crate::main_health_gate::{MainHealthState, WorkspaceHealthStates};
use crate::sweep_registry::{
    DispatchBackoffError, LiveClaimDispatchError, OpenPrDispatchError, ParkedIssueDispatchError,
    PreflightDispatchGate,
};
use crate::tokens::{token_pool_size, token_pool_size_at_dir};
use crate::types::{Event, WorkFinderTickSummary};
use crate::workspace_pool::WorkspacePool;
use crate::workspace_registry::{filter_missing_roots, WorkspaceRegistry};

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

/// Labels marking a **deliberate park** — a human (or an agent acting on a
/// human's behalf) has taken the issue out of the automation queue and it must
/// stay out until the label is cleared (Issue #4444).
///
/// This is the strict subset of [`SKIP_LABELS`] that survives *every* dispatch
/// route, so it is the constant the dispatch-time guard in
/// `SweepRegistry::dispatch()` (step 2.7) consults. It deliberately EXCLUDES
/// [`BUILDING_LABEL`]: `loom:building` is legitimately present on the daemon's
/// own in-flight claim, so a guard that refused it would break the watchdogs'
/// cancel-and-re-dispatch and the reaper's checkpoint-resume — both of which
/// re-dispatch an issue the daemon itself already flipped to `loom:building`.
pub const PARK_LABELS: &[&str] = &["loom:blocked", "loom:operator-only"];

/// The daemon's own claim label. Disqualifies a *fresh* work-finder candidate
/// (a `loom:building` row is already being worked), but is NOT a park — see
/// [`PARK_LABELS`].
pub const BUILDING_LABEL: &str = "loom:building";

/// Labels that disqualify an issue from dispatch even if it still appears in
/// the `loom:issue`-filtered listing.
///
/// A `loom:issue` row should never itself carry these (they are mutually
/// exclusive states in the `.github/labels.yml` state machine), but `gh`'s
/// label cache can be briefly stale, so the finder checks defensively.
///
/// Composed as [`BUILDING_LABEL`] + [`PARK_LABELS`] rather than re-listing the
/// label strings, so the two constants can never drift apart (#4444).
pub const SKIP_LABELS: &[&str] = &[BUILDING_LABEL, PARK_LABELS[0], PARK_LABELS[1]];

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
    /// The issue's markdown body, when the listing supplied it (#4827).
    ///
    /// The ETag-cached REST listing already returns it at zero extra cost, and
    /// dispatch reads the Curator's `<!-- loom:complexity=<tier> -->` marker out
    /// of it (see [`Self::complexity`]) to stratify the model-cost A/B
    /// experiment's arm assignment per issue. `None` (a synthetic item, or a
    /// listing without bodies) simply falls back to the `routine` stratum.
    pub body: Option<String>,
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
            body: None,
        }
    }

    /// Constructor carrying the issue's `createdAt` timestamp for age ordering.
    #[must_use]
    pub fn with_created_at(number: u32, labels: Vec<String>, created_at: Option<String>) -> Self {
        Self {
            number,
            labels,
            created_at,
            body: None,
        }
    }

    /// Builder-style setter for the issue body (#4827).
    #[must_use]
    pub fn with_body(mut self, body: Option<String>) -> Self {
        self.body = body;
        self
    }

    /// The Curator's `<!-- loom:complexity=<tier> -->` stratum for this issue,
    /// extracted from [`Self::body`] (#4827).
    ///
    /// `None` when no body was fetched or no marker is present — which
    /// [`crate::script_helpers::sweep_experiment::assign_arm`] treats as the
    /// `routine` stratum, exactly as before this field existed.
    #[must_use]
    pub fn complexity(&self) -> Option<&str> {
        self.body
            .as_deref()
            .and_then(crate::script_helpers::sweep_experiment::extract_complexity_marker)
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
    /// The issue's `<!-- loom:complexity=<tier> -->` stratum (#4827), carried
    /// from its [`WorkItem`] so pass 2's `dispatch()` can stratify the
    /// model-cost A/B arm assignment without re-fetching the body. Not part of
    /// the ordering keys — [`candidate_cmp`] ignores it.
    pub complexity: Option<String>,
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

    /// The set of issue numbers currently inside a **per-issue dispatch backoff
    /// window** after a failed dispatch (Issue #4485). Skipped exactly like
    /// [`quarantined`](Self::quarantined) — filtered out *before* the
    /// concurrency budget is filled, so a backed-off candidate never reserves a
    /// shared dispatch slot.
    ///
    /// Complements (does not replace) the registry-side step-2.8 guard: this
    /// keeps a backed-off issue from consuming a slot and from logging a refusal
    /// every tick, while the guard is the authoritative brake that also covers
    /// the watchdog / IPC / epic-supervisor dispatch paths.
    ///
    /// Defaults to empty so a dispatcher that does not model the backoff (e.g. a
    /// test fake) opts out with zero boilerplate.
    fn backed_off(&self) -> HashSet<u32> {
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
    /// `complexity` is the issue's `<!-- loom:complexity=<tier> -->` stratum
    /// (#4827), already extracted from the listing's issue body by the caller —
    /// the dispatcher never re-fetches it. Used ONLY to stratify the model-cost
    /// A/B experiment's arm assignment; `None` (no marker / no body) keeps the
    /// pre-#4827 `routine` stratum and is never an error.
    ///
    /// # Errors
    ///
    /// Returns an error when the dispatch fails (e.g. a claim-lock collision).
    /// The caller logs and counts it; it is never fatal.
    fn dispatch(&mut self, issue: u32, complexity: Option<&str>) -> Result<bool>;

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
// Last-tick publication (Issue #4761)
// ============================================================================

/// Process-global slot holding the most recent completed tick's summary.
///
/// Mirrors the "loop publishes, status reads" discipline
/// [`crate::auto_update::global_status_snapshot`] and
/// [`crate::host_breaker::global_snapshot`] already use: the work-finder loop
/// writes here at the end of every tick, and `build_daemon_status` reads it
/// back so a cross-process consumer (`loom-daemon health`) can see the last
/// tick's dispatch/skip breakdown without scraping the daemon log.
///
/// `None` (the initial value) honestly means "no tick has completed in this
/// process yet" — never "nothing was dispatched".
static LAST_TICK: OnceLock<Mutex<Option<WorkFinderTickSummary>>> = OnceLock::new();

fn last_tick_slot() -> &'static Mutex<Option<WorkFinderTickSummary>> {
    LAST_TICK.get_or_init(|| Mutex::new(None))
}

/// Publish `report` (as run under `max_concurrent`, completed at `at`) as the
/// most recent work-finder tick (Issue #4761). Called by both the
/// single-workspace and multi-workspace loops so the two can never diverge on
/// what "the last tick" means.
pub fn publish_tick_summary_at(
    report: &TickReport,
    max_concurrent: usize,
    at: chrono::DateTime<chrono::Utc>,
) {
    let summary = WorkFinderTickSummary {
        at,
        max_concurrent,
        seen: report.seen,
        dispatched: report.dispatched,
        skipped_labeled: report.skipped_labeled,
        skipped_in_flight: report.skipped_in_flight,
        skipped_quarantined: report.skipped_quarantined,
        skipped_pr_open: report.skipped_pr_open,
        skipped_peer_claim: report.skipped_peer_claim,
        skipped_backoff: report.skipped_backoff,
        deferred_capacity: report.deferred_capacity,
        deferred_ramp_cap: report.deferred_ramp_cap,
        deferred_saturation: report.deferred_saturation,
        errors: report.errors,
        halted: report.halted,
        saturation_held: report.saturation_held,
        collisions: report.collisions,
    };
    *last_tick_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(summary);
}

/// [`publish_tick_summary_at`] stamped with the current wall clock.
pub fn publish_tick_summary(report: &TickReport, max_concurrent: usize) {
    publish_tick_summary_at(report, max_concurrent, chrono::Utc::now());
}

/// Read back the most recently published tick summary, or `None` when no tick
/// has completed in this process (Issue #4761).
#[must_use]
pub fn last_tick_summary() -> Option<WorkFinderTickSummary> {
    last_tick_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Test-only reset of the process-global last-tick slot.
#[cfg(test)]
fn reset_last_tick_summary() {
    *last_tick_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
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
    /// Issues skipped because they carried a [`SKIP_LABELS`] entry — either in
    /// the candidate listing this tick, or at dispatch time when the
    /// dispatch-side [`PARK_LABELS`] guard (#4444) found a park label the
    /// listing had not caught yet (`ParkedIssueDispatchError`). Both are the
    /// same reason, so they share one counter rather than splitting a stale-cache
    /// race across `labeled-skip` and `error(s)`.
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
    /// Issues deferred to a future tick because the **saturation admission
    /// brake** (#4903, [`crate::admission_brake`]) held new admissions: the host
    /// is already at/over the configured load-per-core hold threshold, so adding
    /// work would only slow the sweeps already running.
    ///
    /// Deliberately its own counter, not folded into
    /// [`deferred_capacity`](Self::deferred_capacity): the concurrency cap was
    /// *not* reached — the host was. Conflating them would report a token/disk
    /// shortage on a machine whose only problem is that it is already full, and
    /// send an operator to raise a knob that is not binding.
    pub deferred_saturation: usize,
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
    /// Issues skipped because they are inside a per-issue dispatch-backoff
    /// window after a failed dispatch (Issue #4485) — either filtered out before
    /// the capacity gate via [`WorkDispatcher::backed_off`], or refused by the
    /// registry's step-2.8 guard with the typed [`DispatchBackoffError`].
    /// Attributed here rather than to [`errors`](Self::errors) because a backoff
    /// refusal is a deliberate skip, not a failure.
    pub skipped_backoff: usize,
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
    /// True when the saturation admission brake (#4903) was engaged for this
    /// tick. Reported separately from
    /// [`deferred_saturation`](Self::deferred_saturation) so "the host was
    /// holding" is visible even when the backlog was empty and nothing was
    /// deferred — otherwise a saturated host with no queued work is
    /// indistinguishable from a healthy idle one, which is the exact reporting
    /// gap #4903 was filed on.
    pub saturation_held: bool,
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
    tick_with_saturation_brake(
        source,
        dispatcher,
        max_concurrent,
        halted,
        max_admissions_per_tick,
        false,
    )
}

/// Like [`tick_with_admission_cap`], but additionally honors the **saturation
/// admission brake** (#4903, [`crate::admission_brake`]): when
/// `saturation_held` is `true` the host is already at/over its load-per-core
/// hold threshold, so this tick admits **no new sweeps** and attributes every
/// otherwise-eligible candidate to [`TickReport::deferred_saturation`].
/// `false` (what [`tick_with_admission_cap`] passes) reduces to the pre-#4903
/// behavior byte-for-byte.
///
/// Three properties are load-bearing, and each maps to an acceptance criterion
/// of #4903:
///
/// 1. **In-flight sweeps are never touched.** The brake is applied *inside the
///    candidate loop*, which only ever visits ready `loom:issue` rows that are
///    not already in flight. There is no branch here — and no path from this
///    module — that cancels, signals, or reaps a running sweep. A held tick
///    simply dispatches nothing and returns, and the running sweeps drain
///    normally, which is how the host recovers.
/// 2. **The hold is re-evaluated every tick.** Nothing latches: the caller
///    re-samples load each tick and passes a fresh `saturation_held`, so the
///    moment the host drops back under the threshold admissions resume (the
///    sticky, cool-down-bearing guard is the host breaker, #4235 — deliberately
///    a different mechanism).
/// 3. **A healthy host is unchanged.** With `saturation_held = false` this
///    function is the pre-#4903 code path exactly, so an idle 8-core host still
///    fills its configured cap — no re-introduction of the over-throttling
///    #4512 removed.
///
/// The check sits **before** the concurrency-cap check so a saturated host
/// reports `deferred-saturation`, not a misleading `deferred-capacity` (the cap
/// was not reached; the host was).
///
/// # Errors
///
/// Same as [`tick`].
pub fn tick_with_saturation_brake(
    source: &mut impl WorkSource,
    dispatcher: &mut impl WorkDispatcher,
    max_concurrent: usize,
    halted: bool,
    max_admissions_per_tick: usize,
    saturation_held: bool,
) -> Result<TickReport> {
    let ready = source.list_ready_issues()?;
    let mut report = TickReport {
        seen: ready.len(),
        // Record the brake's engagement even on a tick that defers nothing, so a
        // saturated host with an empty backlog still reads as "holding" rather
        // than "idle" (#4903).
        saturation_held,
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
    let backed_off = dispatcher.backed_off();
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
        // 2b2. Dispatch backoff (#4485): this issue's last dispatch failed and
        //      its backoff window has not elapsed. Skipped here — before the
        //      capacity gate, like quarantine — so it neither reserves a slot
        //      nor re-flips its label every tick.
        if backed_off.contains(&item.number) {
            report.skipped_backoff += 1;
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
        // 2d. Saturation admission brake (#4903) — the host is already at/over
        //     its load-per-core hold threshold, so hold this candidate and
        //     re-check next tick. Checked BEFORE the concurrency cap so the
        //     deferral is attributed to the host, not to a cap that is not
        //     actually binding. Running sweeps are untouched: this loop only
        //     ever sees candidates that are NOT in flight.
        if saturation_held {
            report.deferred_saturation += 1;
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
        match dispatcher.dispatch(item.number, item.complexity()) {
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
                } else if let Some(parked) = e.downcast_ref::<ParkedIssueDispatchError>() {
                    // Park-label guard refusal (#4444). The candidate query
                    // already filters `SKIP_LABELS`, so reaching this means the
                    // listing was stale relative to the forge — the dispatch-time
                    // probe is the authoritative read. Same reason as the query
                    // filter, so it lands on the same `labeled-skip` counter
                    // rather than on `error(s)`.
                    report.skipped_labeled += 1;
                    log::info!(
                        "work_finder: skipping issue #{} — it carries `{}` on the forge \
                         (#4444 park-label guard; the candidate listing was stale)",
                        item.number,
                        parked.label
                    );
                } else if e.downcast_ref::<DispatchBackoffError>().is_some() {
                    // Dispatch backoff refusal (#4485) — a deliberate skip, not
                    // a failure. Reachable even when `backed_off()` was empty at
                    // tick start (the window can be armed mid-tick by a reap).
                    report.skipped_backoff += 1;
                    log::info!("work_finder: skipping issue #{} — {e}", item.number);
                } else if e.downcast_ref::<LiveClaimDispatchError>().is_some() {
                    // Live-claim guard refusal (#4556): a sweep process for this
                    // issue is confirmed still running, so this candidate is
                    // genuinely in flight — `in_flight()` just could not see it
                    // (a reverted label, a released lock, or another daemon
                    // instance on this host). Counted as an in-flight skip rather
                    // than an error, but logged at WARN: reaching here means one
                    // of those weaker signals lied, which is the #4275
                    // duplicate-dispatch storm signature and worth an operator's
                    // attention.
                    report.skipped_in_flight += 1;
                    log::warn!("work_finder: skipping issue #{} — {e}", item.number);
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

/// Fold a per-root claude-wrapper pre-flight-advisory hold (#5030) on top of the
/// #3930 verified-red + #4084 gate-in-flight per-root holds.
///
/// `preflight_held` is a slice **parallel to `roots`**: `preflight_held[i] ==
/// true` means root `i`'s pre-flight advisory is tripped AND its half-open
/// breaker is currently in the "held" window (not a probe tick), so no new
/// dispatch should go to that root this tick. The caller computes it from each
/// root's own [`SweepRegistry::preflight_dispatch_gate`](crate::sweep_registry::SweepRegistry::preflight_dispatch_gate)
/// — a broken workspace burns at most one ~1s pre-flight death per probe
/// cooldown instead of one every tick.
///
/// The hold is strictly **per root**, matching the #3930 per-repo isolation
/// contract: a workspace with a broken `.mcp.json` never halts dispatch to a
/// healthy sibling repo. A missing entry (`preflight_held.len() < roots.len()`)
/// defaults to *not held*. With an all-`false` slice the result is byte-for-byte
/// [`dispatch_held_per_root`].
#[must_use]
pub fn dispatch_held_per_root_with_preflight(
    health_states: &WorkspaceHealthStates,
    roots: &[std::path::PathBuf],
    suppress_dispatch_during_gate: bool,
    preflight_held: &[bool],
) -> Vec<bool> {
    dispatch_held_per_root(health_states, roots, suppress_dispatch_during_gate)
        .into_iter()
        .enumerate()
        .map(|(i, h)| h || preflight_held.get(i).copied().unwrap_or(false))
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
    tick_multi_with_saturation_brake(
        workspaces,
        priorities,
        max_concurrent,
        halted,
        max_admissions_per_tick,
        false,
    )
}

/// Like [`tick_multi_with_admission_cap`], but additionally honors the
/// **saturation admission brake** (#4903) — the multi-workspace analogue of
/// [`tick_with_saturation_brake`]; see it for the full rationale and the three
/// load-bearing properties.
///
/// The brake is **daemon-global**, not per-root: it measures the *host*, and
/// every workspace's sweeps run on that one host, so a single `saturation_held`
/// flag holds admissions across every repo at once (the same shape the
/// host-distress breaker and the scheduled drain already use). Unlike those two,
/// it is applied in pass 2 rather than folded into the per-root `halted` slice,
/// so its deferrals stay attributable to saturation instead of disappearing into
/// the main-health halt.
///
/// `false` (what [`tick_multi_with_admission_cap`] passes) reduces to the
/// pre-#4903 behavior byte-for-byte.
pub fn tick_multi_with_saturation_brake<S: WorkSource, D: WorkDispatcher>(
    workspaces: &mut [(S, D)],
    priorities: &[u32],
    max_concurrent: usize,
    halted: &[bool],
    max_admissions_per_tick: usize,
    saturation_held: bool,
) -> TickReport {
    use crate::workspace_registry::DEFAULT_WORKSPACE_PRIORITY;

    let mut report = TickReport {
        saturation_held,
        ..TickReport::default()
    };

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

    // Snapshot each workspace's dispatch-backoff set (#4485) alongside its
    // quarantined set — dropped in pass 1 for the same reason: a backed-off
    // candidate must not reserve a shared slot it cannot use.
    let backed_off_sets: Vec<HashSet<u32>> =
        workspaces.iter().map(|(_, d)| d.backed_off()).collect();

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
                // A rate-limit failure trips the global breaker (#4429) so the
                // NEXT tick skips its gh fan-out entirely; this tick still
                // isolates per-workspace as before.
                crate::rate_limit_breaker::global_observe_failure(&e.to_string(), "work_finder");
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
            // Dispatch backoff (#4485): a failing issue inside its backoff
            // window — drop before the global queue, like quarantine.
            if backed_off_sets[idx].contains(&item.number) {
                report.skipped_backoff += 1;
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
                complexity: item.complexity().map(str::to_owned),
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
        // Saturation admission brake (#4903) — daemon-global, checked before the
        // shared cap so the deferral names the host rather than a cap that is
        // not binding. In-flight sweeps across every workspace are untouched.
        if saturation_held {
            report.deferred_saturation += 1;
            continue;
        }
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
        match dispatcher.dispatch(cand.number, cand.complexity.as_deref()) {
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
                } else if let Some(parked) = e.downcast_ref::<ParkedIssueDispatchError>() {
                    // Park-label guard refusal (#4444) — see the single-workspace
                    // `tick` for the rationale. A labeled-skip, not a failure.
                    report.skipped_labeled += 1;
                    log::info!(
                        "work_finder: skipping issue #{} — it carries `{}` on the forge \
                         (#4444 park-label guard; the candidate listing was stale)",
                        cand.number,
                        parked.label
                    );
                } else if e.downcast_ref::<DispatchBackoffError>().is_some() {
                    // Dispatch backoff refusal (#4485) — see the single-workspace
                    // `tick` for the rationale. A skip, not a failure.
                    report.skipped_backoff += 1;
                    log::info!("work_finder: skipping issue #{} — {e}", cand.number);
                } else if e.downcast_ref::<LiveClaimDispatchError>().is_some() {
                    // Live-claim guard refusal (#4556) — see the single-workspace
                    // `tick` for the rationale. An in-flight skip, not a failure.
                    report.skipped_in_flight += 1;
                    log::warn!("work_finder: skipping issue #{} — {e}", cand.number);
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
    /// Names of **retired** config keys found in `autonomous` — currently
    /// `cpuUtilizationTarget` / `estCoresPerSweep` ([`DEPRECATED_CPU_CONFIG_KEYS`]),
    /// whose CPU-headroom admission term #4512 deleted.
    ///
    /// They are **accepted-but-ignored**, never a config error: a fleet's
    /// committed `.loom/config.json` must keep parsing across the upgrade. Their
    /// presence (at any value — no range filtering, since nothing consumes the
    /// value) is recorded here purely so
    /// [`warn_deprecated_cpu_knobs`] can log one deprecation line naming exactly
    /// which keys to delete.
    pub deprecated_cpu_keys: Vec<&'static str>,
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

    // `cpuUtilizationTarget` / `estCoresPerSweep` used to live at the
    // `autonomous` level too (#4032), feeding the CPU-headroom admission term
    // #4512 deleted. They are now accepted-but-ignored: note their presence for
    // the one-shot deprecation warning and parse nothing — no range filtering,
    // no type coercion, because no consumer reads the value any more. A
    // consumer's committed config keeps parsing unchanged (never a hard error).
    let deprecated_cpu_keys: Vec<&'static str> = DEPRECATED_CPU_CONFIG_KEYS
        .iter()
        .copied()
        .filter(|key| autonomous.get(*key).is_some_and(|v| !v.is_null()))
        .collect();

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
        deprecated_cpu_keys,
    }
}

/// Retired `autonomous.*` config keys, accepted-but-ignored since #4512 (they
/// fed the deleted CPU-headroom admission term, #3978/#4031).
pub const DEPRECATED_CPU_CONFIG_KEYS: [&str; 2] = ["cpuUtilizationTarget", "estCoresPerSweep"];

/// Retired env vars, accepted-but-ignored since #4512 — the env half of
/// [`DEPRECATED_CPU_CONFIG_KEYS`].
pub const DEPRECATED_CPU_ENV_VARS: [&str; 2] =
    ["LOOM_CPU_UTILIZATION_TARGET", "LOOM_EST_CORES_PER_SWEEP"];

/// One-shot guard so the deprecation warning is logged **once per process**, not
/// once per config read (the config is re-read on several paths, including every
/// `status` request).
static DEPRECATION_WARNED: std::sync::Once = std::sync::Once::new();

/// Render the deprecation notice for any retired CPU-headroom knob still set in
/// `config` or the environment — `None` when none is set (#4512).
///
/// Split out from [`warn_deprecated_cpu_knobs`] because the two channels an
/// operator actually watches are different processes: the **daemon** has a
/// logger (`~/.loom/daemon.log`) and warns through it, while a **CLI**
/// subcommand (`loom-daemon calibrate`) returns from `main` *before*
/// `setup_logging()` runs, so a `log::warn!` there is a silent no-op. The CLI
/// therefore prints this same string to stderr instead of relying on the log
/// (see `handle_calibrate_command`). One message, two delivery paths — never a
/// warning that exists only in a file nobody is tailing.
#[must_use]
pub fn deprecated_cpu_knob_notice(config: &WorkFinderConfig) -> Option<String> {
    let env_set: Vec<&str> = DEPRECATED_CPU_ENV_VARS
        .iter()
        .copied()
        .filter(|v| std::env::var_os(v).is_some())
        .collect();
    if config.deprecated_cpu_keys.is_empty() && env_set.is_empty() {
        return None;
    }
    let mut sources = Vec::new();
    if !config.deprecated_cpu_keys.is_empty() {
        sources.push(format!("config `autonomous.{{{}}}`", config.deprecated_cpu_keys.join(", ")));
    }
    if !env_set.is_empty() {
        sources.push(format!("env {}", env_set.join(", ")));
    }
    Some(format!(
        "{} set but IGNORED — #4512 removed the CPU-headroom term from the admission formula \
         (now min(token axis, disk headroom, maxConcurrent)). Tune \
         `autonomous.workFinder.maxConcurrent` for this machine instead; heavy build/test stages \
         are serialized by the machine-wide build slot (LOOM_BUILD_SLOTS), and the host-distress \
         breaker remains the load safety net. Delete the setting(s) to silence this warning.",
        sources.join(" and ")
    ))
}

/// Log a single deprecation warning naming any retired CPU-headroom knob still
/// set in config or the environment (#4512).
///
/// Accepted-but-ignored is a deliberate compatibility contract: a fleet upgrades
/// the daemon binary before it edits every repo's committed `.loom/config.json`,
/// so a stale key must **never** be a parse error — it must be a *visible*
/// no-op. Called once at daemon startup, it is internally idempotent via
/// [`std::sync::Once`], so extra call sites are free. CLI subcommands print
/// [`deprecated_cpu_knob_notice`] to stderr instead (no logger is initialized on
/// that path).
pub fn warn_deprecated_cpu_knobs(config: &WorkFinderConfig) {
    let Some(notice) = deprecated_cpu_knob_notice(config) else {
        return;
    };
    DEPRECATION_WARNED.call_once(|| {
        log::warn!("work_finder: {notice}");
    });
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
/// [`resolve_max_concurrent_with_config`] — and threaded through to
/// [`spawn_work_finder_task`] / [`spawn_multi_work_finder_task`] as a plain
/// `usize`. See [`WORK_FINDER_MAX_ADMISSIONS_PER_TICK_ENV`] for why this is a
/// deliberate startup-capture, not a per-tick re-read: the ramp cap's whole
/// purpose is to smooth admission *within* the live per-tick re-computation of
/// `max_concurrent`, so it does not itself need to be live — an operator
/// retuning it takes effect on the next daemon restart, exactly like
/// `configured_max` today.
#[must_use]
pub fn resolve_max_admissions_per_tick_with_config(config: &WorkFinderConfig) -> usize {
    env_max_admissions_per_tick()
        .or(config.max_admissions_per_tick)
        .unwrap_or(DEFAULT_MAX_ADMISSIONS_PER_TICK)
}

/// Compute the **machine-headroom dynamic concurrency cap** (Phase B, #3811;
/// CPU term removed in #4512; **token axis removed and RAM headroom added in
/// #5270**):
/// `min(disk_headroom, ram_headroom, configured_max)`.
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
/// the disk/RAM/configured ceiling). Keeping the cap as `min(disk, ram,
/// configured)` and letting `tick` apply the backlog bound is what makes
/// concurrency scale up with the backlog and drain to zero when it empties.
///
/// The three remaining bounds map directly to the resource each protects:
/// - `disk_headroom` — never provision more worktrees than the scratch volume
///   can hold at `LOOM_PER_WORKTREE_GB` each ([`crate::disk_headroom`]).
/// - `ram_headroom` — never provision more worktrees than the host's
///   currently-available memory can hold at `LOOM_PER_WORKTREE_RAM_GB` each
///   (#5270, [`crate::ram_headroom`]) — the operator-directed "dumb mode"
///   RAM gate, applied the same way disk headroom already was.
/// - `configured_max` — **the** per-machine admission knob
///   (`LOOM_WORK_FINDER_MAX_CONCURRENT` / `autonomous.workFinder.maxConcurrent`),
///   tuned empirically per host.
///
/// # Why there is no token-axis term any more (#5270)
///
/// Until #5270 a third axis sat in this `min(...)`: `token_limit ×
/// per_token_concurrency`, where `token_limit` was the count of *healthy*
/// (`available`) accounts read from the rotation ranking (#3902). That policy
/// was right when a rate-limited account genuinely stopped serving requests, but
/// it stopped being a real resource ceiling once accounts are provisioned with
/// **overage** (metered credit beyond the plan window) or a single API key
/// backed by a large credit balance — operator direction on #5270: "we can have
/// extra usage on our subscription tokens too... we should only ever limit
/// parallelism based on the machine disk/RAM/CPU." Counting *accounts* was never
/// a proxy for the daemon host's actual capacity to run more sweeps; it modeled
/// a per-plan-window ceiling that no longer holds unconditionally.
///
/// `.ranking` health is **not** deleted — it still drives spawn-time
/// **selection** ([`crate::tokens_pool::select`]: prefer fresher/healthier
/// accounts, skip `blocked`/revoked ones), and [`crate::capacity`]'s advisory
/// machinery still reports account health on the status surface. It simply no
/// longer gates *how many* sweeps may run concurrently.
///
/// # Why there is no CPU term either (#4512, superseded by the admission brake)
///
/// A fourth axis used to sit in this `min(...)`: `cpu_headroom = (logical_cpus ×
/// cpuUtilizationTarget − consumed_cores) / estCoresPerSweep` (#3978, measured-idle
/// signal #4031). It was **deleted**, deliberately reversing that design:
///
/// - It priced **every** sweep as a build (`estCoresPerSweep`, calibrated
///   against Rust build phases), but sweep wall-clock is dominated by API-wait
///   (curator / builder / judge conversations). It therefore throttled the
///   low-CPU majority to defend against the heavy-build minority: on an 8-core
///   worker measured **95% idle**, the term computed a cap of `2`.
/// - Disk headroom meters a genuinely **exhaustible** resource (bytes) and can
///   be counted exactly. A CPU *estimate* is neither exact nor exhaustible — it
///   was a proxy for "will a build starve another build".
/// - That real concern is now handled where the load actually is: the
///   machine-wide build slot ([`crate::build_slot`]) serializes the designated
///   high-CPU stages across concurrent sweeps, so N sweeps run while at most 1–2
///   build, and the saturation admission brake ([`crate::admission_brake`],
///   #4903) holds **new** admissions once the host's observed load-per-core
///   crosses a threshold — the CPU/RAM "dumb mode" gate the #5270 operator
///   direction asks for, applied at *admission* time rather than folded back
///   into this formula.
/// - The safety net for a mis-set knob is **measurement, not estimation**: the
///   host-distress circuit breaker ([`crate::host_breaker`], #4235) suspends
///   dispatch on observed load-per-core, and the per-tick admission ramp cap
///   (#4234) still bounds how fast occupancy can grow.
#[must_use]
pub fn resolve_dynamic_max_concurrent(
    disk_headroom: usize,
    ram_headroom: usize,
    configured_max: usize,
) -> usize {
    disk_headroom.min(ram_headroom).min(configured_max)
}

// ============================================================================
// Runtime wiring — the loop runs on the shared daemon runtime
// ============================================================================

/// Spawn the work-finder loop on the shared daemon runtime and return its task
/// handle so the daemon can keep it alive for the process lifetime.
///
/// Every `interval`, the task recomputes the **dynamic** concurrency cap
/// (Phase B, #3811; CPU term removed in #4512) — `min(token axis, disk
/// headroom, configured_max)` via [`resolve_dynamic_max_concurrent`] — from
/// live inputs read fresh under `workspace_root`, then runs one [`tick`] with
/// it. The cap is **not** captured once at startup, so a pool that
/// grows/shrinks (`loom-daemon tokens bootstrap`), a scratch volume that fills/frees,
/// or a draining backlog are all honored without a daemon restart.
/// `configured_max` is the per-machine admission knob
/// (`LOOM_WORK_FINDER_MAX_CONCURRENT`).
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
         max_admissions_per_tick={max_admissions_per_tick}, \
         dynamic cap = min(disk, ram, configured_max) — token axis is \
         selection-only, not a cap, since #5270)",
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
        // Healthy-account transition state (#4344): log the count of healthy
        // (`available`) token accounts once when it *changes* tick-to-tick —
        // never every tick — so an operator sees the token axis move (a batch
        // of accounts resetting from `exhausted`, or the whole pool going
        // token-starved) without a steady-state stream. Distinct from the cap
        // line: the healthy count can change while the cap does not (another
        // axis binds), and vice versa.
        let mut was_healthy_tokens: Option<usize> = None;
        // Rate-limit skip state (#4429): log the pause/resume edges once, not
        // every skipped tick — same dedup discipline as `was_halted`.
        let mut was_rate_limited = false;
        loop {
            ticker.tick().await;
            // GitHub rate-limit circuit breaker (#4429): when the shared API
            // budget is exhausted the candidate list is a doomed gh call, so
            // skip the entire tick body until the probed reset epoch passes.
            // See the multi-workspace loop for the full rationale.
            if let Some(rl) = crate::rate_limit_breaker::global() {
                let now = chrono::Utc::now();
                if let Some(transition) = rl.observe_tick(now) {
                    log::info!("rate_limit_breaker: {}", transition.reason);
                    crate::rate_limit_breaker::emit_transition_event(&event_bus, &transition);
                }
                if rl.is_suppressed(now) {
                    if was_rate_limited {
                        log::debug!("work_finder: tick skipped — rate-limit cooldown active");
                    } else {
                        log::info!(
                            "work_finder: tick skipped — shared GitHub API rate limit \
                             exhausted; forge polling paused until the window resets \
                             (#4429; further skips logged at DEBUG)"
                        );
                    }
                    was_rate_limited = true;
                    continue;
                }
                was_rate_limited = false;
            }
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
            //
            // ONE load reading serves both the breaker and the saturation
            // admission brake (#4903) this tick: sampling twice would let the
            // two disagree about the host within a single tick, which is exactly
            // the race a load-aware admission decision must not have.
            let ncpu = crate::cpu_headroom::logical_cpu_count();
            let loadavg_1m = crate::cpu_headroom::read_loadavg_1m();
            if let Some(breaker) = crate::host_breaker::global() {
                let load_per_core = crate::cpu_headroom::load_per_core_from(loadavg_1m, ncpu);
                if let Some(transition) = breaker.observe(load_per_core, chrono::Utc::now()) {
                    crate::host_breaker::emit_transition_event(&event_bus, &transition);
                }
            }
            // Saturation admission brake (#4903): a point-in-time hold on NEW
            // admissions while the host is already saturated. Deliberately NOT
            // folded into `halted` — a held tick must report `deferred-saturation`
            // (and `SATURATION-HELD` on the status surface) rather than claim the
            // main-health gate stopped it.
            //
            // #5715: pass this loop's own in-flight sweep count so the brake can
            // tell "held, sweeps genuinely draining" (healthy backpressure) apart
            // from "held, 0 sweeps in flight" (starvation — the brake can never
            // release on its own because nothing it is blocking is running to
            // relieve the load). This single-workspace loop is retained for
            // reference/tests, so its own dispatcher's view is the right scope
            // here; the production multi-workspace loop below uses the
            // cross-root [`crate::ipc::count_in_flight_sweeps`] instead.
            //
            // #6102: also pass the live role-runner agent count. It changes no
            // brake decision (those still turn on sweeps alone) — it is what
            // lets a starvation message say whether the host is genuinely idle
            // or loaded by agents this brake has no authority over.
            let in_flight_sweeps = dispatcher.in_flight().len();
            let saturation_held = crate::admission_brake::global_observe(
                loadavg_1m,
                ncpu,
                chrono::Utc::now(),
                in_flight_sweeps,
                crate::role_runner::global_active_run_count(),
            );
            let halted = health_state.is_halted()
                || (suppress_dispatch_during_gate && health_state.is_gate_in_flight())
                || crate::host_breaker::global_is_suppressed();
            // Recompute the dynamic cap from live inputs every tick (Phase B),
            // now with token-capacity backpressure (#3902): the token axis is the
            // count of *healthy* accounts from the ranking, not the flat pool.
            let pool_size = token_pool_size(&workspace_root);
            let ranking = capacity::read_ranking(&workspace_root);
            let token_limit = ranking.as_ref().map_or(pool_size, |r| r.available);
            log_healthy_token_transition(&mut was_healthy_tokens, token_limit, ranking.as_ref());
            let disk = disk_headroom_limit(&workspace_root);
            // RAM headroom (#5270): the second "dumb mode" machine-headroom
            // axis alongside disk, folded into the same `min(...)`.
            let ram = crate::ram_headroom::ram_headroom_limit();
            // Refresh the memoized CPU idle sample. Purely **observational**
            // since #4512 — it no longer feeds admission, it feeds the
            // `idle=` figure below plus `loom-daemon status` / `calibrate`, which
            // is how an operator decides whether to raise or lower
            // `maxConcurrent` for this machine. The refresh sleeps ~1s on macOS
            // (`iostat`), so it stays on `spawn_blocking`; a join error just
            // leaves the previous sample in place.
            let _ = tokio::task::spawn_blocking(crate::cpu_headroom::refresh_cpu_util_cache).await;
            let idle = crate::cpu_headroom::cached_cpu_idle_fraction();
            let max_concurrent = resolve_dynamic_max_concurrent(disk, ram, configured_max);
            let axis_line = format!(
                "work_finder: dynamic cap = {max_concurrent} (pool={pool_size}, \
                 healthy_tokens={token_limit} [informational only, not capacity-limiting \
                 since #5270], disk={disk}, ram={ram}, configured_max={configured_max}, \
                 max_admissions_per_tick={max_admissions_per_tick}, halted={halted}, \
                 saturation_held={saturation_held}, \
                 observed_idle={})",
                format_idle(idle)
            );
            if was_max_concurrent != Some(max_concurrent) {
                log::info!("{axis_line}");
                was_max_concurrent = Some(max_concurrent);
            } else {
                log::debug!("{axis_line}");
            }
            match tick_with_saturation_brake(
                &mut source,
                &mut dispatcher,
                max_concurrent,
                halted,
                max_admissions_per_tick,
                saturation_held,
            ) {
                Ok(report) => {
                    // Publish before any logging so `loom-daemon health` sees the
                    // same tick the log line describes (#4761).
                    publish_tick_summary(&report, max_concurrent);
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
                        || report.skipped_backoff > 0
                        || report.skipped_pr_open > 0
                        || report.skipped_peer_claim > 0
                        || report.deferred_ramp_cap > 0
                        || report.deferred_saturation > 0
                    {
                        log::info!(
                            "work_finder: tick — cap {max_concurrent} (pool={pool_size}, \
                             healthy={token_limit}, disk={disk}, \
                             ram={ram}, ceiling={configured_max}, ramp_cap={max_admissions_per_tick}); \
                             {} seen, {} dispatched, {} labeled-skip, {} in-flight-skip, \
                             {} quarantine-skip, {} backoff-skip, {} pr-open-skip, \
                             {} peer-claim-skip, \
                             {} deferred (capacity), {} deferred (ramp), \
                             {} deferred (host saturated), {} error(s), \
                             {} cross-host-collision(s)",
                            report.seen,
                            report.dispatched,
                            report.skipped_labeled,
                            report.skipped_in_flight,
                            report.skipped_quarantined,
                            report.skipped_backoff,
                            report.skipped_pr_open,
                            report.skipped_peer_claim,
                            report.deferred_capacity,
                            report.deferred_ramp_cap,
                            report.deferred_saturation,
                            report.errors,
                            report.collisions
                        );
                    }
                    // Token-capacity advisory (#3902) — surface on state change.
                    // Skip while halted: a red-main halt defers everything, so the
                    // token axis is not the (relevant) bottleneck this tick.
                    if !report.halted {
                        // #5305: since #5270 the token axis is not part of the
                        // dynamic concurrency cap, so this is no longer a
                        // cross-axis "did tokens bind the tick" comparison —
                        // `token_limit == 0` (zero healthy accounts) is the
                        // only condition that fires the add-accounts advisory,
                        // regardless of which axis (disk/RAM/ceiling) actually
                        // deferred the work this tick.
                        let assessment = capacity::assess_pressure(
                            ranking.as_ref(),
                            pool_size,
                            token_limit,
                            report.deferred_capacity,
                            capacity::DEFAULT_ADVISORY_MIN_QUEUED,
                        );
                        was_pressured =
                            emit_capacity_transition(&event_bus, was_pressured, &assessment);
                    }
                }
                Err(e) => {
                    log::warn!("work_finder: tick failed to list ready issues: {e}");
                    // A rate-limit failure trips the global breaker (#4429) so
                    // the next tick skips its gh polling entirely.
                    crate::rate_limit_breaker::global_observe_failure(
                        &e.to_string(),
                        "work_finder",
                    );
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
/// shared across every workspace — never replicated per repo. The token pool
/// specifically is resolved via
/// [`resolve_tokens_dir_anchored`](crate::tokens_pool::paths::resolve_tokens_dir_anchored)
/// against the freshly-reloaded registry (issue #4292, trip-wire 1): when
/// `fallback_root` is not itself a recognized Loom workspace (e.g. a
/// machine-level daemon started under systemd with a bare `$HOME` cwd and no
/// `WorkingDirectory=` override), this anchors straight to the shared
/// machine-level pool rather than probing a per-repo(`fallback_root`) path
/// that can coincidentally collide with the shared default and mask a real,
/// differently-located bootstrap.
///
/// # Known limitation (documented tradeoff, deferred to phase c #3929)
///
/// The event-bus `sweep.issue.{N}.*` topics are keyed by issue number only
/// (frozen taxonomy). Two repos that each have an open issue #N publish on the
/// same topic string. This is an accepted, documented limitation for phase b;
/// the `(repo, issue)` key that disambiguates them is phase c (#3929). No new
/// topic shape is introduced here (CLAUDE.md: "New topics require a follow-up
/// issue").
#[allow(clippy::too_many_arguments)] // dynamic-cap inputs + shared state.
pub fn spawn_multi_work_finder_task(
    pool: Arc<WorkspacePool>,
    fallback_root: PathBuf,
    interval: Duration,
    configured_max: usize,
    max_admissions_per_tick: usize,
    health_states: Arc<WorkspaceHealthStates>,
    suppress_dispatch_during_gate: bool,
    event_bus: Arc<EventBus>,
    drain: Arc<std::sync::atomic::AtomicBool>,
    role_in_progress: crate::role_runner::InProgressGuard,
) -> tokio::task::JoinHandle<()> {
    log::info!(
        "work_finder: starting multi-workspace loop (interval={}s, configured_max={configured_max}, \
         max_admissions_per_tick={max_admissions_per_tick}, \
         dynamic cap = min(disk, ram, configured_max) — token axis is selection-only, \
         not a cap, since #5270; global across workspaces)",
        interval.as_secs()
    );
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // First tick fires immediately; skip it so we don't churn at boot.
        ticker.tick().await;
        let mut was_halted = false;
        let mut was_pressured = false;
        // Pre-flight-advisory hold transition state (#5030): log the distinct
        // "held because pre-flight is broken" warning once per transition rather
        // than every tick, mirroring `was_halted`.
        let mut was_preflight_held_count: usize = 0;
        // Axis-visibility state (#4234) — see the single-workspace loop above
        // for the full rationale.
        let mut was_max_concurrent: Option<usize> = None;
        // Healthy-account transition state (#4344) — see the single-workspace
        // loop above for the full rationale.
        let mut was_healthy_tokens: Option<usize> = None;
        // Missing-root hygiene (#4326): tracks which registered roots are
        // currently missing so `filter_missing_roots` logs a warning once per
        // transition rather than once per tick.
        let mut missing_roots_warned: HashSet<PathBuf> = HashSet::new();
        // Idle-edge role triggering (#4364): per-root idle level + per-(root,
        // role) debounce state. Fed one post-tick idle observation per root; on
        // the non-idle → idle edge it fire-and-forgets each configured on-idle
        // role. Boot state is "already idle" so an empty-queue startup never
        // fires.
        let mut idle_trigger = crate::role_runner::IdleTrigger::new();
        let mut was_rate_limited = false;
        loop {
            ticker.tick().await;

            // GitHub rate-limit circuit breaker (#4429): when the shared API
            // budget is exhausted every workspace's candidate list is a doomed
            // gh call, so skip the ENTIRE tick body — no listing, no dispatch,
            // no idle-edge role firing (a spawned role would hit the same
            // wall). The breaker lazily releases itself once the probed reset
            // epoch passes; the edge is logged once each way, never per-tick.
            if let Some(rl) = crate::rate_limit_breaker::global() {
                let now = chrono::Utc::now();
                if let Some(transition) = rl.observe_tick(now) {
                    log::info!("rate_limit_breaker: {}", transition.reason);
                    crate::rate_limit_breaker::emit_transition_event(&event_bus, &transition);
                }
                if rl.is_suppressed(now) {
                    if was_rate_limited {
                        log::debug!("work_finder: tick skipped — rate-limit cooldown active");
                    } else {
                        log::info!(
                            "work_finder: tick skipped — shared GitHub API rate limit \
                             exhausted; forge polling paused until the window resets \
                             (#4429; further skips logged at DEBUG)"
                        );
                    }
                    was_rate_limited = true;
                    continue;
                }
                was_rate_limited = false;
            }

            // Resolve the current set of workspaces fresh each tick so registry
            // edits (add / remove / set-priority) are hot-applied. Loaded before
            // the token-pool probe below so both can share the same read
            // (issue #4292, trip-wire 1: registry-aware anchoring needs it too).
            let registry = WorkspaceRegistry::load_default().unwrap_or_else(|e| {
                log::warn!("work_finder: could not load workspace registry ({e}); using cwd");
                WorkspaceRegistry::default()
            });

            // Dynamic cap from live *machine-level* inputs (one token pool, one
            // scratch volume) probed from the daemon's primary workspace.
            // `fallback_root` (the daemon's own seeded default) may not itself
            // be a recognized Loom workspace — e.g. a machine-level daemon
            // started under systemd with a bare `$HOME` cwd — in which case
            // `resolve_tokens_dir_anchored` (#4292) resolves straight to the
            // shared machine-level pool instead of a coincidentally-identical,
            // but empty, per-repo(`$HOME`) path.
            let tokens_dir =
                crate::tokens_pool::paths::resolve_tokens_dir_anchored(&fallback_root, &registry);
            let pool_size = token_pool_size_at_dir(&tokens_dir);
            let ranking = capacity::read_ranking_at(&tokens_dir);
            let token_limit = ranking.as_ref().map_or(pool_size, |r| r.available);
            log_healthy_token_transition(&mut was_healthy_tokens, token_limit, ranking.as_ref());
            let disk = disk_headroom_limit(&fallback_root);
            // RAM headroom (#5270): the second "dumb mode" machine-headroom
            // axis alongside disk, folded into the same `min(...)`.
            let ram = crate::ram_headroom::ram_headroom_limit();
            // Refresh the memoized CPU idle sample — **observational only**
            // since #4512 (see the single-workspace loop above). It feeds the
            // `observed_idle=` figure in the axis line and `loom-daemon status`
            // / `calibrate`, which is how an operator tunes this machine's
            // `maxConcurrent`; it no longer gates admission.
            let _ = tokio::task::spawn_blocking(crate::cpu_headroom::refresh_cpu_util_cache).await;
            let idle = crate::cpu_headroom::cached_cpu_idle_fraction();
            let max_concurrent = resolve_dynamic_max_concurrent(disk, ram, configured_max);

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
            //
            // ONE load reading serves both the breaker and the saturation
            // admission brake (#4903) — see the single-workspace loop for why
            // the two must never sample separately within a tick.
            let ncpu = crate::cpu_headroom::logical_cpu_count();
            let loadavg_1m = crate::cpu_headroom::read_loadavg_1m();
            if let Some(breaker) = crate::host_breaker::global() {
                let load_per_core = crate::cpu_headroom::load_per_core_from(loadavg_1m, ncpu);
                if let Some(transition) = breaker.observe(load_per_core, chrono::Utc::now()) {
                    crate::host_breaker::emit_transition_event(&event_bus, &transition);
                }
            }
            let breaker_suppressed = crate::host_breaker::global_is_suppressed();
            // Saturation admission brake (#4903): daemon-global (it measures the
            // one host every workspace's sweeps run on), passed to the tick
            // rather than OR'd into `halted` so its deferrals stay attributable.
            //
            // #5715: pass the CROSS-ROOT in-flight sweep count (not just this
            // tick's candidate backlog) so the brake can tell "held, sweeps
            // genuinely draining somewhere" (healthy backpressure) apart from
            // "held, 0 sweeps in flight anywhere" (starvation — a brake that
            // cannot itself reduce the load it is reacting to, e.g. when the
            // load is entirely role-runner ticks the brake has no authority
            // over, would otherwise hold new admissions forever; #5715).
            //
            // #6102: the role-agent count passed alongside it is the other half
            // of this host's agent load — the half neither this brake nor
            // `maxConcurrent` bounds (the role runner's own
            // `autonomous.roleRunner.maxConcurrent` ceiling does). Reported into
            // the brake so its starvation messages name it instead of asserting
            // an idle host.
            let in_flight_sweeps = crate::ipc::count_in_flight_sweeps(&pool, &fallback_root);
            let saturation_held = crate::admission_brake::global_observe(
                loadavg_1m,
                ncpu,
                chrono::Utc::now(),
                in_flight_sweeps,
                crate::role_runner::global_active_run_count(),
            );
            // Per-root claude-wrapper pre-flight-advisory hold (#5030): consult
            // each root's own SweepRegistry breaker. A workspace that has
            // accumulated `threshold` consecutive pre-flight deaths (broken
            // `.mcp.json`, dead token pool, ...) is held so the work-finder
            // stops burning dispatch slots on doomed ~1s deaths, EXCEPT one
            // half-open probe dispatch per cooldown to test recovery (which
            // clears the advisory automatically on success — no operator
            // action). Strictly per root: a broken workspace never holds a
            // healthy sibling (the #3930 isolation contract).
            let now_tick = chrono::Utc::now();
            let mut preflight_probe_roots: Vec<std::path::PathBuf> = Vec::new();
            let preflight_held: Vec<bool> = roots
                .iter()
                .map(|root| {
                    let registry = pool.get_or_provision(root);
                    let mut registry = registry
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    match registry.preflight_dispatch_gate(now_tick) {
                        PreflightDispatchGate::Open => false,
                        PreflightDispatchGate::Held => true,
                        PreflightDispatchGate::Probe => {
                            preflight_probe_roots.push(root.clone());
                            false
                        }
                    }
                })
                .collect();
            let halted: Vec<bool> = dispatch_held_per_root_with_preflight(
                &health_states,
                &roots,
                suppress_dispatch_during_gate,
                &preflight_held,
            )
            .into_iter()
            .map(|h| h || draining || breaker_suppressed)
            .collect();
            let preflight_held_count = preflight_held.iter().filter(|&&h| h).count();
            // Distinguish a pre-flight-advisory hold from the main-health /
            // gate-in-flight holds (#5030 AC4) so an operator can tell "held
            // because pre-flight is broken" apart from "held because CI is red."
            if preflight_held_count != was_preflight_held_count {
                if preflight_held_count > 0 {
                    log::warn!(
                        "work_finder: {preflight_held_count} of {} repo(s) held — \
                         claude-wrapper pre-flight advisory tripped (broken .mcp.json / dead token \
                         pool); dispatch is suppressed except one probe per cooldown until a \
                         dispatch reaches CLI start (#5030)",
                        roots.len()
                    );
                } else {
                    log::info!(
                        "work_finder: pre-flight advisory cleared for all repos — dispatch \
                         resuming (#5030)"
                    );
                }
                was_preflight_held_count = preflight_held_count;
            }
            for probe_root in &preflight_probe_roots {
                log::info!(
                    "work_finder: pre-flight advisory recovery probe — allowing one dispatch to \
                     {} to test recovery (#5030)",
                    probe_root.display()
                );
            }
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
                 healthy_tokens={token_limit} [informational only, not capacity-limiting \
                 since #5270], disk={disk}, ram={ram}, configured_max={configured_max}, \
                 max_admissions_per_tick={max_admissions_per_tick}, any_halted={any_halted}, \
                 preflight_held={preflight_held_count}, \
                 saturation_held={saturation_held}, \
                 observed_idle={}, workspaces={}, priorities={priorities:?})",
                format_idle(idle),
                pairs.len()
            );
            if was_max_concurrent != Some(max_concurrent) {
                log::info!("{axis_line}");
                was_max_concurrent = Some(max_concurrent);
            } else {
                log::debug!("{axis_line}");
            }

            let report = tick_multi_with_saturation_brake(
                &mut pairs,
                &priorities,
                max_concurrent,
                &halted,
                max_admissions_per_tick,
                saturation_held,
            );

            // Publish before any logging so `loom-daemon health` sees the same
            // tick the log line describes (#4761).
            publish_tick_summary(&report, max_concurrent);

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
                || report.skipped_backoff > 0
                || report.skipped_pr_open > 0
                || report.skipped_peer_claim > 0
                || report.deferred_ramp_cap > 0
                || report.deferred_saturation > 0
            {
                log::info!(
                    "work_finder: tick — cap {max_concurrent} (pool={pool_size}, \
                     healthy={token_limit}, disk={disk}, \
                     ram={ram}, ceiling={configured_max}, ramp_cap={max_admissions_per_tick}); \
                     {} workspace(s), \
                     {} seen, {} dispatched, {} labeled-skip, {} in-flight-skip, \
                     {} quarantine-skip, {} backoff-skip, {} pr-open-skip, \
                     {} peer-claim-skip, \
                     {} deferred (capacity), {} deferred (ramp), \
                     {} deferred (host saturated), {} error(s), \
                     {} cross-host-collision(s)",
                    pairs.len(),
                    report.seen,
                    report.dispatched,
                    report.skipped_labeled,
                    report.skipped_in_flight,
                    report.skipped_quarantined,
                    report.skipped_backoff,
                    report.skipped_pr_open,
                    report.skipped_peer_claim,
                    report.deferred_capacity,
                    report.deferred_ramp_cap,
                    report.deferred_saturation,
                    report.errors,
                    report.collisions
                );
            }

            if !report.halted {
                // #5305: see the single-workspace loop above — `token_bound`
                // is now a pure starvation check, not a cross-axis comparison
                // against disk/RAM/ceiling.
                let assessment = capacity::assess_pressure(
                    ranking.as_ref(),
                    pool_size,
                    token_limit,
                    report.deferred_capacity,
                    capacity::DEFAULT_ADVISORY_MIN_QUEUED,
                );
                was_pressured = emit_capacity_transition(&event_bus, was_pressured, &assessment);
            }

            // Idle-edge role triggering (#4364). A dispatch this tick registers
            // in that root's registry immediately, so a **post-tick** per-root
            // `in_flight().is_empty()` already encodes both halves of "idle":
            // nothing running AND nothing dispatched this tick. `observe_edge`
            // then converts that level into the non-idle → idle EDGE; on the
            // edge, `observe_and_fire_idle` fire-and-forgets each configured
            // on-idle role (never awaited here — the tick must not block on a
            // multi-minute role session). `draining` (#4090) suppresses firing;
            // per-root config gating (enabled + `onIdle`) is applied inside.
            for (root, (_src, dispatcher)) in roots.iter().zip(pairs.iter()) {
                let idle_now = dispatcher.in_flight().is_empty();
                crate::role_runner::observe_and_fire_idle(
                    &mut idle_trigger,
                    &role_in_progress,
                    root,
                    idle_now,
                    draining,
                );
            }
        }
    })
}

/// Render the measured CPU idle fraction for the per-tick axis line as a
/// percentage, or `"n/a"` when no sample exists yet (#4512).
///
/// This figure is **observational**: it is no longer an input to the cap (the
/// CPU term is gone), it is the evidence an operator uses to decide whether this
/// machine's `maxConcurrent` is too low (host sits idle) or too high (host is
/// saturated / the breaker trips).
fn format_idle(idle: Option<f64>) -> String {
    idle.map_or_else(|| "n/a".to_string(), |f| format!("{:.0}%", f * 100.0))
}

/// Log the count of healthy (`available`) token accounts once, on a **state
/// change** — never every tick (#4344 AC).
///
/// `prev` is the last logged healthy count (carried across ticks); it is
/// updated in place. The very first observation seeds `prev` silently (no
/// startup line); every subsequent change logs a single `info!` edge naming the
/// old → new healthy count and the ranking total, so an operator can see the
/// token axis move — an account batch resetting, or the pool going
/// token-starved (`… -> 0 …`) — without a steady-state stream. Mirrors the
/// state-change-dedup discipline the cap line (`was_max_concurrent`) and the
/// capacity advisory (`was_pressured`) already use.
fn log_healthy_token_transition(
    prev: &mut Option<usize>,
    healthy: usize,
    ranking: Option<&capacity::RankingSnapshot>,
) {
    if *prev == Some(healthy) {
        return;
    }
    if let Some(old) = *prev {
        let total = ranking
            .map_or_else(|| "n/a (no ranking; raw pool)".to_string(), |r| r.total.to_string());
        if healthy == 0 {
            log::warn!(
                "work_finder: healthy token accounts {old} -> 0 (of {total}) — dispatch is \
                 token-starved until an account resets or is added (`loom-daemon tokens check --ranking`)"
            );
        } else {
            log::info!(
                "work_finder: healthy token accounts {old} -> {healthy} (of {total}) — \
                 dynamic-cap token axis follows"
            );
        }
    }
    *prev = Some(healthy);
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
    use anyhow::{anyhow, Result};
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

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
            // ETag-cached REST listing (#4428): a poll where nothing changed
            // costs zero rate limit (304), replacing the per-tick GraphQL
            // `gh issue list`. REST issue listings include PRs, so filter the
            // `pull_request`-marked rows to keep the pre-#4428 issue-only set.
            let rows = crate::forge_listing::list_issues_cached(
                &self.gh_bin,
                self.cwd.as_deref(),
                self.repo.as_deref(),
                "loom:issue",
                "open",
            )?;
            Ok(rows
                .into_iter()
                .filter(|r| !r.is_pull_request)
                // The REST listing already returns `body` (#4827) — carrying it
                // onto the item costs no extra request and lets dispatch read
                // the `<!-- loom:complexity=... -->` stratum without a
                // per-issue `gh issue view`.
                .map(|r| {
                    WorkItem::with_created_at(r.number, r.labels, r.created_at).with_body(r.body)
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

        /// Issues inside a live per-issue dispatch-backoff window (Issue #4485).
        /// Pure in-memory read of the registry state the reaper maintains — no
        /// forge round trip, mirroring `quarantined()`.
        fn backed_off(&self) -> HashSet<u32> {
            match self.registry.lock() {
                Ok(reg) => reg.dispatch_backoff_issues(chrono::Utc::now()),
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

        fn dispatch(&mut self, issue: u32, complexity: Option<&str>) -> Result<bool> {
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
            //
            // Issue #4809: this resolution ALSO inserts the model-cost A/B
            // experiment's forced arm model when the workspace resolves to
            // `experiment` mode (CANARY-gated) — the daemon-native replacement
            // for the sweep.md prose instrumentation, which never executed in a
            // headless child and was in any case overridden by this very
            // default-pin precedence. `off`/`observe` modes are unaffected.
            //
            // Issue #4827: `complexity` is the issue's real
            // `<!-- loom:complexity=... -->` stratum, read from the body the
            // ETag-cached REST listing already returned — so the experiment's
            // `complex` and `routine` strata each get an independent ~50/50 A/B
            // balance instead of the whole population being stratified as
            // `routine`. No extra forge call: the body arrives with the listing.
            let repo_root = reg.config().workspace_root.clone();
            let resolved = crate::sweep_registry::resolve_autonomous_dispatch_model(
                &repo_root, issue, complexity,
            );
            match resolved.arm {
                Some(arm) => log::info!(
                    "work_finder: dispatching issue #{issue} with arm={arm} \
                     (complexity={}) model={} (source={})",
                    complexity.unwrap_or("routine"),
                    resolved.model,
                    resolved.source_label
                ),
                None => log::info!(
                    "work_finder: dispatching issue #{issue} with model={} (source={})",
                    resolved.model,
                    resolved.source_label
                ),
            }
            let model = resolved.model;
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
    // Healthy-account transition tracking (#4344)
    // ===================================================================

    fn snap(total: usize, available: usize) -> capacity::RankingSnapshot {
        capacity::RankingSnapshot {
            total,
            available,
            exhausted: total - available,
            ..capacity::RankingSnapshot::default()
        }
    }

    #[test]
    fn healthy_token_transition_dedups_by_state() {
        // The tracker only advances `prev` when the healthy count changes —
        // repeated identical ticks are no-ops (the once-per-transition contract
        // the AC requires). We assert on the carried state, since the log line
        // itself is a side effect.
        let mut prev: Option<usize> = None;

        // First observation seeds silently.
        log_healthy_token_transition(&mut prev, 6, Some(&snap(7, 6)));
        assert_eq!(prev, Some(6));

        // Stable ticks: no change.
        log_healthy_token_transition(&mut prev, 6, Some(&snap(7, 6)));
        assert_eq!(prev, Some(6));

        // Drop to token-starved (0 healthy) — a transition.
        log_healthy_token_transition(&mut prev, 0, Some(&snap(7, 0)));
        assert_eq!(prev, Some(0));

        // Recovery to a new count — another transition.
        log_healthy_token_transition(&mut prev, 4, Some(&snap(7, 4)));
        assert_eq!(prev, Some(4));

        // No-ranking fallback path (raw pool size) still tracks the count.
        log_healthy_token_transition(&mut prev, 3, None);
        assert_eq!(prev, Some(3));
    }

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
        /// Issue numbers whose dispatch should be refused by the park-label
        /// guard (#4444) — the dispatcher returns the typed
        /// [`ParkedIssueDispatchError`], simulating a `loom:blocked` park the
        /// candidate listing had not caught yet.
        parked_issues: HashSet<u32>,
        /// Issue numbers this dispatcher reports as quarantined (Issue #3939).
        quarantined: HashSet<u32>,
        /// Issue numbers this dispatcher reports as inside a dispatch-backoff
        /// window (Issue #4485).
        backed_off: HashSet<u32>,
        /// Issue numbers whose dispatch should be refused by the dispatch-backoff
        /// guard (#4485) — the dispatcher returns the typed
        /// [`DispatchBackoffError`], as `SweepRegistry::dispatch` step 2.8 does
        /// when a window is armed mid-tick.
        backoff_refuse_issues: HashSet<u32>,
        /// Cumulative cross-host collision count this dispatcher reports (#4085).
        collisions: u64,
        /// Issue numbers a peer host has soft-claimed over safehouse (#4028).
        peer_claimed: HashSet<u32>,
        /// Issue numbers whose dispatch should be refused by the live-claim
        /// guard (#4556) — the dispatcher returns the typed
        /// [`LiveClaimDispatchError`], as `SweepRegistry::dispatch` step 2.9 does
        /// when a sweep process for the issue is confirmed still running while
        /// `in_flight()` cannot see it (a reverted label, a released lock, or a
        /// second daemon instance on the same host).
        live_claim_issues: HashSet<u32>,
        /// Every `(issue, complexity)` pair `dispatch` was called with (#4827),
        /// so a test can assert the REAL per-issue complexity stratum reached
        /// the dispatcher rather than the pre-#4827 `None`.
        dispatched_complexity: Vec<(u32, Option<String>)>,
    }

    impl WorkDispatcher for RecordingDispatcher {
        fn in_flight(&self) -> HashSet<u32> {
            self.in_flight.clone()
        }
        fn quarantined(&self) -> HashSet<u32> {
            self.quarantined.clone()
        }
        fn backed_off(&self) -> HashSet<u32> {
            self.backed_off.clone()
        }
        fn collisions(&self) -> u64 {
            self.collisions
        }
        fn peer_claimed(&self) -> HashSet<u32> {
            self.peer_claimed.clone()
        }
        fn dispatch(&mut self, issue: u32, complexity: Option<&str>) -> Result<bool> {
            self.dispatched_complexity
                .push((issue, complexity.map(str::to_owned)));
            if self.backoff_refuse_issues.contains(&issue) {
                return Err(DispatchBackoffError {
                    issue,
                    consecutive: 2,
                    retry_after_secs: 120,
                }
                .into());
            }
            if self.pr_open_issues.contains(&issue) {
                // Mirror the production `SweepRegistry::dispatch` open-PR guard:
                // refuse with the typed, downcast-matchable error (#4123).
                return Err(OpenPrDispatchError { issue, pr: 9999 }.into());
            }
            if self.parked_issues.contains(&issue) {
                // Mirror the production `SweepRegistry::dispatch` park-label
                // guard: refuse with the typed, downcast-matchable error (#4444).
                return Err(ParkedIssueDispatchError {
                    issue,
                    label: "loom:blocked".to_string(),
                }
                .into());
            }
            if self.live_claim_issues.contains(&issue) {
                // Mirror the production `SweepRegistry::dispatch` live-claim
                // guard: refuse with the typed, downcast-matchable error (#4556).
                return Err(LiveClaimDispatchError {
                    issue,
                    evidence: crate::live_claim::LiveClaimEvidence::SweepProcess { pid: 4242 },
                }
                .into());
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
    // Per-issue complexity threading (#4827)
    // ===================================================================

    /// A ready issue whose body carries the Curator's `<!-- loom:complexity=... -->`
    /// marker — the shape `GhWorkSource::list_ready_issues` now materializes
    /// from the REST listing's `body` field.
    fn issue_with_complexity(n: u32, tier: &str) -> WorkItem {
        issue(n).with_body(Some(format!(
            "## Context\n\nSome body text.\n\n<!-- loom:complexity={tier} -->\n"
        )))
    }

    /// `WorkItem::complexity()` reads the marker out of the carried body, and
    /// degrades to `None` (the unchanged `routine` stratum) when the listing
    /// supplied no body or the body carries no marker.
    #[test]
    fn work_item_extracts_complexity_from_its_body() {
        assert_eq!(issue_with_complexity(1, "complex").complexity(), Some("complex"));
        assert_eq!(issue_with_complexity(1, "mechanical").complexity(), Some("mechanical"));
        // No body at all (a synthetic item / a listing without bodies).
        assert_eq!(issue(1).complexity(), None);
        // A body with no marker (a pre-marker issue).
        assert_eq!(issue(1).with_body(Some("no marker".into())).complexity(), None);
    }

    /// The core #4827 acceptance criterion for the single-workspace path: the
    /// issue's REAL complexity stratum reaches `dispatch()` instead of the
    /// pre-#4827 hardcoded `None`.
    #[test]
    fn tick_threads_per_issue_complexity_into_dispatch() {
        let mut source = FakeSource::once(vec![
            issue_with_complexity(10, "complex"),
            issue_with_complexity(11, "mechanical"),
            issue(12), // no body → None → `routine`, unchanged
        ]);
        let mut dispatcher = RecordingDispatcher::default();
        let report = tick(&mut source, &mut dispatcher, 10, false).unwrap();

        assert_eq!(report.dispatched, 3);
        assert_eq!(
            dispatcher.dispatched_complexity,
            vec![
                (10, Some("complex".to_string())),
                (11, Some("mechanical".to_string())),
                (12, None),
            ]
        );
    }

    /// The same criterion for the multi-workspace path, where the stratum
    /// travels on `PriorityCandidate` from pass 1 (listing) to pass 2
    /// (dispatch) — it must survive the global priority sort.
    #[test]
    fn tick_multi_threads_per_issue_complexity_into_dispatch() {
        let mut workspaces = vec![
            (
                FakeSource::once(vec![issue_with_complexity(20, "complex")]),
                RecordingDispatcher::default(),
            ),
            (
                FakeSource::once(vec![issue_with_complexity(21, "routine"), issue(22)]),
                RecordingDispatcher::default(),
            ),
        ];
        let report = tick_multi(&mut workspaces, &[0, 0], 10, &[false, false]);

        assert_eq!(report.dispatched, 3);
        assert_eq!(workspaces[0].1.dispatched_complexity, vec![(20, Some("complex".to_string()))]);
        assert_eq!(
            workspaces[1].1.dispatched_complexity,
            vec![(21, Some("routine".to_string())), (22, None)]
        );
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

        let was_new = dispatcher
            .dispatch(3964, None)
            .expect("dispatch should succeed");
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

    // ========================================================================
    // Saturation admission brake (#4903)
    // ========================================================================

    #[test]
    fn test_saturated_host_admits_no_new_sweeps() {
        // AC1: a host at/over the load-per-core hold threshold admits nothing.
        // The cap is generous (12, the value the reported worker ran with) and
        // the backlog is deep — only the brake stops it.
        let mut source = FakeSource::once((1..=5).map(issue).collect());
        let mut disp = RecordingDispatcher::default();

        let report =
            tick_with_saturation_brake(&mut source, &mut disp, 12, false, usize::MAX, true)
                .unwrap();

        assert_eq!(report.dispatched, 0, "a saturated host must admit nothing");
        assert!(disp.dispatched.is_empty());
        assert_eq!(report.deferred_saturation, 5);
        // Attributed to the HOST, not to a cap that was nowhere near binding —
        // the whole point of a separate counter.
        assert_eq!(report.deferred_capacity, 0);
        assert_eq!(report.deferred_ramp_cap, 0);
        assert!(report.saturation_held);
        // Not a main-health halt: `halted` stays false so the operator log/status
        // never blames a red main for a load hold.
        assert!(!report.halted);
        assert_eq!(report.seen, 5, "the backlog is still observed and reported");
    }

    #[test]
    fn test_brake_never_preempts_in_flight_sweeps() {
        // AC2: in-flight sweeps are neither killed nor counted against the brake.
        // Three sweeps are already running (the reported incident's shape); the
        // brake holds new admissions and leaves the running set exactly as it was.
        let in_flight = HashSet::from([101, 102, 103]);
        let mut source = FakeSource::once(vec![issue(1), issue(2)]);
        let mut disp = RecordingDispatcher {
            in_flight: in_flight.clone(),
            ..Default::default()
        };

        let report =
            tick_with_saturation_brake(&mut source, &mut disp, 12, false, usize::MAX, true)
                .unwrap();

        assert_eq!(report.dispatched, 0);
        assert_eq!(report.deferred_saturation, 2);
        // The running set is untouched — the brake has no path to it at all.
        assert_eq!(
            disp.in_flight, in_flight,
            "the brake must never preempt or drop a running sweep"
        );
        // And a running sweep is never mistaken for a deferred candidate.
        assert_eq!(report.skipped_in_flight, 0);
    }

    #[test]
    fn test_brake_holds_an_in_flight_candidate_as_in_flight_not_saturation() {
        // A ready row that is ALSO already in flight (label-flip lag) must be
        // attributed to the in-flight dedup, not swept into the brake's counter —
        // otherwise "held by saturation" would over-report on a busy host.
        let mut source = FakeSource::once(vec![issue(7), issue(8)]);
        let mut disp = RecordingDispatcher {
            in_flight: HashSet::from([7]),
            ..Default::default()
        };

        let report =
            tick_with_saturation_brake(&mut source, &mut disp, 12, false, usize::MAX, true)
                .unwrap();

        assert_eq!(report.skipped_in_flight, 1);
        assert_eq!(report.deferred_saturation, 1);
    }

    #[test]
    fn test_healthy_host_still_reaches_its_configured_cap() {
        // AC3 (regression guard for #4512): with the brake NOT engaged, an idle
        // host fills its configured cap exactly as before. If this ever fails,
        // the brake has re-introduced the over-throttling #4512 removed.
        let mut source = FakeSource::once((1..=8).map(issue).collect());
        let mut disp = RecordingDispatcher::default();

        let report =
            tick_with_saturation_brake(&mut source, &mut disp, 8, false, usize::MAX, false)
                .unwrap();

        assert_eq!(report.dispatched, 8, "an idle 8-core host must reach its cap");
        assert_eq!(report.deferred_saturation, 0);
        assert!(!report.saturation_held);
    }

    #[test]
    fn test_brake_disengaged_is_byte_for_byte_the_pre_brake_path() {
        // The `false` path must be indistinguishable from the pre-#4903 wrapper,
        // so the brake can never change a healthy host's schedule.
        let ready: Vec<WorkItem> = (1..=6).map(issue).collect();

        let mut source_a = FakeSource::once(ready.clone());
        let mut disp_a = RecordingDispatcher::default();
        let before = tick_with_admission_cap(&mut source_a, &mut disp_a, 4, false, 3).unwrap();

        let mut source_b = FakeSource::once(ready);
        let mut disp_b = RecordingDispatcher::default();
        let after =
            tick_with_saturation_brake(&mut source_b, &mut disp_b, 4, false, 3, false).unwrap();

        assert_eq!(before, after);
        assert_eq!(disp_a.dispatched, disp_b.dispatched);
    }

    #[test]
    fn test_brake_releases_the_moment_the_host_recovers() {
        // The hold is re-evaluated every tick and nothing latches (that is the
        // host breaker's cool-down, deliberately a different mechanism): tick 1
        // saturated holds everything, tick 2 recovered dispatches everything.
        let ready: Vec<WorkItem> = (1..=3).map(issue).collect();
        let mut disp = RecordingDispatcher::default();

        let mut hot = FakeSource::once(ready.clone());
        let held =
            tick_with_saturation_brake(&mut hot, &mut disp, 10, false, usize::MAX, true).unwrap();
        assert_eq!(held.dispatched, 0);
        assert_eq!(held.deferred_saturation, 3);

        let mut cool = FakeSource::once(ready);
        let resumed =
            tick_with_saturation_brake(&mut cool, &mut disp, 10, false, usize::MAX, false).unwrap();
        assert_eq!(resumed.dispatched, 3, "admissions resume with no cool-down");
        assert_eq!(resumed.deferred_saturation, 0);
        assert!(!resumed.saturation_held);
        assert_eq!(disp.dispatched, vec![1, 2, 3]);
    }

    #[test]
    fn test_brake_engaged_with_empty_backlog_still_reports_held() {
        // A saturated host with nothing queued must not read as idle-healthy —
        // that indistinguishability is the reporting half of #4903.
        let mut source = FakeSource::once(vec![]);
        let mut disp = RecordingDispatcher::default();

        let report =
            tick_with_saturation_brake(&mut source, &mut disp, 12, false, usize::MAX, true)
                .unwrap();

        assert_eq!(report.deferred_saturation, 0);
        assert!(report.saturation_held, "held state must survive an empty backlog");
    }

    #[test]
    fn test_brake_and_main_health_halt_compose_halt_wins_early_return() {
        // A red main short-circuits before the candidate loop, so nothing is
        // attributed to the brake — but the brake's engagement is still recorded
        // so status does not claim the host is fine.
        let mut source = FakeSource::once((1..=3).map(issue).collect());
        let mut disp = RecordingDispatcher::default();

        let report =
            tick_with_saturation_brake(&mut source, &mut disp, 12, true, usize::MAX, true).unwrap();

        assert!(report.halted);
        assert!(report.saturation_held);
        assert_eq!(report.dispatched, 0);
        assert_eq!(report.deferred_saturation, 0);
    }

    #[test]
    fn test_tick_multi_brake_holds_every_workspace() {
        // The brake is daemon-global: it measures the one host every workspace's
        // sweeps run on, so a hold applies across repos at once.
        let source_a = FakeSource::once(vec![issue(1), issue(2)]);
        let disp_a = RecordingDispatcher::default();
        let source_b = FakeSource::once(vec![issue(3), issue(4)]);
        let disp_b = RecordingDispatcher::default();
        let mut multi = vec![(source_a, disp_a), (source_b, disp_b)];

        let report = tick_multi_with_saturation_brake(
            &mut multi,
            &[],
            10,
            &[false, false],
            usize::MAX,
            true,
        );

        assert_eq!(report.dispatched, 0);
        assert_eq!(report.deferred_saturation, 4);
        assert_eq!(report.deferred_capacity, 0);
        assert!(report.saturation_held);
        assert!(multi.iter().all(|(_, d)| d.dispatched.is_empty()));
    }

    #[test]
    fn test_tick_multi_healthy_host_unchanged_by_the_brake() {
        // AC3 for the production (multi-workspace) path: disengaged ⇒ the
        // pre-#4903 schedule, cap and all.
        let source_a = FakeSource::once(vec![issue(1), issue(2)]);
        let disp_a = RecordingDispatcher::default();
        let source_b = FakeSource::once(vec![issue(3), issue(4)]);
        let disp_b = RecordingDispatcher::default();
        let mut multi = vec![(source_a, disp_a), (source_b, disp_b)];

        let report = tick_multi_with_saturation_brake(
            &mut multi,
            &[],
            10,
            &[false, false],
            usize::MAX,
            false,
        );

        assert_eq!(report.dispatched, 4);
        assert_eq!(report.deferred_saturation, 0);
        assert!(!report.saturation_held);
    }

    #[test]
    fn test_tick_multi_brake_does_not_disturb_in_flight_across_workspaces() {
        // AC2 on the multi path: every workspace's running set is preserved and
        // its occupancy is never re-attributed to the brake.
        let source_a = FakeSource::once(vec![issue(1)]);
        let disp_a = RecordingDispatcher {
            in_flight: HashSet::from([900]),
            ..Default::default()
        };
        let source_b = FakeSource::once(vec![issue(2)]);
        let disp_b = RecordingDispatcher {
            in_flight: HashSet::from([901, 902]),
            ..Default::default()
        };
        let mut multi = vec![(source_a, disp_a), (source_b, disp_b)];

        let report = tick_multi_with_saturation_brake(
            &mut multi,
            &[],
            10,
            &[false, false],
            usize::MAX,
            true,
        );

        assert_eq!(report.deferred_saturation, 2);
        assert_eq!(multi[0].1.in_flight, HashSet::from([900]));
        assert_eq!(multi[1].1.in_flight, HashSet::from([901, 902]));
    }

    #[test]
    fn test_saturation_deferrals_reach_the_published_tick_summary() {
        // AC4's plumbing: the counter must survive into the process-global
        // summary `loom-daemon health` / `status` read back, and the summary
        // line must NAME it rather than hide it among the zeros.
        let report = TickReport {
            seen: 4,
            deferred_saturation: 4,
            saturation_held: true,
            ..TickReport::default()
        };
        publish_tick_summary(&report, 12);
        let summary = last_tick_summary().expect("a tick was just published");
        assert_eq!(summary.deferred_saturation, 4);
        assert!(summary.saturation_held);
        let line = summary.reason_summary();
        assert!(line.contains("4 deferred-saturation"), "got: {line}");
        assert!(line.contains("SATURATION-HELD"), "got: {line}");
        reset_last_tick_summary();
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
    fn test_tick_skips_backed_off_issue() {
        // Dispatch backoff (#4485): an issue inside its backoff window is skipped
        // — never dispatched — and counted in `skipped_backoff`, while its
        // healthy siblings dispatch normally.
        let mut source = FakeSource::once(vec![issue(1), issue(2), issue(3)]);
        let mut disp = RecordingDispatcher {
            backed_off: HashSet::from([2]),
            ..Default::default()
        };
        let report = tick(&mut source, &mut disp, 10, false).unwrap();

        assert_eq!(report.skipped_backoff, 1, "#2 is inside its backoff window");
        assert_eq!(report.dispatched, 2, "#1 and #3 still dispatch");
        assert_eq!(disp.dispatched, vec![1, 3], "#2 never dispatched");
    }

    #[test]
    fn test_tick_backed_off_does_not_consume_capacity_slot() {
        // The backoff skip happens BEFORE the capacity gate (like quarantine), so
        // a backed-off issue never reserves a slot the healthy sibling could use.
        let mut source = FakeSource::once(vec![issue(1), issue(2)]);
        let mut disp = RecordingDispatcher {
            backed_off: HashSet::from([1]),
            ..Default::default()
        };
        let report = tick(&mut source, &mut disp, 1, false).unwrap();

        assert_eq!(report.skipped_backoff, 1);
        assert_eq!(report.dispatched, 1);
        assert_eq!(disp.dispatched, vec![2], "the single slot goes to the healthy #2");
    }

    #[test]
    fn test_tick_attributes_backoff_refusal_to_skipped_backoff() {
        // A backoff window armed mid-tick (by a reap between `backed_off()` and
        // the dispatch call) surfaces as the typed `DispatchBackoffError`. That is
        // a deliberate skip, NOT a dispatch failure: it must land in
        // `skipped_backoff`, never in `errors`.
        let mut source = FakeSource::once(vec![issue(7), issue(8)]);
        let mut disp = RecordingDispatcher {
            backoff_refuse_issues: HashSet::from([7]),
            ..Default::default()
        };
        let report = tick(&mut source, &mut disp, 10, false).unwrap();

        assert_eq!(report.skipped_backoff, 1, "#7's refusal is a backoff skip");
        assert_eq!(report.errors, 0, "a backoff refusal is never a dispatch error");
        assert_eq!(report.dispatched, 1);
        assert_eq!(disp.dispatched, vec![8]);
    }

    #[test]
    fn test_tick_attributes_live_claim_refusal_to_skipped_in_flight() {
        // #4556: `in_flight()` is scoped to ONE daemon process and is seeded from
        // labels/locks that a false-dead verdict may already have cleared — so an
        // issue whose sweep is genuinely still running can reach the dispatch
        // call. The registry's step-2.9 guard refuses it with the typed
        // `LiveClaimDispatchError`. That is an in-flight skip (the issue really
        // IS in flight), never a dispatch error, and it must not consume the
        // healthy candidate's slot.
        let mut source = FakeSource::once(vec![issue(4275), issue(8)]);
        let mut disp = RecordingDispatcher {
            live_claim_issues: HashSet::from([4275]),
            ..Default::default()
        };
        let report = tick(&mut source, &mut disp, 10, false).unwrap();

        assert_eq!(report.skipped_in_flight, 1, "#4275's refusal is an in-flight skip");
        assert_eq!(report.errors, 0, "a live-claim refusal is never a dispatch error");
        assert_eq!(report.dispatched, 1);
        assert_eq!(disp.dispatched, vec![8]);
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
    fn test_tick_multi_backed_off_workspace_does_not_starve_sibling() {
        // #4485, mirroring the #3939 quarantine property: workspace A's only
        // candidate is inside its dispatch-backoff window; workspace B has a
        // healthy candidate. With a shared cap of 1, B's issue MUST be dispatched
        // — a backed-off candidate never reserves the shared slot.
        let mut multi = vec![
            (
                FakeSource::once(vec![issue(1)]),
                RecordingDispatcher {
                    backed_off: HashSet::from([1]),
                    ..Default::default()
                },
            ),
            (FakeSource::once(vec![issue(10)]), RecordingDispatcher::default()),
        ];
        let report = tick_multi(&mut multi, &[], 1, &[false, false]);

        assert_eq!(report.skipped_backoff, 1, "workspace A's #1 is backed off");
        assert_eq!(report.dispatched, 1);
        assert!(multi[0].1.dispatched.is_empty(), "backed-off workspace dispatches nothing");
        assert_eq!(multi[1].1.dispatched, vec![10], "healthy sibling gets the shared slot");
    }

    #[test]
    fn test_tick_multi_backoff_refusal_counts_as_backoff_skip() {
        // A mid-tick backoff refusal in `tick_multi` is attributed to
        // `skipped_backoff`, not `errors` — same typed-downcast rule as `tick`.
        let mut multi = vec![(
            FakeSource::once(vec![issue(5), issue(6)]),
            RecordingDispatcher {
                backoff_refuse_issues: HashSet::from([5]),
                ..Default::default()
            },
        )];
        let report = tick_multi(&mut multi, &[], 10, &[false]);

        assert_eq!(report.skipped_backoff, 1);
        assert_eq!(report.errors, 0);
        assert_eq!(multi[0].1.dispatched, vec![6]);
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
    fn test_park_labels_are_the_non_building_subset_of_skip_labels() {
        // #4444: the dispatch-time guard keys on PARK_LABELS, so the two
        // constants must stay in lockstep — SKIP_LABELS is exactly
        // BUILDING_LABEL + PARK_LABELS, and PARK_LABELS must never contain
        // `loom:building` (a guard that refused it would break the watchdogs'
        // and the reaper's re-dispatch of the daemon's OWN claim).
        assert!(
            !PARK_LABELS.contains(&BUILDING_LABEL),
            "PARK_LABELS must exclude {BUILDING_LABEL}: it is legitimately present on a \
             watchdog / checkpoint-resume re-dispatch of the daemon's own claim"
        );
        for park in PARK_LABELS {
            assert!(
                SKIP_LABELS.contains(park),
                "{park} is a park label, so the work-finder query must skip it too"
            );
        }
        let mut expected: Vec<&str> = vec![BUILDING_LABEL];
        expected.extend_from_slice(PARK_LABELS);
        assert_eq!(
            SKIP_LABELS, expected,
            "SKIP_LABELS is composed as BUILDING_LABEL + PARK_LABELS"
        );
    }

    #[test]
    fn test_tick_park_label_refusal_counts_as_labeled_skip_not_error() {
        // #2 carries `loom:blocked` on the forge but the candidate listing was
        // stale: `dispatch()` refuses with the typed ParkedIssueDispatchError,
        // which the finder attributes to `skipped_labeled` — NOT `errors` — while
        // its siblings dispatch normally (#4444).
        let mut source = FakeSource::once(vec![issue(1), issue(2), issue(3)]);
        let mut disp = RecordingDispatcher {
            parked_issues: HashSet::from([2]),
            ..Default::default()
        };
        let report = tick(&mut source, &mut disp, 10, false).unwrap();

        assert_eq!(report.skipped_labeled, 1, "#2's park refusal is a labeled-skip");
        assert_eq!(report.errors, 0, "a park-label skip is never a dispatch error");
        assert_eq!(report.skipped_pr_open, 0, "and it is not an open-PR skip either");
        assert_eq!(report.dispatched, 2, "#1 and #3 still dispatch");
        assert_eq!(disp.dispatched, vec![1, 3], "#2 never dispatched");
    }

    #[test]
    fn test_tick_multi_park_label_refusal_counts_as_labeled_skip() {
        // Same attribution in the multi-workspace tick (#4444).
        let mut multi = vec![
            (
                FakeSource::once(vec![issue(1)]),
                RecordingDispatcher {
                    parked_issues: HashSet::from([1]),
                    ..Default::default()
                },
            ),
            (FakeSource::once(vec![issue(2)]), RecordingDispatcher::default()),
        ];
        let report = tick_multi(&mut multi, &[0, 0], 10, &[false, false]);

        assert_eq!(report.skipped_labeled, 1);
        assert_eq!(report.errors, 0);
        assert_eq!(report.dispatched, 1);
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
            complexity: None,
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
    // per-token concurrency factor (#3947), CPU term REMOVED (#4512)
    // ===================================================================

    #[test]
    fn test_dynamic_cap_is_min_of_three_inputs() {
        // Never exceeds any bound. `usize::MAX` ram = the term doesn't bind.
        assert_eq!(resolve_dynamic_max_concurrent(10, usize::MAX, 10), 10);
        assert_eq!(resolve_dynamic_max_concurrent(3, usize::MAX, 9), 3, "disk binds");
        assert_eq!(resolve_dynamic_max_concurrent(9, usize::MAX, 4), 4, "maxConcurrent binds");
    }

    #[test]
    fn test_dynamic_cap_has_no_cpu_term_at_all() {
        // #4512 AC1: the arity itself is the guard (a >3-arg call no longer
        // compiles), and the value must be independent of host CPU state: this
        // same input yields the same cap on a 95%-idle 8-core worker and on a
        // saturated one, which is the whole point — an idle host must not be
        // throttled to 2 by an estimate (`estCoresPerSweep`) that priced every
        // sweep as a build.
        assert_eq!(resolve_dynamic_max_concurrent(36, usize::MAX, 10), 10);
    }

    #[test]
    fn test_dynamic_cap_has_no_token_axis_term_either() {
        // #5270 AC1: a starved token pool (few/no healthy accounts) must NOT
        // cap the dynamic concurrency — only disk headroom, RAM headroom, and
        // the configured ceiling do. The arity itself is the guard (a >3-arg
        // call no longer compiles); this asserts the *value* is independent of
        // any token-pool state, unlike the pre-#5270 formula which would have
        // pinned this to (near-)zero when accounts were exhausted.
        assert_eq!(
            resolve_dynamic_max_concurrent(36, usize::MAX, 10),
            10,
            "disk (36) and ceiling (10) alone determine the cap"
        );
    }

    #[test]
    fn test_dynamic_cap_disk_headroom_bound() {
        // A nearly-full scratch volume (disk headroom 1) caps concurrency at 1
        // even with a high ceiling.
        assert_eq!(resolve_dynamic_max_concurrent(1, usize::MAX, 8), 1);
        // A full volume (0 headroom) drops the cap to 0 — dispatch nothing.
        // Disk meters an exhaustible resource, so this hard floor stays.
        assert_eq!(resolve_dynamic_max_concurrent(0, usize::MAX, 8), 0);
    }

    #[test]
    fn test_dynamic_cap_ram_headroom_bound() {
        // #5270 AC3: critically-low available RAM caps concurrency exactly the
        // way disk headroom already does — same posture, same hard floor.
        assert_eq!(resolve_dynamic_max_concurrent(usize::MAX, 1, 8), 1, "ram binds");
        assert_eq!(resolve_dynamic_max_concurrent(usize::MAX, 0, 8), 0, "ram exhausted");
        // Whichever of disk/ram is smaller binds, regardless of position.
        assert_eq!(resolve_dynamic_max_concurrent(2, 5, 100), 2, "disk (2) < ram (5)");
        assert_eq!(resolve_dynamic_max_concurrent(5, 2, 100), 2, "ram (2) < disk (5)");
    }

    #[test]
    fn test_dynamic_cap_zero_disk_dispatches_nothing() {
        // No disk headroom ⇒ cap 0 ⇒ a subsequent tick dispatches nothing.
        let cap = resolve_dynamic_max_concurrent(0, usize::MAX, 10);
        assert_eq!(cap, 0);
        let mut source = FakeSource::once((1..=3).map(issue).collect());
        let mut disp = RecordingDispatcher::default();
        let report = tick(&mut source, &mut disp, cap, false).unwrap();
        assert_eq!(report.dispatched, 0);
        assert_eq!(report.deferred_capacity, 3);
        assert!(disp.dispatched.is_empty());
    }

    #[test]
    fn test_dynamic_cap_zero_ram_dispatches_nothing() {
        // No available RAM ⇒ cap 0 ⇒ a subsequent tick dispatches nothing —
        // mirrors test_dynamic_cap_zero_disk_dispatches_nothing exactly.
        let cap = resolve_dynamic_max_concurrent(usize::MAX, 0, 10);
        assert_eq!(cap, 0);
        let mut source = FakeSource::once((1..=3).map(issue).collect());
        let mut disp = RecordingDispatcher::default();
        let report = tick(&mut source, &mut disp, cap, false).unwrap();
        assert_eq!(report.dispatched, 0);
        assert_eq!(report.deferred_capacity, 3);
        assert!(disp.dispatched.is_empty());
    }

    #[test]
    fn test_dynamic_cap_unbounded_by_max_concurrent_default() {
        // A machine whose operator has NOT tuned the knob rides the shipped
        // default rather than an estimate of its cores or its token pool.
        assert_eq!(
            resolve_dynamic_max_concurrent(100, usize::MAX, DEFAULT_WORK_FINDER_MAX_CONCURRENT),
            DEFAULT_WORK_FINDER_MAX_CONCURRENT
        );
    }

    // ===================================================================
    // Dynamic cap composed with tick — scale-up / scale-to-zero (#3811)
    // ===================================================================

    #[test]
    fn test_scale_up_with_growing_backlog_bounded_by_dynamic_cap() {
        // Fixed resources: disk=4, ceiling=10 ⇒ dynamic cap 4. As the backlog
        // grows tick-over-tick, effective concurrency scales up but is bounded
        // by the cap (min(cap, backlog)).
        let cap = resolve_dynamic_max_concurrent(4, usize::MAX, 10);
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
        let cap = resolve_dynamic_max_concurrent(5, usize::MAX, 5);
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
    #[serial(loom_config_env)]
    fn test_config_missing_file_is_all_none() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        let cfg = read_work_finder_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(cfg, WorkFinderConfig::default());
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_malformed_json_is_all_none() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), "{not valid json");
        let cfg = read_work_finder_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(cfg, WorkFinderConfig::default());
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_missing_autonomous_block_is_all_none() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"terminals": []}"#);
        let cfg = read_work_finder_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(cfg, WorkFinderConfig::default());
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_missing_work_finder_block_is_all_none() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"mainHealthGate": {"enabled": true}}}"#);
        let cfg = read_work_finder_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(cfg, WorkFinderConfig::default());
    }

    #[test]
    fn test_config_full_block_is_parsed() {
        let tmp = tempfile::tempdir().unwrap();
        // `perTokenConcurrency` is deliberately included here even though it is
        // retired (#5743) and no longer has a corresponding `WorkFinderConfig`
        // field — this is exactly the "a fleet host still has the old key in its
        // committed config" scenario, and parsing must silently ignore it (a
        // `serde_json::Value` walk never errors on an unread key) rather than
        // failing to start.
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
                // Retired keys are recorded (accepted-but-ignored), not parsed.
                deprecated_cpu_keys: vec!["cpuUtilizationTarget", "estCoresPerSweep"],
            }
        );
    }

    // ===================================================================
    // Retired cpuUtilizationTarget / estCoresPerSweep knobs: accepted but
    // IGNORED, never a config error (#4512, replacing the #4032 parsing tests)
    // ===================================================================

    #[test]
    fn test_deprecated_cpu_knobs_are_accepted_and_recorded_not_parsed() {
        // A fleet upgrades the daemon binary before it edits every repo's
        // committed config, so a stale key must parse fine and simply do nothing
        // — recorded only so the deprecation warning can name it.
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous": {"cpuUtilizationTarget": 0.5, "estCoresPerSweep": 1.5,
                "workFinder": {"enabled": true, "maxConcurrent": 10}}}"#,
        );
        let cfg = read_work_finder_config(tmp.path());
        assert_eq!(cfg.deprecated_cpu_keys, vec!["cpuUtilizationTarget", "estCoresPerSweep"]);
        // The live knobs in the same block still parse normally: a deprecated
        // sibling must never poison the rest of the config.
        assert_eq!(cfg.enabled, Some(true));
        assert_eq!(cfg.max_concurrent, Some(10));
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_deprecated_cpu_knobs_accepted_at_any_value_including_nonsense() {
        // Pre-#4512 these were range-filtered/type-checked because a value was
        // consumed. Nothing consumes them now, so out-of-range, wrong-type, and
        // even absurd values are all equally inert — and equally non-fatal.
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        for body in [
            r#"{"autonomous": {"cpuUtilizationTarget": 0}}"#,
            r#"{"autonomous": {"cpuUtilizationTarget": 1.5}}"#,
            r#"{"autonomous": {"estCoresPerSweep": -2}}"#,
            r#"{"autonomous": {"estCoresPerSweep": "many"}}"#,
            r#"{"autonomous": {"estCoresPerSweep": true}}"#,
        ] {
            let tmp = tempfile::tempdir().unwrap();
            write_config(tmp.path(), body);
            let cfg = read_work_finder_config(tmp.path());
            assert_eq!(cfg.deprecated_cpu_keys.len(), 1, "must be accepted-but-noted: {body}");
            // And it must not disturb any live knob.
            assert_eq!(cfg.max_concurrent, None);
            assert_eq!(cfg.enabled, None);
        }
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    }

    #[test]
    fn test_deprecated_cpu_knobs_absent_or_null_are_not_reported() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"workFinder": {"maxConcurrent": 4}}}"#);
        assert!(read_work_finder_config(tmp.path())
            .deprecated_cpu_keys
            .is_empty());

        // An explicit `null` is "not set" — warning about it would be noise.
        let tmp2 = tempfile::tempdir().unwrap();
        write_config(tmp2.path(), r#"{"autonomous": {"estCoresPerSweep": null}}"#);
        assert!(read_work_finder_config(tmp2.path())
            .deprecated_cpu_keys
            .is_empty());
    }

    #[test]
    fn test_warn_deprecated_cpu_knobs_is_a_noop_without_any_retired_setting() {
        // No config keys, no env vars — nothing to warn about, and (crucially)
        // no panic and no config error. The one-shot `Once` inside means this
        // test cannot assert the log line itself; the observable contract is
        // that it is safe and side-effect-free on the clean path.
        warn_deprecated_cpu_knobs(&WorkFinderConfig::default());
    }

    #[test]
    #[serial]
    fn test_deprecated_cpu_knob_notice_is_none_when_nothing_is_set() {
        for var in DEPRECATED_CPU_ENV_VARS {
            std::env::remove_var(var);
        }
        assert!(deprecated_cpu_knob_notice(&WorkFinderConfig::default()).is_none());
    }

    #[test]
    #[serial]
    fn test_deprecated_cpu_knob_notice_names_the_config_keys_that_are_set() {
        for var in DEPRECATED_CPU_ENV_VARS {
            std::env::remove_var(var);
        }
        let cfg = WorkFinderConfig {
            deprecated_cpu_keys: vec!["estCoresPerSweep"],
            ..Default::default()
        };
        let notice = deprecated_cpu_knob_notice(&cfg).expect("a set key must produce a notice");
        assert!(notice.contains("estCoresPerSweep"), "must name the key: {notice}");
        assert!(notice.contains("IGNORED"), "must say it is ignored: {notice}");
        // Actionable: it must point at the knob that replaced it.
        assert!(
            notice.contains("autonomous.workFinder.maxConcurrent"),
            "must name the replacement knob: {notice}"
        );
        // A config-only notice must not fabricate an env source.
        assert!(!notice.contains("env LOOM_"), "no env source was set: {notice}");
    }

    #[test]
    #[serial]
    fn test_deprecated_cpu_knob_notice_names_env_vars_and_combines_both_sources() {
        std::env::set_var(DEPRECATED_CPU_ENV_VARS[0], "0.85");
        let env_only = deprecated_cpu_knob_notice(&WorkFinderConfig::default())
            .expect("a set env var must produce a notice");
        assert!(env_only.contains(DEPRECATED_CPU_ENV_VARS[0]), "{env_only}");

        let both = deprecated_cpu_knob_notice(&WorkFinderConfig {
            deprecated_cpu_keys: vec!["estCoresPerSweep"],
            ..Default::default()
        })
        .expect("notice");
        assert!(both.contains("estCoresPerSweep"), "{both}");
        assert!(both.contains(DEPRECATED_CPU_ENV_VARS[0]), "{both}");
        assert!(both.contains(" and "), "both sources must be joined: {both}");

        for var in DEPRECATED_CPU_ENV_VARS {
            std::env::remove_var(var);
        }
    }

    #[test]
    fn test_deprecated_knob_name_tables_stay_in_sync() {
        // The config keys and env vars are two halves of one deprecation; a
        // future edit that adds one must add the other (the warning names both).
        assert_eq!(DEPRECATED_CPU_CONFIG_KEYS.len(), DEPRECATED_CPU_ENV_VARS.len());
        assert!(DEPRECATED_CPU_ENV_VARS
            .iter()
            .all(|v| v.starts_with("LOOM_")));
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_retired_per_token_concurrency_key_is_ignored() {
        // #5743: `perTokenConcurrency` fed a disclaimed, causally-irrelevant
        // status number and has been fully retired — `WorkFinderConfig` no
        // longer has a field for it. A host whose committed
        // `.loom/config.json` still sets the key (this repo's own included, at
        // the time this issue was filed) must start cleanly and simply ignore
        // it, not fail to parse.
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"perTokenConcurrency": 3}}"#);
        let cfg = read_work_finder_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(cfg, WorkFinderConfig::default());
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_enabled_false_is_disabled_flag() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"workFinder": {"enabled": false}}}"#);
        let cfg = read_work_finder_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
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
            r#"{"autonomous": {"workFinder": {"enabled": true, "maxConcurrent": 5}}}"#,
        );
        let cfg = read_work_finder_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(cfg.enabled, Some(true));
        assert_eq!(cfg.max_concurrent, Some(5));
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
    fn test_retired_per_token_concurrency_env_var_is_silently_ignored() {
        // #5743: `LOOM_PER_TOKEN_CONCURRENCY` no longer resolves to anything —
        // no function reads it any more. Setting it must not be a startup
        // error and must not affect the dynamic cap, which has had no token
        // term since #5270.
        std::env::set_var("LOOM_PER_TOKEN_CONCURRENCY", "7");
        let cfg = WorkFinderConfig {
            max_concurrent: Some(10),
            ..Default::default()
        };
        assert_eq!(resolve_max_concurrent_with_config(&cfg), 10);
        assert_eq!(
            resolve_dynamic_max_concurrent(36, usize::MAX, 10),
            10,
            "a retired env var must not clamp the cap"
        );
        std::env::remove_var("LOOM_PER_TOKEN_CONCURRENCY");
    }

    #[test]
    #[serial]
    fn test_retired_cpu_env_vars_are_ignored_not_honored() {
        // #4512: `LOOM_CPU_UTILIZATION_TARGET` / `LOOM_EST_CORES_PER_SWEEP` no
        // longer resolve to anything (the functions that read them are gone).
        // The observable contract is that setting them changes NO cap input and
        // is not an error — only the deprecation warning notices them.
        for var in DEPRECATED_CPU_ENV_VARS {
            std::env::set_var(var, "0.01");
        }
        let cfg = WorkFinderConfig {
            max_concurrent: Some(10),
            ..Default::default()
        };
        assert_eq!(resolve_max_concurrent_with_config(&cfg), 10);
        assert_eq!(
            resolve_dynamic_max_concurrent(36, usize::MAX, 10),
            10,
            "a retired env knob must not clamp the cap"
        );
        // Safe to call with only env-side deprecation present (no config keys).
        warn_deprecated_cpu_knobs(&cfg);
        for var in DEPRECATED_CPU_ENV_VARS {
            std::env::remove_var(var);
        }
    }

    // ===================================================================
    // Token-capacity advisory transitions (#3902)
    // ===================================================================

    fn pressured_assessment() -> capacity::PressureAssessment {
        // token_limit 0 (zero healthy accounts); 12 deferred ⇒ genuinely
        // token-starved + pressured.
        let snap = capacity::RankingSnapshot {
            total: 7,
            available: 0,
            exhausted: 7,
            ..capacity::RankingSnapshot::default()
        };
        capacity::assess_pressure(Some(&snap), 7, 0, 12, capacity::DEFAULT_ADVISORY_MIN_QUEUED)
    }

    fn calm_assessment() -> capacity::PressureAssessment {
        // Nothing deferred ⇒ not pressured (healthy pool).
        let snap = capacity::RankingSnapshot {
            total: 7,
            available: 7,
            ..capacity::RankingSnapshot::default()
        };
        capacity::assess_pressure(Some(&snap), 7, 7, 0, capacity::DEFAULT_ADVISORY_MIN_QUEUED)
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
                assert_eq!(healthy_accounts, 0);
                assert!(message.contains("loom-daemon tokens bootstrap"));
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

    /// Issue #6007 — the livelock, from the *admission* side. The work finder
    /// reads exactly one bit (`DrainState::flag`, surfaced here as `draining`), so
    /// what matters is what a **refused** drain deadline does to that bit. Before
    /// #6007 the first refusal cleared it, dispatch resumed, more sweeps were
    /// admitted, and the next drain was strictly harder to satisfy — a busy host
    /// could never roll. Now the refusal *retains* the roll, so admission stays
    /// held and the in-flight set can actually reach zero; only once the roll is
    /// abandoned (its paused-dispatch budget spent) does admission resume, so real
    /// work is never blocked indefinitely.
    #[test]
    fn test_admission_stays_held_across_a_roll_refusal_then_resumes_when_abandoned() {
        use crate::ipc::{DrainState, RollRefusal};

        let states = WorkspaceHealthStates::new();
        let root_a = std::path::PathBuf::from("/tmp/repo-a");
        let root_b = std::path::PathBuf::from("/tmp/repo-b");
        let roots = [root_a, root_b];

        let drain = DrainState::new();
        let _ = drain.begin(std::time::Duration::from_secs(1800), false, false);
        assert_eq!(
            dispatch_held_per_root_with_drain(&states, &roots, true, drain.is_draining()),
            vec![true, true],
            "a scheduled drain holds every root"
        );

        // The deadline passes with sweeps still in flight: refused, roll retained.
        let started = drain.snapshot().started_at.expect("started_at");
        assert!(matches!(
            drain.refuse_roll_deadline(started + chrono::Duration::seconds(1800)),
            RollRefusal::Deferred { .. }
        ));
        assert_eq!(
            dispatch_held_per_root_with_drain(&states, &roots, true, drain.is_draining()),
            vec![true, true],
            "#6007: admission must STAY held across the refusal — resuming here is the livelock"
        );

        // Budget spent: the roll is abandoned and admission resumes, so a wedged
        // sweep cannot starve the host of work forever.
        assert!(matches!(
            drain.refuse_roll_deadline(started + chrono::Duration::seconds(7200)),
            RollRefusal::Abandoned { .. }
        ));
        assert_eq!(
            dispatch_held_per_root_with_drain(&states, &roots, true, drain.is_draining()),
            vec![false, false],
            "an abandoned roll returns the admission window to the work finder"
        );
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
    // Pre-flight-advisory dispatch hold (#5030)
    // ===================================================================

    #[test]
    fn test_dispatch_held_per_root_with_preflight_holds_only_the_tripped_root() {
        // A workspace whose pre-flight breaker is holding (broken .mcp.json)
        // holds only its own root; a healthy sibling keeps dispatching — the
        // #3930 per-repo isolation contract must survive the #5030 fold.
        let states = WorkspaceHealthStates::new();
        let root_a = std::path::PathBuf::from("/tmp/repo-a"); // pre-flight held
        let root_b = std::path::PathBuf::from("/tmp/repo-b"); // healthy
        let preflight_held = [true, false];
        let held = dispatch_held_per_root_with_preflight(
            &states,
            &[root_a, root_b],
            true,
            &preflight_held,
        );
        assert_eq!(
            held,
            vec![true, false],
            "only the tripped root is held; its healthy sibling keeps dispatching"
        );
    }

    #[test]
    fn test_dispatch_held_per_root_with_preflight_empty_slice_matches_per_root() {
        // An all-false / missing pre-flight slice is byte-for-byte the plain
        // per-root vector — the fold is a pure superset, never a regression of
        // the #4084 / #3930 semantics.
        let states = WorkspaceHealthStates::new();
        let root_a = std::path::PathBuf::from("/tmp/repo-a");
        let root_b = std::path::PathBuf::from("/tmp/repo-b");
        states.get_or_create(&root_a).set_gate_in_flight(true);
        states.get_or_create(&root_b).set_halted(true);
        let roots = [root_a, root_b];
        let plain = dispatch_held_per_root(&states, &roots, true);
        let folded = dispatch_held_per_root_with_preflight(&states, &roots, true, &[]);
        assert_eq!(plain, folded, "empty pre-flight slice ⇒ identical to dispatch_held_per_root");
    }

    #[test]
    fn test_dispatch_held_per_root_with_preflight_composes_with_verified_red() {
        // The pre-flight hold and the #3930 verified-red hold compose
        // additively per root: root_a is held by pre-flight, root_b by a red
        // main, root_c is healthy on both axes.
        let states = WorkspaceHealthStates::new();
        let root_a = std::path::PathBuf::from("/tmp/repo-a");
        let root_b = std::path::PathBuf::from("/tmp/repo-b");
        let root_c = std::path::PathBuf::from("/tmp/repo-c");
        states.get_or_create(&root_b).set_halted(true);
        let held = dispatch_held_per_root_with_preflight(
            &states,
            &[root_a, root_b, root_c],
            true,
            &[true, false, false],
        );
        assert_eq!(held, vec![true, true, false]);
    }

    #[test]
    fn test_preflight_held_root_dispatches_zero_new_sweeps() {
        // Regression (#5030): end-to-end through `tick_multi`, a workspace whose
        // pre-flight advisory has tripped (its breaker is holding) dispatches
        // ZERO new sweeps even with a full backlog, while its healthy sibling
        // takes the shared slot — the burn-every-slot incident cannot recur.
        // Mirrors `test_gate_in_flight_root_dispatches_zero_new_sweeps`.
        let states = WorkspaceHealthStates::new();
        let root_a = std::path::PathBuf::from("/tmp/repo-a"); // pre-flight held
        let root_b = std::path::PathBuf::from("/tmp/repo-b"); // healthy
        let halted =
            dispatch_held_per_root_with_preflight(&states, &[root_a, root_b], true, &[true, false]);

        let mut multi = vec![
            (FakeSource::once((1..=5).map(issue).collect()), RecordingDispatcher::default()),
            (FakeSource::once(vec![issue(10)]), RecordingDispatcher::default()),
        ];
        let report = tick_multi(&mut multi, &[], 10, &halted);

        assert!(report.halted, "the pre-flight-held root marks the tick as halted");
        assert_eq!(report.seen, 6, "both backlogs are still observed");
        assert!(
            multi[0].1.dispatched.is_empty(),
            "a pre-flight-broken workspace dispatches zero new sweeps (no slot burn)"
        );
        assert_eq!(
            multi[1].1.dispatched,
            vec![10],
            "the healthy sibling keeps dispatching against the shared budget"
        );
    }

    #[test]
    fn test_preflight_probe_tick_resumes_dispatch_to_recovering_root() {
        // Under the half-open design, a probe tick reports the root as NOT held
        // (`preflight_held=false` for that root), so `tick_multi` lets one
        // dispatch through to test recovery — proving the breaker never blocks
        // the very dispatch needed to prove the workspace is fixed.
        let states = WorkspaceHealthStates::new();
        let root_a = std::path::PathBuf::from("/tmp/repo-a"); // probing this tick
        let root_b = std::path::PathBuf::from("/tmp/repo-b"); // healthy
                                                              // A probe tick maps `PreflightDispatchGate::Probe` → not held.
        let halted = dispatch_held_per_root_with_preflight(
            &states,
            &[root_a, root_b],
            true,
            &[false, false],
        );

        let mut multi = vec![
            (FakeSource::once(vec![issue(1)]), RecordingDispatcher::default()),
            (FakeSource::once(vec![issue(10)]), RecordingDispatcher::default()),
        ];
        let report = tick_multi(&mut multi, &[], 10, &halted);
        assert!(!report.halted);
        assert_eq!(
            multi[0].1.dispatched,
            vec![1],
            "a probe tick lets one dispatch through to the recovering root"
        );
        assert_eq!(multi[1].1.dispatched, vec![10]);
    }

    // ===================================================================
    // Last-tick publication (#4761)
    // ===================================================================

    #[test]
    #[serial(work_finder_last_tick)]
    fn last_tick_summary_is_none_before_any_tick() {
        reset_last_tick_summary();
        assert!(last_tick_summary().is_none());
    }

    #[test]
    #[serial(work_finder_last_tick)]
    fn publishing_a_tick_makes_its_counters_readable_cross_process() {
        reset_last_tick_summary();
        let at = chrono::Utc::now();
        let report = TickReport {
            seen: 12,
            dispatched: 2,
            skipped_in_flight: 9,
            skipped_pr_open: 1,
            errors: 0,
            halted: false,
            ..TickReport::default()
        };
        publish_tick_summary_at(&report, 7, at);

        let summary = last_tick_summary().expect("a published tick must be readable");
        assert_eq!(summary.at, at);
        assert_eq!(summary.max_concurrent, 7);
        assert_eq!(summary.seen, 12);
        assert_eq!(summary.dispatched, 2);
        assert_eq!(summary.skipped_in_flight, 9);
        assert_eq!(summary.skipped_pr_open, 1);
        assert!(!summary.halted);
    }

    #[test]
    #[serial(work_finder_last_tick)]
    fn publishing_replaces_the_previous_tick() {
        reset_last_tick_summary();
        publish_tick_summary(
            &TickReport {
                seen: 1,
                ..TickReport::default()
            },
            3,
        );
        publish_tick_summary(
            &TickReport {
                seen: 99,
                ..TickReport::default()
            },
            4,
        );
        let summary = last_tick_summary().unwrap();
        assert_eq!(summary.seen, 99);
        assert_eq!(summary.max_concurrent, 4);
    }

    /// The rendered summary must show only the *non-zero* skip reasons, so an
    /// operator reading one line is not scanning a wall of zeros.
    #[test]
    fn reason_summary_omits_zero_terms() {
        let summary = crate::types::WorkFinderTickSummary {
            seen: 12,
            dispatched: 2,
            skipped_in_flight: 10,
            ..Default::default()
        };
        assert_eq!(summary.reason_summary(), "12 seen, 2 dispatched, 10 in-flight-skip");
    }

    #[test]
    fn reason_summary_flags_a_halted_tick() {
        let summary = crate::types::WorkFinderTickSummary {
            seen: 5,
            halted: true,
            ..Default::default()
        };
        assert!(summary.reason_summary().ends_with("HALTED"));
    }

    /// Issue #5302: `TickReport::collisions` was already logged on the
    /// per-tick `work_finder: tick — …` line (#4085) but never reached the
    /// wire-carried [`crate::types::WorkFinderTickSummary`], so
    /// `loom-daemon status` / `GetDaemonStatus` could not see a cross-host
    /// collision without scraping the daemon log. Assert the count now
    /// survives publication.
    #[test]
    fn publish_tick_summary_carries_collisions_through() {
        reset_last_tick_summary();
        publish_tick_summary(
            &TickReport {
                seen: 4,
                dispatched: 1,
                collisions: 3,
                ..TickReport::default()
            },
            2,
        );
        let summary = last_tick_summary().unwrap();
        assert_eq!(summary.collisions, 3, "collision total must survive publication");
        assert!(
            summary
                .reason_summary()
                .contains("3 cross-host-collision(s)"),
            "reason_summary must surface a non-zero collision count: {}",
            summary.reason_summary()
        );
    }

    /// A clean tick (no collisions) must not mention collisions at all — the
    /// same "only non-zero terms" discipline every other counter follows.
    #[test]
    fn reason_summary_omits_zero_collisions() {
        let summary = crate::types::WorkFinderTickSummary {
            seen: 1,
            dispatched: 1,
            ..Default::default()
        };
        assert!(!summary.reason_summary().contains("collision"));
    }
}
