//! Per-task liveness for the daemon's long-running loops (Issue #10414).
//!
//! # Why
//!
//! On 2026-10-05 two long-running loops on both AWS workers, the self-update
//! loop and the ETA fleet refresh, went silent at ~01:30Z while the daemon
//! process stayed up. Nothing reported it: a loop that has exited, or whose
//! blocking cycle never returns, looks exactly like a loop with nothing to say.
//!
//! This module makes that state visible. Each loop **beats** once per completed
//! iteration. A task is alive while its newest beat (or, before the first one,
//! its registration) is younger than its staleness window, and dead once
//! the window passes or the loop marks itself dead. The result is exported as
//! the `loom.daemon.task_alive{task}` gauge (1/0) every 60 s
//! ([`crate::observability::ops::liveness`]) and listed under
//! `Task liveness:` in `loom-daemon status`.
//!
//! # Staleness window
//!
//! `stale_after` is set per task when it registers. The default
//! ([`default_stale_after`]) is two intervals plus [`GRACE`], so one late tick
//! does not flap the gauge. A loop whose single iteration can legitimately run
//! longer (the self-update tick runs a fetch/build bounded by its own timeout)
//! registers a longer window. A loop that exits, panics past its own handler,
//! or blocks forever stops beating, and reads as dead once the window passes.
//! It never takes longer than that.
//!
//! The registry is process-global and in memory. A restart forgets it, which
//! is correct: a fresh process registers its loops again as it spawns them.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Slack added to two intervals in the default staleness window, so a tick
/// that runs a little late does not read as dead.
pub const GRACE: Duration = Duration::from_secs(60);

/// Task name of the self-update loop (`auto_update`).
pub const AUTO_UPDATE: &str = "auto_update";
/// Task name of the ETA fleet snapshot refresh.
pub const ETA_FLEET_REFRESH: &str = "eta_fleet_refresh";
/// Task name of the 5-minute ETA pass (run by the observability collector).
/// Beats only when the pass *emitted* (authority with a working delivery
/// path), not merely when the loop finished (#10898).
pub const ETA_PASS: &str = "eta_pass";
/// Task name of the Codex session-container watch (#10600): its snapshot is
/// what dispatch selection and `loom-daemon status` read.
pub const CODEX_SESSION_WATCH: &str = "codex_session_watch";
/// Prefix of the per-role role-runner loops: `role_runner.<role>`.
pub const ROLE_RUNNER_PREFIX: &str = "role_runner.";

/// Two intervals plus [`GRACE`].
#[must_use]
pub fn default_stale_after(interval: Duration) -> Duration {
    interval.saturating_mul(2).saturating_add(GRACE)
}

/// One task's liveness, as `loom-daemon status` and the gauge report it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskLivenessEntry {
    /// The task name ([`AUTO_UPDATE`], [`ETA_PASS`], `role_runner.champion`, …).
    pub task: String,
    /// Whether the task beat within its staleness window and was not marked
    /// dead.
    pub alive: bool,
    /// Wall time of the newest beat; `None` before the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_beat: Option<DateTime<Utc>>,
    /// Seconds since the newest beat, or since registration before the first.
    pub silent_secs: u64,
    /// The task's nominal iteration interval.
    pub interval_secs: u64,
    /// The staleness window: silence longer than this reads as dead.
    pub stale_after_secs: u64,
    /// Why the task marked itself dead, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dead_reason: Option<String>,
}

#[derive(Debug, Clone)]
struct Slot {
    interval: Duration,
    stale_after: Duration,
    /// Registration, or the newest beat: the staleness clock's origin.
    since: Instant,
    last_beat_wall: Option<DateTime<Utc>>,
    dead_reason: Option<String>,
}

/// The liveness registry. Process-global through [`global`]; a fresh one in
/// tests, driven with explicit instants.
#[derive(Debug, Default)]
pub struct Registry {
    slots: Mutex<BTreeMap<String, Slot>>,
}

impl Registry {
    fn with_slots<R>(&self, f: impl FnOnce(&mut BTreeMap<String, Slot>) -> R) -> R {
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut slots)
    }

    /// Register `task`, starting its staleness clock at `now`. Re-registering
    /// (a respawned loop) resets the clock and clears a dead mark.
    pub fn register(&self, task: &str, interval: Duration, stale_after: Duration, now: Instant) {
        self.with_slots(|slots| {
            slots.insert(
                task.to_string(),
                Slot {
                    interval,
                    stale_after,
                    since: now,
                    last_beat_wall: None,
                    dead_reason: None,
                },
            );
        });
    }

    /// Record one completed iteration of `task`. An unregistered task is
    /// registered with [`default_stale_after`] of `interval`.
    pub fn beat(&self, task: &str, interval: Duration, now: Instant, wall: DateTime<Utc>) {
        self.with_slots(|slots| {
            let slot = slots.entry(task.to_string()).or_insert_with(|| Slot {
                interval,
                stale_after: default_stale_after(interval),
                since: now,
                last_beat_wall: None,
                dead_reason: None,
            });
            slot.since = now;
            slot.last_beat_wall = Some(wall);
            slot.dead_reason = None;
        });
    }

    /// [`Self::beat`] for a task that registered, keeping its interval and
    /// window; a no-op for one that did not (a loop configured off).
    pub fn beat_if_registered(&self, task: &str, now: Instant, wall: DateTime<Utc>) {
        self.with_slots(|slots| {
            if let Some(slot) = slots.get_mut(task) {
                slot.since = now;
                slot.last_beat_wall = Some(wall);
                slot.dead_reason = None;
            }
        });
    }

    /// Mark `task` dead now, with `reason`: the loop is exiting for good.
    pub fn mark_dead(&self, task: &str, reason: &str) {
        self.with_slots(|slots| {
            if let Some(slot) = slots.get_mut(task) {
                slot.dead_reason = Some(reason.to_string());
            }
        });
    }

    /// Every registered task's liveness as of `now`, sorted by name.
    #[must_use]
    pub fn snapshot_at(&self, now: Instant) -> Vec<TaskLivenessEntry> {
        self.with_slots(|slots| {
            slots
                .iter()
                .map(|(task, slot)| {
                    let silent = now.saturating_duration_since(slot.since);
                    TaskLivenessEntry {
                        task: task.clone(),
                        alive: slot.dead_reason.is_none() && silent <= slot.stale_after,
                        last_beat: slot.last_beat_wall,
                        silent_secs: silent.as_secs(),
                        interval_secs: slot.interval.as_secs(),
                        stale_after_secs: slot.stale_after.as_secs(),
                        dead_reason: slot.dead_reason.clone(),
                    }
                })
                .collect()
        })
    }
}

/// The process-global registry.
#[must_use]
pub fn global() -> &'static Registry {
    static GLOBAL: OnceLock<Registry> = OnceLock::new();
    GLOBAL.get_or_init(Registry::default)
}

/// [`Registry::register`] on the global registry, now.
pub fn register(task: &str, interval: Duration, stale_after: Duration) {
    global().register(task, interval, stale_after, Instant::now());
}

/// [`Registry::beat`] on the global registry, now.
pub fn beat(task: &str, interval: Duration) {
    global().beat(task, interval, Instant::now(), Utc::now());
}

/// [`Registry::beat_if_registered`] on the global registry, now.
pub fn beat_if_registered(task: &str) {
    global().beat_if_registered(task, Instant::now(), Utc::now());
}

/// A beat from the role-runner loop for `role` (`role_runner.<role>`).
pub fn beat_role(role: &str, interval: Duration) {
    beat(&format!("{ROLE_RUNNER_PREFIX}{role}"), interval);
}

/// [`Registry::mark_dead`] on the global registry.
pub fn mark_dead(task: &str, reason: &str) {
    global().mark_dead(task, reason);
}

/// The global registry's liveness, now.
#[must_use]
pub fn snapshot() -> Vec<TaskLivenessEntry> {
    global().snapshot_at(Instant::now())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "task_liveness_tests.rs"]
mod tests;
