//! Opt-in daemon idle exit for remote hosts (issue #4467).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::event_bus::EventBus;
use crate::role_runner::{active_run_count, InProgressGuard};
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
    fn as_str(self) -> &'static str {
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
        let mut interval = tokio::time::interval(Duration::from_secs(15));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let mut lifecycle_activity = false;
            while events.try_recv().is_ok() {
                lifecycle_activity = true;
            }
            let in_flight = crate::ipc::count_in_flight_sweeps(&pool, &root);
            let active_roles = active_run_count(&roles);
            let (healthy_tokens, total_tokens) = token_counts(&root);
            let Some(trigger) = tracker.observe(
                Observation {
                    in_flight,
                    active_roles,
                    lifecycle_activity,
                    healthy_tokens,
                },
                Instant::now(),
            ) else {
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
}
