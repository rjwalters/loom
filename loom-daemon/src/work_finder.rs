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
//!    (`loom:building` / `loom:blocked` / `loom:operator`).
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
//! ([`crate::disk_admission::tick`] since #11191: free space net of the
//! in-flight sweeps' expected growth, at each repo's measured charge; the flat
//! [`crate::disk_headroom::disk_headroom_limit`] when disabled) and the host's
//! available-RAM headroom (#5270, [`crate::ram_headroom::ram_headroom_limit`])
//! — bounded by the per-machine operator ceiling
//! (`LOOM_WORK_FINDER_MAX_CONCURRENT` / `autonomous.workFinder.maxConcurrent`).
//! Since #9060 that ceiling is re-read every multi-workspace tick too
//! ([`configured_max::ConfiguredMaxReloader`]), so an edit to
//! `autonomous.workFinder.maxConcurrent` hot-applies on the next tick. The
//! `LOOM_WORK_FINDER_MAX_CONCURRENT` env override is the exception: it is the
//! daemon's own process environment, fixed at launch, so while it is set it
//! keeps shadowing config until a restart (#6203 documented the old
//! startup-only behavior). The effective per-tick concurrency is then
//! `min(dynamic_cap, backlog_depth)`: [`tick`] iterates the ready
//! `loom:issue` rows and stops at the cap, so concurrency scales **up** as the
//! backlog grows and drains to **zero** dispatches when the queue is empty —
//! all without a daemon restart, since every term is read fresh each tick
//! (only the env override of the ceiling is launch-fixed, above).
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

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use crate::capacity::{self, CapacityAdvisory};
use crate::event_bus::EventBus;
use crate::main_health_gate::WorkspaceHealthStates;
use crate::tokens::token_pool_size_at_dir;
use crate::types::Event;
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

/// The default host budget (#11911, 2am#4463): one number for every agent the
/// daemon starts, steps and role-runner runs together. The fleet store sets it
/// per host.
pub const DEFAULT_WORK_FINDER_MAX_CONCURRENT: usize = 20;

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
/// (`autonomous.workFinder.maxAdmissionsPerTick`) > default**. Since #12090 the
/// config layer is re-read every multi-workspace tick
/// ([`ConfiguredMaxReloader::max_admissions_per_tick`]), so a fleet-store edit
/// hot-applies; this env override, being process environment, is read only at
/// daemon start. A zero/unparseable value is
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

pub mod affected_files;
pub mod build_backoff;
pub mod dispatch_plan;
pub mod dispatch_plan_merge;
pub mod halt_cause;
mod labels;
pub mod landed_prefilter;
pub mod main_red_autofile;
pub mod main_red_fix;
pub mod open_pr_prefilter;
pub mod operator_priority;
mod ordering;
pub mod ready_queue;
mod recheck_interval;
pub mod startup_gate;
pub mod step_item;
mod tick_line;
mod tick_report;
mod tick_summary;
mod tracking_parent;
pub mod vi_cap;
pub mod workspace_draw;
pub use labels::*;
pub use main_red_fix::RedMainLane;
pub use operator_priority::{CapTerms, OverflowSlot};
pub use ordering::{candidate_cmp, PriorityCandidate};
pub use tick_report::{error_kind::DispatchErrorKind, Admission, TickReport};
#[cfg(test)]
use tick_summary::reset_last_tick_summary;
pub use tick_summary::{
    last_tick_summary, publish_tick, publish_tick_summary, publish_tick_summary_at,
    publish_tick_summary_with_roots_at, tick_summary,
};

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
    /// the self-declared re-check interval marker is read out of it (see
    /// [`Self::recheck_interval`]). `None` (a synthetic item, or a listing
    /// without bodies) simply carries no marker.
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
    /// Who filed the issue, when the listing supplied it (#10118). Only a
    /// trusted author's red-main-fix marker admits an unpromoted row (#9548);
    /// `None` (a synthetic item) is never trusted. See [`main_red_fix`].
    pub author: Option<crate::comment_trust::Author>,
    /// Open native sub-issues (#11483); see [`tracking_parent`].
    pub open_sub_issues: Option<u32>,
}

impl WorkItem {
    /// Convenience constructor (no creation timestamp — the item sorts by number
    /// as its age proxy).
    #[must_use]
    pub fn new(number: u32, labels: Vec<String>) -> Self {
        Self::with_created_at(number, labels, None)
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
            author: None,
            open_sub_issues: None,
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

    /// How the capability lane (#6893) routes this item, given the capabilities
    /// this worker declares it holds.
    ///
    /// A thin adapter over [`crate::capability::route_mechanical`] — the labels
    /// and body already fetched by the listing, no extra forge read. Returns
    /// [`MechanicalRouting::NotApplicable`](crate::capability::MechanicalRouting::NotApplicable)
    /// for every item that is not a capability-only `loom:blocked` park (#11861),
    /// which is all but a handful.
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

    /// True when the issue is skipped, with the `loom:blocked` park made
    /// **capability-aware for capability-only parks** (#6893, AC1; #11795;
    /// re-keyed from `loom:operator` by #11861).
    ///
    /// Identical to [`Self::is_skipped_with_extra`] except for one narrow
    /// exemption: a blocked hold whose body declares capabilities (the
    /// `<!-- loom:capability=<name> -->` markers from #6892) that
    /// `held_capabilities` fully covers, names no blocker in a park record and
    /// carries no decision block, is **not** skipped — it is dispatchable into
    /// the propose-mode lane:
    ///
    /// - A decision (a body decision block) or a named blocker stays
    ///   hard-skipped unconditionally, marker or no marker.
    /// - `loom:operator` (under any name: no role acts on it), `loom:frozen`,
    ///   [`BUILDING_LABEL`] and any `extra_skip_labels` entry veto the
    ///   exemption outright — it only ever relaxes the blocked hold, never any
    ///   other reason to skip.
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
        // The exemption may only cancel the blocked hold (any of its names).
        // If any OTHER skip reason applies, the item stays skipped.
        let other_skip_reason = self.labels.iter().any(|l| {
            (SKIP_LABELS.contains(&l.as_str()) && !labels::BLOCKED_HOLD_NAMES.contains(&l.as_str()))
                || extra_skip_labels.iter().any(|e| e == l)
        });
        if other_skip_reason {
            return true;
        }
        !self.mechanical_routing(held_capabilities).is_dispatchable()
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

    /// Whether the last [`Self::list_ready_issues`] returned this repo's
    /// whole queue (#11139). `false` when it returned only the rows it could
    /// read (a later page failed, the page cap, a mid-walk change): the tick
    /// records the repo in [`TickReport::listing_incomplete`]. A source that
    /// cannot fall short keeps the default.
    fn listing_complete(&self) -> bool {
        true
    }

    /// Every open `loom:very-important` issue of this repo, in any state
    /// (`loom:issue`, `loom:building`, in curation, ...), for the flood cap
    /// ([`vi_cap`], #11509), so an issue being built keeps the workspace's
    /// very-important slot. `None` when unknown (a failed read, or a source
    /// without the listing): the cap then counts the ready listing alone.
    /// Called only on a tick whose ready listing holds a very-important issue.
    fn list_open_very_important(&mut self) -> Option<Vec<WorkItem>> {
        None
    }
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

    /// The absolute expiry of each entry in [`backed_off`](Self::backed_off)
    /// (Issue #9311), when known — same membership, keyed the same way, a
    /// sibling snapshot rather than a replacement so `backed_off`'s existing
    /// callers and fakes are unaffected. Populates `dispatch_plan`'s
    /// `held_until` field; the work finder's own skip decision still reads
    /// `backed_off` alone.
    ///
    /// Defaults to empty so a dispatcher that does not model the backoff
    /// (e.g. a test fake) opts out with zero boilerplate.
    fn dispatch_backoff_expiry(&self) -> HashMap<u32, chrono::DateTime<chrono::Utc>> {
        HashMap::new()
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

    /// The absolute expiry of each entry in [`noop_cooldown`](Self::noop_cooldown)
    /// (Issue #9311), when known — a sibling snapshot, mirroring
    /// [`dispatch_backoff_expiry`](Self::dispatch_backoff_expiry). Defaults to
    /// empty so a dispatcher that does not model the cooldown (e.g. a test
    /// fake) opts out with zero boilerplate.
    fn noop_cooldown_expiry(&self) -> HashMap<u32, chrono::DateTime<chrono::Utc>> {
        HashMap::new()
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

    /// The absolute expiry of each entry in [`declined`](Self::declined)
    /// (Issue #9311), when known — a sibling snapshot, mirroring
    /// [`dispatch_backoff_expiry`](Self::dispatch_backoff_expiry). Defaults to
    /// empty so a dispatcher that does not model the cooldown (e.g. a test
    /// fake) opts out with zero boilerplate.
    fn declined_expiry(&self) -> HashMap<u32, chrono::DateTime<chrono::Utc>> {
        HashMap::new()
    }

    /// The set of issue numbers currently inside a **PR-less retry window**
    /// (Issue #7972): a previous dispatch claimed the issue, released it, and
    /// left no pull request behind, and the window the reaper armed has not yet
    /// elapsed. Skipped exactly like [`quarantined`](Self::quarantined) /
    /// [`backed_off`](Self::backed_off) / [`noop_cooldown`](Self::noop_cooldown)
    /// — filtered out *before* the concurrency budget is filled.
    ///
    /// A fourth, distinct bound, keyed on the one signal the observed #7893
    /// loop could not fake: **did the dispatch produce a PR?** That loop
    /// advanced its checkpoint on every attempt, which reset the quarantine
    /// tally and cleared the dispatch backoff, so it re-claimed the same issue
    /// fourteen times in four hours — several times inside the same minute —
    /// without ever tripping `quarantined`, `backed_off` or `noop_cooldown`.
    ///
    /// Defaults to empty so a dispatcher that does not model the bound (e.g. a
    /// test fake) opts out with zero boilerplate.
    fn prless_retry(&self) -> HashSet<u32> {
        HashSet::new()
    }

    /// The absolute expiry of each entry in [`prless_retry`](Self::prless_retry)
    /// (Issue #9311), when known — a sibling snapshot, mirroring
    /// [`dispatch_backoff_expiry`](Self::dispatch_backoff_expiry). Defaults to
    /// empty so a dispatcher that does not model the bound (e.g. a test fake)
    /// opts out with zero boilerplate.
    fn prless_retry_expiry(&self) -> HashMap<u32, chrono::DateTime<chrono::Utc>> {
        HashMap::new()
    }

    /// Whether this dispatcher's workspace is missing
    /// `.claude/commands/loom/sweep.md` — the **structural, workspace-level**
    /// refusal the registry's step-2.4 guard (#4027) enforces via the typed
    /// [`WorkspaceCommandsMissingDispatchError`](crate::sweep_registry::WorkspaceCommandsMissingDispatchError). Unlike
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
    /// decide whether a mechanical operator hold's declared
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
    /// `fleet.hostId` / `$LOOM_HOST_ID` / persisted-id resolution every
    /// other host-identity consumer in the daemon (peer-claim
    /// self-recognition, observability) already uses, so this feature can
    /// never disagree with them about what "this host" means. A test fake
    /// overrides this to a fixed string rather than mutating process-global
    /// env vars.
    fn current_host_id(&self) -> String {
        crate::sweep_registry::host_identity()
    }

    /// The batched open-PR pre-filter (#8865, [`open_pr_prefilter`]): the
    /// issues in `candidates` with a VERIFIED open linked PR, each mapped to
    /// that PR. Asked at most once per workspace per tick; the multi-workspace
    /// tick skips those candidates without calling
    /// [`dispatch_with`](Self::dispatch_with). It can only remove candidates:
    /// anything not verified (unreadable listing included) must be left out,
    /// and still meets `dispatch()`'s own open-PR guard. Defaults to empty.
    fn open_pr_blocked(&self, _candidates: &[u32]) -> HashMap<u32, u32> {
        HashMap::new()
    }

    /// The landed pre-filter (#11853, [`landed_prefilter`]): the issues in
    /// `candidates` that are still open although a PR closing them merged,
    /// each mapped to that PR. `open_pr` are the workspace's candidates the
    /// open-PR pre-filter blocked this tick. Asked at most once per workspace
    /// per tick, after [`open_pr_blocked`](Self::open_pr_blocked); like it,
    /// it can only remove candidates. Defaults to empty.
    fn landed_closing_prs(&self, _candidates: &[u32], _open_pr: &[u32]) -> HashMap<u32, u32> {
        HashMap::new()
    }

    /// The landed-cooldown lift (#11901, [`landed_prefilter::lift_in`]): of
    /// `held`, this workspace's listed candidates inside a dispatch-backoff
    /// window, the ones whose landed cooldown was just lifted because the
    /// issue was reopened after its landing or Curator kept it. The tick then
    /// treats them as not backed off. Defaults to empty.
    fn lift_landed_cooldowns(&self, _held: &[u32]) -> HashSet<u32> {
        HashSet::new()
    }

    /// Whether a live sweep on this host is already marked `overflow`
    /// (#9244): the [`OverflowSlot`] is then taken for the whole tick.
    /// Defaults to `false`.
    fn overflow_in_flight(&self) -> bool {
        false
    }

    /// Whether this workspace's `main` is red by its latest CI conclusion —
    /// the red-main-fix lane's fallback when the main-health gate is disabled
    /// for the repo (#9244). Consulted only when the repo has a
    /// marker-bearing candidate. Defaults to `false` (no boost).
    fn main_red_via_ci(&self) -> bool {
        false
    }

    /// The red-main-fix escalation (#10118): `waiting` is this repo's
    /// marker-bearing candidates on a red `main` (empty when `red` is false).
    /// The production dispatcher alerts the operator once per fix left
    /// unclaimed past the threshold. Defaults to a no-op.
    fn escalate_red_fix(&mut self, _red: bool, _waiting: &[u32]) {}

    /// File, reuse or close this repo's auto-filed red-main fix issue
    /// (#11766, [`main_red_autofile`]). Defaults to a no-op.
    fn autofile_red_main(&mut self, _lane: RedMainLane) {}

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
/// workspace `i`'s draw weight (#11742; higher = drawn more, 0 = never; a
/// missing entry defaults to [`crate::workspace_registry::DEFAULT_WORKSPACE_WEIGHT`]).
/// Rather than dispatching each workspace's backlog in registration order, this
/// gathers every eligible candidate across all workspaces and orders them by
/// **weighted workspace draws** ([`workspace_draw`], #11103): each draw picks a
/// workspace at random weighted by its priority (a workspace with
/// `loom:very-important` work next wins), then takes that workspace's next
/// candidate by level, oldest `createdAt`, number. The single shared
/// concurrency budget is filled in that order, so a higher-priority tool repo
/// is usually served first without a permanently-full tier starving the rest.
/// The cap/budget mechanics are unchanged — this only orders the queue.
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
//
// WorkFinderConfig / read_work_finder_config / the deprecated-CPU-knob notice
// live in their own file, `config.rs` (Issue #9034) — the same file-size-
// ratchet reason as `repo_cap` / `configured_max` below (`work_finder.rs` is
// frozen at its current size; see `.loom/docs/file-size-policy.md`). Pure
// move, re-exported here so every existing `WorkFinderConfig` /
// `read_work_finder_config` reference in this file and its siblings is
// unchanged.
pub mod config;
pub use config::{
    deprecated_cpu_knob_notice, read_work_finder_config, warn_deprecated_cpu_knobs,
    WorkFinderConfig, DEPRECATED_CPU_CONFIG_KEYS, DEPRECATED_CPU_ENV_VARS,
};

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
/// (`autonomous.workFinder.maxAdmissionsPerTick`) > default** (#4234). The
/// production multi-workspace loop re-reads it every tick since #12090 (via
/// [`configured_max::resolve_admissions_with_source`]); the single-workspace
/// reference loop still takes it as a fixed `usize`.
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
///
/// # One configured number since #11911
///
/// Operator ruling (2am#4463): the host budget is **one number** set per host
/// in the fleet store (`autonomous.workFinder.maxConcurrent`, default
/// [`DEFAULT_WORK_FINDER_MAX_CONCURRENT`] = 20), not a slot calculation. The
/// disk and RAM headroom no longer size it. What stays is the halt: a host
/// whose scratch volume or memory is exhausted (headroom `0`) admits nothing.
/// The per-repo disk reservation and RAM caps ([`RepoCap`]), the disk-full
/// admission check (`disk_admission`), the saturation brake and the
/// host-distress breaker are unchanged.
#[must_use]
pub fn resolve_dynamic_max_concurrent(
    disk_headroom: usize,
    ram_headroom: usize,
    configured_max: usize,
) -> usize {
    if disk_headroom == 0 || ram_headroom == 0 {
        0
    } else {
        configured_max
    }
}

// ============================================================================
// Runtime wiring — the loop runs on the shared daemon runtime
// ============================================================================

/// Spawn the **multi-workspace** work-finder loop (#3928) on the shared daemon
/// runtime.
///
/// Every tick it:
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
///    check (#8121), so `loom-daemon workspace add|remove|set-weight` is
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
    configured_max: ConfiguredMax,
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
    // has landed. Since #12222 the wait is bounded: see `startup_gate`.
    mut startup_reconciliation_ready: tokio::sync::watch::Receiver<
        crate::daemon_startup_reconciliation::StartupPasses,
    >,
) -> tokio::task::JoinHandle<()> {
    configured_max::log_loop_start(interval, configured_max);
    // #12222: about daemon start (spawned during `run_daemon` startup).
    let spawned_at = std::time::Instant::now();
    tokio::spawn(async move {
        // #12073: a per-host delay before the first gather, so a fleet that
        // restarts together does not gather all at once. First run only.
        crate::startup_jitter::wait_first_run(&fallback_root, "work_finder").await;
        // #9060: the operator ceiling is re-resolved every tick (see the
        // `configured_max` module) — no longer a frozen startup value.
        let mut configured_max_reload =
            ConfiguredMaxReloader::new(fallback_root.clone(), configured_max);
        // An `Interval` that a `forge.event` prompt may also tick early
        // (#8766). Disarmed unless `forgeEvents.events.workFinderTick` is on
        // for `fallback_root` — the daemon's primary workspace, which is also
        // where every other machine-level knob is resolved from. When
        // disarmed this is exactly `tokio::time::interval(interval)`: no bus
        // subscription, no bridge task, no wake path.
        let mut ticker = crate::forge_events::wake::EarlyTicker::for_work_finder(
            interval,
            &event_bus,
            &fallback_root,
        );
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // First tick fires immediately; skip it so we don't churn at boot.
        ticker.tick().await;
        // Block the loop's first REAL tick body until the startup passes
        // have finished (#6615/#7974) — but the forge-bound claim pass only
        // up to a cap (#12222): it took 20–40+ min on a host with hundreds of
        // open-PR claims. Ticking past the cap is safe; see `startup_gate`.
        // Already-finished resolves instantly, and only the first tick waits.
        let startup_outcome = startup_gate::wait(
            &mut startup_reconciliation_ready,
            startup_gate::wait_cap(&fallback_root),
        )
        .await;
        let mut first_tick = startup_gate::FirstTick::new(spawned_at, startup_outcome);
        let mut was_halted = false;
        let mut was_pressured = false;
        // #9410/#10624: the build back-off's per-repo (and optional host
        // ceiling) hysteresis states, held across ticks.
        let mut build_backoff = build_backoff::BuildBackoffs::default();
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
        // Eager reclaim trigger state (#7512, #11192) — see the
        // single-workspace loop above.
        let mut eager_trigger = crate::eager_reclaim::EagerTrigger::default();
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
        let mut step_driver = crate::step_dispatch::driver::StepDriver::default();
        loop {
            ticker.tick().await;
            // #12074: one INFO line per tick body, and its wall-clock deadline.
            let tick_budget = crate::step_dispatch::gather_budget::begin_tick(&fallback_root);

            // #8121: reload the registry and provision any newly-registered
            // root's `SweepRegistry` BEFORE the rate-limit breaker check
            // below — see `registry_refresh`'s module doc comment for
            // why this purely-local, no-GitHub-API work must never be gated
            // behind the breaker (`loom-daemon workspace add|remove
            // |set-weight` must hot-apply even while gh polling is
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
                if rl.skip_if_suppressed("work_finder", now) {
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
            // scratch volume, and — since #9060 — the operator ceiling itself)
            // probed from the daemon's primary workspace.
            // #11911: one host budget, one configured number (2am#4463).
            let configured_max = configured_max_reload.refresh();
            crate::host_budget::log_resolved(configured_max);
            // Per-repo cap (#9090): the same per-tick config read, so an edit to
            // `maxConcurrentPerRepo` hot-applies exactly as the ceiling does.
            // `None` (the default) leaves dispatch byte-for-byte pre-#9090.
            let max_concurrent_per_repo = configured_max_reload.per_repo();
            // #12090: and the per-tick admission (ramp) cap, from the same read.
            let max_admissions_per_tick = configured_max_reload.max_admissions_per_tick();
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
            // #11094: samples every live agent scope's `memory.peak` (folding
            // finished scopes into the repo history) and charges admission
            // with the observed per-repo peak.
            // The budget charges each candidate its OWN repo's peak (pass 2).
            let (ram, ram_budget) = crate::ram_headroom::ram_headroom_limit_tick(&pool, &roots);
            // Bounded tmpfs-fraction warning (#8572, split from #8512) — logs
            // only, never gates dispatch; see `tmpfs_warning`'s module doc.
            tmpfs_warning::check_and_warn(&fallback_root);
            // Eager, out-of-cycle reclaim (#7512, #11192) for `fallback_root`
            // — the same root this machine-level disk term is probed against.
            // Edge plus level (below the floor, or still falling). The disk
            // term is #11191's admission term (it nets out the in-flight
            // sweeps' expected growth; the budget charges each repo its
            // measured footprint); both come back re-ticked after a pass,
            // before this tick's cap is finalized. See the single-workspace
            // loop above and `EagerTrigger::tick_admission`.
            let (disk, disk_budget) = eager_trigger
                .tick_admission(&roots, &fallback_root, ram, configured_max)
                .await;
            // Refresh the memoized CPU idle sample — **observational only**
            // since #4512 (see the single-workspace loop above). It feeds the
            // `observed_idle=` figure in the axis line and `loom-daemon status`
            // / `calibrate`, which is how an operator tunes this machine's
            // `maxConcurrent`; it no longer gates admission.
            let _ = tokio::task::spawn_blocking(crate::cpu_headroom::refresh_cpu_util_cache).await;
            let idle = crate::cpu_headroom::cached_cpu_idle_fraction();
            let max_concurrent = resolve_dynamic_max_concurrent(disk, ram, configured_max);
            // #12074: from here to the step tick nothing awaits, so every forge
            // read in between is bounded by the tick deadline and fails fast
            // past it; a listing does not fall through to the writer.
            let tick_deadline = std::time::Instant::now() + tick_budget;
            let read_scope = crate::gh_invocation::read_deadline::enter(tick_deadline, true);

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

            // Per-repo draw weights (#11742), parallel to `pairs`: higher = drawn
            // more. The empty-registry cwd fallback resolves to the default.
            let priorities: Vec<u32> = roots.iter().map(|r| registry.weight_of(r)).collect();

            // Shard verdicts, per root, for the dispatch plan's `owning_shard`
            // only (the fleet-merge primary rule, #9288). They no longer
            // shape dispatch: the #6243 slice preference was retired (#11682)
            // because pass 2 could reject every in-slice candidate after the
            // slice had already deferred everything else. The role runner's
            // own (#6374) use of the same ring is unchanged.
            let shard_decisions = dispatch_plan::shard_decisions(&roots);

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
            // #6102: the role-agent count passed alongside it is the other part
            // of this host's agent load, which this brake does not hold back
            // (it shares the one host budget, #11911). Reported into
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
            // #9410/#10624: in-memory reads of the PR-debt ledger the step
            // tick feeds, one per root; fails open when unobserved.
            let ledger = crate::pr_debt::global();
            ledger.retain_roots(&roots);
            let build_backoff_held = build_backoff.step(&fallback_root, &roots, ledger);
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
            let (preflight_held, preflight_causes, preflight_probe_roots) =
                pool_preflight::preflight_held_causes_per_root(&pool, &roots, now_tick);
            // #9017: compute the per-root CAUSE, then project the `halted`
            // bool from it, so the slice the tick routes on and the causes
            // its rows record are one fold and can never disagree — the same
            // terms, same inputs, same instant as the pre-#9017 inline bool
            // fold (`dispatch_held_per_root_with_preflight` + drain +
            // breaker, which `causes_per_root` mirrors cause-for-cause).
            let halt_causes = halt_cause::causes_per_root(
                &health_states,
                &roots,
                suppress_dispatch_during_gate,
                &preflight_causes,
                draining,
                breaker_suppressed,
            );
            // #11191: a repo whose disk charge does not fit is held, named.
            // #11564: the result is recorded with its start for `fleet.state`.
            let halt_causes = halt_cause::publish(halt_causes, disk_budget.as_ref(), &roots);
            let disk_note = crate::disk_admission::note(disk_budget.as_ref());
            let halted: Vec<bool> = halt_causes.iter().map(Option::is_some).collect();
            let preflight_held_count = preflight_held.iter().filter(|&&h| h).count();
            // Distinguish a pre-flight-advisory hold from the main-health /
            // gate-in-flight holds (#5030 AC4) so an operator can tell "held
            // because pre-flight is broken" apart from "held because CI is red."
            // The slice folds four holds (#5030, #7708, write scope, #10719),
            // so this edge line counts the causes in force and defers the
            // specifics to whichever hold logged its own edge line.
            if preflight_held_count != was_preflight_held_count {
                if preflight_held_count > 0 {
                    log::warn!(
                        "work_finder: {preflight_held_count} of {} repo(s) held before dispatch \
                         ({}); see each hold's own edge line. A preflight_advisory hold (#5030) \
                         still allows one probe per cooldown; token_pool (#7708), write_scope and \
                         the workspace holds (#10719) allow none",
                        roots.len(),
                        halt_cause::held_summary(&preflight_causes)
                    );
                } else {
                    log::info!(
                        "work_finder: pre-dispatch holds cleared for all repos — dispatch \
                         resuming (#5030/#7708/#10719)"
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
            // #9244 red-main-fix lanes, parallel to `roots`: which halted repos
            // are verified red with no other hold (they admit their fixes).
            let lanes = main_red_fix::lanes_per_root(
                &health_states,
                &roots,
                suppress_dispatch_during_gate,
                &preflight_held,
                draining || breaker_suppressed,
            );

            let axis_line = format!(
                "work_finder: dynamic cap = {max_concurrent} (pool={pool_size}, \
                 healthy_tokens={token_limit} [informational only, not capacity-limiting \
                 since #5270], disk={disk}, ram={ram}, configured_max={configured_max}, \
                 max_admissions_per_tick={max_admissions_per_tick}, any_halted={any_halted}, \
                 per_repo_cap={max_concurrent_per_repo:?} (#9090), \
                 preflight_held={preflight_held_count}, \
                 saturation_held={saturation_held}, \
                 build_backoff_held={build_backoff_held} (#9410), \
                 observed_idle={}, workspaces={}, priorities={priorities:?})",
                format_idle(idle),
                roots.len()
            );
            if was_max_concurrent != Some(max_concurrent) {
                log::info!("{axis_line}");
                was_max_concurrent = Some(max_concurrent);
            } else {
                log::debug!("{axis_line}");
            }

            let tick_started = chrono::Utc::now();
            // #11909: the step tick is the only item dispatch path.
            let report = step_driver.tick(&crate::step_dispatch::driver::TickInputs {
                pool: &pool,
                roots: &roots,
                weights: &priorities,
                halted: &halted,
                halt_causes: &halt_causes,
                lanes: &lanes,
                build_backoff: &build_backoff_held.per_workspace,
                max_concurrent,
                // #11911: role-runner agents draw from the same budget.
                role_runs: crate::role_runner::active_run_count(&role_in_progress),
                max_admissions: max_admissions_per_tick,
                saturation_held,
                limits: (max_concurrent_per_repo, ram_budget, disk_budget).into(),
                deadline: Some(tick_deadline),
            });
            drop(read_scope);

            // Publish before any logging so `loom-daemon health` sees the same
            // tick the log line describes (#4761).
            if let Some(occupancy) = report.occupancy {
                crate::host_budget::publish(max_concurrent, occupancy);
            }
            let plan = dispatch_plan::PlanInputs::new(max_concurrent, interval, &shard_decisions);
            publish_tick(&report, max_concurrent, tick_started, &roots, Some(&plan));

            if report.halted && !was_halted {
                // #9591: name the hold(s) actually active — a drain is not a red main.
                let flags = (suppress_dispatch_during_gate, draining, breaker_suppressed);
                let t = halt_log::tally(&halted, &health_states, &roots, &preflight_held, flags);
                log::warn!("{}", t.warn_line());
            } else if !report.halted && was_halted {
                log::info!("work_finder: dispatch hold cleared — resuming dispatch");
            }
            was_halted = report.halted;

            // The tick line, with the disk budget's figures (#11191).
            let tick_line = tick_line::TickLine {
                max_concurrent,
                pool_size,
                token_limit,
                min_workspace_healthy,
                disk,
                ram,
                configured_max,
                max_admissions_per_tick,
                workspaces: roots.len(),
                disk_note: &disk_note,
            };
            tick_line::log(&report, &tick_line);
            // #10624: which repos the build back-off deferred (INFO on change).
            build_backoff.log_deferred(&report.queue, &roots);

            // #11857: a partial hold (say one maintain-only repo) must not
            // silence the capacity advisory for the healthy repos beside it.
            if !report.halted_fully() {
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
            for root in &roots {
                let idle_now = pool.get_or_provision(root).lock().map_or(true, |mut reg| {
                    reg.reap_liveness();
                    reg.occupied_issues().is_empty()
                });
                crate::role_runner::observe_and_fire_idle(
                    &mut idle_trigger,
                    &role_in_progress,
                    root,
                    idle_now,
                    draining,
                );
            }
            // #12222: time from daemon start to the first completed tick.
            first_tick.completed(&event_bus);
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
/// finder to the live forge (`gh`) and the daemon's [`SweepRegistry`]. Lives in
/// its own file (#8572) for the same file-size-ratchet reason as
/// [`registry_refresh`]; the move was pure, with no behavior change.
pub mod forge;

// Re-export the concrete adapters at the module root for ergonomic wiring.
pub use forge::{GhWorkSource, RegistryDispatcher};

pub mod pool_preflight;

// Purely-local registry reload + workspace provisioning for the tick loop
// (#8121). Lives in its own file because this one is over the file-size
// ratchet threshold (.loom/docs/file-size-policy.md) and may not grow.
mod registry_refresh;

/// The bounded, non-spammy tmpfs-fraction warning (issue #8572, split from
/// #8512) — logs, never gates dispatch. Lives in its own file for the same
/// file-size-ratchet reason as [`registry_refresh`].
mod tmpfs_warning;

/// Cause-attributed wording for the "dispatch halted" transition line (#9591).
/// Its own file for the same file-size-ratchet reason as [`registry_refresh`].
mod halt_log;

/// Per-tick hot-reload of the operator ceiling `configured_max` (#9060). Its
/// own file for the same file-size-ratchet reason as [`registry_refresh`].
pub mod configured_max;
pub use configured_max::{ConfiguredMax, ConfiguredMaxReloader};

/// Per-repo dispatch cap + repo-track affinity (#9090), including the
/// candidate-list shaping (the affinity partition). Its own
/// file for the same file-size-ratchet reason as [`registry_refresh`].
pub mod repo_cap;
pub use repo_cap::RepoCap;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
