//! Schedule-driven rolls (Issue #9132): arm a drain-and-restart at a scheduled
//! window, not on every new build.
//!
//! Before this module a drain was armed whenever a newer build appeared on disk
//! (after the settle gate). On a host that releases several times a day that
//! generates rolls faster than drains complete, pauses dispatch repeatedly, and —
//! because hosts rebuild at slightly different moments — pauses *several hosts at
//! once*. #8998 and #9010 act at the point of failure; this acts upstream.
//!
//! ## Model
//!
//! * **Opt-in.** `autonomous.autoUpdate.rollWindowSecs` (env
//!   `LOOM_AUTO_UPDATE_ROLL_WINDOW_SECS`) is the period. Absent/zero/invalid means
//!   *no window*: every behaviour below is inert and the loop is byte-for-byte what
//!   it was (settle gate, arm-on-build).
//! * **Windows** start at `offset + k * period` (UTC epoch seconds) and stay open
//!   for [`RollWindowTuning::open_for`], which is always at least two tick
//!   intervals so a tick is guaranteed to land inside every window.
//! * **Per-host offset.** `rollWindowOffsetSecs` (env
//!   `LOOM_AUTO_UPDATE_ROLL_WINDOW_OFFSET_SECS`) or, when unset, a deterministic
//!   hash of the host id modulo the period ([`derive_offset`]) — the same on every
//!   restart, spread across the period for distinct hosts.
//! * **One arm per window.** A new build outside an open window arms nothing. Once
//!   a roll has been armed in a window, nothing re-arms until the next window —
//!   however many ticks or releases pass in between. Composes with #8514 (an
//!   armed roll overtaken by a newer release before it has stopped any agent is
//!   retargeted, once, inside the same open window). Since #10831 an armed roll
//!   is a pause roll bounded by its pause budget, so there is no timed-out
//!   drain left to abandon here.
//! * **Settle is bypassed** while a window is configured: the window *is* the
//!   batching mechanism, and a commit-quiescence gate would let a busy `main`
//!   starve a host forever (the 2026-10-03 field report).
//!
//! ## What this does NOT guarantee
//!
//! Distinct offsets reduce *synchronized starts*. They do **not** guarantee that
//! two hosts' drains never overlap: a drain can last up to its own timeout, which
//! may exceed the gap between two hosts' offsets. There is no fleet coordination
//! rule here (a host does not look at what its peers are doing).
//!
//! ## Restart path
//!
//! [`select_restart_path`] is the pure platform decision. systemd always uses the
//! bounded drain. launchd uses it too unless `autoUpdate.launchdLiveReload` is set
//! (default **false**; depends on #9452). The live-reload *execution* is not wired
//! by this module: selecting it is reported (and logged), the bounded drain still
//! runs.
//!
//! Everything time-dependent takes an injected `now`; nothing here sleeps.

use super::roll_trigger::RollTrigger;
use super::TickDecision;
use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Env override for the window period, in seconds.
pub const ROLL_WINDOW_SECS_ENV: &str = "LOOM_AUTO_UPDATE_ROLL_WINDOW_SECS";
/// Env override for the per-host offset within the period, in seconds.
pub const ROLL_WINDOW_OFFSET_SECS_ENV: &str = "LOOM_AUTO_UPDATE_ROLL_WINDOW_OFFSET_SECS";
/// Env override for the launchd live-reload opt-in.
pub const LAUNCHD_LIVE_RELOAD_ENV: &str = "LOOM_AUTO_UPDATE_LAUNCHD_LIVE_RELOAD";

/// Floor on how long a window stays open, so a window is never narrower than the
/// tick cadence can observe.
const MIN_OPEN_SECS: u64 = 600;

// ============================================================================
// Config
// ============================================================================

/// The `autonomous.autoUpdate` keys this module owns. Zero/invalid values are
/// dropped to `None` (fall through to env/default), like every sibling knob.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RollWindowConfig {
    /// `rollWindowSecs`.
    pub period_secs: Option<u64>,
    /// `rollWindowOffsetSecs`.
    pub offset_secs: Option<u64>,
    /// `launchdLiveReload`.
    pub launchd_live_reload: Option<bool>,
}

impl RollWindowConfig {
    /// Read the three keys from the `autonomous.autoUpdate` JSON block.
    #[must_use]
    pub fn from_block(block: &serde_json::Value) -> Self {
        let positive = |key: &str| {
            block
                .get(key)
                .and_then(serde_json::Value::as_u64)
                .filter(|&s| s > 0)
        };
        Self {
            period_secs: positive("rollWindowSecs"),
            offset_secs: positive("rollWindowOffsetSecs"),
            launchd_live_reload: block
                .get("launchdLiveReload")
                .and_then(serde_json::Value::as_bool),
        }
    }
}

/// The resolved window knobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RollWindowTuning {
    /// Window period; `None` disables windowing entirely.
    pub period: Option<Duration>,
    /// Offset of window 0 within the period (always `< period` when enabled).
    pub offset: Duration,
    /// How long each window stays open.
    pub open_for: Duration,
    /// Whether the launchd live-reload path is opted in.
    pub launchd_live_reload: bool,
}

/// Parse a positive integer env value; zero/garbage is `None`.
fn env_positive(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
}

impl RollWindowTuning {
    /// Resolve with **env > config > default**, reading the process env and the
    /// daemon's host id.
    #[must_use]
    pub fn resolve(config: &RollWindowConfig, interval: Duration) -> Self {
        let live = std::env::var(LAUNCHD_LIVE_RELOAD_ENV)
            .ok()
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"));
        Self::resolve_with(
            config,
            env_positive(ROLL_WINDOW_SECS_ENV),
            env_positive(ROLL_WINDOW_OFFSET_SECS_ENV),
            live,
            &crate::sweep_registry::host_identity(),
            interval,
        )
    }

    /// The pure form of [`Self::resolve`]: every ambient input is a parameter.
    #[must_use]
    pub fn resolve_with(
        config: &RollWindowConfig,
        env_period: Option<u64>,
        env_offset: Option<u64>,
        env_live_reload: Option<bool>,
        host_id: &str,
        interval: Duration,
    ) -> Self {
        let launchd_live_reload = env_live_reload
            .or(config.launchd_live_reload)
            .unwrap_or(false);
        let Some(period_secs) = env_period
            .filter(|&s| s > 0)
            .or(config.period_secs)
            .filter(|&s| s > 0)
        else {
            return Self {
                launchd_live_reload,
                ..Self::default()
            };
        };
        // An explicit offset is reduced into the period so "always < period"
        // holds whatever the operator typed; unset derives from the host id.
        let offset_secs = env_offset
            .filter(|&o| o > 0)
            .or(config.offset_secs)
            .map_or_else(|| derive_offset(host_id, period_secs), |o| o % period_secs);
        let open_secs = interval
            .as_secs()
            .saturating_mul(2)
            .max(MIN_OPEN_SECS)
            .min(period_secs);
        Self {
            period: Some(Duration::from_secs(period_secs)),
            offset: Duration::from_secs(offset_secs),
            open_for: Duration::from_secs(open_secs),
            launchd_live_reload,
        }
    }

    /// One-line rendering for the startup log.
    #[must_use]
    pub fn describe(&self) -> String {
        self.period.map_or_else(
            || "rollWindow=off".to_string(),
            |p| {
                format!(
                    "rollWindow={}s, rollWindowOffset={}s, launchdLiveReload={}",
                    p.as_secs(),
                    self.offset.as_secs(),
                    self.launchd_live_reload
                )
            },
        )
    }
}

// ============================================================================
// Pure schedule arithmetic
// ============================================================================

/// A stable per-host offset in `[0, period_secs)`: FNV-1a over the host id, modulo
/// the period. FNV is used (not `DefaultHasher`) because its output is specified
/// and therefore identical across restarts, builds and Rust versions.
#[must_use]
pub fn derive_offset(host_id: &str, period_secs: u64) -> u64 {
    if period_secs == 0 {
        return 0;
    }
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in host_id.trim().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash % period_secs
}

fn to_i64(d: Duration) -> i64 {
    i64::try_from(d.as_secs()).unwrap_or(i64::MAX)
}

/// Index `k` of the latest window whose start is at or before `now`.
#[must_use]
pub fn window_index(now: DateTime<Utc>, period: Duration, offset: Duration) -> i64 {
    (now.timestamp() - to_i64(offset)).div_euclid(to_i64(period).max(1))
}

fn window_start(index: i64, period: Duration, offset: Duration) -> DateTime<Utc> {
    Utc.timestamp_opt(index * to_i64(period) + to_i64(offset), 0)
        .single()
        .unwrap_or_default()
}

/// Whether `now` falls inside an open window.
#[must_use]
pub fn window_is_open(
    now: DateTime<Utc>,
    period: Duration,
    offset: Duration,
    open_for: Duration,
) -> bool {
    let start = window_start(window_index(now, period, offset), period, offset);
    (now - start).num_seconds() < to_i64(open_for)
}

/// The first window start strictly after `now`.
#[must_use]
pub fn next_window_open(now: DateTime<Utc>, period: Duration, offset: Duration) -> DateTime<Utc> {
    window_start(window_index(now, period, offset) + 1, period, offset)
}

// ============================================================================
// Restart path
// ============================================================================

/// The supervisor family this daemon runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// Linux/systemd: the stop job reaps the unit's cgroup, so a drain is required.
    Systemd,
    /// macOS/launchd.
    Launchd,
    /// Anything else: treated as systemd (the conservative choice).
    Other,
}

impl Platform {
    /// The platform this binary was built for.
    #[must_use]
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Launchd
        } else if cfg!(target_os = "linux") {
            Self::Systemd
        } else {
            Self::Other
        }
    }
}

/// How a due roll restarts the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPath {
    /// The bounded drain (#4090/#6007).
    BoundedDrain,
    /// `restart --reload-supervisor` with no drain (launchd, opt-in only).
    LiveReload,
}

/// The live-reload path is selected **only** on launchd **and** only with the
/// explicit opt-in; everything else drains.
#[must_use]
pub fn select_restart_path(platform: Platform, launchd_live_reload: bool) -> RestartPath {
    match (platform, launchd_live_reload) {
        (Platform::Launchd, true) => RestartPath::LiveReload,
        _ => RestartPath::BoundedDrain,
    }
}

// ============================================================================
// Status
// ============================================================================

/// What `loom-daemon status` shows about the schedule. Additive on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollWindowStatus {
    /// Window period in seconds.
    pub period_secs: u64,
    /// This host's offset within the period, in seconds.
    pub offset_secs: u64,
    /// When the next window opens (UTC).
    pub next_window_open: DateTime<Utc>,
    /// Whether a window is open right now.
    pub window_open_now: bool,
    /// The roll target: the armed roll's artifact, else the build waiting for the
    /// next window.
    pub roll_target: Option<String>,
    /// Whether an auto-update drain currently holds dispatch paused.
    pub dispatch_paused_by_update: bool,
    /// The last deferral, `"scheduled wait: …"` or `"drain timed out, waiting for
    /// next window: …"`, or `None` when nothing is being deferred.
    pub deferral: Option<String>,
    /// The restart path selected for this host (`"bounded_drain"`/`"live_reload"`).
    pub restart_path: String,
}

// ============================================================================
// Gate
// ============================================================================

/// The loop's window state: which window has already been used, and what to show.
#[derive(Debug, Default)]
pub struct WindowGate {
    tuning: RollWindowTuning,
    /// Index of the latest window a roll was armed in.
    consumed: Option<i64>,
    /// Set for the tick on which #8514 discarded a still-draining roll, so the
    /// replacement for the newer release may arm inside the same window.
    retarget: bool,
    /// The build waiting for the next window.
    waiting_target: Option<String>,
    paused_by_update: bool,
    armed_target: Option<String>,
    deferral: Option<String>,
    status: Option<RollWindowStatus>,
    /// #10713: the window index this tick's [`Self::gate`] let an arming
    /// decision through in. Promoted to `consumed` by `mark_armed` once the
    /// drain is actually armed, so the consumption is recorded (and persisted)
    /// before the roll's own restart can end the process.
    armable: Option<i64>,
}

impl WindowGate {
    /// A gate for `tuning`. Logs once when the live-reload path is selected,
    /// because its execution is not wired (see the module doc).
    #[must_use]
    pub fn new(tuning: RollWindowTuning) -> Self {
        if select_restart_path(Platform::current(), tuning.launchd_live_reload)
            == RestartPath::LiveReload
        {
            log::warn!(
                "auto_update: launchdLiveReload is set, but the live-reload restart is not wired \
                 yet (blocked on #9452 verification) — rolls still use the bounded drain"
            );
        }
        Self {
            tuning,
            ..Self::default()
        }
    }

    fn enabled(&self) -> Option<(Duration, Duration, Duration)> {
        self.tuning
            .period
            .map(|p| (p, self.tuning.offset, self.tuning.open_for))
    }

    /// Mark that #8514 discarded a still-draining roll this tick, so its
    /// replacement may arm in the same open window.
    pub fn allow_retarget(&mut self) {
        self.retarget = true;
    }

    /// Start-of-tick bookkeeping. Observes any armed roll (consuming the current
    /// window) and returns the settle window the tick should use (zero while
    /// windowed).
    pub fn begin_tick<T: RollTrigger>(
        &mut self,
        now: DateTime<Utc>,
        trigger: &T,
        settle: Duration,
    ) -> Duration {
        let Some((period, offset, _)) = self.enabled() else {
            return settle;
        };
        self.retarget = false;
        let index = window_index(now, period, offset);
        let armed = trigger.armed_roll();
        // Only a roll this loop armed: labelled with a target, not a teardown.
        let owned = armed
            .as_ref()
            .filter(|roll| !roll.then_exit && roll.target.is_some());
        self.paused_by_update = owned.is_some();
        self.armed_target = owned.and_then(|roll| roll.target.clone());
        if owned.is_some() {
            self.consumed = Some(index);
        }
        self.refresh(now);
        Duration::ZERO
    }

    /// Filter a tick's decision: an arming decision (`Rebuild`/`FetchArtifact`)
    /// passes only inside an open window that has not already been used.
    pub fn gate(&mut self, now: DateTime<Utc>, decision: TickDecision) -> TickDecision {
        let Some((period, offset, open_for)) = self.enabled() else {
            return decision;
        };
        self.armable = None;
        let target = match &decision {
            TickDecision::FetchArtifact { version, .. } => Some(version.clone()),
            TickDecision::Rebuild { .. } => Some("source rebuild".to_string()),
            _ => None,
        };
        let Some(target) = target else {
            self.waiting_target = None;
            self.refresh(now);
            return decision;
        };
        let index = window_index(now, period, offset);
        let open = window_is_open(now, period, offset, open_for);
        let spent = self.consumed == Some(index) && !self.retarget;
        if open && !spent {
            self.armable = Some(index);
            self.waiting_target = None;
            self.deferral = None;
            self.refresh(now);
            return decision;
        }
        let next = next_window_open(now, period, offset).format("%Y-%m-%dT%H:%M:%SZ");
        let reason = if open {
            format!(
                "scheduled wait: this window's roll was already armed; {target} waits for {next}"
            )
        } else {
            format!("scheduled wait: {target} waits for the roll window at {next}")
        };
        self.waiting_target = Some(target);
        self.deferral = Some(reason.clone());
        self.refresh(now);
        TickDecision::Skip(reason)
    }

    fn refresh(&mut self, now: DateTime<Utc>) {
        self.status = self
            .enabled()
            .map(|(period, offset, open_for)| RollWindowStatus {
                period_secs: period.as_secs(),
                offset_secs: offset.as_secs(),
                next_window_open: next_window_open(now, period, offset),
                window_open_now: window_is_open(now, period, offset, open_for),
                roll_target: self
                    .armed_target
                    .clone()
                    .or_else(|| self.waiting_target.clone()),
                dispatch_paused_by_update: self.paused_by_update,
                deferral: self.deferral.clone(),
                restart_path: match select_restart_path(
                    Platform::current(),
                    self.tuning.launchd_live_reload,
                ) {
                    RestartPath::BoundedDrain => "bounded_drain",
                    RestartPath::LiveReload => "live_reload",
                }
                .to_string(),
            });
    }

    /// The status to publish, or `None` when windowing is off.
    #[must_use]
    pub fn status(&self) -> Option<RollWindowStatus> {
        self.status.clone()
    }
}

// #10713: window consumption <-> the persisted `WindowConsumption`.
mod persist;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
