//! Opt-in daemon idle exit for remote hosts (issue #4467).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::event_bus::EventBus;
use crate::role_runner::{active_run_count, role_run_start_generation, InProgressGuard};
use crate::types::Event;
use crate::workspace_pool::WorkspacePool;

pub const ENABLE_ENV: &str = "LOOM_AUTONOMOUS_IDLE_EXIT_ENABLED";
pub const MINUTES_ENV: &str = "LOOM_AUTONOMOUS_IDLE_EXIT_MINUTES";
pub const STARVATION_ENV: &str = "LOOM_AUTONOMOUS_IDLE_EXIT_ON_TOKEN_STARVATION";
pub const DEFAULT_IDLE_MINUTES: u64 = 60;
pub const MARKER_FILENAME: &str = "idle-exit.json";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdleExitConfig {
    pub enabled: Option<bool>,
    pub idle_minutes: Option<u64>,
    pub on_token_starvation: Option<bool>,
}

#[must_use]
pub fn read_config(root: &Path) -> IdleExitConfig {
    let config = crate::config_resolver::resolve_effective_config(root);
    let Some(block) = crate::config_resolver::get_path(&config, "autonomous.idleExit") else {
        return IdleExitConfig::default();
    };
    IdleExitConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        idle_minutes: block
            .get("idleMinutes")
            .and_then(serde_json::Value::as_u64)
            .filter(|value| *value > 0),
        on_token_starvation: block
            .get("onTokenStarvation")
            .and_then(serde_json::Value::as_bool),
    }
}

fn env_bool(name: &str) -> Option<bool> {
    std::env::var(name).ok().map(|value| {
        matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    })
}

#[must_use]
pub fn resolve_enabled(config: &IdleExitConfig) -> bool {
    env_bool(ENABLE_ENV).or(config.enabled).unwrap_or(false)
}

#[must_use]
pub fn resolve_minutes(config: &IdleExitConfig) -> u64 {
    std::env::var(MINUTES_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .or(config.idle_minutes)
        .unwrap_or(DEFAULT_IDLE_MINUTES)
}

#[must_use]
pub fn resolve_starvation(config: &IdleExitConfig) -> bool {
    env_bool(STARVATION_ENV)
        .or(config.on_token_starvation)
        .unwrap_or(true)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdleExitTrigger {
    Idle,
    TokenStarvation,
}

impl IdleExitTrigger {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::TokenStarvation => "token_starvation",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Observation {
    pub in_flight: usize,
    pub active_roles: usize,
    pub lifecycle_activity: bool,
    pub healthy_tokens: usize,
}

#[derive(Debug)]
pub struct IdleTracker {
    threshold: Duration,
    starvation_enabled: bool,
    idle_since: Instant,
    starved_since: Instant,
}

impl IdleTracker {
    #[must_use]
    pub fn new(threshold: Duration, starvation_enabled: bool, now: Instant) -> Self {
        Self {
            threshold,
            starvation_enabled,
            idle_since: now,
            starved_since: now,
        }
    }

    #[must_use]
    pub fn observe(&mut self, value: Observation, now: Instant) -> Option<IdleExitTrigger> {
        if value.in_flight > 0 {
            self.idle_since = now;
            self.starved_since = now;
            return None;
        }
        if value.active_roles > 0 || value.lifecycle_activity {
            self.idle_since = now;
        }
        if value.healthy_tokens > 0 || value.lifecycle_activity {
            self.starved_since = now;
        }
        if self.starvation_enabled
            && value.healthy_tokens == 0
            && now.duration_since(self.starved_since) >= self.threshold
        {
            return Some(IdleExitTrigger::TokenStarvation);
        }
        if value.active_roles == 0
            && !value.lifecycle_activity
            && now.duration_since(self.idle_since) >= self.threshold
        {
            return Some(IdleExitTrigger::Idle);
        }
        None
    }

    /// How long the ordinary-idle clock has been running as of `now` —
    /// read-only, does not mutate tracker state. Used to publish the live
    /// eligibility snapshot (#5565) alongside each [`Self::observe`] call.
    #[must_use]
    pub fn idle_elapsed(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.idle_since)
    }

    /// How long the starvation clock has been running as of `now` —
    /// read-only, mirrors [`Self::idle_elapsed`].
    #[must_use]
    pub fn starved_elapsed(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.starved_since)
    }

    /// The configured idle threshold both clocks above are measured against.
    #[must_use]
    pub fn threshold(&self) -> Duration {
        self.threshold
    }

    /// Whether the token-starvation trigger is enabled for this tracker.
    #[must_use]
    pub fn starvation_enabled(&self) -> bool {
        self.starvation_enabled
    }
}

// ============================================================================
// Status snapshot (published to the process-global, read by
// `crate::ipc::build_daemon_status`) — issue #5565
// ============================================================================

/// The publicly-observable idle-exit determination for `loom-daemon status`
/// (#5565). Lets the fleet cron idle-shutdown guard
/// (`render_idle_shutdown()` in `fleet::add_worker`) ask the RUNNING daemon
/// "would you exit for idleness right now" instead of vetoing on bare
/// `loom-daemon` process presence — which, under the fleet's own
/// `Restart=on-success` systemd supervision, is essentially always true and
/// made `--idle-shutdown-minutes` a no-op.
///
/// Mirrors exactly the same 0-in-flight / no-active-role / no-lifecycle-
/// activity-within-the-window (or token-starvation) determination
/// [`IdleTracker::observe`] uses — this is a read-only snapshot of that same
/// tracker's state, not a second, independently-computed veto.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IdleExitStatusSnapshot {
    /// Whether `autonomous.idleExit` is enabled (the tracker task was
    /// spawned) for THIS running daemon process. `false` when the task was
    /// never spawned — the guard must treat that as "cannot determine
    /// eligibility here", not as "eligible".
    pub enabled: bool,
    /// Whether the tracker's own determination would fire an idle-exit right
    /// now — i.e. the most recent [`IdleTracker::observe`] call returned
    /// `Some(_)`. Always `false` while `enabled` is `false`.
    pub eligible: bool,
    /// Which trigger would fire (`Idle` or `TokenStarvation`); `None` while
    /// not eligible.
    pub trigger: Option<IdleExitTrigger>,
    /// The configured idle window, in minutes.
    pub idle_minutes: u64,
    /// The most recently observed in-flight sweep count.
    pub in_flight_sweeps: usize,
    /// The most recently observed active role-run count.
    pub active_role_runs: usize,
    /// The most recently observed healthy-account count.
    pub healthy_tokens: usize,
    /// The most recently observed total-account count.
    pub total_tokens: usize,
    /// Seconds the ordinary-idle clock has been running uninterrupted.
    pub idle_elapsed_secs: u64,
    /// Seconds the starvation clock has been running uninterrupted.
    pub starved_elapsed_secs: u64,
    /// Whether the token-starvation trigger is enabled for this tracker.
    pub starvation_enabled: bool,
    /// Wall-clock time of the tick that produced this snapshot; `None`
    /// before the tracker's first tick.
    pub observed_at: Option<DateTime<Utc>>,
}

/// Thread-safe handle the idle-exit task publishes a fresh
/// [`IdleExitStatusSnapshot`] to on every tick; `crate::ipc::build_daemon_status`
/// reads it back via [`global_status_snapshot`]. Mirrors
/// [`crate::auto_update::AutoUpdateStatus`]'s process-global pattern.
#[derive(Debug, Default)]
pub struct IdleExitStatus {
    inner: Mutex<IdleExitStatusSnapshot>,
}

// Allow expect_used-equivalent recovery: a poisoned status mutex means
// another thread panicked while holding it. Recovering (rather than
// panicking again) matches ipc.rs's registry-lock recovery policy (#4279) —
// `status` must stay answerable even after an unrelated fault.
impl IdleExitStatus {
    #[must_use]
    pub fn new(idle_minutes: u64, starvation_enabled: bool) -> Self {
        Self {
            inner: Mutex::new(IdleExitStatusSnapshot {
                enabled: true,
                idle_minutes,
                starvation_enabled,
                ..IdleExitStatusSnapshot::default()
            }),
        }
    }

    /// A snapshot of the current status for rendering.
    #[must_use]
    pub fn snapshot(&self) -> IdleExitStatusSnapshot {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Overwrite the published snapshot.
    fn publish(&self, snap: IdleExitStatusSnapshot) {
        *self.inner.lock().unwrap_or_else(PoisonError::into_inner) = snap;
    }
}

/// Process-global status handle. The single spawned task registers its
/// handle here so [`crate::ipc::build_daemon_status`] can read the live
/// eligibility determination without threading an `Arc` through the whole
/// IPC server (mirrors [`crate::auto_update::register_global_status`]).
/// Unset (task never spawned — `autonomous.idleExit` disabled) reads as the
/// default `enabled: false, eligible: false` snapshot via
/// [`global_status_snapshot`].
static GLOBAL_STATUS: OnceLock<Arc<IdleExitStatus>> = OnceLock::new();

/// Register the task's status handle as the process-global. Idempotent: only
/// the first registration wins (there is exactly one idle-exit task per
/// process).
pub fn register_global_status(status: Arc<IdleExitStatus>) {
    let _ = GLOBAL_STATUS.set(status);
}

/// The process-global idle-exit status snapshot, or the default
/// (`enabled: false, eligible: false`) when the task was never spawned.
#[must_use]
pub fn global_status_snapshot() -> IdleExitStatusSnapshot {
    GLOBAL_STATUS
        .get()
        .map_or_else(IdleExitStatusSnapshot::default, |s| s.snapshot())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IdleExitMarker {
    pub exited_at: DateTime<Utc>,
    pub trigger: IdleExitTrigger,
    pub idle_minutes: u64,
    pub in_flight_sweeps: usize,
    pub active_role_runs: usize,
    pub healthy_tokens: usize,
    pub total_tokens: usize,
}

#[must_use]
pub fn marker_path(socket: &Path) -> PathBuf {
    socket
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(MARKER_FILENAME)
}

pub fn write_marker(path: &Path, marker: &IdleExitMarker) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "marker has no parent".to_string())?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temporary = path.with_extension(format!("json.tmp-{}", std::process::id()));
    fs::write(
        &temporary,
        serde_json::to_vec_pretty(marker).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    fs::rename(temporary, path).map_err(|error| error.to_string())
}

fn token_counts(root: &Path) -> (usize, usize) {
    crate::capacity::read_ranking(root).map_or_else(
        || {
            let total = crate::tokens::token_pool_size(root);
            (total, total)
        },
        |ranking| (ranking.available, ranking.total),
    )
}

pub fn spawn_task(
    config: IdleExitConfig,
    root: PathBuf,
    pool: Arc<WorkspacePool>,
    roles: InProgressGuard,
    bus: Arc<EventBus>,
    socket: PathBuf,
) -> tokio::task::JoinHandle<()> {
    let minutes = resolve_minutes(&config);
    let starvation = resolve_starvation(&config);
    // #5565: register the process-global status handle unconditionally at
    // spawn time (before the first tick) so `loom-daemon status` reports
    // `enabled: true` — with `eligible: false` until the first tick lands —
    // from the moment the task exists, rather than only after its first
    // 15s-interval observation.
    let status = Arc::new(IdleExitStatus::new(minutes, starvation));
    register_global_status(status.clone());
    tokio::spawn(async move {
        let marker_path = marker_path(&socket);
        if let Err(error) = fs::remove_file(&marker_path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                log::warn!("idle_exit: stale-marker cleanup failed: {error}");
            }
        }
        let mut tracker = IdleTracker::new(
            Duration::from_secs(minutes.saturating_mul(60)),
            starvation,
            Instant::now(),
        );
        let mut events = bus.subscribe(["sweep.global.dispatch", "sweep.global.completed"]);
        let mut role_generation = role_run_start_generation();
        let mut interval = tokio::time::interval(Duration::from_secs(15));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let mut lifecycle_activity = false;
            while events.try_recv().is_ok() {
                lifecycle_activity = true;
            }
            let current_role_generation = role_run_start_generation();
            if current_role_generation != role_generation {
                lifecycle_activity = true;
                role_generation = current_role_generation;
            }
            let in_flight = crate::ipc::count_in_flight_sweeps(&pool, &root);
            let active_roles = active_run_count(&roles);
            let (healthy_tokens, total_tokens) = token_counts(&root);
            let now = Instant::now();
            let trigger_opt = tracker.observe(
                Observation {
                    in_flight,
                    active_roles,
                    lifecycle_activity,
                    healthy_tokens,
                },
                now,
            );
            // #5565: publish the live eligibility determination EVERY tick
            // (not only on a fired trigger) so `loom-daemon status` always
            // reflects the tracker's current state, letting the fleet cron
            // guard ask "are you eligible right now" instead of vetoing on
            // bare process presence.
            status.publish(IdleExitStatusSnapshot {
                enabled: true,
                eligible: trigger_opt.is_some(),
                trigger: trigger_opt,
                idle_minutes: minutes,
                in_flight_sweeps: in_flight,
                active_role_runs: active_roles,
                healthy_tokens,
                total_tokens,
                idle_elapsed_secs: tracker.idle_elapsed(now).as_secs(),
                starved_elapsed_secs: tracker.starved_elapsed(now).as_secs(),
                starvation_enabled: starvation,
                observed_at: Some(Utc::now()),
            });
            let Some(trigger) = trigger_opt else {
                continue;
            };
            let marker = IdleExitMarker {
                exited_at: Utc::now(),
                trigger,
                idle_minutes: minutes,
                in_flight_sweeps: in_flight,
                active_role_runs: active_roles,
                healthy_tokens,
                total_tokens,
            };
            if let Err(error) = write_marker(&marker_path, &marker) {
                log::error!("idle_exit: marker write failed; refusing exit: {error}");
                continue;
            }
            let message = format!(
                "idle for {minutes}m ({}) — exiting for host idle-shutdown",
                trigger.as_str()
            );
            let _ = bus.publish(Event::DaemonIdleExit {
                trigger: trigger.as_str().to_string(),
                idle_minutes: minutes,
                in_flight_sweeps: in_flight,
                active_role_runs: active_roles,
                healthy_tokens,
                total_tokens,
                message: message.clone(),
            });
            log::info!("idle_exit: {message}");
            // Give asynchronous narration sinks a bounded chance to forward the
            // already-published fleet announcement before process termination.
            tokio::time::sleep(Duration::from_millis(250)).await;
            let _ = tokio::fs::remove_file(&socket).await;
            std::process::exit(0);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn idle() -> Observation {
        Observation {
            in_flight: 0,
            active_roles: 0,
            lifecycle_activity: false,
            healthy_tokens: 1,
        }
    }

    #[test]
    fn accrues_idle_and_resets_on_activity() {
        let start = Instant::now();
        let mut tracker = IdleTracker::new(Duration::from_secs(60), false, start);
        assert_eq!(tracker.observe(idle(), start + Duration::from_secs(59)), None);
        let mut active = idle();
        active.lifecycle_activity = true;
        assert_eq!(tracker.observe(active, start + Duration::from_secs(59)), None);
        assert_eq!(
            tracker.observe(idle(), start + Duration::from_secs(119)),
            Some(IdleExitTrigger::Idle)
        );
        let mut running = idle();
        running.in_flight = 1;
        assert_eq!(tracker.observe(running, start + Duration::from_secs(120)), None);
    }

    #[test]
    fn active_role_resets_ordinary_idle_clock() {
        let start = Instant::now();
        let mut tracker = IdleTracker::new(Duration::from_secs(60), false, start);
        let mut role = idle();
        role.active_roles = 1;
        assert_eq!(tracker.observe(role, start + Duration::from_secs(59)), None);
        assert_eq!(tracker.observe(idle(), start + Duration::from_secs(118)), None);
        assert_eq!(
            tracker.observe(idle(), start + Duration::from_secs(119)),
            Some(IdleExitTrigger::Idle)
        );
    }

    #[test]
    fn role_start_generation_captures_short_completed_run() {
        let before = role_run_start_generation();
        let roles = crate::role_runner::new_in_progress_guard();
        {
            let _run = crate::role_runner::RoleRunGuard::try_acquire(
                roles,
                PathBuf::from("/tmp/idle-exit-role-test"),
                "champion",
            )
            .unwrap();
        }
        assert!(role_run_start_generation() > before);
    }

    #[test]
    fn starvation_ignores_roles_but_not_sweeps_or_recovery() {
        let start = Instant::now();
        let mut tracker = IdleTracker::new(Duration::from_secs(60), true, start);
        let starved = Observation {
            active_roles: 1,
            healthy_tokens: 0,
            ..idle()
        };
        assert_eq!(
            tracker.observe(starved, start + Duration::from_secs(60)),
            Some(IdleExitTrigger::TokenStarvation)
        );
        assert_eq!(
            tracker.observe(
                Observation {
                    in_flight: 1,
                    ..starved
                },
                start + Duration::from_secs(61)
            ),
            None
        );
        assert_eq!(
            tracker.observe(
                Observation {
                    healthy_tokens: 1,
                    ..starved
                },
                start + Duration::from_secs(121)
            ),
            None
        );
    }

    #[test]
    fn marker_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(MARKER_FILENAME);
        let marker = IdleExitMarker {
            exited_at: Utc::now(),
            trigger: IdleExitTrigger::Idle,
            idle_minutes: 60,
            in_flight_sweeps: 0,
            active_role_runs: 0,
            healthy_tokens: 1,
            total_tokens: 2,
        };
        write_marker(&path, &marker).unwrap();
        let parsed: IdleExitMarker = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(parsed, marker);
    }

    #[test]
    fn absent_config_is_disabled_with_documented_defaults() {
        let config = IdleExitConfig::default();
        assert_eq!(config.enabled, None);
        assert_eq!(config.idle_minutes.unwrap_or(DEFAULT_IDLE_MINUTES), 60);
        assert!(config.on_token_starvation.unwrap_or(true));
    }

    // ---- status snapshot surface (#5565) -----------------------------------

    #[test]
    fn idle_elapsed_and_starved_elapsed_track_since_last_reset_not_since_construction() {
        let start = Instant::now();
        let mut tracker = IdleTracker::new(Duration::from_secs(60), true, start);
        // Freshly constructed: both clocks read zero elapsed at `start`.
        assert_eq!(tracker.idle_elapsed(start), Duration::ZERO);
        assert_eq!(tracker.starved_elapsed(start), Duration::ZERO);
        assert_eq!(tracker.threshold(), Duration::from_secs(60));
        assert!(tracker.starvation_enabled());

        // Activity at +10s resets the idle clock; observing again at +40s
        // reports 30s elapsed since the reset, not 40s since construction.
        let mut active = idle();
        active.lifecycle_activity = true;
        assert_eq!(tracker.observe(active, start + Duration::from_secs(10)), None);
        assert_eq!(tracker.observe(idle(), start + Duration::from_secs(40)), None);
        assert_eq!(tracker.idle_elapsed(start + Duration::from_secs(40)), Duration::from_secs(30));
    }

    #[test]
    fn status_handle_publish_and_snapshot_round_trip() {
        // Exercises the LOCAL handle only (never the true process-global —
        // registering that would race every other test in this binary that
        // also registers it, per `auto_update::test_global_status_defaults_when_unset`'s
        // documented workaround).
        let status = IdleExitStatus::new(45, true);
        let fresh = status.snapshot();
        assert!(fresh.enabled);
        assert!(!fresh.eligible);
        assert_eq!(fresh.idle_minutes, 45);
        assert!(fresh.starvation_enabled);
        assert_eq!(fresh.trigger, None);

        status.publish(IdleExitStatusSnapshot {
            enabled: true,
            eligible: true,
            trigger: Some(IdleExitTrigger::Idle),
            idle_minutes: 45,
            in_flight_sweeps: 0,
            active_role_runs: 0,
            healthy_tokens: 2,
            total_tokens: 2,
            idle_elapsed_secs: 2_700,
            starved_elapsed_secs: 0,
            starvation_enabled: true,
            observed_at: Some(Utc::now()),
        });
        let published = status.snapshot();
        assert!(published.eligible);
        assert_eq!(published.trigger, Some(IdleExitTrigger::Idle));
        assert_eq!(published.idle_elapsed_secs, 2_700);
    }

    #[test]
    fn status_snapshot_default_reads_disabled_and_not_eligible() {
        // The zero-behavior-change baseline a fleet cron guard must treat as
        // "cannot determine eligibility here", never as "eligible" (#5565).
        let snap = IdleExitStatusSnapshot::default();
        assert!(!snap.enabled);
        assert!(!snap.eligible);
        assert_eq!(snap.trigger, None);
    }
}
