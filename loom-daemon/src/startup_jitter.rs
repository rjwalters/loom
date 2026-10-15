//! Per-host post-restart jitter for the fleet-wide GitHub gathers (#12073).
//!
//! When the fleet floor rises every dispatcher self-rolls at about the same
//! moment, and each new daemon's first full gather (the work finder's step
//! and discovery listings, the stage-dwell sampler, the no-op hold
//! re-checks) used to start within seconds of boot. On 2026-10-15 nine
//! daemons restarting together drained every gateway App and stopped step
//! dispatch fleet-wide for about 13 minutes.
//!
//! This module holds one process-wide gate. It opens a deterministic,
//! per-host delay after it is first armed (at daemon start, by the first
//! loop that asks), and stays open for the rest of the process: only the
//! first run of each gathering loop waits, normal cadence resumes after.
//!
//! - The delay is a stable hash (FNV-1a) of the host id, modulo the window,
//!   so a host always lands on the same offset and the fleet's hosts spread
//!   across the window.
//! - The window is `LOOM_STARTUP_JITTER_WINDOW_SECS` (env) >
//!   `autonomous.startupJitterWindowSecs` (any config tier) >
//!   [`DEFAULT_WINDOW_SECS`]. `0` disables the delay.
//! - A loop calls [`wait_first_run`] before its first tick (arms, then
//!   sleeps until the gate opens), or arms with [`arm`] and checks
//!   [`is_open`] where it cannot sleep. [`is_open`] never arms: an unarmed
//!   gate is open, so a code path reached without a daemon start (tests,
//!   CLI) is never delayed.

use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use tokio::time::Instant;

/// Env override for the window, in seconds (`0` disables).
pub const WINDOW_ENV: &str = "LOOM_STARTUP_JITTER_WINDOW_SECS";
/// Dotted config key for the window, in seconds (`0` disables).
pub const WINDOW_KEY: &str = "autonomous.startupJitterWindowSecs";
/// Default window: about the time the fleet's buckets need to absorb nine
/// first gathers. `0` in this crate's unit tests, which spawn the loops
/// in-process and would otherwise share a closed gate for minutes.
pub const DEFAULT_WINDOW_SECS: u64 = if cfg!(test) { 0 } else { 600 };

/// This host's delay inside `window`: FNV-1a 64 of `host_id`, modulo the
/// window in milliseconds. Pure and stable across builds (no std hasher,
/// whose algorithm is not guaranteed). Always `< window`; zero for a zero
/// window.
#[must_use]
pub fn delay_for(host_id: &str, window: Duration) -> Duration {
    let window_ms = u64::try_from(window.as_millis()).unwrap_or(u64::MAX);
    if window_ms == 0 {
        return Duration::ZERO;
    }
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in host_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    Duration::from_millis(hash % window_ms)
}

/// The configured window: env > config > default. An unparsable value at a
/// tier falls through to the next one.
#[must_use]
pub fn window(root: &Path) -> Duration {
    let env = std::env::var(WINDOW_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok());
    let secs = env.unwrap_or_else(|| {
        let effective = crate::config_resolver::resolve_effective_config(root);
        crate::config_resolver::get_path(&effective, WINDOW_KEY)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(DEFAULT_WINDOW_SECS)
    });
    Duration::from_secs(secs)
}

/// A gate that opens `delay` after `armed_at` and never closes again.
#[derive(Debug, Clone, Copy)]
pub struct Gate {
    opens_at: Instant,
}

impl Gate {
    #[must_use]
    pub fn new(armed_at: Instant, delay: Duration) -> Self {
        Self {
            opens_at: armed_at + delay,
        }
    }

    /// The instant the gate opens.
    #[must_use]
    pub fn opens_at(&self) -> Instant {
        self.opens_at
    }

    /// Time left until the gate opens at `now`; zero once it has.
    #[must_use]
    pub fn remaining(&self, now: Instant) -> Duration {
        self.opens_at.saturating_duration_since(now)
    }

    /// Sleep until the gate opens (no-op once open).
    pub async fn wait(&self) {
        tokio::time::sleep_until(self.opens_at).await;
    }
}

static GATE: OnceLock<Gate> = OnceLock::new();

/// Arm the process gate from `root`'s config and this host's id. Idempotent:
/// the first call fixes the delay and the instant it counts from.
pub fn arm(root: &Path) -> Gate {
    *GATE.get_or_init(|| {
        let host = crate::host_identity::host_identity();
        let window = window(root);
        let delay = delay_for(&host, window);
        log::info!(
            "startup_jitter: first fleet-wide gathers wait {}s after start (window={}s, \
             per-host, #12073)",
            delay.as_secs(),
            window.as_secs()
        );
        Gate::new(Instant::now(), delay)
    })
}

/// Whether the first-run delay is over. An unarmed gate is open.
#[must_use]
pub fn is_open() -> bool {
    GATE.get()
        .is_none_or(|gate| gate.remaining(Instant::now()).is_zero())
}

/// Arm the gate, then sleep until it opens. Call once before a gathering
/// loop's first tick.
pub async fn wait_first_run(root: &Path, loop_name: &str) {
    let gate = arm(root);
    let left = gate.remaining(Instant::now());
    if !left.is_zero() {
        log::info!("{loop_name}: first run waits {}s (startup jitter)", left.as_secs());
        gate.wait().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_secs(600);

    #[test]
    fn delay_is_deterministic_per_host() {
        assert_eq!(delay_for("host-a", WINDOW), delay_for("host-a", WINDOW));
        assert_ne!(delay_for("host-a", WINDOW), delay_for("host-b", WINDOW));
    }

    #[test]
    fn delay_is_bounded_by_the_window() {
        for i in 0..500 {
            let host = format!("worker-{i}");
            assert!(delay_for(&host, WINDOW) < WINDOW, "{host}");
            assert!(delay_for(&host, Duration::from_secs(1)) < Duration::from_secs(1));
        }
    }

    #[test]
    fn delays_spread_across_the_window() {
        let delays: Vec<Duration> = (0..9)
            .map(|i| delay_for(&format!("loom-worker-{i}"), WINDOW))
            .collect();
        let distinct: std::collections::BTreeSet<_> = delays.iter().collect();
        assert_eq!(distinct.len(), delays.len(), "{delays:?}");
    }

    #[test]
    fn zero_window_means_no_delay() {
        assert_eq!(delay_for("host-a", Duration::ZERO), Duration::ZERO);
        let now = Instant::now();
        assert!(Gate::new(now, delay_for("host-a", Duration::ZERO))
            .remaining(now)
            .is_zero());
    }

    #[tokio::test(start_paused = true)]
    async fn only_the_first_run_is_delayed() {
        let delay = Duration::from_secs(120);
        let start = Instant::now();
        let gate = Gate::new(start, delay);
        assert_eq!(gate.remaining(start), delay);
        gate.wait().await;
        assert_eq!(Instant::now() - start, delay);
        // Every later run passes straight through.
        let after = Instant::now();
        gate.wait().await;
        gate.wait().await;
        assert_eq!(Instant::now(), after);
        assert!(gate.remaining(after + Duration::from_secs(3600)).is_zero());
    }

    #[test]
    fn gate_is_open_in_unit_tests() {
        // Unarmed, or armed with the zero test default: never closed here.
        assert!(is_open());
        assert!(Gate::new(Instant::now(), Duration::ZERO)
            .remaining(Instant::now())
            .is_zero());
    }

    #[test]
    #[serial_test::serial]
    fn window_reads_env_then_config_then_default() {
        let dir = tempfile::tempdir().unwrap();
        std::env::remove_var(WINDOW_ENV);
        assert_eq!(window(dir.path()), Duration::from_secs(DEFAULT_WINDOW_SECS));
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        std::fs::write(
            dir.path().join(".loom/config.json"),
            r#"{"autonomous":{"startupJitterWindowSecs":42}}"#,
        )
        .unwrap();
        assert_eq!(window(dir.path()), Duration::from_secs(42));
        std::env::set_var(WINDOW_ENV, "0");
        assert_eq!(window(dir.path()), Duration::ZERO);
        std::env::remove_var(WINDOW_ENV);
    }
}
