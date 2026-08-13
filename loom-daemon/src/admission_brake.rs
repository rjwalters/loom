//! Saturation admission brake (Issue #4903).
//!
//! # Why this exists
//!
//! #4512 removed the CPU term from the dynamic concurrency cap
//! ([`crate::work_finder::resolve_dynamic_max_concurrent`]) and that removal was
//! right for the workload it measured: an issue→PR sweep is dominated by
//! API-wait, so pricing every sweep as a build throttled the ~90% low-CPU
//! majority to defend against the minority case. But it left admission with **no
//! term at all** that reads the host: the cap was `min(token axis, disk
//! headroom, configured_max)` (and, since #5270, is `min(disk headroom,
//! configured_max)` — the token axis no longer participates), and when
//! `maxConcurrent` is unset (or generously set for an API-bound repo) nothing
//! bounds admission by observed load.
//!
//! A CPU-heavy workload then walks straight past it. `loom-worker-1` (8 vCPU) was
//! observed at **load average 95** — `12×` overcommit, `0.07%` CPU idle — from
//! only three in-flight sweeps, because all three were analog-simulation repos
//! (`gf180-*`) that had spawned 16 `ngspice` processes between them. The daemon
//! *measured* the saturation (`loadavg_1m` and `cpu_idle_fraction` are in the
//! dispatch headroom report) and did not act on it: `capacity_bound: false`, cap
//! `12`, nine more slots nominally free.
//!
//! This module is the missing backstop, and it is deliberately **not** a return
//! to CPU-based sizing: it does not resize the cap, it does not estimate
//! per-sweep cores, and it never touches work that is already running. It answers
//! exactly one question, once per work-finder tick — *is this host already too
//! full to take on more?* — and when the answer is yes it **holds new
//! admissions** and re-checks on the next tick.
//!
//! # Distinction from the host-distress circuit breaker (#4235)
//!
//! Both read the same normalized ratio ([`crate::cpu_headroom::load_per_core`]),
//! and they are complements, not duplicates:
//!
//! | | Admission brake (this module, #4903) | Host breaker ([`crate::host_breaker`], #4235) |
//! |---|---|---|
//! | Nature | **Point-in-time** — one reading, one decision | **Stateful** — trips only on N consecutive over-threshold ticks |
//! | Default threshold | `0.95` load/core (see [`DEFAULT_ADMISSION_BRAKE_LOAD_PER_CORE`]; `4.0` before #5270) | `2.5` load/core |
//! | Release | Immediate, the first tick load drops back under | After a 5-minute cool-down |
//! | Explicit `dispatch_sweep` | **Never blocks it** (an operator asking by hand is not autonomous admission) | Hard-blocks unless `force` |
//! | Purpose | Stop *adding* to a full host | Remember a *meltdown* so a load dip cannot re-admit a whole wave |
//!
//! So the brake is the cheap, fast-releasing guard that keeps a busy host from
//! being handed more work this minute; the breaker is the slow, sticky guard that
//! remembers genuine distress across minutes. **Since #5270 the brake's default
//! threshold sits *below* the breaker's** (the reverse of the pre-#5270
//! ordering) — with the token axis gone, this brake is promoted from a rarely-
//! tripped backstop to the primary CPU admission gate for "dumb mode" (operator
//! direction: hold admission at ≥95% few-minute load-per-core), so it now
//! engages well before the breaker's higher, sustained-distress trip.
//!
//! # Fail-safe
//!
//! A missing load reading (`None` from [`crate::cpu_headroom::read_loadavg_1m`],
//! or a zero CPU count) means **no hold** — [`crate::cpu_headroom::is_host_saturated`]
//! returns `None` on absent evidence and this module treats that as "not
//! saturated", exactly the contract the build gate's load-aware deferral (#4259)
//! follows. Absent evidence must never look like load.
//!
//! # In-flight sweeps are never touched
//!
//! The brake is consulted only where *new* candidates are admitted
//! ([`crate::work_finder::tick_with_admission_cap`] /
//! [`crate::work_finder::tick_multi_with_admission_cap`], immediately before the
//! concurrency-cap check). It has no path to the reaper, to `cancel_sweep`, or to
//! any running child process. A held tick dispatches nothing and returns; the
//! sweeps already running finish normally, which is what lets the host recover.
//!
//! # Starvation escape hatch (Issue #5715)
//!
//! The brake's release condition — "the very next reading drops back under
//! threshold" — has an unstated assumption: that *something the brake is
//! blocking* is what is holding the load up, so simply waiting eventually
//! relieves the pressure. That assumption breaks when the load is generated
//! entirely by work the brake has no authority over — e.g. the role runner's
//! own champion/curator/judge/doctor/guide ticks (#5270) — because then
//! holding admission cannot ever reduce the thing being measured, and the
//! brake livelocks: held forever, load never drops, release condition never
//! met. Observed on `robb-studio` for 33h with **zero** sweeps in flight the
//! entire time.
//!
//! [`SharedAdmissionBrake::observe`] is passed the number of sweeps currently
//! in flight (from [`crate::ipc::count_in_flight_sweeps`], or a dispatcher's
//! own [`crate::work_finder::WorkDispatcher::in_flight`] in the
//! single-workspace loop) precisely to detect this shape: *held* + *zero in
//! flight*, continuously, is never healthy backpressure — backpressure by
//! definition has something running to drain. Two bounded reactions follow
//! from that one signal, both configurable via [`AdmissionBrakeConfig`]:
//!
//! 1. **Escalating log.** After
//!    [`AdmissionBrakeConfig::starvation_warn_secs`] of continuous starvation
//!    a `WARN`-level `admission_brake: STARVING` line fires once, naming the
//!    elapsed duration — the per-tick `deferred (host saturated)` counter
//!    alone cannot be told apart from one healthy backpressure tick.
//! 2. **Escape hatch.** After
//!    [`AdmissionBrakeConfig::starvation_escape_secs`] the brake yields for
//!    exactly one tick — [`BrakeDecision::held`] reports `false` even though
//!    the raw load reading is still over threshold
//!    ([`BrakeDecision::starvation_escape`] flags this) — logged at `ERROR`.
//!    That one tick lets the work-finder's ordinary capacity/ramp logic admit
//!    (bounded) new work, which starts consuming the load the brake could
//!    never itself reduce. The starvation streak resets immediately after an
//!    escape, so this is a periodic safety valve (bounded by the configured
//!    window each time it recurs), never a standing bypass of #5270's "dumb
//!    mode" gate.
//!
//! Both reactions require **zero** in-flight sweeps throughout the whole
//! window — a single tick with even one sweep running resets the streak, so
//! genuine backpressure (a busy host actually shedding sweep load) is never
//! mistaken for starvation, however long it holds.

use std::sync::{Arc, Mutex, OnceLock};

use chrono::{DateTime, Utc};

// ============================================================================
// Config surface (env > config > default, mirroring host_breaker + work_finder)
// ============================================================================

/// Env var toggling the saturation admission brake. `0`/`false`/`no`/`off`
/// disables; anything truthy (`1`/`true`/`yes`/`on`) forces on. Overrides
/// config. Defaults ON — a safety backstop, the same call
/// [`crate::host_breaker::HOST_BREAKER_ENABLE_ENV`] makes.
pub const ADMISSION_BRAKE_ENABLE_ENV: &str = "LOOM_ADMISSION_BRAKE";

/// Env var overriding the load-per-core hold threshold. A `<= 0`/invalid value
/// falls through to config/default.
pub const ADMISSION_BRAKE_LOAD_PER_CORE_ENV: &str = "LOOM_ADMISSION_BRAKE_LOAD_PER_CORE";

/// Env var overriding [`DEFAULT_ADMISSION_BRAKE_STARVATION_WARN_SECS`]. A
/// `<= 0`/invalid value falls through to config/default.
pub const ADMISSION_BRAKE_STARVATION_WARN_SECS_ENV: &str =
    "LOOM_ADMISSION_BRAKE_STARVATION_WARN_SECS";

/// Env var overriding [`DEFAULT_ADMISSION_BRAKE_STARVATION_ESCAPE_SECS`]. A
/// `<= 0`/invalid value falls through to config/default.
pub const ADMISSION_BRAKE_STARVATION_ESCAPE_SECS_ENV: &str =
    "LOOM_ADMISSION_BRAKE_STARVATION_ESCAPE_SECS";

/// Default: the brake is enabled. It is a backstop, and like the host breaker
/// it only acts on measured evidence and never kills running work, so a repo
/// that never enables the work-finder sees zero behavior change (the finder loop
/// is the brake's sole sampler) and a repo that does gets the guard for free.
pub const DEFAULT_ADMISSION_BRAKE_ENABLED: bool = true;

/// Default load-per-core ratio at/above which new admissions are held.
///
/// **Retuned in #5270 from `4.0` to `0.95`.** Until #5270 the token axis
/// ([`crate::capacity::token_axis_limit`]) and disk headroom were the two hard
/// admission floors, with this brake acting only as a generous backstop above
/// them. #5270 removed the token axis from admission entirely (operator
/// direction: "we should only ever limit parallelism based on the machine
/// disk/RAM/CPU"), which promotes this brake from a rarely-tripped safety net
/// to **the** CPU admission gate in that "dumb mode" — so its threshold now
/// matches the operator's literal ask: hold new admissions once the host's
/// few-minute load average reaches ~95% of its logical core count, resume
/// below it. `1.0` load-per-core means "as many runnable/uninterruptible
/// threads as logical cores"; `0.95` holds a notch below full saturation,
/// mirroring the build gate's own `0.9` deferral point
/// ([`crate::cpu_headroom::DEFAULT_GATE_LOAD_THRESHOLD`]) rather than the old
/// `4.0`, which would have let a host run at 4× overcommit before this brake
/// ever engaged.
///
/// It now sits **below**
/// [`crate::host_breaker::DEFAULT_HOST_BREAKER_LOAD_PER_CORE`] (`2.5`) — the
/// reverse of the pre-#5270 ordering, and intentional under the new model: the
/// brake is the fast, cheap, per-tick hold that engages first (a single
/// over-threshold reading), and the breaker remains the slower, stickier trip
/// for genuine sustained distress (several consecutive over-threshold ticks)
/// well past the point the brake has already held new admissions. See the
/// module docs' comparison table for the full distinction between the two
/// mechanisms.
pub const DEFAULT_ADMISSION_BRAKE_LOAD_PER_CORE: f64 = 0.95;

/// Default: how long the brake may hold admission with **zero** sweeps in
/// flight before it escalates its per-tick `deferred (host saturated)` INFO
/// counter to a standalone `WARN`-level `STARVING` log line naming the
/// elapsed duration (Issue #5715). One tick of "held, 0 in flight" is
/// unremarkable — a role-runner burst can transiently push load over the
/// brake's `0.95`/core threshold — but continuing past 5 minutes with nothing
/// at all draining is never ordinary backpressure.
pub const DEFAULT_ADMISSION_BRAKE_STARVATION_WARN_SECS: i64 = 300;

/// Default: how long the brake may hold admission with **zero** sweeps in
/// flight before the starvation escape hatch yields for exactly one tick,
/// admitting new work despite the raw load reading still being over threshold
/// (Issue #5715). Bounds the livelock the brake cannot otherwise break out of
/// on its own — a host whose *only* load source is work the brake has no
/// authority over (the role runner) can never satisfy the brake's normal
/// release condition, so left unbounded that host starves sweep admission
/// forever (33h, observed). 15 minutes is generous next to the default 60s
/// work-finder tick interval (many ticks to confirm this is not a fluke) while
/// still being a small fraction of an outage that would otherwise run for
/// days.
pub const DEFAULT_ADMISSION_BRAKE_STARVATION_ESCAPE_SECS: i64 = 900;

/// Resolved brake parameters (env > config > default), captured once at daemon
/// startup and held by [`SharedAdmissionBrake`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AdmissionBrakeConfig {
    /// Whether the brake is active. When `false` it never holds and never
    /// records a hold (byte-for-byte the pre-#4903 admission path).
    pub enabled: bool,
    /// Load-per-core at/above which new admissions are held this tick.
    pub load_per_core_threshold: f64,
    /// Seconds of continuous "held, 0 sweeps in flight" before the starvation
    /// `WARN` log fires once (Issue #5715). See
    /// [`DEFAULT_ADMISSION_BRAKE_STARVATION_WARN_SECS`].
    pub starvation_warn_secs: i64,
    /// Seconds of continuous "held, 0 sweeps in flight" before the escape
    /// hatch yields for one tick (Issue #5715). See
    /// [`DEFAULT_ADMISSION_BRAKE_STARVATION_ESCAPE_SECS`].
    pub starvation_escape_secs: i64,
}

impl Default for AdmissionBrakeConfig {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_ADMISSION_BRAKE_ENABLED,
            load_per_core_threshold: DEFAULT_ADMISSION_BRAKE_LOAD_PER_CORE,
            starvation_warn_secs: DEFAULT_ADMISSION_BRAKE_STARVATION_WARN_SECS,
            starvation_escape_secs: DEFAULT_ADMISSION_BRAKE_STARVATION_ESCAPE_SECS,
        }
    }
}

/// The raw config half read from
/// `.loom/config.json → autonomous.workFinder.saturationBrake` (each field
/// `None` when absent/malformed), before env/default resolution.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AdmissionBrakeConfigFile {
    pub enabled: Option<bool>,
    pub load_per_core_threshold: Option<f64>,
    /// `starvationWarnSecs` (Issue #5715).
    pub starvation_warn_secs: Option<i64>,
    /// `starvationEscapeSecs` (Issue #5715).
    pub starvation_escape_secs: Option<i64>,
}

/// Read `.loom/config.json → autonomous.workFinder.saturationBrake`, soft-failing
/// every field to `None` (env/default resolution) on a missing file, malformed
/// JSON, or a missing block. Mirrors [`crate::host_breaker::read_host_breaker_config`].
///
/// Nested under `workFinder` rather than beside `hostBreaker` because the brake
/// is part of the work-finder's **admission** policy — the same block that holds
/// `maxConcurrent`, which it backstops.
#[must_use]
pub fn read_admission_brake_config(repo_root: &std::path::Path) -> AdmissionBrakeConfigFile {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(autonomous) = crate::config_resolver::get_path(&effective, "autonomous") else {
        return AdmissionBrakeConfigFile::default();
    };
    let brake = autonomous
        .get("workFinder")
        .and_then(|w| w.get("saturationBrake"));
    AdmissionBrakeConfigFile {
        enabled: brake
            .and_then(|b| b.get("enabled"))
            .and_then(serde_json::Value::as_bool),
        load_per_core_threshold: brake
            .and_then(|b| b.get("loadPerCoreHold"))
            .and_then(serde_json::Value::as_f64)
            .filter(|&f| f > 0.0),
        starvation_warn_secs: brake
            .and_then(|b| b.get("starvationWarnSecs"))
            .and_then(serde_json::Value::as_i64)
            .filter(|&s| s > 0),
        starvation_escape_secs: brake
            .and_then(|b| b.get("starvationEscapeSecs"))
            .and_then(serde_json::Value::as_i64)
            .filter(|&s| s > 0),
    }
}

/// Env override for [`ADMISSION_BRAKE_ENABLE_ENV`] — `Some(true/false)` when set
/// to a recognized truthy/falsy value, `None` when unset (config/default decides).
fn env_enabled() -> Option<bool> {
    match std::env::var(ADMISSION_BRAKE_ENABLE_ENV) {
        Ok(v) => {
            Some(matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        }
        Err(_) => None,
    }
}

fn env_load_per_core() -> Option<f64> {
    std::env::var(ADMISSION_BRAKE_LOAD_PER_CORE_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|&f| f > 0.0)
}

fn env_starvation_warn_secs() -> Option<i64> {
    std::env::var(ADMISSION_BRAKE_STARVATION_WARN_SECS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|&s| s > 0)
}

fn env_starvation_escape_secs() -> Option<i64> {
    std::env::var(ADMISSION_BRAKE_STARVATION_ESCAPE_SECS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|&s| s > 0)
}

/// Resolve the full [`AdmissionBrakeConfig`] with precedence **env > config >
/// default** for every field independently.
#[must_use]
pub fn resolve_config(file: &AdmissionBrakeConfigFile) -> AdmissionBrakeConfig {
    AdmissionBrakeConfig {
        enabled: env_enabled()
            .or(file.enabled)
            .unwrap_or(DEFAULT_ADMISSION_BRAKE_ENABLED),
        load_per_core_threshold: env_load_per_core()
            .or(file.load_per_core_threshold)
            .unwrap_or(DEFAULT_ADMISSION_BRAKE_LOAD_PER_CORE),
        starvation_warn_secs: env_starvation_warn_secs()
            .or(file.starvation_warn_secs)
            .unwrap_or(DEFAULT_ADMISSION_BRAKE_STARVATION_WARN_SECS),
        starvation_escape_secs: env_starvation_escape_secs()
            .or(file.starvation_escape_secs)
            .unwrap_or(DEFAULT_ADMISSION_BRAKE_STARVATION_ESCAPE_SECS),
    }
}

/// Convenience: read `repo_root`'s config and resolve it end-to-end.
#[must_use]
pub fn resolve_config_for(repo_root: &std::path::Path) -> AdmissionBrakeConfig {
    resolve_config(&read_admission_brake_config(repo_root))
}

// ============================================================================
// Pure decision
// ============================================================================

/// One tick's brake decision — the pure output of [`decide`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BrakeDecision {
    /// Whether **new** admissions are held this tick. Always `false` when the
    /// brake is disabled or the load reading is absent. Also `false` on an
    /// escape-hatch tick (see [`Self::starvation_escape`]) even though the raw
    /// load reading remains over threshold.
    pub held: bool,
    /// The observed load-per-core, or `None` when no load source was available.
    pub load_per_core: Option<f64>,
    /// The threshold the decision was taken against (echoed for the log/status
    /// line so an operator never has to guess which knob was in force).
    pub threshold: f64,
    /// `true` only on the one tick the starvation escape hatch overrides
    /// [`Self::held`] to `false` despite the host still reading over threshold
    /// (Issue #5715) — i.e. this is *not* a genuine load recovery, it is the
    /// bounded livelock breaker admitting one tick of work anyway. Always
    /// `false` from the pure [`decide`] function; only
    /// [`SharedAdmissionBrake::observe`] (which has the in-flight-sweep count
    /// `decide` does not) can set it.
    pub starvation_escape: bool,
}

/// The pure decision function: is this host too saturated to admit new work?
///
/// Delegates the ratio and the comparison to
/// [`crate::cpu_headroom::is_host_saturated`] so the brake, the build gate's
/// load-aware deferral (#4259), and the host breaker (#4235) all agree on one
/// definition of load-per-core.
///
/// Fail-safe in two directions:
/// - `enabled: false` ⇒ never held (the reading is still reported, so the status
///   surface stays honest about *why* nothing is being held).
/// - Missing load / zero CPUs ⇒ `is_host_saturated` yields `None` ⇒ not held.
///   Absent evidence is never treated as load.
#[must_use]
pub fn decide(
    config: &AdmissionBrakeConfig,
    loadavg_1m: Option<f64>,
    ncpu: usize,
) -> BrakeDecision {
    let load_per_core = crate::cpu_headroom::load_per_core_from(loadavg_1m, ncpu);
    let held = config.enabled
        && crate::cpu_headroom::is_host_saturated(loadavg_1m, ncpu, config.load_per_core_threshold)
            .unwrap_or(false);
    BrakeDecision {
        held,
        load_per_core,
        threshold: config.load_per_core_threshold,
        starvation_escape: false,
    }
}

// ============================================================================
// Runtime state + transitions
// ============================================================================

/// The evolving brake state held by [`SharedAdmissionBrake`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BrakeRuntime {
    /// Whether the most recent observation held new admissions.
    pub held: bool,
    /// When the current hold streak began (`None` while not holding).
    pub held_since: Option<DateTime<Utc>>,
    /// How many consecutive ticks the current streak has held (`0` when not
    /// holding).
    pub held_ticks: u32,
    /// The most recent load-per-core sample observed.
    pub last_load_per_core: Option<f64>,
    /// When the current **starvation** streak began — the brake has held
    /// admission AND zero sweeps have been in flight, continuously, since
    /// this timestamp. `None` whenever that is not true this tick (Issue
    /// #5715). Distinct from `held_since`: a brake held while sweeps drain is
    /// healthy backpressure and never sets this.
    pub starving_since: Option<DateTime<Utc>>,
    /// How many consecutive ticks the current starvation streak has held
    /// (`0` when not starving).
    pub starving_ticks: u32,
    /// The escalation phase already logged for the current starvation streak,
    /// so [`SharedAdmissionBrake::observe`] emits its `WARN`/`ERROR` line once
    /// per phase, not every tick.
    starvation_phase: StarvationPhase,
    /// Cumulative count of starvation-escape-hatch grants across this
    /// process's lifetime — a debugging aid on the status snapshot: `0` on a
    /// healthy host forever; any nonzero count means the brake has had to
    /// force an admission through at least once (Issue #5715).
    pub escape_hatch_grants: u32,
}

/// The escalation phase of the current starvation streak (Issue #5715),
/// tracked so [`SharedAdmissionBrake::observe`] logs each `WARN`/`ERROR` line
/// exactly once per streak rather than every tick.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum StarvationPhase {
    /// Not starving, or starving but under [`AdmissionBrakeConfig::starvation_warn_secs`].
    #[default]
    None,
    /// Past [`AdmissionBrakeConfig::starvation_warn_secs`]; the `WARN` line
    /// has already fired for this streak.
    Warned,
}

/// Which edge a [`Transition`] represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionKind {
    /// Not holding → holding (the host crossed the threshold).
    Engaged,
    /// Holding → not holding (the host recovered).
    Released,
    /// Held, zero sweeps in flight, continuously for
    /// [`AdmissionBrakeConfig::starvation_warn_secs`] — logged once per
    /// streak at `WARN` (Issue #5715). The brake is still holding; this is a
    /// severity escalation, not a state change in [`BrakeDecision::held`].
    Starving,
    /// Held, zero sweeps in flight, continuously for
    /// [`AdmissionBrakeConfig::starvation_escape_secs`] — the escape hatch
    /// yielded this one tick despite the raw load reading still being over
    /// threshold, logged at `ERROR` (Issue #5715).
    StarvationEscape,
}

impl TransitionKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            TransitionKind::Engaged => "engaged",
            TransitionKind::Released => "released",
            TransitionKind::Starving => "starving",
            TransitionKind::StarvationEscape => "starvation_escape",
        }
    }
}

/// A recorded hold/release edge — the payload for the operator log line. Emitted
/// only on a real state change, never every tick.
#[derive(Debug, Clone, PartialEq)]
pub struct Transition {
    pub kind: TransitionKind,
    pub reason: String,
}

// ============================================================================
// A serializable snapshot for the status surface
// ============================================================================

/// A point-in-time snapshot of the brake for `loom-daemon status`. Maps to
/// [`crate::types::AdmissionBrakeStatus`] via [`Self::into_status`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BrakeSnapshot {
    pub enabled: bool,
    pub held: bool,
    pub load_per_core: Option<f64>,
    pub load_per_core_threshold: f64,
    pub held_since: Option<DateTime<Utc>>,
    pub held_ticks: u32,
    /// When the current starvation streak began (Issue #5715); `None` when
    /// not currently starving.
    pub starving_since: Option<DateTime<Utc>>,
    /// How many consecutive ticks the current starvation streak has held.
    pub starving_ticks: u32,
    /// Cumulative escape-hatch grants this process lifetime.
    pub escape_hatch_grants: u32,
}

impl BrakeSnapshot {
    /// Convert to the wire/status type in [`crate::types`].
    #[must_use]
    pub fn into_status(self) -> crate::types::AdmissionBrakeStatus {
        crate::types::AdmissionBrakeStatus {
            enabled: self.enabled,
            held: self.held,
            load_per_core: self.load_per_core,
            load_per_core_threshold: self.load_per_core_threshold,
            held_since: self.held_since,
            held_ticks: self.held_ticks,
            starving_since: self.starving_since,
            starving_ticks: self.starving_ticks,
            escape_hatch_grants: self.escape_hatch_grants,
        }
    }
}

// ============================================================================
// Shared runtime handle + process-global registration
// ============================================================================

/// Thread-safe brake handle: the work-finder loop [`observe`](Self::observe)s a
/// fresh load sample into it each tick and acts on the returned decision; the
/// IPC status path reads [`snapshot`](Self::snapshot). All decisions delegate to
/// the pure [`decide`].
#[derive(Debug)]
pub struct SharedAdmissionBrake {
    config: AdmissionBrakeConfig,
    inner: Mutex<BrakeRuntime>,
}

// Allow expect_used: a poisoned brake mutex means another thread panicked while
// holding it — unrecoverable, matching host_breaker / ipc.rs / auto_update.rs.
#[allow(clippy::expect_used)]
impl SharedAdmissionBrake {
    #[must_use]
    pub fn new(config: AdmissionBrakeConfig) -> Self {
        Self {
            config,
            inner: Mutex::new(BrakeRuntime::default()),
        }
    }

    #[must_use]
    pub fn config(&self) -> AdmissionBrakeConfig {
        self.config
    }

    /// Fold one load reading into the brake, returning the decision plus any
    /// hold/release/starvation edge (the caller logs on `Some`).
    ///
    /// `loadavg_1m` / `ncpu` are passed in — never re-probed here — so the
    /// reading that produced the decision is exactly the reading the caller
    /// already sampled for the host breaker this tick. One read, one decision:
    /// no window in which the brake and the breaker disagree about the host.
    ///
    /// `in_flight_sweeps` is the caller's own count of currently-running
    /// sweeps (Issue #5715) — e.g.
    /// [`crate::ipc::count_in_flight_sweeps`] across every managed root, or a
    /// single dispatcher's [`crate::work_finder::WorkDispatcher::in_flight`]
    /// in the single-workspace loop. It is the signal that distinguishes
    /// healthy backpressure (held, but sweeps are genuinely draining) from
    /// starvation (held with **nothing** running to ever relieve it) — see
    /// the module docs' "Starvation escape hatch" section.
    ///
    /// `active_role_agents` (Issue #6102) is the caller's count of role-runner
    /// agents in flight across every managed workspace
    /// ([`crate::role_runner::global_active_run_count`]). It does **not** change
    /// any hold, release, or escape decision — every one of those still turns on
    /// `in_flight_sweeps`, byte-for-byte as #5715 defined them. It exists so the
    /// starvation transitions can state the truth about what is running:
    /// "nothing is running to relieve it" is simply false on a host with three
    /// Curators mid-tick, and the brake has no authority to hold those back.
    /// Diagnosing the incident behind #6102 required `pgrep` precisely because
    /// this number never appeared in the brake's own messages.
    pub fn observe(
        &self,
        loadavg_1m: Option<f64>,
        ncpu: usize,
        now: DateTime<Utc>,
        in_flight_sweeps: usize,
        active_role_agents: usize,
    ) -> (BrakeDecision, Option<Transition>) {
        let raw = decide(&self.config, loadavg_1m, ncpu);
        let mut guard = self.inner.lock().expect("admission brake mutex poisoned");
        let was_held = guard.held;
        let load_str = raw
            .load_per_core
            .map_or_else(|| "n/a".to_string(), |l| format!("{l:.2}"));

        // ---- starvation bookkeeping (#5715) --------------------------
        // Held + zero in flight, continuously, is never healthy backpressure
        // (backpressure by definition has something running to drain it) —
        // any tick with even one sweep in flight, or that stops being held,
        // resets the streak.
        let starving_now = raw.held && in_flight_sweeps == 0;
        if starving_now {
            if guard.starving_since.is_none() {
                guard.starving_since = Some(now);
            }
            guard.starving_ticks = guard.starving_ticks.saturating_add(1);
        } else {
            guard.starving_since = None;
            guard.starving_ticks = 0;
            guard.starvation_phase = StarvationPhase::None;
        }
        let starving_elapsed_secs = guard.starving_since.map(|s| (now - s).num_seconds().max(0));

        // #6102: what IS running while zero sweeps are. Appended to both
        // starvation messages so they attribute the load honestly instead of
        // asserting an unqualified "nothing is running".
        let role_clause = role_agent_clause(active_role_agents);

        let mut decision = raw;
        let mut starvation_transition = None;
        if let Some(secs) = starving_elapsed_secs {
            if secs >= self.config.starvation_escape_secs {
                // Escape hatch: yield this one tick regardless of load, so a
                // brake that cannot itself reduce the load it is reacting to
                // cannot starve sweep admission forever.
                decision.held = false;
                decision.starvation_escape = true;
                guard.escape_hatch_grants = guard.escape_hatch_grants.saturating_add(1);
                starvation_transition = Some(Transition {
                    kind: TransitionKind::StarvationEscape,
                    reason: format!(
                        "STARVATION ESCAPE HATCH — held admission for {} with 0 sweeps in \
                         flight (load {load_str}/core ≥ {:.2}); admitting new work this tick \
                         despite saturation, because no sweep is running for the brake to drain \
                         (Issue #5715){role_clause}",
                        format_hms(secs),
                        self.config.load_per_core_threshold
                    ),
                });
                // Reset the streak: the next escape needs its own full grace
                // window, so this can never degrade into "yield every tick"
                // (which would defeat the brake outright).
                guard.starving_since = None;
                guard.starving_ticks = 0;
                guard.starvation_phase = StarvationPhase::None;
            } else if secs >= self.config.starvation_warn_secs
                && guard.starvation_phase == StarvationPhase::None
            {
                guard.starvation_phase = StarvationPhase::Warned;
                starvation_transition = Some(Transition {
                    kind: TransitionKind::Starving,
                    reason: format!(
                        "STARVING — held admission for {} with 0 sweeps in flight (load \
                         {load_str}/core ≥ {:.2}); this is not healthy backpressure — no sweep \
                         is running for the brake to drain — the escape hatch admits one sweep \
                         anyway after {} (Issue #5715){role_clause}",
                        format_hms(secs),
                        self.config.load_per_core_threshold,
                        format_hms(self.config.starvation_escape_secs)
                    ),
                });
            }
        }

        // ---- plain hold/release bookkeeping, using the (possibly
        // starvation-overridden) `decision.held` -----------------------
        guard.held = decision.held;
        guard.last_load_per_core = decision.load_per_core;
        let held_edge_transition = if decision.held {
            guard.held_ticks = guard.held_ticks.saturating_add(1);
            if was_held {
                None
            } else {
                guard.held_since = Some(now);
                Some(Transition {
                    kind: TransitionKind::Engaged,
                    reason: format!(
                        "host saturated (load {load_str}/core ≥ {:.2}) — holding NEW sweep \
                         admissions; in-flight sweeps are untouched and will drain",
                        decision.threshold
                    ),
                })
            }
        } else {
            let ticks = guard.held_ticks;
            guard.held_ticks = 0;
            guard.held_since = None;
            // A starvation-escape tick is NOT a genuine recovery — the raw
            // load reading is still over threshold — so it must never log
            // the "host recovered" message; `starvation_transition` already
            // carries the correct (ERROR-level) explanation for this tick.
            if was_held && !decision.starvation_escape {
                Some(Transition {
                    kind: TransitionKind::Released,
                    reason: format!(
                        "host recovered (load {load_str}/core < {:.2} after {ticks} held tick(s)) \
                         — resuming admissions",
                        decision.threshold
                    ),
                })
            } else {
                None
            }
        };

        let transition = starvation_transition.or(held_edge_transition);
        (decision, transition)
    }

    /// Whether the brake is currently holding new admissions.
    #[must_use]
    pub fn is_holding(&self) -> bool {
        if !self.config.enabled {
            return false;
        }
        self.inner
            .lock()
            .expect("admission brake mutex poisoned")
            .held
    }

    /// A snapshot for the status surface.
    #[must_use]
    pub fn snapshot(&self) -> BrakeSnapshot {
        let guard = self.inner.lock().expect("admission brake mutex poisoned");
        BrakeSnapshot {
            enabled: self.config.enabled,
            held: self.config.enabled && guard.held,
            load_per_core: guard.last_load_per_core,
            load_per_core_threshold: self.config.load_per_core_threshold,
            held_since: guard.held_since,
            held_ticks: guard.held_ticks,
            starving_since: guard.starving_since,
            starving_ticks: guard.starving_ticks,
            escape_hatch_grants: guard.escape_hatch_grants,
        }
    }
}

/// Render a duration in seconds as a compact `HhMMmSSs`-ish human string —
/// e.g. `65` → `1m05s`, `3725` → `1h02m`, `42` → `42s` (Issue #5715). Used
/// only in starvation log lines, where naming the elapsed hold duration is
/// the whole point (a bare tick/second count reads as noise next to a
/// multi-hour outage).
fn format_hms(total_secs: i64) -> String {
    let total_secs = total_secs.max(0);
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// The role-agent attribution clause appended to the starvation messages
/// (Issue #6102). Empty when no role agent is in flight — the pre-#6102
/// wording is then already accurate.
///
/// # Why the messages needed this
///
/// The brake's guarantee is narrower than its own text implied. It samples
/// **host-wide** load, so a role agent's CPU absolutely pushes it over the hold
/// threshold — but the only thing it can withhold is a NEW *sweep* admission.
/// It has no authority over role-runner agents (they are admitted by
/// [`crate::role_runner`]'s own loops, bounded since #6102 by
/// `autonomous.roleRunner.maxConcurrent`, never by this brake or by
/// `autonomous.workFinder.maxConcurrent`), and it can shed nothing already
/// running. So "held, 0 sweeps in flight" does **not** imply an idle host, and
/// saying "nothing is running to relieve it" was wrong in exactly the case that
/// matters: a host loaded entirely by role ticks.
#[must_use]
fn role_agent_clause(active_role_agents: usize) -> String {
    if active_role_agents == 0 {
        return String::new();
    }
    format!(
        " — NOTE: {active_role_agents} role-runner agent(s) ARE running; the brake cannot hold \
         those back (they are admitted outside it — bound them with \
         autonomous.roleRunner.maxConcurrent) and cannot shed load already in flight, so this \
         load may be role-driven rather than idle (#6102)"
    )
}

/// Process-global brake handle. The daemon registers one at startup (alongside
/// the host breaker) so [`crate::ipc::build_daemon_status`] can read it without
/// threading an `Arc` through the IPC server. Unset (never registered) reads as
/// "not holding", i.e. zero behavior change.
static GLOBAL: OnceLock<Arc<SharedAdmissionBrake>> = OnceLock::new();

/// Register the process-global brake handle. Idempotent: only the first
/// registration wins (there is exactly one brake per daemon process).
pub fn register_global(brake: Arc<SharedAdmissionBrake>) {
    let _ = GLOBAL.set(brake);
}

/// The process-global brake handle, if one has been registered.
#[must_use]
pub fn global() -> Option<Arc<SharedAdmissionBrake>> {
    GLOBAL.get().cloned()
}

/// Whether the process-global brake is currently holding new admissions.
/// `false` when none is registered (zero behavior change).
#[must_use]
pub fn global_is_holding() -> bool {
    GLOBAL.get().is_some_and(|b| b.is_holding())
}

/// Fold one load reading into the process-global brake and return whether new
/// admissions are held this tick, logging any hold/release/starvation edge.
///
/// `in_flight_sweeps` is the caller's current in-flight sweep count (Issue
/// #5715) and `active_role_agents` its current role-runner agent count (Issue
/// #6102) — see [`SharedAdmissionBrake::observe`] for what each is used for
/// (only the former affects any decision).
///
/// A no-op returning `false` when no brake is registered — the work-finder loop
/// calls this unconditionally, exactly as it does
/// [`crate::host_breaker::global_is_suppressed`].
pub fn global_observe(
    loadavg_1m: Option<f64>,
    ncpu: usize,
    now: DateTime<Utc>,
    in_flight_sweeps: usize,
    active_role_agents: usize,
) -> bool {
    let Some(brake) = GLOBAL.get() else {
        return false;
    };
    let (decision, transition) =
        brake.observe(loadavg_1m, ncpu, now, in_flight_sweeps, active_role_agents);
    if let Some(t) = transition {
        match t.kind {
            TransitionKind::Engaged => {
                log::warn!("admission_brake: engaged — {}", t.reason);
            }
            TransitionKind::Released => {
                log::info!("admission_brake: released — {}", t.reason);
            }
            TransitionKind::Starving => {
                log::warn!("admission_brake: {}", t.reason);
            }
            TransitionKind::StarvationEscape => {
                log::error!("admission_brake: {}", t.reason);
            }
        }
    }
    decision.held
}

/// A snapshot of the process-global brake, or `None` when none is registered.
#[must_use]
pub fn global_snapshot() -> Option<BrakeSnapshot> {
    GLOBAL.get().map(|b| b.snapshot())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::float_cmp)]
mod tests {
    use super::*;

    fn cfg(enabled: bool, threshold: f64) -> AdmissionBrakeConfig {
        AdmissionBrakeConfig {
            enabled,
            load_per_core_threshold: threshold,
            starvation_warn_secs: DEFAULT_ADMISSION_BRAKE_STARVATION_WARN_SECS,
            starvation_escape_secs: DEFAULT_ADMISSION_BRAKE_STARVATION_ESCAPE_SECS,
        }
    }

    /// Like [`cfg`], but with explicit (usually tiny, for fast tests)
    /// starvation thresholds — Issue #5715.
    fn cfg_with_starvation(
        enabled: bool,
        threshold: f64,
        starvation_warn_secs: i64,
        starvation_escape_secs: i64,
    ) -> AdmissionBrakeConfig {
        AdmissionBrakeConfig {
            enabled,
            load_per_core_threshold: threshold,
            starvation_warn_secs,
            starvation_escape_secs,
        }
    }

    fn t0() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-08-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    // ---- pure decision ----------------------------------------------------

    #[test]
    fn saturated_host_holds_new_admissions() {
        // The reported incident: load 95 on 8 cores = 11.9/core, far over 4.0.
        let d = decide(&cfg(true, 4.0), Some(95.29), 8);
        assert!(d.held);
        assert!((d.load_per_core.unwrap() - 11.911_25).abs() < 1e-6);
        assert_eq!(d.threshold, 4.0);
    }

    #[test]
    fn threshold_is_inclusive_at_the_boundary() {
        // Exactly at the threshold holds (`is_host_saturated` uses `>=`).
        assert!(decide(&cfg(true, 4.0), Some(32.0), 8).held);
        // A hair under does not.
        assert!(!decide(&cfg(true, 4.0), Some(31.9), 8).held);
    }

    #[test]
    fn healthy_host_never_holds() {
        // A busy-but-fine 8-core host: load 6 = 0.75/core.
        assert!(!decide(&cfg(true, 4.0), Some(6.0), 8).held);
        // Even a heavy burst well above 1.0/core stays under the generous bar.
        assert!(!decide(&cfg(true, 4.0), Some(24.0), 8).held);
        // An idle host, obviously.
        assert!(!decide(&cfg(true, 4.0), Some(0.05), 8).held);
    }

    #[test]
    fn missing_load_fails_safe_to_no_hold() {
        let d = decide(&cfg(true, 4.0), None, 8);
        assert!(!d.held, "absent evidence must never read as load");
        assert_eq!(d.load_per_core, None);
        // A zero CPU count is the other missing-data shape.
        assert!(!decide(&cfg(true, 4.0), Some(95.0), 0).held);
    }

    #[test]
    fn disabled_brake_never_holds_but_still_reports_the_reading() {
        let d = decide(&cfg(false, 4.0), Some(95.29), 8);
        assert!(!d.held);
        assert!(d.load_per_core.is_some(), "status must stay honest when disabled");
    }

    #[test]
    fn default_config_holds_at_95_percent_and_resumes_below_it() {
        // #5270 AC2: the *default* config (not an explicitly-passed threshold)
        // must defer dispatch at >=95% few-minute load-per-core and resume
        // below it — this is the literal operator-directed "dumb mode" gate.
        let default_cfg = AdmissionBrakeConfig::default();
        assert_eq!(default_cfg.load_per_core_threshold, 0.95);

        // 8 cores at load 7.6 = 0.95/core exactly at the boundary -> held.
        assert!(decide(&default_cfg, Some(7.6), 8).held);
        // A hair under 95% -> not held.
        assert!(!decide(&default_cfg, Some(7.59), 8).held);
        // A comfortably busy host well under the gate -> not held.
        assert!(!decide(&default_cfg, Some(4.0), 8).held);
        // Fully idle -> not held.
        assert!(!decide(&default_cfg, Some(0.0), 8).held);
    }

    #[test]
    fn brake_default_threshold_sits_below_the_host_breaker_trip() {
        // #5270 retuned the brake default from a generous backstop (4.0) to the
        // primary "dumb mode" CPU admission gate (0.95). The ordering versus the
        // host breaker inverted on purpose: the brake now engages FIRST (a
        // single over-threshold reading), well before the breaker's higher,
        // sustained-distress trip. The ordering is load-bearing (see the module
        // docs' comparison table), so pin it.
        const {
            assert!(
                DEFAULT_ADMISSION_BRAKE_LOAD_PER_CORE
                    < crate::host_breaker::DEFAULT_HOST_BREAKER_LOAD_PER_CORE
            );
        }
        // It still sits a notch above the (unrelated) build gate's own
        // scheduling-deferral threshold, so the two never get conflated.
        const {
            assert!(
                DEFAULT_ADMISSION_BRAKE_LOAD_PER_CORE
                    > crate::cpu_headroom::DEFAULT_GATE_LOAD_THRESHOLD
            );
        }
    }

    // ---- shared handle / transitions --------------------------------------

    #[test]
    fn observe_reports_engage_and_release_edges_once_each() {
        let brake = SharedAdmissionBrake::new(cfg(true, 4.0));
        let now = t0();

        // Tick 1: saturated → engage edge.
        let (d, tr) = brake.observe(Some(95.0), 8, now, 1, 0);
        assert!(d.held);
        assert_eq!(tr.map(|t| t.kind), Some(TransitionKind::Engaged));
        assert!(brake.is_holding());

        // Tick 2: still saturated → no repeated edge (no per-tick log spam).
        let (d, tr) = brake.observe(Some(90.0), 8, now + chrono::Duration::seconds(60), 1, 0);
        assert!(d.held);
        assert!(tr.is_none());
        assert_eq!(brake.snapshot().held_ticks, 2);
        assert_eq!(brake.snapshot().held_since, Some(now));

        // Tick 3: recovered → release edge, streak state cleared.
        let (d, tr) = brake.observe(Some(4.0), 8, now + chrono::Duration::seconds(120), 1, 0);
        assert!(!d.held);
        assert_eq!(tr.map(|t| t.kind), Some(TransitionKind::Released));
        assert!(!brake.is_holding());
        let snap = brake.snapshot();
        assert_eq!(snap.held_ticks, 0);
        assert_eq!(snap.held_since, None);

        // Tick 4: still healthy → no edge.
        let (_, tr) = brake.observe(Some(3.0), 8, now + chrono::Duration::seconds(180), 1, 0);
        assert!(tr.is_none());
    }

    #[test]
    fn a_transient_dip_releases_immediately_unlike_the_breaker_cooldown() {
        // The brake is point-in-time by design: it re-checks every tick and does
        // NOT hold through a cool-down (that is the host breaker's job).
        let brake = SharedAdmissionBrake::new(cfg(true, 4.0));
        brake.observe(Some(95.0), 8, t0(), 1, 0);
        assert!(brake.is_holding());
        brake.observe(Some(1.0), 8, t0() + chrono::Duration::seconds(60), 1, 0);
        assert!(!brake.is_holding());
    }

    #[test]
    fn disabled_handle_never_holds_and_snapshot_says_so() {
        let brake = SharedAdmissionBrake::new(cfg(false, 4.0));
        let (d, tr) = brake.observe(Some(95.0), 8, t0(), 1, 0);
        assert!(!d.held);
        assert!(tr.is_none());
        assert!(!brake.is_holding());
        let snap = brake.snapshot();
        assert!(!snap.enabled);
        assert!(!snap.held);
        assert!(snap.load_per_core.is_some());
    }

    #[test]
    fn snapshot_maps_onto_the_wire_status_type() {
        let brake = SharedAdmissionBrake::new(cfg(true, 4.0));
        brake.observe(Some(95.0), 8, t0(), 1, 0);
        let status = brake.snapshot().into_status();
        assert!(status.enabled);
        assert!(status.held);
        assert_eq!(status.load_per_core_threshold, 4.0);
        assert_eq!(status.held_ticks, 1);
        assert_eq!(status.held_since, Some(t0()));
    }

    // ---- starvation escape hatch (#5715) -----------------------------------

    #[test]
    fn normal_backpressure_never_starves_however_long_it_holds() {
        // Held + sweeps genuinely in flight, for a duration that would trip
        // the escape hatch if it were starving — must never starve or escape.
        let brake = SharedAdmissionBrake::new(cfg_with_starvation(true, 4.0, 60, 120));
        let now = t0();
        for i in 0..10 {
            let (d, tr) =
                brake.observe(Some(95.0), 8, now + chrono::Duration::seconds(i * 60), 3, 0);
            assert!(d.held, "tick {i}: still held (host is genuinely saturated)");
            assert!(!d.starvation_escape, "tick {i}: never an escape — sweeps are in flight");
            assert!(
                !matches!(
                    tr.map(|t| t.kind),
                    Some(TransitionKind::Starving | TransitionKind::StarvationEscape)
                ),
                "tick {i}: healthy backpressure must never read as starvation"
            );
        }
        let snap = brake.snapshot();
        assert_eq!(snap.starving_ticks, 0);
        assert_eq!(snap.starving_since, None);
        assert_eq!(snap.escape_hatch_grants, 0);
    }

    #[test]
    fn starvation_warn_fires_once_after_the_grace_window() {
        // Held + 0 in flight, past `starvation_warn_secs` but under
        // `starvation_escape_secs` — a WARN-level Starving edge fires exactly
        // once for the streak, and the brake keeps holding (no escape yet).
        let brake = SharedAdmissionBrake::new(cfg_with_starvation(true, 0.95, 300, 900));
        let now = t0();

        // Tick 1: engages, under the warn window — plain Engaged edge.
        let (d, tr) = brake.observe(Some(20.0), 8, now, 0, 0);
        assert!(d.held);
        assert_eq!(tr.map(|t| t.kind), Some(TransitionKind::Engaged));

        // Tick 2 (t+301s): past the warn window, still under escape — Starving.
        let (d, tr) = brake.observe(Some(20.0), 8, now + chrono::Duration::seconds(301), 0, 0);
        assert!(d.held, "still held — the warn is an escalation, not a release");
        assert!(!d.starvation_escape);
        assert_eq!(tr.map(|t| t.kind), Some(TransitionKind::Starving));

        // Tick 3 (t+400s): still past warn, under escape — no repeat WARN.
        let (d, tr) = brake.observe(Some(20.0), 8, now + chrono::Duration::seconds(400), 0, 0);
        assert!(d.held);
        assert!(tr.is_none(), "the WARN line must fire once per streak, not every tick");

        let snap = brake.snapshot();
        assert!(snap.starving_ticks >= 2);
        assert_eq!(snap.escape_hatch_grants, 0);
    }

    #[test]
    fn escape_hatch_admits_one_tick_after_the_bounded_window_then_resets() {
        // The core #5715 fix: held + 0 in flight past `starvation_escape_secs`
        // yields exactly one tick despite the raw load still being over
        // threshold, then the streak resets so the next escape needs its own
        // full window (never "yield every tick forever").
        let brake = SharedAdmissionBrake::new(cfg_with_starvation(true, 0.95, 300, 900));
        let now = t0();

        brake.observe(Some(20.0), 8, now, 0, 0); // engage
        brake.observe(Some(20.0), 8, now + chrono::Duration::seconds(301), 0, 0); // warn

        // t+901s: past the escape window — the brake yields this one tick.
        let (d, tr) = brake.observe(Some(20.0), 8, now + chrono::Duration::seconds(901), 0, 0);
        assert!(!d.held, "escape hatch must admit this tick");
        assert!(d.starvation_escape, "flagged as an escape, not a real recovery");
        assert_eq!(tr.map(|t| t.kind), Some(TransitionKind::StarvationEscape));
        assert_eq!(brake.snapshot().escape_hatch_grants, 1);
        assert_eq!(brake.snapshot().starving_ticks, 0, "the streak resets after an escape");
        assert_eq!(brake.snapshot().starving_since, None);

        // Next tick: load is still high and 0 sweeps admitted by the escape
        // have started yet (an escape does not itself change load), so the
        // brake re-engages immediately — but the starvation clock restarts
        // from zero, it does not escape again immediately.
        let (d, tr) = brake.observe(Some(20.0), 8, now + chrono::Duration::seconds(961), 0, 0);
        assert!(d.held, "re-engages once load is sampled saturated again");
        assert!(!d.starvation_escape);
        assert_eq!(tr.map(|t| t.kind), Some(TransitionKind::Engaged));
    }

    #[test]
    fn escape_hatch_never_fires_while_disabled_or_healthy() {
        // Disabled brake: `decide` never holds, so starvation bookkeeping
        // never engages regardless of in-flight count or elapsed time.
        let brake = SharedAdmissionBrake::new(cfg_with_starvation(false, 0.95, 1, 1));
        let (d, tr) = brake.observe(Some(95.0), 8, t0(), 0, 0);
        assert!(!d.held);
        assert!(!d.starvation_escape);
        assert!(tr.is_none());

        // Healthy host: never holds, so never starves either.
        let brake = SharedAdmissionBrake::new(cfg_with_starvation(true, 0.95, 1, 1));
        let (d, tr) = brake.observe(Some(0.1), 8, t0(), 0, 0);
        assert!(!d.held);
        assert!(!d.starvation_escape);
        assert!(tr.is_none());
    }

    #[test]
    fn robb_studio_incident_shape_escapes_within_the_bounded_window() {
        // Reproduction of the issue's "Observed" log: load climbing
        // 1.56 -> 1.81/core (well over the 0.95 default), 0 sweeps in flight,
        // tick after tick, for far longer than a bounded escape window. The
        // fix must break the livelock inside that window rather than holding
        // forever (33h in the real incident).
        let brake = SharedAdmissionBrake::new(cfg_with_starvation(
            true,
            DEFAULT_ADMISSION_BRAKE_LOAD_PER_CORE,
            300,
            900,
        ));
        let now = t0();
        let loads = [
            1.56, 1.60, 1.64, 1.68, 1.72, 1.75, 1.78, 1.81, 1.81, 1.81, 1.81, 1.81,
        ];
        let mut escaped = false;
        for (i, load) in loads.iter().enumerate() {
            // Ticks every 90s, matching a plausible work-finder interval.
            let now = now + chrono::Duration::seconds(i as i64 * 90);
            let (d, _tr) = brake.observe(Some(*load * 28.0), 28, now, 0, 0);
            if d.starvation_escape {
                escaped = true;
                break;
            }
        }
        assert!(
            escaped,
            "0-in-flight starvation under sustained saturated load must escape within the \
             bounded window, not hold indefinitely"
        );
    }

    // ---- role-agent attribution in starvation messages (#6102) -------------

    /// #6102 AC4: with zero sweeps in flight but role agents running, "held +
    /// 0 in flight" does NOT mean an idle host — the load may be entirely
    /// role-driven, which the brake has no authority to hold back. Both
    /// starvation messages must say so instead of asserting "nothing is
    /// running to relieve it".
    #[test]
    fn starvation_messages_name_active_role_agents() {
        let brake = SharedAdmissionBrake::new(cfg_with_starvation(true, 0.95, 300, 900));
        let now = t0();
        // Engage, then cross the warn threshold with 0 sweeps and 3 role agents.
        brake.observe(Some(28.0), 8, now, 0, 3);
        let (_, tr) = brake.observe(Some(28.0), 8, now + chrono::Duration::seconds(301), 0, 3);
        let warn = tr.expect("starvation warn transition");
        assert_eq!(warn.kind, TransitionKind::Starving);
        assert!(
            warn.reason.contains("3 role-runner agent(s) ARE running"),
            "the warn must name the role agents driving the load: {}",
            warn.reason
        );
        assert!(
            warn.reason.contains("cannot hold those back"),
            "the warn must state the brake's real (narrower) authority: {}",
            warn.reason
        );
        assert!(
            !warn.reason.contains("nothing is running to relieve it"),
            "the pre-#6102 claim is false with role agents in flight: {}",
            warn.reason
        );

        // The escape-hatch message carries the same attribution.
        let (d, tr) = brake.observe(Some(28.0), 8, now + chrono::Duration::seconds(901), 0, 3);
        let escape = tr.expect("starvation escape transition");
        assert!(d.starvation_escape);
        assert!(
            escape.reason.contains("3 role-runner agent(s) ARE running"),
            "the escape must name the role agents too: {}",
            escape.reason
        );
    }

    /// With no role agents in flight, the messages are byte-for-byte their
    /// pre-#6102 selves apart from the "no sweep is running for the brake to
    /// drain" rewording — the clause is appended only when it is true.
    #[test]
    fn starvation_messages_omit_the_clause_when_no_role_agents() {
        let brake = SharedAdmissionBrake::new(cfg_with_starvation(true, 0.95, 300, 900));
        let now = t0();
        brake.observe(Some(28.0), 8, now, 0, 0);
        let (_, tr) = brake.observe(Some(28.0), 8, now + chrono::Duration::seconds(301), 0, 0);
        let warn = tr.expect("starvation warn transition");
        assert!(
            !warn.reason.contains("role-runner agent"),
            "no role agents ⇒ no attribution clause: {}",
            warn.reason
        );
    }

    /// The role-agent count is **reporting only**: it must not change any
    /// hold / release / starvation decision. Every one of those still turns on
    /// `in_flight_sweeps`, exactly as #5715 defined them — so a host loaded by
    /// role agents still escapes on schedule rather than being held forever
    /// (the livelock #5715 exists to break).
    #[test]
    fn role_agent_count_does_not_change_any_brake_decision() {
        let mk = || SharedAdmissionBrake::new(cfg_with_starvation(true, 0.95, 300, 900));
        let now = t0();
        let (with_roles, without_roles) = (mk(), mk());

        for (i, offset) in [0_i64, 301, 901, 961].iter().enumerate() {
            let at = now + chrono::Duration::seconds(*offset);
            let (a, ta) = with_roles.observe(Some(28.0), 8, at, 0, 5);
            let (b, tb) = without_roles.observe(Some(28.0), 8, at, 0, 0);
            assert_eq!(a.held, b.held, "held differs at step {i}");
            assert_eq!(a.starvation_escape, b.starvation_escape, "escape differs at step {i}");
            assert_eq!(
                ta.map(|t| t.kind),
                tb.map(|t| t.kind),
                "transition kind differs at step {i}"
            );
        }
    }

    // ---- config resolution -------------------------------------------------

    #[test]
    fn config_defaults_when_no_block_present() {
        let tmp = tempfile::tempdir().unwrap();
        let file = read_admission_brake_config(tmp.path());
        assert_eq!(file, AdmissionBrakeConfigFile::default());
        let resolved = resolve_config(&file);
        assert!(resolved.enabled);
        assert_eq!(resolved.load_per_core_threshold, DEFAULT_ADMISSION_BRAKE_LOAD_PER_CORE);
    }

    #[test]
    fn config_reads_the_work_finder_saturation_brake_block() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".loom")).unwrap();
        std::fs::write(
            tmp.path().join(".loom/config.json"),
            r#"{"autonomous":{"workFinder":{"maxConcurrent":3,
                 "saturationBrake":{"enabled":false,"loadPerCoreHold":2.75}}}}"#,
        )
        .unwrap();
        let file = read_admission_brake_config(tmp.path());
        assert_eq!(file.enabled, Some(false));
        assert_eq!(file.load_per_core_threshold, Some(2.75));
    }

    #[test]
    fn config_rejects_a_non_positive_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".loom")).unwrap();
        std::fs::write(
            tmp.path().join(".loom/config.json"),
            r#"{"autonomous":{"workFinder":{"saturationBrake":{"loadPerCoreHold":0}}}}"#,
        )
        .unwrap();
        // A zero/negative hold would freeze admission forever — treated as absent.
        assert_eq!(read_admission_brake_config(tmp.path()).load_per_core_threshold, None);
        assert_eq!(
            resolve_config(&read_admission_brake_config(tmp.path())).load_per_core_threshold,
            DEFAULT_ADMISSION_BRAKE_LOAD_PER_CORE
        );
    }

    // ---- starvation config (#5715) -----------------------------------------

    #[test]
    fn default_config_carries_the_starvation_defaults() {
        let default_cfg = AdmissionBrakeConfig::default();
        assert_eq!(default_cfg.starvation_warn_secs, DEFAULT_ADMISSION_BRAKE_STARVATION_WARN_SECS);
        assert_eq!(
            default_cfg.starvation_escape_secs,
            DEFAULT_ADMISSION_BRAKE_STARVATION_ESCAPE_SECS
        );
        // The escape window must be strictly longer than the warn window, or
        // the WARN escalation would never have a chance to fire before the
        // escape hatch already yielded.
        assert!(default_cfg.starvation_escape_secs > default_cfg.starvation_warn_secs);
    }

    #[test]
    fn config_reads_starvation_knobs_from_the_saturation_brake_block() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".loom")).unwrap();
        std::fs::write(
            tmp.path().join(".loom/config.json"),
            r#"{"autonomous":{"workFinder":{"saturationBrake":
                 {"starvationWarnSecs":120,"starvationEscapeSecs":600}}}}"#,
        )
        .unwrap();
        let file = read_admission_brake_config(tmp.path());
        assert_eq!(file.starvation_warn_secs, Some(120));
        assert_eq!(file.starvation_escape_secs, Some(600));
        let resolved = resolve_config(&file);
        assert_eq!(resolved.starvation_warn_secs, 120);
        assert_eq!(resolved.starvation_escape_secs, 600);
    }

    #[test]
    fn config_rejects_a_non_positive_starvation_window() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".loom")).unwrap();
        std::fs::write(
            tmp.path().join(".loom/config.json"),
            r#"{"autonomous":{"workFinder":{"saturationBrake":
                 {"starvationWarnSecs":0,"starvationEscapeSecs":-5}}}}"#,
        )
        .unwrap();
        // Non-positive windows would either never warn or (worse) escape on
        // every tick — both treated as absent, falling back to the default.
        let file = read_admission_brake_config(tmp.path());
        assert_eq!(file.starvation_warn_secs, None);
        assert_eq!(file.starvation_escape_secs, None);
        let resolved = resolve_config(&file);
        assert_eq!(resolved.starvation_warn_secs, DEFAULT_ADMISSION_BRAKE_STARVATION_WARN_SECS);
        assert_eq!(resolved.starvation_escape_secs, DEFAULT_ADMISSION_BRAKE_STARVATION_ESCAPE_SECS);
    }
}
