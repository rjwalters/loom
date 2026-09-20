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
//! it with a cap **recomputed every tick** by [`resolve_dynamic_max_concurrent`]
//! from two live inputs — the worktree-root disk headroom
//! ([`crate::disk_headroom::disk_headroom_limit`]) and the host's
//! available-RAM headroom (#5270, [`crate::ram_headroom::ram_headroom_limit`])
//! — bounded by the per-machine operator ceiling
//! (`LOOM_WORK_FINDER_MAX_CONCURRENT` / `autonomous.workFinder.maxConcurrent`).
//! **That ceiling is the one term this "every tick" framing does not cover
//! (#6203)**: unlike disk/RAM, it is resolved once at daemon bring-up
//! ([`resolve_max_concurrent_with_config`], captured by the caller and
//! threaded into [`spawn_multi_work_finder_task`] as a plain `usize`) and does
//! not itself change value tick-to-tick — an operator edit to
//! `autonomous.workFinder.maxConcurrent` takes effect only on the next daemon
//! restart. The effective per-tick concurrency is then
//! `min(dynamic_cap, backlog_depth)`: [`tick`] iterates the ready
//! `loom:issue` rows and stops at the cap, so concurrency scales **up** as the
//! backlog grows and drains to **zero** dispatches when the queue is empty —
//! all without a daemon restart for the disk/RAM/backlog terms, since those
//! are read fresh each tick (the configured ceiling is the exception, above).
//! Token-pool health ([`crate::tokens::token_pool_size`] /
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

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::Result;

use crate::capacity::{self, CapacityAdvisory};
use crate::disk_headroom::disk_headroom_limit;
use crate::event_bus::EventBus;
use crate::main_health_gate::{MainHealthState, WorkspaceHealthStates};
use crate::sweep_registry::{
    DispatchBackoffError, LeaseOrderDispatchError, LiveClaimDispatchError, OpenPrDispatchError,
    ParkedIssueDispatchError, TokenSelectionDispatchError, WorkspaceCommandsMissingDispatchError,
};
use crate::tokens::{token_pool_size, token_pool_size_at_dir};
use crate::types::{Event, WorkFinderTickSummary};
use crate::workspace_pool::WorkspacePool;

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
/// ([`resolve_dynamic_max_concurrent`]) is bounded by disk headroom and ram
/// headroom in addition to this ceiling, so this is an upper bound, not a
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

/// Environment variable overriding the per-workspace/per-repo **additional**
/// skip-label list (Issue #6685) — a comma-separated list of label names,
/// e.g. `blocked-upstream,needs-vendor-fix`. Overrides
/// `autonomous.workFinder.extraSkipLabels` when set (even to an empty
/// string, which resolves to an empty list — the same "env decides, full
/// stop" precedence [`WORK_FINDER_ENABLE_ENV`] uses, not a soft merge with
/// config). See [`resolve_extra_skip_labels_with_config`].
pub const WORK_FINDER_EXTRA_SKIP_LABELS_ENV: &str = "LOOM_WORK_FINDER_EXTRA_SKIP_LABELS";

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
///
/// **One narrow exemption exists (#6893).** `loom:operator-only`'s park is
/// capability-aware for the `loom:operator-mechanical` sub-kind *only*: an item
/// carrying both labels, declaring `<!-- loom:capability=<name> -->` markers
/// (#6892) that this worker's own `LOOM_WORKER_CAPABILITIES` declaration fully
/// covers, may be dispatched into a propose-mode lane instead of parked. See
/// [`WorkItem::is_skipped_with_capabilities`] and [`crate::capability`]. The
/// list here is unchanged and stays the authoritative *set* of park labels —
/// the exemption is applied by the callers that opt into it, never by removing
/// a label from this constant, and it is inert unless a host opts in.
///
/// **`loom:operator` deliberately does NOT belong here (vibesql#6664).** The
/// generic hold is *re-evaluable* by design (`defaults/docs/label-state-machine.md`:
/// "the hold that put the label on can also be the mechanism that takes it back
/// off"), so it must never refuse the dispatch routes that re-evaluate held
/// work — the watchdogs' cancel-and-re-dispatch, the reaper's checkpoint-resume,
/// and an operator's explicit `loom-daemon dispatch <N>` override. Its
/// automation-stop effect on *new builder candidates* is expressed one layer
/// up, in [`SKIP_LABELS`] via [`OPERATOR_HOLD_LABEL`]: the work finder does not
/// START fresh `--claim-owned` builds on a held item (the vibesql#6664
/// 3×-in-13-minutes redispatch loop), while every park-guarded route above
/// keeps working the item. The invariant is pinned by test in
/// `work_finder::tests`.
pub const PARK_LABELS: &[&str] = &["loom:blocked", "loom:operator-only"];

/// The daemon's own claim label. Disqualifies a *fresh* work-finder candidate
/// (a `loom:building` row is already being worked), but is NOT a park — see
/// [`PARK_LABELS`].
pub const BUILDING_LABEL: &str = "loom:building";

/// The generic operator hold — "the engine has stopped on this artifact and a
/// human must act" (`defaults/docs/label-state-machine.md`). Disqualifies a
/// *fresh work-finder candidate* exactly like [`BUILDING_LABEL`] does, but is
/// **not** a park ([`PARK_LABELS`]) and must never become one: the hold is
/// re-evaluable by design, and the park-guarded routes (watchdog re-dispatch,
/// reaper checkpoint-resume, explicit `loom-daemon dispatch <N>`) must keep
/// reaching held items.
///
/// vibesql#6664: a sweep that concludes "a human is needed" releases its claim
/// (restoring `loom:issue`) and applies this label in one motion. Without this
/// constant in [`SKIP_LABELS`] the work finder immediately re-listed the issue
/// and dispatched another `--claim-owned` builder onto the fresh hold —
/// observed 3× in 13 minutes on vibesql#6172 (each new session noticed the
/// hold in the comment trail and declined, which is luck, not a contract).
/// Skipping the candidate here IS the contract; the human (or the re-evaluation
/// lanes) takes it from there.
pub const OPERATOR_HOLD_LABEL: &str = "loom:operator";

/// Labels that disqualify an issue from dispatch even if it still appears in
/// the `loom:issue`-filtered listing.
///
/// A `loom:issue` row should never itself carry these (they are mutually
/// exclusive states in the `.github/labels.yml` state machine), but `gh`'s
/// label cache can be briefly stale, so the finder checks defensively.
///
/// Composed as [`BUILDING_LABEL`] + [`PARK_LABELS`] + [`OPERATOR_HOLD_LABEL`]
/// rather than re-listing the label strings, so the constants can never drift
/// apart (#4444). The operator hold sits in this list but NOT in
/// [`PARK_LABELS`]: it stops the work finder from *starting* new builder work
/// on a held item (vibesql#6664) without refusing the dispatch routes that
/// re-evaluate held work — dispatch step 2.7's park guard deliberately consults
/// `PARK_LABELS` only, so `loom:operator` alone never trips it.
pub const SKIP_LABELS: &[&str] = &[
    BUILDING_LABEL,
    PARK_LABELS[0],
    PARK_LABELS[1],
    OPERATOR_HOLD_LABEL,
];

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
    /// The issue's RFC-3339 `updatedAt` timestamp, when the listing supplied
    /// it (Issue #6685) — the ETag-cached REST listing already returns it at
    /// zero extra cost (see [`crate::forge_listing::RestIssue::updated_at`]).
    ///
    /// Used as the "last checked" proxy for [`Self::is_within_recheck_interval`]:
    /// every dispatch (the `loom:issue → loom:building` label flip) and every
    /// sweep pass that appends so much as a comment advances an issue's own
    /// `updatedAt`, so it is a reasonable stand-in for "when was this last
    /// looked at" without the work-finder maintaining its own per-issue
    /// last-dispatch clock. `None` (a synthetic item, or a listing without
    /// timestamps) simply disables the recheck-interval check for that item.
    pub updated_at: Option<String>,
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
            updated_at: None,
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
            updated_at: None,
        }
    }

    /// Builder-style setter for the issue body (#4827).
    #[must_use]
    pub fn with_body(mut self, body: Option<String>) -> Self {
        self.body = body;
        self
    }

    /// Builder-style setter for the issue's `updatedAt` timestamp (#6685).
    #[must_use]
    pub fn with_updated_at(mut self, updated_at: Option<String>) -> Self {
        self.updated_at = updated_at;
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

    /// True when the issue carries any [`SKIP_LABELS`] entry OR any of
    /// `extra_skip_labels` (Issue #6685) — a per-workspace/per-repo
    /// configurable list of **additional** skip-label names beyond the
    /// hardcoded [`PARK_LABELS`], resolved via
    /// [`resolve_extra_skip_labels_with_config`]. Lets a repo-local label
    /// (e.g. `blocked-upstream`) that is never going to be renamed to a
    /// `loom:*` label still act as a durable park, without weakening
    /// [`SKIP_LABELS`] itself — [`BUILDING_LABEL`] is never part of
    /// `extra_skip_labels` in the resolved config (see
    /// [`resolve_extra_skip_labels_with_config`]'s doc for the guard), so this
    /// can never re-introduce the `loom:building`-is-never-a-park regression
    /// [`SKIP_LABELS`]'s own doc comment warns against.
    ///
    /// An empty `extra_skip_labels` (the default — no config, no env
    /// override) makes this byte-for-byte [`Self::is_skipped`].
    #[must_use]
    pub fn is_skipped_with_extra(&self, extra_skip_labels: &[String]) -> bool {
        self.is_skipped()
            || self
                .labels
                .iter()
                .any(|l| extra_skip_labels.iter().any(|e| e == l))
    }

    /// How the capability-aware `loom:operator-mechanical` lane (#6893) routes
    /// this item, given the capabilities this worker declares it holds.
    ///
    /// A thin adapter over [`crate::capability::route_mechanical`] — the labels
    /// and body already fetched by the listing, no extra forge read. Returns
    /// [`MechanicalRouting::NotApplicable`](crate::capability::MechanicalRouting::NotApplicable)
    /// for every item that is not a `loom:operator-only` +
    /// `loom:operator-mechanical` pair, which is all but a handful.
    #[must_use]
    pub fn mechanical_routing(
        &self,
        held_capabilities: &BTreeSet<String>,
    ) -> crate::capability::MechanicalRouting {
        crate::capability::route_mechanical(&self.labels, self.body.as_deref(), held_capabilities)
    }

    /// This issue's host-affinity constraint (#7456) — the union of its
    /// `loom:host:<id>` labels and `<!-- loom:requires-host=<id> -->` body
    /// markers, with any-of semantics. A thin adapter over
    /// [`crate::host_affinity::host_constraint`], using the labels and body
    /// the listing already fetched (no extra forge read). Empty for the
    /// overwhelmingly common case — an issue that never references this
    /// convention — which makes [`HostConstraint::matches`](crate::host_affinity::HostConstraint::matches)
    /// true for every host.
    #[must_use]
    pub fn host_constraint(&self) -> crate::host_affinity::HostConstraint {
        crate::host_affinity::host_constraint(&self.labels, self.body.as_deref())
    }

    /// True when the issue is skipped, with the `loom:operator-only` hard park
    /// made **capability-aware for the mechanical sub-kind only** (#6893, AC1).
    ///
    /// Identical to [`Self::is_skipped_with_extra`] except for one narrow
    /// exemption: an item carrying `loom:operator-only` **and**
    /// `loom:operator-mechanical`, whose body declares capabilities (the
    /// `<!-- loom:capability=<name> -->` markers from #6892) that
    /// `held_capabilities` fully covers, is **not** skipped — it is dispatchable
    /// into the propose-mode lane. Everything else about the park is unchanged:
    ///
    /// - The other three sub-kinds (`loom:operator-decision`,
    ///   `loom:operator-blocked`, `loom:operator-objective`) stay hard-skipped
    ///   unconditionally, marker or no marker.
    /// - `loom:blocked`, `loom:needs-capability` and `loom:operator` veto the
    ///   exemption outright, as does [`BUILDING_LABEL`] and any
    ///   `extra_skip_labels` entry — the exemption only ever relaxes
    ///   `loom:operator-only`, never any other reason to skip.
    /// - No declaration, an unrecognized value, or a capability this worker does
    ///   not hold all leave the item skipped (fail-closed).
    ///
    /// **An empty `held_capabilities` — the default on every host, since
    /// [`crate::capability::held_capabilities`] reads an environment variable
    /// nobody sets by default — makes this byte-for-byte
    /// [`Self::is_skipped_with_extra`].** The lane is opt-in per host and inert
    /// until a host opts in.
    #[must_use]
    pub fn is_skipped_with_capabilities(
        &self,
        extra_skip_labels: &[String],
        held_capabilities: &BTreeSet<String>,
    ) -> bool {
        let skipped = self.is_skipped_with_extra(extra_skip_labels);
        if !skipped || held_capabilities.is_empty() {
            // Fast path: nothing to relax, or no worker declaration at all.
            return skipped;
        }
        // The exemption may only cancel `loom:operator-only`. If any OTHER skip
        // reason applies, the item stays skipped regardless of capabilities.
        let other_skip_reason = self.labels.iter().any(|l| {
            (SKIP_LABELS.contains(&l.as_str()) && l != crate::capability::OPERATOR_ONLY_LABEL)
                || extra_skip_labels.iter().any(|e| e == l)
        });
        if other_skip_reason {
            return true;
        }
        !self.mechanical_routing(held_capabilities).is_dispatchable()
    }

    /// True when the issue carries the [`URGENT_LABEL`] (#3946) — it dispatches
    /// ahead of non-urgent siblings in the same workspace-priority tier.
    #[must_use]
    pub fn is_urgent(&self) -> bool {
        self.labels.iter().any(|l| l == URGENT_LABEL)
    }

    /// The issue's self-declared minimum re-check interval (Issue #6685),
    /// extracted from a `<!-- loom:recheck-interval=<value> -->` marker in
    /// [`Self::body`] — in the spirit of the Curator's
    /// `<!-- loom:complexity=<tier> -->` marker (see [`Self::complexity`]),
    /// but declaring a standing polling policy rather than a cost stratum.
    ///
    /// `<value>` is a bare duration: an integer optionally followed by a
    /// single unit suffix — `s` (seconds, the default when no suffix is
    /// given), `m` (minutes), `h` (hours), or `d` (days). E.g.
    /// `<!-- loom:recheck-interval=6h -->`. `None` when no body was fetched,
    /// no marker is present, or the value is empty/zero/malformed.
    #[must_use]
    pub fn recheck_interval(&self) -> Option<Duration> {
        self.body
            .as_deref()
            .and_then(extract_recheck_interval_marker)
            .and_then(parse_recheck_interval_value)
    }

    /// True when this item declares a [`Self::recheck_interval`] AND its
    /// [`Self::updated_at`] is still within that interval of `now` — i.e. the
    /// issue told the work-finder up front it does not need re-checking yet
    /// (Issue #6685).
    ///
    /// Deliberately independent of and orthogonal to
    /// [`WorkDispatcher::noop_cooldown`] (Issue #6670): that mechanism is
    /// dispatcher-armed, only after an explicit self-report from a completed
    /// sweep pass ("no actionable delta THIS time"); this one is
    /// issue-declared, in effect from the moment the marker is added,
    /// independent of any sweep having run at all. An issue can be in neither,
    /// either, or both cooldowns at once — this check never reads
    /// `noop_cooldown` state and vice versa.
    ///
    /// `false` whenever either half is missing (no marker, or `updated_at`
    /// absent/unparseable) — a byte-for-byte no-op for every issue that does
    /// not carry the marker, which is every issue today.
    #[must_use]
    pub fn is_within_recheck_interval(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        let Some(interval) = self.recheck_interval() else {
            return false;
        };
        let Some(updated_at) = self.updated_at.as_deref() else {
            return false;
        };
        let Ok(updated_at) = chrono::DateTime::parse_from_rfc3339(updated_at) else {
            return false;
        };
        let Ok(interval) = chrono::Duration::from_std(interval) else {
            return false;
        };
        now.signed_duration_since(updated_at.with_timezone(&chrono::Utc)) < interval
    }
}

/// The marker key inside the `<!-- ... -->` comment declaring a tracker
/// issue's self-declared minimum re-check interval (Issue #6685). Mirrors
/// [`crate::script_helpers::sweep_experiment`]'s `loom:complexity=` marker
/// convention but lives here (rather than in that module) since it is a
/// work-finder-only concept, never read by dispatch-time complexity
/// stratification.
const RECHECK_INTERVAL_MARKER_KEY: &str = "loom:recheck-interval=";

/// Extract the LAST well-formed `<!-- loom:recheck-interval=<value> -->`
/// value from `body`, if any (Issue #6685).
///
/// Line-oriented and last-match-wins, mirroring
/// [`crate::script_helpers::sweep_experiment::extract_complexity_marker`]'s
/// contract — the canonical marker placement is at the end of the body, and a
/// marker split across a newline is not recognized (grep-line semantics).
/// Simpler than that function's non-overlapping multi-match-per-line scan:
/// this marker is expected to appear at most once, so a single `find` per
/// line is sufficient and avoids duplicating the general-purpose scanner for
/// a syntax with a different value vocabulary (a duration, not a closed tier
/// enum).
fn extract_recheck_interval_marker(body: &str) -> Option<&str> {
    body.lines().rev().find_map(|line| {
        let idx = line.find(RECHECK_INTERVAL_MARKER_KEY)?;
        // Anchor to the canonical `<!-- ... -->` comment form so prose that
        // merely mentions the marker key (e.g. this very doc comment, if it
        // ever ends up quoted in an issue body) does not false-fire.
        if !line[..idx].trim_end().ends_with("<!--") {
            return None;
        }
        let after = &line[idx + RECHECK_INTERVAL_MARKER_KEY.len()..];
        let end = after.find("-->")?;
        let value = after[..end].trim();
        if value.is_empty() {
            None
        } else {
            Some(value)
        }
    })
}

/// Parse a bare duration value (`<N>[s|m|h|d]`, e.g. `6h`, `45m`, `2d`, or a
/// plain integer defaulting to seconds) into a [`Duration`] (Issue #6685).
/// `None` on empty, zero, non-numeric, an unrecognized unit suffix, or
/// overflow.
fn parse_recheck_interval_value(value: &str) -> Option<Duration> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let split_at = value
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(value.len());
    let (num_part, unit) = value.split_at(split_at);
    let num: u64 = num_part.parse().ok()?;
    if num == 0 {
        return None;
    }
    let multiplier: u64 = match unit.trim() {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return None,
    };
    num.checked_mul(multiplier).map(Duration::from_secs)
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

    /// The subset of [`backed_off`](Self::backed_off) whose window was armed
    /// specifically by the open-PR guard (#4123) refusing dispatch, rather
    /// than a real dispatch failure (Issue #7606). Checked immediately after
    /// `backed_off` so a candidate matching both is attributed once, to this
    /// more specific reason (`pr-open-backoff` rather than `backoff-skip`) —
    /// this is the pre-`dispatch()` visibility for the ladder
    /// [`crate::sweep_registry::SweepRegistry::record_open_pr_guard_backoff`]
    /// arms: a guarded issue's steady-state re-dispatch attempts drop to the
    /// ladder's cadence, and this counter makes that deferral visible instead
    /// of folding it into the generic backoff tally.
    ///
    /// Defaults to empty so a dispatcher that does not model this cause (e.g.
    /// a test fake) opts out with zero boilerplate — every backed-off
    /// candidate then falls through to the pre-#7606 `skipped_backoff`
    /// counter unchanged.
    fn pr_open_backed_off(&self) -> HashSet<u32> {
        HashSet::new()
    }

    /// The set of issue numbers currently inside a **no-op re-dispatch
    /// cooldown window** (Issue #6670): a sweep self-reported "no actionable
    /// delta this pass" (checkpoint written, claim released cleanly, zero
    /// forge/issue mutation) and the cooldown it armed has not yet elapsed.
    /// Skipped exactly like [`quarantined`](Self::quarantined) /
    /// [`backed_off`](Self::backed_off) — filtered out *before* the
    /// concurrency budget is filled, so a cooling-down candidate never
    /// reserves a shared dispatch slot.
    ///
    /// Distinct from and independent of both `quarantined` and `backed_off`:
    /// this is armed only by an explicit self-report
    /// (`RecordNoopRelease`/`loom-daemon noop-cooldown record`), never
    /// inferred from a crash or a failed-dispatch classification, so an issue
    /// can be quarantined, backed off, and no-op-cooling-down all at once
    /// without any of the three interfering with the others.
    ///
    /// Defaults to empty so a dispatcher that does not model the cooldown
    /// (e.g. a test fake) opts out with zero boilerplate.
    fn noop_cooldown(&self) -> HashSet<u32> {
        HashSet::new()
    }

    /// The set of issue numbers currently inside a **hard-exclusion decline
    /// cooldown window** (Issue #7528): a sweep for this issue exited cleanly
    /// without a checkpoint while the issue carried a
    /// [`crate::hard_exclusion::HARD_EXCLUSION_LABELS`] entry — i.e. it
    /// declined on a label rule, not on the work — and the cooldown the reaper
    /// armed has not yet elapsed. Skipped exactly like
    /// [`quarantined`](Self::quarantined) / [`backed_off`](Self::backed_off) /
    /// [`noop_cooldown`](Self::noop_cooldown) — filtered out *before* the
    /// concurrency budget is filled.
    ///
    /// This is the **backstop** half of #7528, not the primary filter: the
    /// candidate filter drops a hard-excluded issue outright (see
    /// [`crate::hard_exclusion::declining_label`]), so this set only ever
    /// matters for issues that reached dispatch by some other route — a label
    /// added after dispatch, an explicit `dispatch_sweep`/CLI dispatch, a
    /// watchdog or reaper-driven resume.
    ///
    /// Defaults to empty so a dispatcher that does not model the cooldown
    /// (e.g. a test fake) opts out with zero boilerplate.
    fn declined(&self) -> HashSet<u32> {
        HashSet::new()
    }

    /// Whether this dispatcher's workspace is missing
    /// `.claude/commands/loom/sweep.md` — the **structural, workspace-level**
    /// refusal the registry's step-2.4 guard (#4027) enforces via the typed
    /// [`WorkspaceCommandsMissingDispatchError`]. Unlike
    /// [`quarantined`](Self::quarantined) / [`backed_off`](Self::backed_off),
    /// this is not a per-issue set: EVERY candidate in this workspace would
    /// be refused identically, so callers check it once per workspace per
    /// tick and skip the entire candidate batch — instead of calling
    /// `dispatch()` once per ready issue only to rediscover the same
    /// refusal and log it every time (Issue #6440's 865-refusals-in-an-hour
    /// incident).
    ///
    /// Defaults to `false` so a dispatcher that does not model this (e.g. a
    /// test fake) opts out with zero boilerplate.
    fn workspace_commands_missing(&self) -> bool {
        false
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

    /// Additional label names — beyond the hardcoded [`PARK_LABELS`] —
    /// this dispatcher's workspace wants treated as a skip/park signal
    /// (Issue #6685), resolved once per workspace via
    /// [`resolve_extra_skip_labels_with_config`] (env > config
    /// `autonomous.workFinder.extraSkipLabels` > default `[]`, mirroring
    /// every other `autonomous.workFinder.*` knob's precedence). Read fresh
    /// each tick, alongside [`quarantined`](Self::quarantined) /
    /// [`backed_off`](Self::backed_off) / [`noop_cooldown`](Self::noop_cooldown),
    /// and checked via [`WorkItem::is_skipped_with_extra`] at the same point
    /// [`WorkItem::is_skipped`] is checked today.
    ///
    /// Defaults to empty so a dispatcher that does not model this (e.g. a
    /// test fake) opts out with zero boilerplate and **zero behavior
    /// change** — an empty list makes `is_skipped_with_extra` byte-for-byte
    /// `is_skipped`.
    fn extra_skip_labels(&self) -> Vec<String> {
        Vec::new()
    }

    /// The capabilities **this worker/host declares it holds** (#6893), used to
    /// decide whether a `loom:operator-mechanical` item's declared
    /// `<!-- loom:capability=<name> -->` requirements are met — see
    /// [`WorkItem::is_skipped_with_capabilities`]. Read fresh each tick,
    /// alongside [`extra_skip_labels`](Self::extra_skip_labels).
    ///
    /// Defaults to **empty**, which is both the zero-boilerplate opt-out for a
    /// test fake and the real default on every host: the production
    /// implementation resolves it from the `LOOM_WORKER_CAPABILITIES`
    /// environment variable (never from repo config — see
    /// [`crate::capability`]'s "declared by the host, never by the repo"), and
    /// nothing sets that variable by default. An empty set makes
    /// `is_skipped_with_capabilities` byte-for-byte `is_skipped_with_extra`,
    /// i.e. **zero behavior change**.
    fn declared_capabilities(&self) -> BTreeSet<String> {
        BTreeSet::new()
    }

    /// This host's identity (#7456), used to decide whether a candidate's
    /// host-affinity constraint (`loom:host:<id>` label / `<!--
    /// loom:requires-host=<id> -->` marker — see [`crate::host_affinity`])
    /// matches this worker. Read once per tick, alongside
    /// [`declared_capabilities`](Self::declared_capabilities).
    ///
    /// Defaults to [`crate::sweep_registry::host_identity`] — the same
    /// `$LOOM_HOST_ID` / `$HOSTNAME` / `hostname`-binary resolution every
    /// other host-identity consumer in the daemon (peer-claim
    /// self-recognition, observability) already uses, so this feature can
    /// never disagree with them about what "this host" means. A test fake
    /// overrides this to a fixed string rather than mutating process-global
    /// env vars.
    fn current_host_id(&self) -> String {
        crate::sweep_registry::host_identity()
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
        skipped_workspace_commands_missing: report.skipped_workspace_commands_missing,
        skipped_pr_open: report.skipped_pr_open,
        skipped_peer_claim: report.skipped_peer_claim,
        skipped_backoff: report.skipped_backoff,
        skipped_pr_open_backoff: report.skipped_pr_open_backoff,
        skipped_noop_cooldown: report.skipped_noop_cooldown,
        skipped_declined: report.skipped_declined,
        skipped_recheck_interval: report.skipped_recheck_interval,
        skipped_host_constraint: report.skipped_host_constraint,
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
    /// Issues skipped because their workspace is missing
    /// `.claude/commands/loom/sweep.md` (Issue #4027 guard 2.4, quarantined at
    /// the work-finder level by #6440). Unlike every other `skipped_*`
    /// counter here, this is incremented **once per gated workspace per
    /// tick**, not once per candidate — the whole point is that the finder no
    /// longer calls `dispatch()` (and gets the same
    /// [`WorkspaceCommandsMissingDispatchError`](crate::sweep_registry::WorkspaceCommandsMissingDispatchError)
    /// back, and logs it) once per ready issue in a structurally broken
    /// workspace, every tick, forever.
    pub skipped_workspace_commands_missing: usize,
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
    /// The subset of [`skipped_backoff`](Self::skipped_backoff) whose window
    /// was armed specifically by the open-PR guard (#4123) refusing dispatch,
    /// rather than a real dispatch failure (Issue #7606) — filtered out
    /// before the capacity gate via [`WorkDispatcher::pr_open_backed_off`].
    /// Mutually exclusive with `skipped_backoff`: a candidate counted here is
    /// never also counted there. Makes the #4485 ladder's steady-state
    /// deferral of a guarded issue visible as its own tally, distinct from
    /// both a generic backoff skip and an active-tick [`skipped_pr_open`]
    /// refusal.
    pub skipped_pr_open_backoff: usize,
    /// Issues skipped because they are inside a no-op re-dispatch cooldown
    /// window (Issue #6670): a sweep self-reported "no actionable delta this
    /// pass" via `RecordNoopRelease` and the cooldown it armed has not yet
    /// elapsed. Filtered out before the capacity gate via
    /// [`WorkDispatcher::noop_cooldown`], exactly like
    /// [`skipped_quarantined`](Self::skipped_quarantined) /
    /// [`skipped_backoff`](Self::skipped_backoff) — a distinct counter because
    /// this is a **successful, empty** pass, never a crash or a failed
    /// dispatch.
    pub skipped_noop_cooldown: usize,
    /// Issues skipped because a **hard-exclusion rule** applies (Issue #7528) —
    /// one counter covering both halves of that fix:
    ///
    /// 1. the candidate itself carries a
    ///    [`crate::hard_exclusion::HARD_EXCLUSION_LABELS`] entry (`external`
    ///    today), so no role has standing to act on it at all; or
    /// 2. a previous sweep for it already declined on such a rule and the
    ///    reaper's decline cooldown ([`WorkDispatcher::declined`]) has not
    ///    elapsed.
    ///
    /// Deliberately its own counter rather than folded into
    /// [`skipped_labeled`](Self::skipped_labeled): a park label says "a human
    /// took this out of the queue", a hard exclusion says "this issue is not
    /// Loom's to work on yet". Conflating them hides an intake backlog inside
    /// the park tally — and an operator watching `labeled-skip` climb has no
    /// way to tell which of the two they are looking at.
    pub skipped_declined: usize,
    /// Issues skipped because they self-declared a `<!-- loom:recheck-interval=
    /// <value> -->` marker (Issue #6685) and their own `updatedAt` is still
    /// within that interval — see [`WorkItem::is_within_recheck_interval`].
    /// Filtered out before the capacity gate, exactly like
    /// [`skipped_noop_cooldown`](Self::skipped_noop_cooldown), but a distinct
    /// counter: this is issue-declared policy, checked independently of and
    /// without reading any `noop_cooldown` state.
    pub skipped_recheck_interval: usize,
    /// Issues skipped because their host-affinity constraint (#7456 —
    /// `loom:host:<id>` label / `<!-- loom:requires-host=<id> -->` body
    /// marker, see [`crate::host_affinity`]) does not name this host.
    /// Checked before the in-flight/capacity gates, exactly like
    /// [`skipped_recheck_interval`](Self::skipped_recheck_interval), and
    /// carries none of a real skip label's state side effects: no claim
    /// flip, no comment, no cooldown/backoff record — this candidate is not
    /// actionable on this host at all, so it is never even attempted.
    pub skipped_host_constraint: usize,
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
    /// Candidates deferred THIS TICK because they fell outside this host's
    /// preferred repo slice while the slice still had at least one eligible
    /// in-slice candidate (Issue #6243, [`tick_multi_with_sharding`]).
    /// Always `0` when sharding is not configured at the call site
    /// (`preferred_slice: None`) — see `defaults/docs/dispatcher-repo-sharding.md`.
    /// Purely observational (mirrors [`deferred_saturation`](Self::deferred_saturation)'s
    /// shape): these candidates are NOT lost — they remain ready and are
    /// re-evaluated (and, if still out-of-slice with the slice non-empty,
    /// deferred again) on the next tick.
    pub deferred_out_of_slice: usize,
}

/// Log — at DEBUG, once per skipped candidate — that a candidate was dropped
/// for carrying a hard-exclusion label (#7528), naming the rule.
///
/// DEBUG rather than INFO on purpose. The candidate listing re-evaluates the
/// same rows every tick, so an INFO here would reproduce the #6440
/// 865-refusals-in-an-hour shape for an intake backlog that is doing exactly
/// what it should (sitting still until a maintainer clears the label). The
/// operator-visible signal is the per-tick `declined-skip` count on the
/// `work_finder: tick — …` line, plus the reaper's threshold WARN
/// (`SweepRegistry::record_decline`) for an issue that actually reached
/// dispatch and burned a session.
fn log_hard_exclusion_skip(issue: u32, rule: &str) {
    log::debug!(
        "work_finder: skipping issue #{issue} — carries the hard-exclusion label `{rule}`, \
         which every Loom role declines on; a maintainer must remove it (or close the issue) \
         before it is dispatchable (#7528)"
    );
}

/// Log — once per skipped candidate — *why* a `loom:operator-mechanical` item
/// stayed parked, naming the capability gap (#6893 AC1/AC3).
///
/// This is the daemon-side half of "turn a silent stall into a capability
/// request": the item is still parked (it must be — nothing about it changed),
/// but the reason is now stated instead of being invisible inside a
/// `labeled-skip` tally. The forge-comment half of AC1 lives on the sweep side
/// (`sweep.md`'s "Capability-aware `loom:operator-mechanical` lane"), which is
/// the surface that actually resolves these items: the daemon's own candidate
/// listing is `loom:issue`-filtered and re-evaluates the same rows every tick,
/// so commenting from here would either spam the issue or need a whole
/// dedup-state mechanism to avoid it.
///
/// Costs nothing on the overwhelmingly common path: [`MechanicalRouting::NotApplicable`]
/// for every item that is not a `loom:operator-only` + `loom:operator-mechanical`
/// pair, and the routing is short-circuited entirely when this host declared no
/// capabilities.
///
/// [`MechanicalRouting::NotApplicable`]: crate::capability::MechanicalRouting::NotApplicable
fn log_capability_gap(item: &WorkItem, held_capabilities: &BTreeSet<String>) {
    use crate::capability::MechanicalRouting;
    if held_capabilities.is_empty() {
        return;
    }
    match item.mechanical_routing(held_capabilities) {
        MechanicalRouting::MissingCapabilities { missing } => {
            log::info!(
            "work_finder: issue #{} is `loom:operator-mechanical` but this worker does not hold \
             the capability it declares: {} (held: {}). It stays parked — file/comment a \
             capability request naming the gap rather than waiting (#6893)",
            item.number,
            missing.join(", "),
            held_capabilities.iter().cloned().collect::<Vec<_>>().join(", ")
        )
        }
        MechanicalRouting::NoDeclaration { unknown } if !unknown.is_empty() => log::info!(
            "work_finder: issue #{} declares capability value(s) outside the closed vocabulary \
             and fails closed (stays parked): {}. Fix the marker or extend the vocabulary in \
             defaults/docs/label-state-machine.md (#6892)",
            item.number,
            unknown.join(", ")
        ),
        // A bare mechanical item with no marker at all parks exactly as it did
        // before this lane existed — that is not news, so it is not logged.
        MechanicalRouting::NoDeclaration { .. }
        | MechanicalRouting::NotApplicable
        | MechanicalRouting::ProposeDispatch => {}
    }
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
    // The subset of `backed_off` armed by the open-PR guard rather than a
    // real dispatch failure (Issue #7606) — checked alongside `backed_off`
    // so the pre-filter below can attribute the skip more specifically.
    let pr_open_backed_off = dispatcher.pr_open_backed_off();
    let noop_cooldown = dispatcher.noop_cooldown();
    // Hard-exclusion decline cooldown (#7528) — resolved once per tick,
    // mirroring `noop_cooldown` above. Empty on every host until a sweep
    // actually declines on a label rule.
    let declined = dispatcher.declined();
    let peer_claimed = dispatcher.peer_claimed();
    // Per-workspace additional skip-label list (#6685) — resolved once per
    // tick, mirroring every other dispatcher-supplied set above.
    let extra_skip_labels = dispatcher.extra_skip_labels();
    // Capabilities this host declares it holds (#6893) — resolved once per
    // tick, like `extra_skip_labels` above. Empty on every host that has not
    // opted in, which makes the check below byte-for-byte the pre-#6893 one.
    let held_capabilities = dispatcher.declared_capabilities();
    // This host's identity (#7456) — resolved once per tick, like
    // `held_capabilities` above, so an issue's host-affinity constraint can
    // be checked per candidate without a repeated env/hostname resolution.
    let current_host_id = dispatcher.current_host_id();
    let now = chrono::Utc::now();
    // Workspace-commands guard tripwire (#6440, quarantining #4027 guard
    // 2.4): read ONCE per tick, not once per candidate — every ready issue in
    // this workspace would be refused identically, so the loop below skips
    // the whole batch on this single flag instead of calling `dispatch()`
    // per candidate to rediscover (and log) the same structural refusal.
    let workspace_commands_missing = dispatcher.workspace_commands_missing();
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
        // 0. Workspace-commands guard tripwire (#6440): the whole workspace
        //    is missing .claude/commands/loom/sweep.md, so every candidate
        //    is refused identically. Skip without ever calling dispatch().
        if workspace_commands_missing {
            report.skipped_workspace_commands_missing += 1;
            continue;
        }
        // 0b. Host-affinity constraint (#7456): the issue pins itself to
        //     specific host id(s) via a `loom:host:<id>` label and/or a
        //     `<!-- loom:requires-host=<id> -->` body marker (any-of
        //     semantics; see `crate::host_affinity`). A non-matching host
        //     skips with a single INFO line and none of a real skip label's
        //     state side effects — no claim flip (`dispatch()` is never
        //     called), no comment, no cooldown/backoff record — because this
        //     candidate is not actionable on this host at all, not merely
        //     deferred. Checked before every other gate so it never reserves
        //     a slot or consults any per-issue state either. The empty
        //     constraint (the default — no issue references this
        //     convention) always matches, so this is a no-op for every
        //     candidate that predates #7456.
        let host_constraint = item.host_constraint();
        if !host_constraint.matches(&current_host_id) {
            report.skipped_host_constraint += 1;
            log::info!(
                "work_finder: skipping issue #{} — requires host {}, this is {}",
                item.number,
                host_constraint.describe(),
                current_host_id
            );
            continue;
        }
        // 1. Defensive skip-label filter (stale forge cache), extended with
        //    this workspace's configured extra skip-label list (#6685) and made
        //    capability-aware for the `loom:operator-mechanical` sub-kind
        //    (#6893 — inert unless this host declared capabilities).
        if item.is_skipped_with_capabilities(&extra_skip_labels, &held_capabilities) {
            report.skipped_labeled += 1;
            log_capability_gap(&item, &held_capabilities);
            continue;
        }
        // 1a. Hard-exclusion labels (#7528): the same rule the Curator and
        //     Builder role prompts enforce (`external` today — see
        //     `crate::hard_exclusion`), applied HERE so the issue never costs
        //     a dispatch, a claim flip, or an agent session in the first
        //     place. Before #7528 this rule lived only in the markdown
        //     prompts, so such an issue was dispatched every tick, declined
        //     ~90s later, and had its claim released straight back into the
        //     candidate pool.
        //
        //     Counted under its own `declined-skip` reason rather than
        //     `labeled-skip`: this is not an operator park, it is "no role has
        //     standing to act on this issue yet".
        if let Some(rule) = crate::hard_exclusion::declining_label(&item.labels) {
            report.skipped_declined += 1;
            log_hard_exclusion_skip(item.number, rule);
            continue;
        }
        // 1b. Self-declared re-check interval (#6685): the issue's own body
        //     names a minimum polling interval and its own last forge
        //     activity is still within it. Independent of and never reads
        //     `noop_cooldown` state.
        if item.is_within_recheck_interval(now) {
            report.skipped_recheck_interval += 1;
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
        //      nor re-flips its label every tick. Issue #7606: when the window
        //      was armed by the open-PR guard rather than a real dispatch
        //      failure, attribute it to the more specific `pr_open_backoff`
        //      counter instead — mutually exclusive with `skipped_backoff`.
        if backed_off.contains(&item.number) {
            if pr_open_backed_off.contains(&item.number) {
                report.skipped_pr_open_backoff += 1;
            } else {
                report.skipped_backoff += 1;
            }
            continue;
        }
        // 2b3. No-op re-dispatch cooldown (#6670): a sweep self-reported "no
        //      actionable delta this pass" and its cooldown window has not yet
        //      elapsed. Skipped here — before the capacity gate, like quarantine
        //      and backoff — so it neither reserves a slot nor re-flips its
        //      label every tick, and independently of both those mechanisms.
        if noop_cooldown.contains(&item.number) {
            report.skipped_noop_cooldown += 1;
            continue;
        }
        // 2b4. Hard-exclusion decline cooldown (#7528): a previous sweep for
        //      this issue declined on a hard-exclusion label rule and the
        //      window the reaper armed has not elapsed. The backstop for
        //      issues that reached dispatch by a route step 1a does not cover
        //      (a label added after dispatch, an explicit CLI/IPC dispatch, a
        //      watchdog resume). Skipped here — before the capacity gate, like
        //      quarantine/backoff/no-op — so it neither reserves a slot nor
        //      re-flips its label every tick.
        if declined.contains(&item.number) {
            report.skipped_declined += 1;
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
                // Issue #7482: the only place the past-tense "dispatched" line
                // is logged — a confirmed new spawn, past every pre-spawn
                // guard `dispatch()` runs internally.
                log::info!("work_finder: dispatched issue #{}", item.number);
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
                if let Some(open_pr) = e.downcast_ref::<OpenPrDispatchError>() {
                    report.skipped_pr_open += 1;
                    // #6350 AC: the log line must name the open PR, not just
                    // gesture at "an open linked PR" — this holds regardless
                    // of which host originally opened it, since the guard
                    // itself is a forge (not host-local) probe.
                    log::info!(
                        "work_finder: skipping issue #{} — it already has an open linked PR \
                         #{} (#4123 open-PR guard)",
                        item.number,
                        open_pr.pr
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
                } else if e.downcast_ref::<LeaseOrderDispatchError>().is_some() {
                    // Lease-order tie-break loss (#6287, Epic #6165 Phase 2):
                    // this host lost a race to an earlier lease and stood
                    // down before spawning anything. It is a deliberate
                    // skip, not a failure — and `dispatch()` itself already
                    // armed this issue's dispatch backoff (#6350) so the
                    // very next tick does not immediately repeat the same
                    // losing race, hence attributing it to the same
                    // `skipped_backoff` counter that window governs.
                    report.skipped_backoff += 1;
                    log::info!("work_finder: skipping issue #{} — {e}", item.number);
                } else if e
                    .downcast_ref::<WorkspaceCommandsMissingDispatchError>()
                    .is_some()
                {
                    // Defense in depth (#6440): the pre-loop
                    // `workspace_commands_missing` snapshot above should have
                    // already skipped every candidate this tick without ever
                    // reaching `dispatch()`. Reaching here means the
                    // condition appeared mid-tick (a race, not the steady
                    // state) — still a deliberate skip, not a failure, so it
                    // shares the same counter rather than inflating
                    // `errors`.
                    report.skipped_workspace_commands_missing += 1;
                    log::warn!("work_finder: skipping issue #{} — {e}", item.number);
                } else if e.downcast_ref::<TokenSelectionDispatchError>().is_some() {
                    // Empty/unusable token pool (#4689, typed by #6614). Still a
                    // real failure — it keeps the `errors` tally it has always
                    // had — but named explicitly here, because the operator
                    // remedy ("fix the pool") is completely different from any
                    // other dispatch error, and because the registry has just
                    // armed both brakes for it: this issue's own #4485 backoff,
                    // and the cross-issue counter that trips the #4386/#5030
                    // workspace hold once N DIFFERENT issues have died this way.
                    // Without the hold this loop would re-dispatch the whole
                    // backlog into the same dead pool every tick, forever.
                    report.errors += 1;
                    log::warn!(
                        "work_finder: dispatch for issue #{} died at token selection — the token \
                         pool is empty or every account is bad-marked (#6614): {e}",
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
    tick_multi_with_sharding(
        workspaces,
        priorities,
        max_concurrent,
        halted,
        max_admissions_per_tick,
        saturation_held,
        None,
    )
}

/// Like [`tick_multi_with_saturation_brake`], but additionally applies the
/// **repo-sharding slice preference** (Issue #6243, `defaults/docs/dispatcher-repo-sharding.md`):
/// when `preferred_slice` is `Some`, it must be a mask **parallel to
/// `workspaces`** (`preferred_slice[i]` is whether workspace `i` is in this
/// host's preferred slice, computed by the caller — in production, from
/// [`crate::role_shard::decide`]'s `owned` verdict per root, see
/// [`spawn_multi_work_finder_task`]).
///
/// The already-globally-sorted candidate list (by [`candidate_cmp`]) is split
/// into in-slice / out-of-slice while preserving each partition's relative
/// order, so `candidate_cmp`'s ordering guarantees hold WITHIN either
/// partition unchanged. Out-of-slice candidates are dispatched THIS TICK only
/// when the slice was completely empty at the top of the tick
/// (work-conservation, #6243's AC) — never interleaved by priority with
/// in-slice ones, so a lower-priority in-slice repo still holds a shared slot
/// ahead of a higher-priority OUT-of-slice repo for as long as this host has
/// ANY in-slice candidate at all.
///
/// `None` (what [`tick_multi_with_saturation_brake`] passes — e.g. an
/// unsharded host, or sharding not wired at the call site) is byte-for-byte
/// the pre-#6243 candidate list: every existing `tick_multi`
/// priority-ordering test is unaffected.
///
/// **Preference, not the role runner's hard filter.** [`crate::role_shard`]
/// (#6374) uses the same `owned` verdict to decide whether a workspace's role
/// rotation runs here *at all* — an unowned workspace simply never ticks. This
/// function deliberately does NOT do that: dispatch is work-conserving, so an
/// unowned repo is only ever *deferred behind* the owned ones, and is picked
/// up in full the moment this host's own slice runs dry.
pub fn tick_multi_with_sharding<S: WorkSource, D: WorkDispatcher>(
    workspaces: &mut [(S, D)],
    priorities: &[u32],
    max_concurrent: usize,
    halted: &[bool],
    max_admissions_per_tick: usize,
    saturation_held: bool,
    preferred_slice: Option<&[bool]>,
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

    // Snapshot each workspace's open-PR-guard-armed subset of `backed_off`
    // (Issue #7606) alongside it, so pass 1 can attribute a pre-filtered skip
    // more specifically than the generic `skipped_backoff` tally.
    let pr_open_backed_off_sets: Vec<HashSet<u32>> = workspaces
        .iter()
        .map(|(_, d)| d.pr_open_backed_off())
        .collect();

    // Snapshot each workspace's no-op-cooldown set (#6670) alongside its
    // dispatch-backoff set — dropped in pass 1 for the same reason: a
    // cooling-down candidate must not reserve a shared slot it cannot use, and
    // independently of both the quarantine and backoff sets above.
    let noop_cooldown_sets: Vec<HashSet<u32>> =
        workspaces.iter().map(|(_, d)| d.noop_cooldown()).collect();

    // Snapshot each workspace's hard-exclusion decline set (#7528) alongside
    // its no-op-cooldown set, dropped in pass 1 for the same reason.
    let declined_sets: Vec<HashSet<u32>> = workspaces.iter().map(|(_, d)| d.declined()).collect();

    // Snapshot each workspace's peer-claim set (#4028) alongside its quarantined
    // set. A peer's live soft claim drops the candidate in pass 1, before the
    // global sort and slot fill, so a peer-claimed issue never reserves a shared
    // dispatch slot.
    let peer_claimed_sets: Vec<HashSet<u32>> =
        workspaces.iter().map(|(_, d)| d.peer_claimed()).collect();

    // Snapshot each workspace's additional skip-label list (#6685) alongside
    // the peer-claim set — a per-workspace/per-repo configured list of extra
    // label names, beyond the hardcoded `PARK_LABELS`, this workspace wants
    // treated as a skip/park signal.
    let extra_skip_label_sets: Vec<Vec<String>> = workspaces
        .iter()
        .map(|(_, d)| d.extra_skip_labels())
        .collect();

    // Snapshot each workspace's declared host capabilities (#6893), used by the
    // capability-aware `loom:operator-mechanical` exemption in pass 1. Empty on
    // every host that has not opted in, which leaves the filter unchanged.
    let held_capability_sets: Vec<BTreeSet<String>> = workspaces
        .iter()
        .map(|(_, d)| d.declared_capabilities())
        .collect();

    // Snapshot each workspace's host identity (#7456) alongside the declared
    // capability sets — every workspace on one daemon process shares the same
    // physical host, so in practice these are all equal, but reading through
    // each dispatcher keeps this byte-for-byte consistent with the
    // single-workspace `tick_with_saturation_brake` path and with test fakes
    // that override it.
    let current_host_ids: Vec<String> = workspaces
        .iter()
        .map(|(_, d)| d.current_host_id())
        .collect();

    // Snapshot each workspace's workspace-commands-missing flag (#6440,
    // quarantining #4027 guard 2.4) alongside the other pre-filters. Unlike
    // those, this is a per-WORKSPACE bool, not a per-issue set: when set,
    // pass 1 below skips gathering ANY candidate from that workspace this
    // tick — the whole point is to stop calling `dispatch()` once per ready
    // issue in a structurally broken workspace, every tick.
    let commands_missing: Vec<bool> = workspaces
        .iter()
        .map(|(_, d)| d.workspace_commands_missing())
        .collect();

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

        // Workspace-commands guard tripwire (#6440): the whole workspace is
        // missing .claude/commands/loom/sweep.md, so every one of its ready
        // issues would be refused identically by dispatch()'s #4027 guard.
        // Skip the entire batch in one counter bump instead of gathering N
        // candidates only to have each one individually refused.
        if commands_missing[idx] {
            report.skipped_workspace_commands_missing += ready.len();
            continue;
        }

        let in_flight = &in_flights[idx];
        let workspace_priority = priorities
            .get(idx)
            .copied()
            .unwrap_or(DEFAULT_WORKSPACE_PRIORITY);
        let now = chrono::Utc::now();

        for item in ready {
            // Host-affinity constraint (#7456) — checked first, before any
            // other filter, mirroring `tick_with_saturation_brake`'s step 0b:
            // see that step's comment for the full rationale (no claim flip,
            // no comment, no cooldown record — not actionable on this host).
            let host_constraint = item.host_constraint();
            if !host_constraint.matches(&current_host_ids[idx]) {
                report.skipped_host_constraint += 1;
                log::info!(
                    "work_finder: skipping issue #{} — requires host {}, this is {}",
                    item.number,
                    host_constraint.describe(),
                    current_host_ids[idx]
                );
                continue;
            }
            if item.is_skipped_with_capabilities(
                &extra_skip_label_sets[idx],
                &held_capability_sets[idx],
            ) {
                report.skipped_labeled += 1;
                log_capability_gap(&item, &held_capability_sets[idx]);
                continue;
            }
            // Hard-exclusion labels (#7528) — mirrors
            // `tick_with_saturation_brake`'s step 1a: see that step's comment
            // for the full rationale (no role has standing, so never dispatch,
            // never claim, never spend a session).
            if let Some(rule) = crate::hard_exclusion::declining_label(&item.labels) {
                report.skipped_declined += 1;
                log_hard_exclusion_skip(item.number, rule);
                continue;
            }
            // Self-declared re-check interval (#6685): the issue's own body
            // names a minimum polling interval and its own last forge
            // activity is still within it — drop before the global queue,
            // independent of and never reading `noop_cooldown` state.
            if item.is_within_recheck_interval(now) {
                report.skipped_recheck_interval += 1;
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
            // window — drop before the global queue, like quarantine. Issue
            // #7606: attribute a window armed by the open-PR guard to the
            // more specific `pr_open_backoff` counter instead, mutually
            // exclusive with `skipped_backoff`.
            if backed_off_sets[idx].contains(&item.number) {
                if pr_open_backed_off_sets[idx].contains(&item.number) {
                    report.skipped_pr_open_backoff += 1;
                } else {
                    report.skipped_backoff += 1;
                }
                continue;
            }
            // No-op re-dispatch cooldown (#6670): a self-reported "no
            // actionable delta" pass whose cooldown has not yet elapsed —
            // drop before the global queue, like quarantine and backoff, and
            // independently of both.
            if noop_cooldown_sets[idx].contains(&item.number) {
                report.skipped_noop_cooldown += 1;
                continue;
            }
            // Hard-exclusion decline cooldown (#7528): a previous sweep for
            // this issue declined on a label rule and its window has not
            // elapsed — drop before the global queue, like the three brakes
            // above and independently of all of them.
            if declined_sets[idx].contains(&item.number) {
                report.skipped_declined += 1;
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

    // Repo-sharding slice preference (#6243): partition the ALREADY-sorted
    // candidate list into in-slice / out-of-slice, preserving each
    // partition's relative (already-sorted) order — see this function's own
    // doc comment for the full contract. `None` is a no-op: `candidates` is
    // left byte-for-byte unchanged, so the pre-#6243 priority-ordering tests
    // stay exactly as they were.
    let candidates: Vec<PriorityCandidate> = match preferred_slice {
        None => candidates,
        Some(slice) => {
            let (in_slice, out_of_slice): (Vec<_>, Vec<_>) = candidates
                .into_iter()
                .partition(|c| slice.get(c.workspace_idx).copied().unwrap_or(true));
            if in_slice.is_empty() {
                // Work-conservation (#6243 AC): this host's slice has zero
                // eligible candidates this tick — fall back to the full
                // (already globally sorted) out-of-slice queue instead of
                // starving while other repos have ready work.
                out_of_slice
            } else {
                report.deferred_out_of_slice += out_of_slice.len();
                in_slice
            }
        }
    };

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
                // Issue #7482 — see the single-workspace `tick` for the
                // rationale: past-tense line only on a confirmed new spawn.
                log::info!("work_finder: dispatched issue #{}", cand.number);
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
                if let Some(open_pr) = e.downcast_ref::<OpenPrDispatchError>() {
                    report.skipped_pr_open += 1;
                    // #6350 AC: name the open PR, not just "an open linked
                    // PR" — see the single-workspace `tick` for the rationale
                    // (this guard is a forge probe, so it holds cross-host).
                    log::info!(
                        "work_finder: skipping issue #{} — it already has an open linked PR \
                         #{} (#4123 open-PR guard)",
                        cand.number,
                        open_pr.pr
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
                } else if e.downcast_ref::<LeaseOrderDispatchError>().is_some() {
                    // Lease-order tie-break loss (#6287) — see the
                    // single-workspace `tick` for the rationale. `dispatch()`
                    // itself already armed this issue's dispatch backoff
                    // (#6350), so this lands on the same `skipped_backoff`
                    // counter that window governs.
                    report.skipped_backoff += 1;
                    log::info!("work_finder: skipping issue #{} — {e}", cand.number);
                } else if e
                    .downcast_ref::<WorkspaceCommandsMissingDispatchError>()
                    .is_some()
                {
                    // Defense in depth (#6440) — see the single-workspace
                    // `tick` for the rationale. Pass 1's `commands_missing`
                    // snapshot above should already have dropped every
                    // candidate from this workspace; reaching here means the
                    // condition appeared mid-tick, still a deliberate skip.
                    report.skipped_workspace_commands_missing += 1;
                    log::warn!("work_finder: skipping issue #{} — {e}", cand.number);
                } else if e.downcast_ref::<TokenSelectionDispatchError>().is_some() {
                    // Empty/unusable token pool (#6614) — see the
                    // single-workspace `tick` for the rationale. Counted as the
                    // real failure it is; named separately because the remedy
                    // is the pool, not the issue, and because the registry has
                    // just armed the per-issue backoff plus the cross-issue
                    // counter behind the #4386/#5030 workspace hold.
                    report.errors += 1;
                    log::warn!(
                        "work_finder: dispatch for issue #{} died at token selection — the token \
                         pool is empty or every account is bad-marked (#6614): {e}",
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
    /// `autonomous.workFinder.extraSkipLabels` — additional label names
    /// (Issue #6685) beyond the hardcoded [`PARK_LABELS`] this workspace
    /// wants the work-finder to treat as a skip/park signal (e.g. a
    /// repo-local `blocked-upstream` label). `None` when the key is absent —
    /// distinct from `Some(vec![])`, which an explicit `[]` in config would
    /// produce, though both resolve to the same empty-list default behavior.
    pub extra_skip_labels: Option<Vec<String>>,
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
        extra_skip_labels: wf.and_then(|w| w.get("extraSkipLabels")).and_then(|v| {
            v.as_array().map(|arr| {
                arr.iter()
                    .filter_map(|e| e.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
        }),
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
    resolve_max_concurrent_with_source(config).0
}

/// Which layer supplied a resolved **env > config > default** knob — surfaced
/// in startup/tick logs (#6203) so an operator can tell whether an observed
/// ceiling came from `LOOM_WORK_FINDER_MAX_CONCURRENT`, the committed
/// `autonomous.workFinder.maxConcurrent`, or the built-in fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    /// The env var override was set (and valid).
    Env,
    /// No env override; `.loom/config.json` supplied the value.
    Config,
    /// Neither env nor config set it; the built-in default applies.
    Default,
}

impl std::fmt::Display for ConfigSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ConfigSource::Env => "env",
            ConfigSource::Config => "config",
            ConfigSource::Default => "default",
        })
    }
}

/// Resolve the max-concurrency ceiling with precedence **env > config >
/// default**, naming which layer supplied the value (#6203). The companion of
/// [`resolve_max_concurrent_with_config`] for call sites — the startup log
/// line and the per-tick hot-reload below — that need to report *why* the
/// ceiling is what it is, not just the number.
#[must_use]
pub fn resolve_max_concurrent_with_source(config: &WorkFinderConfig) -> (usize, ConfigSource) {
    if let Some(v) = env_max_concurrent() {
        (v, ConfigSource::Env)
    } else if let Some(v) = config.max_concurrent {
        (v, ConfigSource::Config)
    } else {
        (DEFAULT_WORK_FINDER_MAX_CONCURRENT, ConfigSource::Default)
    }
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

/// Parse a comma-separated label list from [`WORK_FINDER_EXTRA_SKIP_LABELS_ENV`],
/// trimming whitespace and dropping empty entries. `Some(vec![])` for a set-but-
/// empty env var (still "env decides"); `None` when the var is unset.
fn env_extra_skip_labels() -> Option<Vec<String>> {
    std::env::var(WORK_FINDER_EXTRA_SKIP_LABELS_ENV)
        .ok()
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
}

/// Resolve the per-workspace/per-repo **additional** skip-label list with
/// precedence **env ([`WORK_FINDER_EXTRA_SKIP_LABELS_ENV`]) > config
/// (`autonomous.workFinder.extraSkipLabels`) > default (`[]`)** (Issue #6685)
/// — the same precedence every other `autonomous.workFinder.*` knob in this
/// module uses (see [`read_work_finder_config`]), not a new scheme.
///
/// Resolved once per workspace (mirroring
/// [`resolve_max_admissions_per_tick_with_config`]'s startup-capture
/// rationale — this is a static per-repo label list, not a live-changing
/// input), then supplied to [`WorkDispatcher::extra_skip_labels`] by the
/// concrete dispatcher.
///
/// **Never returns [`BUILDING_LABEL`]**, even if an operator's config or env
/// var names it — defensively filtered out here so a misconfiguration can
/// never weaken the invariant [`SKIP_LABELS`]'s own doc comment states:
/// `loom:building` legitimately marks the daemon's own in-flight claim and
/// must never be treated as a park, or the watchdogs' cancel-and-re-dispatch
/// and the reaper's checkpoint-resume both break.
#[must_use]
pub fn resolve_extra_skip_labels_with_config(config: &WorkFinderConfig) -> Vec<String> {
    let resolved = env_extra_skip_labels()
        .or_else(|| config.extra_skip_labels.clone())
        .unwrap_or_default();
    resolved
        .into_iter()
        .filter(|l| l != BUILDING_LABEL)
        .collect()
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
/// (Phase B, #3811; CPU term removed in #4512) — `min(disk headroom, ram
/// headroom, configured_max)` via [`resolve_dynamic_max_concurrent`] — from
/// two live inputs read fresh under `workspace_root` (disk and RAM headroom),
/// then runs one [`tick`] with it. Those two terms are **not** captured once
/// at startup, so a pool that grows/shrinks (`loom-daemon tokens bootstrap`),
/// a scratch volume that fills/frees, or a draining backlog are all honored
/// without a daemon restart. `configured_max` — the per-machine admission
/// knob (`LOOM_WORK_FINDER_MAX_CONCURRENT` /
/// `autonomous.workFinder.maxConcurrent`) — is the exception (#6203): it is
/// resolved once by the caller before this function is invoked and passed in
/// as a plain `usize`, so it does **not** itself hot-apply — an operator edit
/// takes effect only on the next daemon restart, unlike the two axes above.
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
        // Workspace-commands-missing transition state (#6440): log the loud
        // one-time WARN only on the false -> true edge (and its recovery),
        // mirroring `was_halted` — never every tick, which is the exact
        // per-tick-churn this issue is about.
        let mut was_commands_missing = false;
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
        // Disk-axis binding state (#7512): tracks whether the disk term was
        // the axis binding the cap DOWN as of the end of the previous tick, so
        // the eager reclaim trigger below fires on the false -> true EDGE only
        // (once per crossing), never on every tick a stubbornly-full disk
        // keeps it true — see `eager_reclaim::should_trigger`'s doc comment
        // for why that matters (the merged-PR worktree reap sub-pass has no
        // cooldown of its own).
        let mut was_disk_binding = false;
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
            // Host-level token-pool exhaustion hold (#7708) — the
            // single-workspace analogue of the per-root fold the production
            // multi-workspace loop below performs via
            // `pool_preflight::preflight_held_per_root`. Re-derived from the
            // live pool every tick, so it self-heals within one tick of a
            // readmission.
            let halted = health_state.is_halted()
                || (suppress_dispatch_during_gate && health_state.is_gate_in_flight())
                || crate::host_breaker::global_is_suppressed()
                || pool_preflight::observe_root(&workspace_root, chrono::Utc::now());
            // Recompute the dynamic cap from live inputs every tick (Phase B),
            // now with token-capacity backpressure (#3902): the token axis is the
            // count of *healthy* accounts from the ranking, not the flat pool.
            let pool_size = token_pool_size(&workspace_root);
            let ranking = capacity::read_ranking(&workspace_root);
            let token_limit = ranking.as_ref().map_or(pool_size, |r| r.available);
            log_healthy_token_transition(&mut was_healthy_tokens, token_limit, ranking.as_ref());
            // RAM headroom (#5270): the second "dumb mode" machine-headroom
            // axis alongside disk, folded into the same `min(...)`. Read
            // BEFORE disk since #7512 — the eager-reclaim trigger below needs
            // to know whether disk is the axis that would bind the cap down,
            // which is a comparison against this term and `configured_max`.
            let ram = crate::ram_headroom::ram_headroom_limit();
            let mut disk = disk_headroom_limit(&workspace_root);
            // Eager, out-of-cycle reclaim (#7512): on the tick the disk axis
            // FIRST becomes the term that binds the cap down, run the existing
            // reclaim passes for this workspace root right now rather than
            // waiting for the worktree reaper's own up-to-15-minute-away next
            // tick, then re-probe disk fresh (the loop's existing per-tick
            // measurement, not a new mechanism) before this tick's cap is
            // finalized — see `eager_reclaim::run_for`'s doc comment for the
            // full rationale and the cooldown-safety argument.
            if crate::eager_reclaim::should_trigger(was_disk_binding, disk, ram, configured_max) {
                let root_for_task = workspace_root.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    crate::eager_reclaim::run_for(&root_for_task)
                })
                .await;
                disk = disk_headroom_limit(&workspace_root);
            }
            // Re-armed from the POST-reclaim reading, so a pass that actually
            // freed space lets a genuine future crossing fire again.
            was_disk_binding =
                crate::eager_reclaim::disk_axis_binds_cap_down(disk, ram, configured_max);
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
                    // Workspace-commands-missing tripwire (#6440): a loud,
                    // ONE-TIME WARN on the transition into (and recovery
                    // from) the condition — never every tick, since the
                    // steady-state count is already folded into the
                    // per-tick summary counter below.
                    let commands_missing_now = report.skipped_workspace_commands_missing > 0;
                    if commands_missing_now && !was_commands_missing {
                        log::warn!(
                            "work_finder: workspace {} is missing \
                             .claude/commands/loom/sweep.md — every dispatch to it is refused \
                             (#4027 guard); {} ready issue(s) this tick were skipped in one \
                             batch rather than retried individually every tick (#6440). Run \
                             `loom-daemon init {}` there. `loom-daemon status` reports this \
                             repo's HEALTH-GATE as `no-sweep` until fixed.",
                            workspace_root.display(),
                            report.skipped_workspace_commands_missing,
                            workspace_root.display()
                        );
                    } else if !commands_missing_now && was_commands_missing {
                        log::info!(
                            "work_finder: workspace {} now has \
                             .claude/commands/loom/sweep.md — dispatch resuming (#6440)",
                            workspace_root.display()
                        );
                    }
                    was_commands_missing = commands_missing_now;
                    if report.dispatched > 0
                        || report.errors > 0
                        || report.skipped_quarantined > 0
                        || report.skipped_workspace_commands_missing > 0
                        || report.skipped_backoff > 0
                        || report.skipped_pr_open_backoff > 0
                        || report.skipped_noop_cooldown > 0
                        || report.skipped_declined > 0
                        || report.skipped_recheck_interval > 0
                        || report.skipped_host_constraint > 0
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
                             {} quarantine-skip, {} workspace-commands-missing-skip, \
                             {} backoff-skip, {} pr-open-backoff, {} noop-cooldown-skip, \
                             {} declined-skip, \
                             {} recheck-interval-skip, \
                             {} host-constraint-skip, \
                             {} pr-open-skip, \
                             {} peer-claim-skip, \
                             {} deferred (capacity), {} deferred (ramp), \
                             {} deferred (host saturated), {} error(s), \
                             {} cross-host-collision(s)",
                            report.seen,
                            report.dispatched,
                            report.skipped_labeled,
                            report.skipped_in_flight,
                            report.skipped_quarantined,
                            report.skipped_workspace_commands_missing,
                            report.skipped_backoff,
                            report.skipped_pr_open_backoff,
                            report.skipped_noop_cooldown,
                            report.skipped_declined,
                            report.skipped_recheck_interval,
                            report.skipped_host_constraint,
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
/// 1. Re-reads the machine-level
///    [`WorkspaceRegistry`](crate::workspace_registry::WorkspaceRegistry) and
///    resolves
///    [`effective_roots`](crate::workspace_registry::WorkspaceRegistry::effective_roots)
///    against
///    `fallback_root` — an **empty** registry yields `vec![fallback_root]`
///    (today's single-workspace behavior); a populated one yields the registered
///    roots. Re-reading each tick — and provisioning any new root — happens in
///    `registry_refresh::refresh_local_workspace_state`, called BEFORE the
///    rate-limit breaker
///    check (#8121), so `loom-daemon workspace add|remove|set-priority` is
///    hot-applied without a daemon restart even while gh polling is
///    suppressed.
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
    // Reconcile-before-dispatch (Issue #6615/#7974): flips to `true` once the
    // daemon's startup claim-reconciliation pass
    // (`crate::daemon_startup_reconciliation::spawn_startup_passes`) has
    // actually finished. That pass now runs on a blocking thread rather than
    // blocking `run_daemon` itself, so this is what still guarantees no sweep
    // is admitted before a startup reclaim of a stale `loom:building` claim
    // has landed.
    mut startup_reconciliation_ready: tokio::sync::watch::Receiver<bool>,
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
        // Block the loop's first REAL tick body until the startup
        // reconciliation pass has finished (#6615/#7974). Already-`true`
        // resolves instantly (the common case: the pass is almost always
        // faster than one tick interval), so this never adds latency once the
        // pass has completed — including on every tick after the first.
        let _ = startup_reconciliation_ready.wait_for(|ready| *ready).await;
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
        // Disk-axis binding state (#7512) — see the single-workspace loop
        // above for the full rationale (edge-triggers the eager reclaim pass
        // on the "disk starts binding the cap down" transition only).
        let mut was_disk_binding = false;
        // Missing-root hygiene (#4326): tracks which registered roots are
        // currently missing so `filter_missing_roots` logs a warning once per
        // transition rather than once per tick.
        let mut missing_roots_warned: HashSet<PathBuf> = HashSet::new();
        // Workspace-commands-missing tripwire (#6440): tracks which roots are
        // currently missing .claude/commands/loom/sweep.md so the loud WARN
        // below fires once per transition (into AND out of the condition),
        // never every tick — mirroring `missing_roots_warned`.
        let mut commands_missing_warned: HashSet<PathBuf> = HashSet::new();
        // Idle-edge role triggering (#4364): per-root idle level + per-(root,
        // role) debounce state. Fed one post-tick idle observation per root; on
        // the non-idle → idle edge it fire-and-forgets each configured on-idle
        // role. Boot state is "already idle" so an empty-queue startup never
        // fires.
        let mut idle_trigger = crate::role_runner::IdleTrigger::new();
        let mut was_rate_limited = false;
        loop {
            ticker.tick().await;

            // #8121: reload the registry and provision any newly-registered
            // root's `SweepRegistry` BEFORE the rate-limit breaker check
            // below — see `registry_refresh`'s module doc comment for
            // why this purely-local, no-GitHub-API work must never be gated
            // behind the breaker (`loom-daemon workspace add|remove
            // |set-priority` must hot-apply even while gh polling is
            // suppressed).
            let (registry, roots) = registry_refresh::refresh_local_workspace_state(
                &pool,
                &fallback_root,
                &mut missing_roots_warned,
            );

            // GitHub rate-limit circuit breaker (#4429): when the shared API
            // budget is exhausted every workspace's candidate list is a doomed
            // gh call, so skip the REST of the tick body — no listing, no
            // dispatch, no idle-edge role firing (a spawned role would hit the
            // same wall). The breaker lazily releases itself once the probed
            // reset epoch passes; the edge is logged once each way, never
            // per-tick. The registry reload + provisioning above already ran
            // unconditionally (#8121), so a workspace registered mid-
            // suppression is still hot-applied even though dispatch itself
            // stays paused.
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
            // RAM headroom (#5270): the second "dumb mode" machine-headroom
            // axis alongside disk, folded into the same `min(...)`. Read
            // BEFORE disk since #7512 — see the single-workspace loop above.
            let ram = crate::ram_headroom::ram_headroom_limit();
            let mut disk = disk_headroom_limit(&fallback_root);
            // Eager, out-of-cycle reclaim (#7512): on the tick the disk axis
            // FIRST becomes the term that binds the cap down, run the existing
            // reclaim passes for `fallback_root` — the same root this
            // machine-level disk term is probed against — right now rather
            // than waiting for the worktree reaper's own up-to-15-minute-away
            // next tick, then re-probe disk fresh (the loop's existing
            // per-tick measurement, not a new mechanism) before this tick's
            // cap is finalized. See `eager_reclaim::run_for`'s doc comment for
            // the full rationale and cooldown-safety argument, and the
            // single-workspace loop above for the identical wiring.
            if crate::eager_reclaim::should_trigger(was_disk_binding, disk, ram, configured_max) {
                let root_for_task = fallback_root.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    crate::eager_reclaim::run_for(&root_for_task)
                })
                .await;
                disk = disk_headroom_limit(&fallback_root);
            }
            // Re-armed from the POST-reclaim reading — see the
            // single-workspace loop above.
            was_disk_binding =
                crate::eager_reclaim::disk_axis_binds_cap_down(disk, ram, configured_max);
            // Refresh the memoized CPU idle sample — **observational only**
            // since #4512 (see the single-workspace loop above). It feeds the
            // `observed_idle=` figure in the axis line and `loom-daemon status`
            // / `calibrate`, which is how an operator tunes this machine's
            // `maxConcurrent`; it no longer gates admission.
            let _ = tokio::task::spawn_blocking(crate::cpu_headroom::refresh_cpu_util_cache).await;
            let idle = crate::cpu_headroom::cached_cpu_idle_fraction();
            let max_concurrent = resolve_dynamic_max_concurrent(disk, ram, configured_max);

            // `roots` was already resolved (registry reload + missing-root
            // filtering) by `refresh_local_workspace_state` above, before the
            // rate-limit breaker check (#8121).
            // Issue #7527: the single `token_limit` probe above reflects only
            // `fallback_root`'s resolved pool — this is the per-workspace
            // minimum across every root's OWN resolved pool, reported
            // alongside it in the tick-summary log so a shadowed repo-local
            // pool's exhaustion is visible even when the daemon's own probe
            // point looks healthy. Display-only; see
            // [`min_available_tokens_across_roots`]'s doc comment.
            let min_workspace_healthy =
                min_available_tokens_across_roots(&roots).unwrap_or(token_limit);

            // Per-repo priority tiers (#3946), parallel to `pairs`: lower = higher
            // priority. The empty-registry cwd fallback resolves to the default.
            let priorities: Vec<u32> = roots.iter().map(|r| registry.priority_of(r)).collect();

            // Dispatcher repo-sharding preferred slice (#6243): reuse the
            // SAME host ring #6374 already established for the role runner
            // (`role_shard`) rather than deriving a second, independent one.
            // A workspace's owner is `fnv1a64(shard_key) % shardCount ==
            // shardIndex` over its cross-host-stable key (repo NWO), so two
            // hosts' slices are disjoint BY ARITHMETIC — no roster, no
            // election, no convergence window. Every unconfigured /
            // malformed / single-shard case resolves to
            // `ShardPosture::Unsharded`, which owns EVERY workspace, so an
            // unsharded daemon's dispatch order is byte-for-byte pre-#6243.
            //
            // The `owned` verdict means something WEAKER here than it does
            // in the role runner: there it is a hard filter (unowned => never
            // ticks), here it is only a preference with a work-conserving
            // fallback (unowned => dispatched as soon as this host's own
            // slice is dry). Dispatch can therefore never be *dropped* by a
            // sharding misconfiguration, only reordered. See
            // `defaults/docs/dispatcher-repo-sharding.md`.
            //
            // Roster mode (#7691) keeps that contract intact: when the
            // roster's admission fence YIELDS, `ShardDecision::owned` still
            // carries the pre-roster (#6374) verdict — only the role runner's
            // `admits_role_tick()` flips. A fence yield therefore never
            // reaches this line as "owns nothing"; it degrades to exactly the
            // work-conserving preference this host had before the roster
            // existed. (When the fence *admits*, `owned` is the roster ring's
            // own verdict, which is the intended, in-scope change: a
            // preference reshuffle, never a dropped dispatch.)
            //
            // Deliberately NOT calling `role_shard::log_decision_once` here:
            // it renders a `role_runner:`-prefixed line and dedups through a
            // process-global per-root map, so calling it from this loop would
            // both mislabel the line and suppress the role runner's own
            // edge-triggered one. The aggregate lands in the axis line below
            // instead.
            let preferred_slice: Vec<bool> = roots
                .iter()
                .map(|root| crate::role_shard::decide(root).owned)
                .collect();

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
            //
            // #7708: the same slice now also carries the host-level token-pool
            // exhaustion hold — a pool with accounts but zero SPAWNABLE ones
            // (every account bad-marked or `.ranking`-hard-excluded) is
            // guaranteed to kill every sweep dispatched into it at token
            // selection, ~30s in, after the claim label flip and the lease
            // comment have already landed on a real issue. See
            // `pool_preflight::preflight_held_per_root` for why the two holds
            // are computed together and why a pool hold outranks a #5030
            // recovery probe.
            let now_tick = chrono::Utc::now();
            let (preflight_held, preflight_probe_roots) =
                pool_preflight::preflight_held_per_root(&pool, &roots, now_tick);
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
            // #7708 folded a second cause into this same slice, so this edge
            // line names both and defers the specifics to whichever hold
            // logged its own edge line (`pool_preflight` logs the pool one).
            if preflight_held_count != was_preflight_held_count {
                if preflight_held_count > 0 {
                    log::warn!(
                        "work_finder: {preflight_held_count} of {} repo(s) held — \
                         claude-wrapper pre-flight advisory tripped (broken .mcp.json, #5030) or \
                         the resolved token pool has zero spawnable accounts (#7708); dispatch is \
                         suppressed (an advisory hold still allows one probe per cooldown, a pool \
                         hold allows none)",
                        roots.len()
                    );
                } else {
                    log::info!(
                        "work_finder: pre-flight + token-pool holds cleared for all repos — \
                         dispatch resuming (#5030/#7708)"
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

            // Workspace-commands-missing tripwire (#6440): a loud, ONE-TIME
            // WARN per root on the transition into (and recovery from) the
            // condition — never every tick. `tick_multi_with_saturation_brake`
            // below independently re-checks this per root to decide whether
            // to skip its candidates; this is purely the log-once concern,
            // mirroring `filter_missing_roots`' `missing_roots_warned` dedup.
            for (root, (_src, dispatcher)) in roots.iter().zip(pairs.iter()) {
                let missing = dispatcher.workspace_commands_missing();
                if missing && commands_missing_warned.insert(root.clone()) {
                    log::warn!(
                        "work_finder: workspace {} is missing \
                         .claude/commands/loom/sweep.md — every dispatch to it is refused \
                         (#4027 guard). Its ready issues will be skipped in one batch per tick \
                         rather than retried individually (#6440). Run `loom-daemon init {}` \
                         there. `loom-daemon status` reports this repo's HEALTH-GATE as `no-sweep` \
                         until fixed.",
                        root.display(),
                        root.display()
                    );
                } else if !missing && commands_missing_warned.remove(root) {
                    log::info!(
                        "work_finder: workspace {} now has .claude/commands/loom/sweep.md — \
                         dispatch resuming (#6440)",
                        root.display()
                    );
                }
            }

            let in_slice_count = preferred_slice.iter().filter(|&&b| b).count();
            let axis_line = format!(
                "work_finder: dynamic cap = {max_concurrent} (pool={pool_size}, \
                 healthy_tokens={token_limit} [informational only, not capacity-limiting \
                 since #5270], disk={disk}, ram={ram}, configured_max={configured_max}, \
                 max_admissions_per_tick={max_admissions_per_tick}, any_halted={any_halted}, \
                 preflight_held={preflight_held_count}, \
                 saturation_held={saturation_held}, \
                 observed_idle={}, workspaces={}, priorities={priorities:?}, \
                 shard_slice={in_slice_count}/{} preferred (#6243/#6374))",
                format_idle(idle),
                pairs.len(),
                preferred_slice.len()
            );
            if was_max_concurrent != Some(max_concurrent) {
                log::info!("{axis_line}");
                was_max_concurrent = Some(max_concurrent);
            } else {
                log::debug!("{axis_line}");
            }

            let report = tick_multi_with_sharding(
                &mut pairs,
                &priorities,
                max_concurrent,
                &halted,
                max_admissions_per_tick,
                saturation_held,
                Some(&preferred_slice),
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
                || report.skipped_workspace_commands_missing > 0
                || report.skipped_backoff > 0
                || report.skipped_pr_open_backoff > 0
                || report.skipped_noop_cooldown > 0
                || report.skipped_declined > 0
                || report.skipped_recheck_interval > 0
                || report.skipped_host_constraint > 0
                || report.skipped_pr_open > 0
                || report.skipped_peer_claim > 0
                || report.deferred_ramp_cap > 0
                || report.deferred_saturation > 0
                || report.deferred_out_of_slice > 0
            {
                log::info!(
                    "work_finder: tick — cap {max_concurrent} (pool={pool_size}, \
                     healthy={token_limit} [fallback_root probe only], \
                     min_workspace_healthy={min_workspace_healthy} [minimum across every \
                     registered workspace's own resolved pool, #7527], disk={disk}, \
                     ram={ram}, ceiling={configured_max}, ramp_cap={max_admissions_per_tick}); \
                     {} workspace(s), \
                     {} seen, {} dispatched, {} labeled-skip, {} in-flight-skip, \
                     {} quarantine-skip, {} workspace-commands-missing-skip, \
                     {} backoff-skip, {} pr-open-backoff, {} noop-cooldown-skip, \
                     {} declined-skip, \
                     {} recheck-interval-skip, \
                     {} host-constraint-skip, \
                     {} pr-open-skip, \
                     {} peer-claim-skip, \
                     {} deferred (capacity), {} deferred (ramp), \
                     {} deferred (host saturated), {} deferred (out-of-slice, #6243), \
                     {} error(s), \
                     {} cross-host-collision(s)",
                    pairs.len(),
                    report.seen,
                    report.dispatched,
                    report.skipped_labeled,
                    report.skipped_in_flight,
                    report.skipped_quarantined,
                    report.skipped_workspace_commands_missing,
                    report.skipped_backoff,
                    report.skipped_pr_open_backoff,
                    report.skipped_noop_cooldown,
                    report.skipped_declined,
                    report.skipped_recheck_interval,
                    report.skipped_host_constraint,
                    report.skipped_pr_open,
                    report.skipped_peer_claim,
                    report.deferred_capacity,
                    report.deferred_ramp_cap,
                    report.deferred_saturation,
                    report.deferred_out_of_slice,
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
/// Minimum "available" (healthy) token count across every registered
/// workspace's **own** resolved pool (issue #7527) — as opposed to the
/// `token_limit` computed once per multi-workspace tick from a single probe
/// of `fallback_root`'s resolved pool ([`spawn_multi_work_finder_task`]'s
/// doc comment), which is then reused unchanged as the `healthy=` figure a
/// tick's log line reports for every workspace it dispatches to.
///
/// The two can diverge: a registered workspace with its own repo-local
/// `.loom/tokens/` pool (#3938 per-repo-then-shared precedence) can be fully
/// exhausted while `fallback_root`'s resolved pool (frequently the shared
/// machine-level pool, since the daemon's own working directory rarely
/// carries a repo-local pool) is healthy. That is exactly the incident
/// behind #7527: an operator readmitted the shared pool, the tick log's
/// `healthy=` figure looked fine, and sweeps in the shadowed repo-local
/// pools kept insta-crashing on account exhaustion regardless — nothing in
/// the log named the divergence.
///
/// This is a **display-only** figure. It is never wired into
/// [`resolve_dynamic_max_concurrent`] or [`capacity::assess_pressure`] —
/// both keep consuming the original single-probe `token_limit`/`ranking`
/// unchanged — so this does not reintroduce the token-count capacity gating
/// #5270 removed; it only makes the log line honest about what `healthy=`
/// does and does not cover.
///
/// Returns `None` when `roots` is empty (nothing to take a minimum over), so
/// the caller can fall back to `token_limit` itself rather than a synthetic
/// zero.
fn min_available_tokens_across_roots(roots: &[PathBuf]) -> Option<usize> {
    roots
        .iter()
        .map(|root| {
            let dir = crate::tokens_pool::paths::resolve_tokens_dir(root);
            let pool_size = token_pool_size_at_dir(&dir);
            capacity::read_ranking_at(&dir).map_or(pool_size, |r| r.available)
        })
        .min()
}

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
    use super::{
        read_work_finder_config, resolve_extra_skip_labels_with_config, WorkDispatcher, WorkItem,
        WorkSource,
    };
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
                    WorkItem::with_created_at(r.number, r.labels, r.created_at)
                        .with_body(r.body)
                        .with_updated_at(r.updated_at)
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

        /// The shared registry behind this dispatcher. Test-only seam so a
        /// restart-survivorship test (#6262) can seed the registry the way the
        /// daemon's own startup pass does — through the real registry, not by
        /// injecting an in-flight set the way the `RecordingDispatcher` fake
        /// allows.
        #[cfg(test)]
        #[must_use]
        pub fn registry_for_test(&self) -> Arc<Mutex<SweepRegistry>> {
            self.registry.clone()
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

        /// The subset of `backed_off()` whose window was armed by the
        /// open-PR guard rather than a real dispatch failure (Issue #7606).
        /// Pure in-memory read, mirroring `backed_off()`.
        fn pr_open_backed_off(&self) -> HashSet<u32> {
            match self.registry.lock() {
                Ok(reg) => reg.open_pr_backoff_issues(chrono::Utc::now()),
                Err(poisoned) => {
                    log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                    HashSet::new()
                }
            }
        }

        /// Issues inside a live no-op re-dispatch cooldown window (Issue
        /// #6670). Pure in-memory read of the registry state a
        /// `RecordNoopRelease` call maintains — no forge round trip,
        /// mirroring `backed_off()`.
        fn noop_cooldown(&self) -> HashSet<u32> {
            match self.registry.lock() {
                Ok(reg) => reg.noop_cooldown_issues(chrono::Utc::now()),
                Err(poisoned) => {
                    log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                    HashSet::new()
                }
            }
        }

        /// Issues inside a live hard-exclusion decline cooldown window (Issue
        /// #7528). Pure in-memory read of the registry state the reaper's
        /// checkpoint-less clean-exit path maintains — no forge round trip,
        /// mirroring `noop_cooldown()`.
        fn declined(&self) -> HashSet<u32> {
            match self.registry.lock() {
                Ok(reg) => reg.decline_cooldown_issues(chrono::Utc::now()),
                Err(poisoned) => {
                    log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                    HashSet::new()
                }
            }
        }

        /// Whether this workspace is missing `.claude/commands/loom/sweep.md`
        /// (Issue #4027 guard 2.4, quarantined at the work-finder level by
        /// #6440). A cheap `stat` via `SweepRegistryConfig::has_sweep_command`
        /// — no forge round trip, no lock contention beyond the same mutex
        /// every other dispatcher method already takes.
        ///
        /// Mirrors `dispatch_inner`'s own `!self.config.skip_label_flip &&
        /// !self.config.has_sweep_command()` gate exactly: `skip_label_flip`
        /// marks a hermetic unit-test fixture that never installs
        /// `.claude/commands/loom/` on disk, so without this term every such
        /// fixture would read as workspace-commands-missing and this
        /// pre-filter would silently zero out their candidate batches —
        /// unlike the guard in `dispatch()` itself, which they never actually
        /// reach (label flips, and thus this guard, are the thing they're
        /// opting out of).
        fn workspace_commands_missing(&self) -> bool {
            match self.registry.lock() {
                Ok(reg) => !reg.config().skip_label_flip && !reg.config().has_sweep_command(),
                Err(poisoned) => {
                    log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                    false
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

        /// Additional skip-label list for this workspace (Issue #6685),
        /// resolved fresh each call from `<workspace_root>/.loom/config.json`
        /// via [`read_work_finder_config`] / [`resolve_extra_skip_labels_with_config`]
        /// — a cheap JSON read (mirrors `workspace_commands_missing()`'s own
        /// per-call `stat`), so an operator's `autonomous.workFinder.extraSkipLabels`
        /// edit takes effect on the very next tick with no registry-side
        /// config plumbing or daemon restart required.
        fn extra_skip_labels(&self) -> Vec<String> {
            match self.registry.lock() {
                Ok(reg) => resolve_extra_skip_labels_with_config(&read_work_finder_config(
                    &reg.config().workspace_root,
                )),
                Err(poisoned) => {
                    log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                    Vec::new()
                }
            }
        }

        /// Capabilities this **host** declares it holds (#6893), read fresh each
        /// call from `LOOM_WORKER_CAPABILITIES`.
        ///
        /// Deliberately NOT resolved from `.loom/config.json` the way
        /// [`extra_skip_labels`](Self::extra_skip_labels) above is: a skip-label
        /// list is a repo policy, but "this machine has root / an admin token /
        /// a production cloud profile" is a property of the host and its
        /// credentials, and a file committed to git must not be able to assert
        /// it. See [`crate::capability`].
        fn declared_capabilities(&self) -> std::collections::BTreeSet<String> {
            crate::capability::held_capabilities()
        }

        fn dispatch(&mut self, issue: u32, complexity: Option<&str>) -> Result<bool> {
            // Issue #6688: only the `repo_root` read needs the lock — grab it
            // and release immediately, rather than holding the registry mutex
            // across the whole call the way the pre-#6688 single `reg.dispatch(..)`
            // call below used to (via `SweepRegistry::dispatch` ->
            // `dispatch_inner`, which holds the lock across the up-to-5s
            // account-selection poll; see `dispatch_issue_releasing_poll_lock`'s
            // doc comment for the full hazard this avoids).
            let repo_root = {
                let reg = self
                    .registry
                    .lock()
                    .map_err(|e| anyhow!("sweep registry mutex poisoned: {e}"))?;
                reg.config().workspace_root.clone()
            };
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
            let resolved = crate::sweep_registry::resolve_autonomous_dispatch_model(
                &repo_root, issue, complexity,
            );
            // Issue #7482: this line is logged BEFORE `dispatch_issue_releasing_poll_lock`
            // below runs the actual pre-spawn guards (open-PR #4123, park-label
            // #4444, lease-order #6287, etc. — see `dispatch_inner`), any of
            // which can still refuse the dispatch. So this must not claim a
            // dispatch happened yet — it only names the *attempt*. The
            // corresponding past-tense "dispatched issue #N" line is logged by
            // each call site only once `dispatch()` returns `Ok(true)` (a
            // confirmed new spawn), never here.
            match resolved.arm {
                Some(arm) => log::info!(
                    "work_finder: attempting issue #{issue} with arm={arm} \
                     (complexity={}) model={} (source={})",
                    complexity.unwrap_or("routine"),
                    resolved.model,
                    resolved.source_label
                ),
                None => log::info!(
                    "work_finder: attempting issue #{issue} with model={} (source={})",
                    resolved.model,
                    resolved.source_label
                ),
            }
            let model = resolved.model;
            // Idempotency key + the registry's claim lock make a re-dispatch of
            // an already-running issue a no-op (`was_new = false`) or a loud
            // lock-collision error.
            let key = format!("workfinder-{issue}");
            // Issue #6688: extends the #6592 begin/poll/finish split (proven
            // for the IPC `DispatchSweep` handler) to this call site, so the
            // account-selection poll no longer holds the registry mutex a
            // concurrent `DaemonStatus`/`health` IPC call's per-root
            // `registry.lock()` (`ipc.rs::build_daemon_status`) needs.
            let outcome = crate::sweep_registry::dispatch_issue_releasing_poll_lock(
                &self.registry,
                &SweepKind::Issue(issue),
                Some(key),
                Some(&model),
                None,
                None,
            )?;
            Ok(outcome.was_new)
        }
    }
}

// Re-export the concrete adapters at the module root for ergonomic wiring.
pub use forge::{GhWorkSource, RegistryDispatcher};

pub mod pool_preflight;

// Purely-local registry reload + workspace provisioning for the tick loop
// (#8121). Lives in its own file because this one is over the file-size
// ratchet threshold (.loom/docs/file-size-policy.md) and may not grow.
mod registry_refresh;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
