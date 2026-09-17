//! Knob defaults and the state-file paths a tick reads and writes.
//!
//! Every one of these falls back to its documented default on a malformed
//! value rather than erroring. That is deliberate and the shell says why: a
//! scheduled tick must never abort on a typo'd environment variable and leave
//! the host with no detector at all. A watchdog that refuses to run because a
//! knob is misspelled has the same effect as no watchdog.

use std::path::{Path, PathBuf};

use super::env;

/// The launchd label probed when neither the marker nor the environment names one.
pub const DEFAULT_LAUNCHD_LABEL: &str = "com.rjwalters.loom-daemon";
/// The systemd unit probed when neither the marker nor the environment names one.
pub const DEFAULT_SYSTEMD_UNIT: &str = "loom-daemon.service";
/// Hard wall-clock budget for the in-band IPC round-trip.
pub const DEFAULT_PROBE_TIMEOUT_SECS: u64 = 15;
/// A pure IPC round-trip with no post-reply work — deliberately NOT `status`,
/// which would also run a per-account token-pool network call after the reply.
pub const DEFAULT_PROBE_ARGS: &str = "quarantine list";
/// Consecutive failed round-trips before a divergence is CONFIRMED.
pub const DEFAULT_PROBE_FAIL_THRESHOLD: u64 = 3;
/// Ticks retained in the windowed/rate signal (#5944).
pub const DEFAULT_PROBE_WINDOW_TICKS: u64 = 6;
/// Failures within that window that trip the rate signal (#5944).
pub const DEFAULT_PROBE_WINDOW_FAIL_THRESHOLD: u64 = 3;
/// Bounded recovery attempts before the circuit breaker opens (#5391).
pub const DEFAULT_RECOVER_MAX_ATTEMPTS: u64 = 5;
/// Base of the recovery backoff, in seconds.
pub const DEFAULT_RECOVER_BACKOFF_SECS: u64 = 60;
/// Ceiling the exponential backoff saturates at, in seconds.
pub const DEFAULT_RECOVER_BACKOFF_CAP_SECS: u64 = 1800;
/// Floor under the heartbeat staleness threshold, in seconds.
pub const MIN_HEARTBEAT_STALE_SECS: u64 = 300;
/// Multiple of the declared heartbeat cadence used as the staleness threshold.
pub const HEARTBEAT_STALE_CADENCE_MULTIPLE: u64 = 5;

/// State files, each overridable so a test can point them at a tempdir.
pub struct StateFiles {
    pub probe_fail_count: PathBuf,
    pub probe_window: PathBuf,
    pub recovery: PathBuf,
    pub escalation_sentinel: PathBuf,
    pub peer_coord_sentinel: PathBuf,
    pub peer_coord_cooldown: PathBuf,
}

impl StateFiles {
    #[must_use]
    pub fn resolve(loom_dir: &Path) -> Self {
        let at = |knob: &str, leaf: &str| -> PathBuf {
            env::var(knob).map_or_else(|| loom_dir.join(leaf), PathBuf::from)
        };
        Self {
            probe_fail_count: at("LOOM_WATCHDOG_IPC_PROBE_STATE", ".watchdog-probe-fail-count"),
            probe_window: at("LOOM_WATCHDOG_IPC_PROBE_WINDOW_STATE", ".watchdog-probe-window"),
            recovery: at("LOOM_WATCHDOG_RECOVERY_STATE", ".watchdog-recovery-state"),
            escalation_sentinel: at(
                "LOOM_WATCHDOG_ESCALATION_SENTINEL",
                ".watchdog-outage-escalated",
            ),
            peer_coord_sentinel: at(
                "LOOM_WATCHDOG_PEER_COORD_SENTINEL",
                ".watchdog-peer-coord-escalated",
            ),
            peer_coord_cooldown: at(
                "LOOM_WATCHDOG_PEER_COORD_COOLDOWN_STATE",
                ".watchdog-peer-coord-cooldown",
            ),
        }
    }
}

/// The heartbeat staleness threshold: an explicit override, else a comfortable
/// multiple of the declared cadence, floored so a fast cadence cannot produce a
/// hair-trigger. A single missed write must never false-positive.
#[must_use]
pub fn stale_threshold_secs(heartbeat_interval_secs: u64) -> u64 {
    if let Some(v) = env::var("LOOM_DAEMON_HEARTBEAT_STALE_SECS")
        .filter(|s| s.bytes().all(|b| b.is_ascii_digit()) && !s.is_empty())
        .and_then(|s| s.parse::<u64>().ok())
    {
        return v;
    }
    (heartbeat_interval_secs * HEARTBEAT_STALE_CADENCE_MULTIPLE).max(MIN_HEARTBEAT_STALE_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_threshold_is_floored_so_a_fast_cadence_is_not_a_hair_trigger() {
        // 5 * 10s = 50s, which would page on one missed write. Floored to 300.
        assert_eq!(stale_threshold_secs(10), MIN_HEARTBEAT_STALE_SECS);
    }

    #[test]
    fn a_slow_cadence_scales_past_the_floor() {
        assert_eq!(stale_threshold_secs(120), 600);
    }

    #[test]
    fn the_default_cadence_lands_on_the_floor_exactly() {
        // 60s cadence * 5 = 300 == the floor.
        assert_eq!(stale_threshold_secs(60), 300);
    }
}
