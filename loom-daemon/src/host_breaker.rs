//! Host-distress circuit breaker (Issue #4235 — item 4 of the #4231 meltdown
//! decomposition).
//!
//! # Why this exists
//!
//! The 2026-07-27→28 host meltdown (#4231) was a 6-way sweep fan-out driving
//! load to 118 on 28 cores; `WindowServer` was watchdog-killed five times and
//! `loom_daemon` itself crashed at 23:52 — a clear distress signal ~2 hours
//! before the GUI finally died. A *second* wave re-spiked the host at 01:41
//! because nothing remembered the first incident: the point-in-time admission
//! checks (CPU headroom #3978, the per-tick load-admission gate #4234) each
//! made a *fresh* decision every tick, so the moment load dipped they re-admitted
//! a full wave.
//!
//! This module is the **stateful** adaptive layer the point-in-time checks
//! lack. When the host shows *sustained* distress it **trips**, stops launching
//! new work, lets the running work drain (it never kills a running sweep), and
//! only resumes after a **cool-down** — so a transient load dip cannot
//! immediately re-admit a full wave.
//!
//! # Distinction from the admission gate (#4234)
//!
//! The headroom/ramp checks in #4234 are *point-in-time admission* decisions.
//! The breaker is *stateful*: it trips only on sustained over-threshold load,
//! stays open across ticks, and releases on a cool-down TTL. It is consulted as
//! a **second dispatch suppressor** alongside the existing main-health halt flag
//! and the ramp cap — never a parallel mechanism.
//!
//! # State machine
//!
//! ```text
//!            load-per-core ≥ threshold                load < threshold
//!            for `sustain_ticks` ticks
//!   Closed ───────────────────────────▶  Open  ───────────────────────▶ CoolDown
//!     ▲                                    ▲                                 │
//!     │ cool-down elapsed                  │ re-spike (load ≥ threshold)     │
//!     └────────────────────────────────── CoolDown ◀────────────────────────┘
//! ```
//!
//! - **Closed** — normal; new dispatch allowed. A per-tick counter tracks how
//!   many *consecutive* ticks load-per-core has been at/over the trip
//!   threshold; reaching `sustain_ticks` trips to **Open**. A single spike
//!   resets the counter, so a transient burst never trips.
//! - **Open** — tripped; new dispatch is suppressed (running work drains). Stays
//!   Open while the host is still hot; once load drops back below the threshold
//!   it moves to **CoolDown**.
//! - **CoolDown** — distress subsided but dispatch stays suppressed for
//!   `cooldown` seconds so a transient dip cannot re-admit a full wave. A
//!   re-spike over the threshold sends it straight back to **Open**; otherwise
//!   the cool-down elapses and it returns to **Closed**.
//!
//! # Testability
//!
//! The decision logic is a pure [`step`] function over an immutable
//! [`BreakerRuntime`] + an observation + a `now` timestamp, mirroring
//! [`crate::quarantine_reconciliation`]'s decide/plan split — it is unit-tested
//! without a forge, a host, or a runtime. [`SharedHostBreaker`] is the thin
//! interior-mutable runtime handle the work-finder loop updates each tick and
//! the IPC status/dispatch paths read; it delegates every decision to [`step`].
//!
//! # Default-ON rationale
//!
//! Unlike the autonomous work-finder itself (default-OFF, opt-in), the breaker
//! **defaults on** — it is a safety backstop, the same call the insta-crash
//! quarantine (#3939, `QuarantineConfig::enabled = true`) made. It only ever
//! acts under genuinely severe *sustained* load, it never kills running work,
//! and it is only ever *sampled* from inside the work-finder loop — which itself
//! only runs when an operator has opted into autonomy. So a repo that never
//! enables the work-finder sees zero behavior change, and a repo that does gets
//! the meltdown backstop for free. The whole lesson of #4231 was that nothing
//! remembered the first incident; a backstop that defaulted off would not have
//! helped.

use std::sync::{Arc, Mutex, OnceLock};

use chrono::{DateTime, Utc};

// ============================================================================
// Config surface (env > config > default, mirroring work_finder + quarantine)
// ============================================================================

/// Env var toggling the host circuit breaker. `0`/`false`/`no`/`off` disables;
/// anything else truthy (`1`/`true`/`yes`/`on`) forces on. Overrides config.
/// Defaults ON — a safety backstop, mirroring
/// [`crate::sweep_registry::QUARANTINE_ENABLE_ENV`].
pub const HOST_BREAKER_ENABLE_ENV: &str = "LOOM_HOST_BREAKER";

/// Env var overriding the load-per-core trip threshold. A `<= 0`/invalid value
/// falls through to config/default.
pub const HOST_BREAKER_LOAD_PER_CORE_ENV: &str = "LOOM_HOST_BREAKER_LOAD_PER_CORE";

/// Env var overriding the number of consecutive over-threshold ticks required
/// to trip. A zero/invalid value falls through to config/default.
pub const HOST_BREAKER_SUSTAIN_TICKS_ENV: &str = "LOOM_HOST_BREAKER_SUSTAIN_TICKS";

/// Env var overriding the cool-down window, in seconds. A zero/invalid value
/// falls through to config/default.
pub const HOST_BREAKER_COOLDOWN_SECS_ENV: &str = "LOOM_HOST_BREAKER_COOLDOWN_SECS";

/// Default: the breaker is enabled (a safety backstop, like the insta-crash
/// quarantine).
pub const DEFAULT_HOST_BREAKER_ENABLED: bool = true;

/// Default load-per-core trip threshold. Normal healthy hosts sit well below
/// `1.0` load-per-core; sustained `2.5×` oversubscription is clear distress
/// (the #4231 meltdown peaked at ~4.2 load-per-core) while leaving bursty
/// build spikes below the line. Deliberately well above the build gate's own
/// saturation-deferral ratio ([`crate::cpu_headroom::DEFAULT_GATE_LOAD_THRESHOLD`],
/// `0.9`) — the breaker is for *distress*, not routine scheduling politeness.
///
/// Since #4512 removed the CPU-headroom estimate from admission, this **measured**
/// breaker is the load safety net that makes a hand-tuned per-machine
/// `maxConcurrent` safe to raise: a mis-set knob trips it (and the machine-wide
/// build slot, [`crate::build_slot`], keeps the heavy stages serialized) instead
/// of melting the host.
pub const DEFAULT_HOST_BREAKER_LOAD_PER_CORE: f64 = 2.5;

/// Default consecutive over-threshold ticks before tripping. At the default
/// 60s work-finder interval this is ~3 minutes of *sustained* distress, so a
/// single noisy tick can never trip. Mirrors the quarantine's
/// `DEFAULT_QUARANTINE_THRESHOLD = 3` "N strikes" shape.
pub const DEFAULT_HOST_BREAKER_SUSTAIN_TICKS: u32 = 3;

/// Default cool-down window: once distress subsides, hold new dispatch for this
/// long so a transient load dip cannot re-admit a full wave (the #4231 01:41
/// re-spike). 5 minutes — long enough to outlast the load-average's own 1-minute
/// smoothing window several times over.
pub const DEFAULT_HOST_BREAKER_COOLDOWN_SECS: u64 = 300;

/// Resolved host-breaker parameters (env > config > default), captured once at
/// daemon startup and held by [`SharedHostBreaker`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HostBreakerConfig {
    /// Whether the breaker is active. When `false` the breaker is permanently
    /// Closed and never suppresses dispatch (byte-for-byte the pre-#4235 path).
    pub enabled: bool,
    /// Load-per-core at/over which a tick counts toward tripping.
    pub load_per_core_threshold: f64,
    /// Consecutive over-threshold ticks required to trip.
    pub sustain_ticks: u32,
    /// How long the cool-down holds dispatch after distress subsides.
    pub cooldown_secs: u64,
}

impl Default for HostBreakerConfig {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_HOST_BREAKER_ENABLED,
            load_per_core_threshold: DEFAULT_HOST_BREAKER_LOAD_PER_CORE,
            sustain_ticks: DEFAULT_HOST_BREAKER_SUSTAIN_TICKS,
            cooldown_secs: DEFAULT_HOST_BREAKER_COOLDOWN_SECS,
        }
    }
}

/// The raw config half read from `.loom/config.json → autonomous.hostBreaker`
/// (each field `None` when absent/malformed), before env/default resolution.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HostBreakerConfigFile {
    pub enabled: Option<bool>,
    pub load_per_core_threshold: Option<f64>,
    pub sustain_ticks: Option<u32>,
    pub cooldown_secs: Option<u64>,
}

/// Read `.loom/config.json → autonomous.hostBreaker`, soft-failing every field
/// to `None` (env/default resolution) on a missing file, malformed JSON, or a
/// missing `autonomous` / `hostBreaker` block. Mirrors the soft-fail contract of
/// [`crate::work_finder::read_work_finder_config`].
#[must_use]
pub fn read_host_breaker_config(repo_root: &std::path::Path) -> HostBreakerConfigFile {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(autonomous) = crate::config_resolver::get_path(&effective, "autonomous") else {
        return HostBreakerConfigFile::default();
    };
    let hb = autonomous.get("hostBreaker");
    HostBreakerConfigFile {
        enabled: hb
            .and_then(|h| h.get("enabled"))
            .and_then(serde_json::Value::as_bool),
        load_per_core_threshold: hb
            .and_then(|h| h.get("loadPerCoreTrip"))
            .and_then(serde_json::Value::as_f64)
            .filter(|&f| f > 0.0),
        sustain_ticks: hb
            .and_then(|h| h.get("sustainTicks"))
            .and_then(serde_json::Value::as_u64)
            .filter(|&n| n > 0)
            .and_then(|n| u32::try_from(n).ok()),
        cooldown_secs: hb
            .and_then(|h| h.get("cooldownSecs"))
            .and_then(serde_json::Value::as_u64)
            .filter(|&n| n > 0),
    }
}

/// Env override for [`HOST_BREAKER_ENABLE_ENV`] — `Some(true/false)` when set to
/// a recognized truthy/falsy value, `None` when unset (config/default decides).
fn env_enabled() -> Option<bool> {
    match std::env::var(HOST_BREAKER_ENABLE_ENV) {
        Ok(v) => {
            Some(matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        }
        Err(_) => None,
    }
}

fn env_load_per_core() -> Option<f64> {
    std::env::var(HOST_BREAKER_LOAD_PER_CORE_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|&f| f > 0.0)
}

fn env_sustain_ticks() -> Option<u32> {
    std::env::var(HOST_BREAKER_SUSTAIN_TICKS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|&n| n > 0)
}

fn env_cooldown_secs() -> Option<u64> {
    std::env::var(HOST_BREAKER_COOLDOWN_SECS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
}

/// Resolve the full [`HostBreakerConfig`] with precedence **env > config >
/// default** for every field independently.
#[must_use]
pub fn resolve_config(file: &HostBreakerConfigFile) -> HostBreakerConfig {
    HostBreakerConfig {
        enabled: env_enabled()
            .or(file.enabled)
            .unwrap_or(DEFAULT_HOST_BREAKER_ENABLED),
        load_per_core_threshold: env_load_per_core()
            .or(file.load_per_core_threshold)
            .unwrap_or(DEFAULT_HOST_BREAKER_LOAD_PER_CORE),
        sustain_ticks: env_sustain_ticks()
            .or(file.sustain_ticks)
            .unwrap_or(DEFAULT_HOST_BREAKER_SUSTAIN_TICKS),
        cooldown_secs: env_cooldown_secs()
            .or(file.cooldown_secs)
            .unwrap_or(DEFAULT_HOST_BREAKER_COOLDOWN_SECS),
    }
}

/// Convenience: read `repo_root`'s config and resolve it end-to-end.
#[must_use]
pub fn resolve_config_for(repo_root: &std::path::Path) -> HostBreakerConfig {
    resolve_config(&read_host_breaker_config(repo_root))
}

// ============================================================================
// Pure state machine
// ============================================================================

/// The three breaker phases (see the module docs for the transition diagram).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerPhase {
    /// Normal — new dispatch allowed.
    Closed,
    /// Tripped on sustained distress — new dispatch suppressed, running work
    /// drains.
    Open,
    /// Distress subsided but still holding dispatch for the cool-down window.
    CoolDown,
}

impl BreakerPhase {
    /// A stable lowercase wire/render tag.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            BreakerPhase::Closed => "closed",
            BreakerPhase::Open => "open",
            BreakerPhase::CoolDown => "cooldown",
        }
    }

    /// Whether this phase suppresses new dispatch (Open or CoolDown).
    #[must_use]
    pub fn suppresses_dispatch(self) -> bool {
        !matches!(self, BreakerPhase::Closed)
    }
}

/// The evolving breaker state. Immutable input / owned output of [`step`].
#[derive(Debug, Clone, PartialEq)]
pub struct BreakerRuntime {
    pub phase: BreakerPhase,
    /// Consecutive over-threshold ticks observed while Closed (the trip counter).
    pub consecutive_over: u32,
    /// When the breaker last tripped to Open (`None` while Closed).
    pub tripped_at: Option<DateTime<Utc>>,
    /// When a CoolDown completes and the breaker returns to Closed (`None`
    /// outside CoolDown).
    pub releases_at: Option<DateTime<Utc>>,
    /// Human-readable reason for the current non-Closed state (`None` while
    /// Closed).
    pub reason: Option<String>,
    /// The most recent load-per-core sample observed (for the status surface).
    pub last_load_per_core: Option<f64>,
}

impl Default for BreakerRuntime {
    fn default() -> Self {
        Self {
            phase: BreakerPhase::Closed,
            consecutive_over: 0,
            tripped_at: None,
            releases_at: None,
            reason: None,
            last_load_per_core: None,
        }
    }
}

/// Which edge a [`Transition`] represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionKind {
    /// Closed → Open (sustained distress reached the trip threshold).
    Tripped,
    /// Open → CoolDown (distress subsided; cool-down begins).
    CoolingDown,
    /// CoolDown → Closed (cool-down elapsed; normal dispatch resumes).
    Released,
    /// CoolDown → Open (host re-spiked during cool-down).
    ReTripped,
}

impl TransitionKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            TransitionKind::Tripped => "tripped",
            TransitionKind::CoolingDown => "cooling_down",
            TransitionKind::Released => "released",
            TransitionKind::ReTripped => "re_tripped",
        }
    }
}

/// A recorded phase transition — the payload for the `daemon.host_breaker.state`
/// event and the operator log line.
#[derive(Debug, Clone, PartialEq)]
pub struct Transition {
    pub kind: TransitionKind,
    /// The phase entered.
    pub phase: BreakerPhase,
    pub reason: String,
    pub tripped_at: Option<DateTime<Utc>>,
    pub releases_at: Option<DateTime<Utc>>,
}

/// The result of one [`step`]: the next runtime plus an optional transition
/// (present only on a phase change).
#[derive(Debug, Clone, PartialEq)]
pub struct StepResult {
    pub next: BreakerRuntime,
    pub transition: Option<Transition>,
}

/// The pure breaker state machine: fold one observation (`load_per_core` at
/// `now`) into `prev`, returning the next runtime and any phase transition.
///
/// A `load_per_core` of `None` (no load-average source available) is treated as
/// **not over threshold** — it never trips a Closed breaker, and it lets an Open
/// breaker begin cooling down rather than latching forever on missing data
/// (fail-toward-recovery, matching the CPU-headroom module's fail-open chain).
#[must_use]
pub fn step(
    prev: &BreakerRuntime,
    config: &HostBreakerConfig,
    load_per_core: Option<f64>,
    now: DateTime<Utc>,
) -> StepResult {
    // Disabled: permanently Closed, no suppression, no transitions — but still
    // record the last sample so the status surface stays honest.
    if !config.enabled {
        return StepResult {
            next: BreakerRuntime {
                last_load_per_core: load_per_core,
                ..BreakerRuntime::default()
            },
            transition: None,
        };
    }

    let over = load_per_core.is_some_and(|l| l >= config.load_per_core_threshold);
    let load_str = load_per_core.map_or_else(|| "n/a".to_string(), |l| format!("{l:.2}"));
    let mut next = prev.clone();
    next.last_load_per_core = load_per_core;

    match prev.phase {
        BreakerPhase::Closed => {
            if over {
                let count = prev.consecutive_over.saturating_add(1);
                next.consecutive_over = count;
                if count >= config.sustain_ticks {
                    let reason = format!(
                        "load-per-core {load_str} ≥ {:.2} sustained for {count} consecutive tick(s)",
                        config.load_per_core_threshold
                    );
                    next.phase = BreakerPhase::Open;
                    next.tripped_at = Some(now);
                    next.releases_at = None;
                    next.reason = Some(reason.clone());
                    return StepResult {
                        transition: Some(Transition {
                            kind: TransitionKind::Tripped,
                            phase: BreakerPhase::Open,
                            reason,
                            tripped_at: Some(now),
                            releases_at: None,
                        }),
                        next,
                    };
                }
            } else {
                next.consecutive_over = 0;
            }
            StepResult {
                next,
                transition: None,
            }
        }
        BreakerPhase::Open => {
            if over {
                // Still hot — stay Open. (The trip counter is unused in Open.)
                StepResult {
                    next,
                    transition: None,
                }
            } else {
                let releases_at = now + chrono::Duration::seconds(config.cooldown_secs as i64);
                let reason = format!(
                    "load-per-core {load_str} < {:.2}; cooling down for {}s",
                    config.load_per_core_threshold, config.cooldown_secs
                );
                next.phase = BreakerPhase::CoolDown;
                next.consecutive_over = 0;
                next.releases_at = Some(releases_at);
                next.reason = Some(reason.clone());
                StepResult {
                    transition: Some(Transition {
                        kind: TransitionKind::CoolingDown,
                        phase: BreakerPhase::CoolDown,
                        reason,
                        tripped_at: prev.tripped_at,
                        releases_at: Some(releases_at),
                    }),
                    next,
                }
            }
        }
        BreakerPhase::CoolDown => {
            if over {
                // Re-spike during cool-down — straight back to Open. This is the
                // #4231 01:41 second wave the breaker exists to stop.
                let reason = format!(
                    "host re-spiked (load-per-core {load_str} ≥ {:.2}) during cool-down",
                    config.load_per_core_threshold
                );
                next.phase = BreakerPhase::Open;
                next.consecutive_over = 0;
                next.tripped_at = Some(now);
                next.releases_at = None;
                next.reason = Some(reason.clone());
                StepResult {
                    transition: Some(Transition {
                        kind: TransitionKind::ReTripped,
                        phase: BreakerPhase::Open,
                        reason,
                        tripped_at: Some(now),
                        releases_at: None,
                    }),
                    next,
                }
            } else if prev.releases_at.is_none_or(|r| now >= r) {
                // Cool-down elapsed (or, defensively, no deadline recorded) —
                // resume normal dispatch.
                let reason = "cool-down elapsed; dispatch resumed".to_string();
                next.phase = BreakerPhase::Closed;
                next.consecutive_over = 0;
                next.tripped_at = None;
                next.releases_at = None;
                next.reason = None;
                StepResult {
                    transition: Some(Transition {
                        kind: TransitionKind::Released,
                        phase: BreakerPhase::Closed,
                        reason,
                        tripped_at: None,
                        releases_at: None,
                    }),
                    next,
                }
            } else {
                // Still cooling.
                StepResult {
                    next,
                    transition: None,
                }
            }
        }
    }
}

// ============================================================================
// A serializable snapshot for the status surface
// ============================================================================

/// A point-in-time snapshot of the breaker for `loom-daemon status` and the
/// transition event payload. Maps to [`crate::types::HostBreakerStatus`] via
/// [`Self::into_status`].
#[derive(Debug, Clone, PartialEq)]
pub struct BreakerSnapshot {
    pub enabled: bool,
    pub phase: BreakerPhase,
    pub suppressed: bool,
    pub reason: Option<String>,
    pub tripped_at: Option<DateTime<Utc>>,
    pub releases_at: Option<DateTime<Utc>>,
    pub last_load_per_core: Option<f64>,
    pub load_per_core_threshold: f64,
    pub sustain_ticks: u32,
    pub cooldown_secs: u64,
    pub consecutive_over: u32,
}

impl BreakerSnapshot {
    /// Convert to the wire/status type in [`crate::types`].
    #[must_use]
    pub fn into_status(self) -> crate::types::HostBreakerStatus {
        crate::types::HostBreakerStatus {
            enabled: self.enabled,
            phase: self.phase.as_str().to_string(),
            suppressed: self.suppressed,
            reason: self.reason,
            tripped_at: self.tripped_at,
            releases_at: self.releases_at,
            last_load_per_core: self.last_load_per_core,
            load_per_core_threshold: self.load_per_core_threshold,
            sustain_ticks: self.sustain_ticks,
            cooldown_secs: self.cooldown_secs,
        }
    }
}

// ============================================================================
// Shared runtime handle + process-global registration
// ============================================================================

/// Thread-safe breaker handle: the work-finder loop [`observe`](Self::observe)s
/// a fresh load sample into it each tick; the IPC status/dispatch paths read
/// [`is_suppressed`](Self::is_suppressed) / [`snapshot`](Self::snapshot). All
/// decisions delegate to the pure [`step`].
#[derive(Debug)]
pub struct SharedHostBreaker {
    config: HostBreakerConfig,
    inner: Mutex<BreakerRuntime>,
}

// Allow expect_used: a poisoned breaker mutex means another thread panicked
// while holding it — unrecoverable, matching the crash-on-poison policy used
// across ipc.rs / auto_update.rs / the drain state.
#[allow(clippy::expect_used)]
impl SharedHostBreaker {
    #[must_use]
    pub fn new(config: HostBreakerConfig) -> Self {
        Self {
            config,
            inner: Mutex::new(BreakerRuntime::default()),
        }
    }

    #[must_use]
    pub fn config(&self) -> HostBreakerConfig {
        self.config
    }

    /// Fold one load-per-core sample into the breaker, returning any phase
    /// transition (the caller emits the log line + event on `Some`).
    pub fn observe(&self, load_per_core: Option<f64>, now: DateTime<Utc>) -> Option<Transition> {
        let mut guard = self.inner.lock().expect("host breaker mutex poisoned");
        let result = step(&guard, &self.config, load_per_core, now);
        *guard = result.next;
        result.transition
    }

    /// Whether the breaker is currently suppressing new dispatch.
    #[must_use]
    pub fn is_suppressed(&self) -> bool {
        if !self.config.enabled {
            return false;
        }
        let guard = self.inner.lock().expect("host breaker mutex poisoned");
        guard.phase.suppresses_dispatch()
    }

    /// A snapshot for the status surface.
    #[must_use]
    pub fn snapshot(&self) -> BreakerSnapshot {
        let guard = self.inner.lock().expect("host breaker mutex poisoned");
        let suppressed = self.config.enabled && guard.phase.suppresses_dispatch();
        BreakerSnapshot {
            enabled: self.config.enabled,
            phase: guard.phase,
            suppressed,
            reason: guard.reason.clone(),
            tripped_at: guard.tripped_at,
            releases_at: guard.releases_at,
            last_load_per_core: guard.last_load_per_core,
            load_per_core_threshold: self.config.load_per_core_threshold,
            sustain_ticks: self.config.sustain_ticks,
            cooldown_secs: self.config.cooldown_secs,
            consecutive_over: guard.consecutive_over,
        }
    }
}

/// Process-global breaker handle. The single work-finder loop registers its
/// handle here so [`crate::ipc::build_daemon_status`] and the `dispatch_sweep`
/// handler can read/consult it without threading an `Arc` through the whole IPC
/// server (mirrors [`crate::auto_update`]'s process-global status handle). Unset
/// (breaker never enabled / work-finder never spawned) reads as "not
/// suppressed", i.e. zero behavior change.
static GLOBAL: OnceLock<Arc<SharedHostBreaker>> = OnceLock::new();

/// Register the process-global breaker handle. Idempotent: only the first
/// registration wins (there is exactly one breaker per daemon process).
pub fn register_global(breaker: Arc<SharedHostBreaker>) {
    let _ = GLOBAL.set(breaker);
}

/// The process-global breaker handle, if one has been registered.
#[must_use]
pub fn global() -> Option<Arc<SharedHostBreaker>> {
    GLOBAL.get().cloned()
}

/// Whether the process-global breaker is currently suppressing dispatch.
/// `false` when no breaker is registered (zero behavior change).
#[must_use]
pub fn global_is_suppressed() -> bool {
    GLOBAL.get().is_some_and(|b| b.is_suppressed())
}

/// A snapshot of the process-global breaker, or `None` when none is registered.
#[must_use]
pub fn global_snapshot() -> Option<BreakerSnapshot> {
    GLOBAL.get().map(|b| b.snapshot())
}

/// Emit the operator log line + the state-change-deduped
/// `daemon.host_breaker.state` [`Generic`](crate::types::Event::Generic) event
/// for a breaker transition. Called by the work-finder loop on every `Some`
/// transition returned by [`SharedHostBreaker::observe`]; the transition itself
/// is already de-duplicated (it fires only on a real phase change), so this is
/// never a per-tick stream. Fire-and-forget: a `NoSubscribers` publish result is
/// logged at debug and ignored (matching the daemon's other publish sites).
pub fn emit_transition_event(event_bus: &Arc<crate::event_bus::EventBus>, transition: &Transition) {
    match transition.kind {
        TransitionKind::Tripped | TransitionKind::ReTripped => {
            log::warn!(
                "host_breaker: {} → {} — {}",
                transition.kind.as_str(),
                transition.phase.as_str(),
                transition.reason
            );
        }
        TransitionKind::CoolingDown | TransitionKind::Released => {
            log::info!(
                "host_breaker: {} → {} — {}",
                transition.kind.as_str(),
                transition.phase.as_str(),
                transition.reason
            );
        }
    }
    if let Err(e) = event_bus.publish_generic(
        "daemon.host_breaker.state",
        serde_json::json!({
            "transition": transition.kind.as_str(),
            "state": transition.phase.as_str(),
            "reason": transition.reason,
            "tripped_at": transition.tripped_at,
            "releases_at": transition.releases_at,
        }),
    ) {
        log::debug!("host_breaker: state event not delivered: {e}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::float_cmp)]
mod tests {
    use super::*;

    fn cfg() -> HostBreakerConfig {
        HostBreakerConfig {
            enabled: true,
            load_per_core_threshold: 2.5,
            sustain_ticks: 3,
            cooldown_secs: 300,
        }
    }

    fn t0() -> DateTime<Utc> {
        "2026-07-28T00:00:00Z".parse().unwrap()
    }

    /// Drive a sequence of samples through `step`, returning the final runtime
    /// and every transition observed.
    fn drive(
        config: &HostBreakerConfig,
        start: BreakerRuntime,
        samples: &[(Option<f64>, DateTime<Utc>)],
    ) -> (BreakerRuntime, Vec<Transition>) {
        let mut state = start;
        let mut transitions = Vec::new();
        for &(load, now) in samples {
            let result = step(&state, config, load, now);
            if let Some(t) = result.transition.clone() {
                transitions.push(t);
            }
            state = result.next;
        }
        (state, transitions)
    }

    // --- Trip only after N sustained ticks -------------------------------

    #[test]
    fn single_spike_does_not_trip() {
        let config = cfg();
        let (state, transitions) = drive(
            &config,
            BreakerRuntime::default(),
            &[
                (Some(5.0), t0()), // over, count=1
                (Some(0.1), t0()), // under, resets
                (Some(5.0), t0()), // over, count=1 again
                (Some(0.1), t0()), // under, resets
            ],
        );
        assert_eq!(state.phase, BreakerPhase::Closed);
        assert!(transitions.is_empty(), "a single spike must never trip");
        assert_eq!(state.consecutive_over, 0);
    }

    #[test]
    fn trips_after_sustain_ticks() {
        let config = cfg();
        let (state, transitions) = drive(
            &config,
            BreakerRuntime::default(),
            &[(Some(3.0), t0()), (Some(3.0), t0()), (Some(3.0), t0())],
        );
        assert_eq!(state.phase, BreakerPhase::Open);
        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].kind, TransitionKind::Tripped);
        assert!(state.tripped_at.is_some());
        assert!(state.reason.as_deref().unwrap().contains("sustained"));
    }

    #[test]
    fn does_not_trip_one_tick_early() {
        let config = cfg();
        let (state, transitions) = drive(
            &config,
            BreakerRuntime::default(),
            &[(Some(3.0), t0()), (Some(3.0), t0())], // only 2 of 3
        );
        assert_eq!(state.phase, BreakerPhase::Closed);
        assert!(transitions.is_empty());
        assert_eq!(state.consecutive_over, 2);
    }

    #[test]
    fn exactly_at_threshold_counts_as_over() {
        let config = cfg();
        let (state, _) = drive(
            &config,
            BreakerRuntime::default(),
            &[(Some(2.5), t0()), (Some(2.5), t0()), (Some(2.5), t0())],
        );
        assert_eq!(state.phase, BreakerPhase::Open);
    }

    // --- Open → CoolDown → Closed ----------------------------------------

    #[test]
    fn open_stays_open_while_hot_then_cools_down() {
        let config = cfg();
        let tripped = drive(
            &config,
            BreakerRuntime::default(),
            &[(Some(3.0), t0()), (Some(3.0), t0()), (Some(3.0), t0())],
        )
        .0;
        assert_eq!(tripped.phase, BreakerPhase::Open);

        // Still hot: stays Open, no transition.
        let (still_open, tr) = drive(&config, tripped.clone(), &[(Some(4.0), t0())]);
        assert_eq!(still_open.phase, BreakerPhase::Open);
        assert!(tr.is_empty());

        // Load drops: enters CoolDown with a releases_at.
        let (cooling, tr) = drive(&config, still_open, &[(Some(0.5), t0())]);
        assert_eq!(cooling.phase, BreakerPhase::CoolDown);
        assert_eq!(tr.len(), 1);
        assert_eq!(tr[0].kind, TransitionKind::CoolingDown);
        assert_eq!(cooling.releases_at, Some(t0() + chrono::Duration::seconds(300)));
    }

    #[test]
    fn cooldown_releases_after_window() {
        let config = cfg();
        let cooling = BreakerRuntime {
            phase: BreakerPhase::CoolDown,
            releases_at: Some(t0() + chrono::Duration::seconds(300)),
            tripped_at: Some(t0()),
            reason: Some("cooling".to_string()),
            ..BreakerRuntime::default()
        };
        // Before the window elapses: still cooling.
        let (still, tr) =
            drive(&config, cooling.clone(), &[(Some(0.1), t0() + chrono::Duration::seconds(299))]);
        assert_eq!(still.phase, BreakerPhase::CoolDown);
        assert!(tr.is_empty());

        // At/after the deadline with an acceptable load: released to Closed.
        let (released, tr) =
            drive(&config, cooling, &[(Some(0.1), t0() + chrono::Duration::seconds(300))]);
        assert_eq!(released.phase, BreakerPhase::Closed);
        assert_eq!(tr.len(), 1);
        assert_eq!(tr[0].kind, TransitionKind::Released);
        assert!(released.tripped_at.is_none());
        assert!(released.releases_at.is_none());
    }

    #[test]
    fn still_hot_host_re_trips_during_cooldown() {
        let config = cfg();
        let cooling = BreakerRuntime {
            phase: BreakerPhase::CoolDown,
            releases_at: Some(t0() + chrono::Duration::seconds(300)),
            tripped_at: Some(t0()),
            reason: Some("cooling".to_string()),
            ..BreakerRuntime::default()
        };
        // A re-spike inside the cool-down window immediately re-trips to Open.
        let (reopened, tr) =
            drive(&config, cooling, &[(Some(3.0), t0() + chrono::Duration::seconds(60))]);
        assert_eq!(reopened.phase, BreakerPhase::Open);
        assert_eq!(tr.len(), 1);
        assert_eq!(tr[0].kind, TransitionKind::ReTripped);
        assert!(reopened.releases_at.is_none());
    }

    #[test]
    fn missing_load_sample_never_trips_and_lets_open_cool() {
        let config = cfg();
        // None samples never trip a Closed breaker.
        let (state, tr) = drive(
            &config,
            BreakerRuntime::default(),
            &[(None, t0()), (None, t0()), (None, t0()), (None, t0())],
        );
        assert_eq!(state.phase, BreakerPhase::Closed);
        assert!(tr.is_empty());

        // An Open breaker with no load data begins cooling (fail-toward-recovery).
        let open = BreakerRuntime {
            phase: BreakerPhase::Open,
            tripped_at: Some(t0()),
            ..BreakerRuntime::default()
        };
        let (cooling, tr) = drive(&config, open, &[(None, t0())]);
        assert_eq!(cooling.phase, BreakerPhase::CoolDown);
        assert_eq!(tr[0].kind, TransitionKind::CoolingDown);
    }

    // --- Disabled ---------------------------------------------------------

    #[test]
    fn disabled_never_trips_or_suppresses() {
        let config = HostBreakerConfig {
            enabled: false,
            ..cfg()
        };
        let (state, tr) = drive(
            &config,
            BreakerRuntime::default(),
            &[
                (Some(99.0), t0()),
                (Some(99.0), t0()),
                (Some(99.0), t0()),
                (Some(99.0), t0()),
            ],
        );
        assert_eq!(state.phase, BreakerPhase::Closed);
        assert!(tr.is_empty());
        assert_eq!(state.last_load_per_core, Some(99.0));
    }

    // --- SharedHostBreaker handle ----------------------------------------

    #[test]
    fn shared_handle_suppresses_only_when_open_or_cooldown() {
        let breaker = SharedHostBreaker::new(cfg());
        assert!(!breaker.is_suppressed(), "fresh breaker is Closed");

        // Three sustained over-threshold observations trip it.
        assert!(breaker.observe(Some(3.0), t0()).is_none());
        assert!(breaker.observe(Some(3.0), t0()).is_none());
        let trip = breaker.observe(Some(3.0), t0());
        assert_eq!(trip.unwrap().kind, TransitionKind::Tripped);
        assert!(breaker.is_suppressed(), "Open suppresses dispatch");

        let snap = breaker.snapshot();
        assert_eq!(snap.phase, BreakerPhase::Open);
        assert!(snap.suppressed);
        assert!(snap.tripped_at.is_some());
        assert_eq!(snap.load_per_core_threshold, 2.5);

        // Drop the load: CoolDown still suppresses.
        breaker.observe(Some(0.1), t0());
        assert!(breaker.is_suppressed(), "CoolDown still suppresses dispatch");
        assert_eq!(breaker.snapshot().phase, BreakerPhase::CoolDown);
    }

    #[test]
    fn disabled_shared_handle_reports_not_suppressed() {
        let breaker = SharedHostBreaker::new(HostBreakerConfig {
            enabled: false,
            ..cfg()
        });
        for _ in 0..5 {
            breaker.observe(Some(50.0), t0());
        }
        assert!(!breaker.is_suppressed());
        assert!(!breaker.snapshot().suppressed);
        assert_eq!(breaker.snapshot().phase, BreakerPhase::Closed);
    }

    #[test]
    fn into_status_maps_fields() {
        let breaker = SharedHostBreaker::new(cfg());
        breaker.observe(Some(3.0), t0());
        breaker.observe(Some(3.0), t0());
        breaker.observe(Some(3.0), t0());
        let status = breaker.snapshot().into_status();
        assert_eq!(status.phase, "open");
        assert!(status.suppressed);
        assert!(status.enabled);
        assert_eq!(status.load_per_core_threshold, 2.5);
        assert_eq!(status.sustain_ticks, 3);
        assert_eq!(status.cooldown_secs, 300);
        assert!(status.reason.is_some());
    }

    // --- Config resolution ------------------------------------------------

    #[test]
    fn resolve_config_defaults_when_empty() {
        let resolved = resolve_config(&HostBreakerConfigFile::default());
        assert_eq!(resolved, HostBreakerConfig::default());
        assert!(resolved.enabled, "breaker defaults ON (safety backstop)");
    }

    #[test]
    fn resolve_config_uses_file_over_default() {
        let file = HostBreakerConfigFile {
            enabled: Some(false),
            load_per_core_threshold: Some(4.0),
            sustain_ticks: Some(5),
            cooldown_secs: Some(600),
        };
        let resolved = resolve_config(&file);
        assert!(!resolved.enabled);
        assert_eq!(resolved.load_per_core_threshold, 4.0);
        assert_eq!(resolved.sustain_ticks, 5);
        assert_eq!(resolved.cooldown_secs, 600);
    }
}
