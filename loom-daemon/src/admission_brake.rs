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

/// Resolved brake parameters (env > config > default), captured once at daemon
/// startup and held by [`SharedAdmissionBrake`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AdmissionBrakeConfig {
    /// Whether the brake is active. When `false` it never holds and never
    /// records a hold (byte-for-byte the pre-#4903 admission path).
    pub enabled: bool,
    /// Load-per-core at/above which new admissions are held this tick.
    pub load_per_core_threshold: f64,
}

impl Default for AdmissionBrakeConfig {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_ADMISSION_BRAKE_ENABLED,
            load_per_core_threshold: DEFAULT_ADMISSION_BRAKE_LOAD_PER_CORE,
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
    /// brake is disabled or the load reading is absent.
    pub held: bool,
    /// The observed load-per-core, or `None` when no load source was available.
    pub load_per_core: Option<f64>,
    /// The threshold the decision was taken against (echoed for the log/status
    /// line so an operator never has to guess which knob was in force).
    pub threshold: f64,
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
}

/// Which edge a [`Transition`] represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionKind {
    /// Not holding → holding (the host crossed the threshold).
    Engaged,
    /// Holding → not holding (the host recovered).
    Released,
}

impl TransitionKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            TransitionKind::Engaged => "engaged",
            TransitionKind::Released => "released",
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
    /// hold/release edge (the caller logs on `Some`).
    ///
    /// `loadavg_1m` / `ncpu` are passed in — never re-probed here — so the
    /// reading that produced the decision is exactly the reading the caller
    /// already sampled for the host breaker this tick. One read, one decision:
    /// no window in which the brake and the breaker disagree about the host.
    pub fn observe(
        &self,
        loadavg_1m: Option<f64>,
        ncpu: usize,
        now: DateTime<Utc>,
    ) -> (BrakeDecision, Option<Transition>) {
        let decision = decide(&self.config, loadavg_1m, ncpu);
        let mut guard = self.inner.lock().expect("admission brake mutex poisoned");
        let was_held = guard.held;
        guard.held = decision.held;
        guard.last_load_per_core = decision.load_per_core;
        let load_str = decision
            .load_per_core
            .map_or_else(|| "n/a".to_string(), |l| format!("{l:.2}"));
        let transition = if decision.held {
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
            if was_held {
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
        }
    }
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
/// admissions are held this tick, logging any hold/release edge.
///
/// A no-op returning `false` when no brake is registered — the work-finder loop
/// calls this unconditionally, exactly as it does
/// [`crate::host_breaker::global_is_suppressed`].
pub fn global_observe(loadavg_1m: Option<f64>, ncpu: usize, now: DateTime<Utc>) -> bool {
    let Some(brake) = GLOBAL.get() else {
        return false;
    };
    let (decision, transition) = brake.observe(loadavg_1m, ncpu, now);
    if let Some(t) = transition {
        match t.kind {
            TransitionKind::Engaged => {
                log::warn!("admission_brake: engaged — {}", t.reason);
            }
            TransitionKind::Released => {
                log::info!("admission_brake: released — {}", t.reason);
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
        let (d, tr) = brake.observe(Some(95.0), 8, now);
        assert!(d.held);
        assert_eq!(tr.map(|t| t.kind), Some(TransitionKind::Engaged));
        assert!(brake.is_holding());

        // Tick 2: still saturated → no repeated edge (no per-tick log spam).
        let (d, tr) = brake.observe(Some(90.0), 8, now + chrono::Duration::seconds(60));
        assert!(d.held);
        assert!(tr.is_none());
        assert_eq!(brake.snapshot().held_ticks, 2);
        assert_eq!(brake.snapshot().held_since, Some(now));

        // Tick 3: recovered → release edge, streak state cleared.
        let (d, tr) = brake.observe(Some(4.0), 8, now + chrono::Duration::seconds(120));
        assert!(!d.held);
        assert_eq!(tr.map(|t| t.kind), Some(TransitionKind::Released));
        assert!(!brake.is_holding());
        let snap = brake.snapshot();
        assert_eq!(snap.held_ticks, 0);
        assert_eq!(snap.held_since, None);

        // Tick 4: still healthy → no edge.
        let (_, tr) = brake.observe(Some(3.0), 8, now + chrono::Duration::seconds(180));
        assert!(tr.is_none());
    }

    #[test]
    fn a_transient_dip_releases_immediately_unlike_the_breaker_cooldown() {
        // The brake is point-in-time by design: it re-checks every tick and does
        // NOT hold through a cool-down (that is the host breaker's job).
        let brake = SharedAdmissionBrake::new(cfg(true, 4.0));
        brake.observe(Some(95.0), 8, t0());
        assert!(brake.is_holding());
        brake.observe(Some(1.0), 8, t0() + chrono::Duration::seconds(60));
        assert!(!brake.is_holding());
    }

    #[test]
    fn disabled_handle_never_holds_and_snapshot_says_so() {
        let brake = SharedAdmissionBrake::new(cfg(false, 4.0));
        let (d, tr) = brake.observe(Some(95.0), 8, t0());
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
        brake.observe(Some(95.0), 8, t0());
        let status = brake.snapshot().into_status();
        assert!(status.enabled);
        assert!(status.held);
        assert_eq!(status.load_per_core_threshold, 4.0);
        assert_eq!(status.held_ticks, 1);
        assert_eq!(status.held_since, Some(t0()));
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
}
