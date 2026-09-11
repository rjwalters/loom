//! Background health monitoring for tmux server
//!
//! This module provides a background thread that periodically checks tmux server health
//! and attempts recovery when crashes are detected.
//!
//! Health monitoring runs by default every 60 seconds. You can customize the interval:
//! ```bash
//! LOOM_TMUX_HEALTH_MONITOR=30 pnpm daemon:preview  # Check every 30 seconds
//! ```
//!
//! To disable health monitoring:
//! ```bash
//! LOOM_TMUX_HEALTH_MONITOR=0 pnpm daemon:preview  # Disabled
//! ```

use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Shared state for tmux health monitoring
pub struct TmuxHealthState {
    /// Whether the tmux server was alive during the last check
    pub server_alive: AtomicBool,
    /// Number of sessions during last successful check
    pub last_session_count: AtomicU64,
    /// Number of consecutive crashes detected
    pub crash_count: AtomicU64,
}

impl Default for TmuxHealthState {
    fn default() -> Self {
        Self {
            server_alive: AtomicBool::new(true),
            last_session_count: AtomicU64::new(0),
            crash_count: AtomicU64::new(0),
        }
    }
}

/// Whether tmux's stderr text indicates a missing socket — i.e. the tmux
/// server process itself has exited, which happens automatically once its
/// last session closes. This is normal tmux behavior (the daemon simply has
/// no live sweeps right now), not a crash or a wedged server.
fn is_missing_socket_error(stderr: &str) -> bool {
    stderr.contains("No such file or directory")
}

/// Classification for the generic "tmux server not responding" branch
/// (connection errors such as a missing socket file, distinct from the
/// explicit "no server running" crash-detection path handled separately).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TmuxUnresponsiveKind {
    /// No session has ever been created since this monitor thread started —
    /// on a fresh boot the `-L loom` socket directory legitimately does not
    /// exist yet (`terminal.rs` starts the server lazily on first session
    /// creation). Benign.
    NeverStarted,
    /// The server was previously observed alive at least once this run and
    /// the socket is now missing — tmux exited its server process because
    /// the last session closed. Normal idle state, not an error.
    IdleAfterAlive,
    /// The server was previously observed alive and is failing to respond
    /// for a reason other than a missing socket (e.g. the socket exists but
    /// the server doesn't answer, a permission error, or a timeout) — a
    /// genuinely wedged server.
    Unresponsive,
}

/// Decide how to classify a generic tmux connection failure (any stderr
/// that doesn't match the explicit "no server running" or "no sessions"
/// arms) so the check loop can pick the right log level/behavior.
fn classify_tmux_unresponsive(ever_seen_alive: bool, stderr: &str) -> TmuxUnresponsiveKind {
    if !ever_seen_alive {
        TmuxUnresponsiveKind::NeverStarted
    } else if is_missing_socket_error(stderr) {
        TmuxUnresponsiveKind::IdleAfterAlive
    } else {
        TmuxUnresponsiveKind::Unresponsive
    }
}

/// Start a background thread that monitors tmux server health
///
/// # Arguments
/// * `interval_secs` - How often to check tmux server health (in seconds)
///
/// # Returns
/// A tuple of (`JoinHandle`, `Arc<TmuxHealthState>`) for monitoring and querying health status
///
/// # Example
/// ```
/// use loom_daemon::health_monitor;
///
/// // Start monitoring every 30 seconds
/// let (_monitor, health_state) = health_monitor::start_tmux_health_monitor(30);
/// ```
pub fn start_tmux_health_monitor(interval_secs: u64) -> (JoinHandle<()>, Arc<TmuxHealthState>) {
    log::info!("🏥 Starting tmux health monitor (checking every {interval_secs} seconds)");

    let health_state = Arc::new(TmuxHealthState::default());
    let health_state_clone = Arc::clone(&health_state);

    let handle = thread::spawn(move || {
        let mut had_sessions = false;
        // Whether we've ever seen the tmux server respond successfully
        // (with or without sessions) since this monitor thread started.
        // Stays false on a fresh boot until the first session is created.
        let mut ever_seen_alive = false;
        // Whether we've already logged the INFO transition into "idle after
        // being alive" (missing socket). Reset once the server is observed
        // alive again, so the next idle transition logs exactly once.
        let mut idle_logged = false;

        loop {
            thread::sleep(Duration::from_secs(interval_secs));

            let output = Command::new("tmux")
                .args(["-L", "loom", "list-sessions", "-F", "#{session_name}"])
                .output();

            match output {
                Ok(out) if out.status.success() => {
                    let stdout_str = String::from_utf8_lossy(&out.stdout);
                    let sessions: Vec<_> = stdout_str
                        .lines()
                        .filter(|s| s.starts_with("loom-"))
                        .collect();

                    let session_count = sessions.len() as u64;
                    ever_seen_alive = true;
                    idle_logged = false;
                    health_state_clone
                        .server_alive
                        .store(true, Ordering::Relaxed);
                    health_state_clone
                        .last_session_count
                        .store(session_count, Ordering::Relaxed);

                    log::info!("📊 tmux health check: {session_count} loom sessions active");

                    // Track if we've seen sessions before
                    if session_count > 0 {
                        had_sessions = true;
                    }

                    // Alert on session count anomalies
                    if sessions.is_empty() && had_sessions {
                        log::warn!(
                            "⚠️  All loom sessions disappeared - server may have crashed and restarted"
                        );
                    }
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);

                    // Distinguish failure modes
                    if stderr.contains("no server running") {
                        let was_alive = health_state_clone
                            .server_alive
                            .swap(false, Ordering::Relaxed);

                        if was_alive && had_sessions {
                            // Server crashed!
                            let crash_count = health_state_clone
                                .crash_count
                                .fetch_add(1, Ordering::Relaxed)
                                + 1;

                            log::error!(
                                "🚨 TMUX SERVER CRASHED (crash #{crash_count}) - All sessions lost!"
                            );
                            log::error!(
                                "💡 Recovery: Use the Loom UI to restart terminals, or manually run:"
                            );
                            log::error!("   1. Check for zombie processes: ps aux | grep tmux");
                            log::error!("   2. Clean up: tmux -L loom kill-server");
                            log::error!("   3. Restart Loom terminals from the UI");
                        }

                        health_state_clone
                            .last_session_count
                            .store(0, Ordering::Relaxed);
                    } else if stderr.contains("no sessions") {
                        // The server responded (just with no sessions), so it
                        // has been observed alive.
                        ever_seen_alive = true;
                        idle_logged = false;
                        health_state_clone
                            .server_alive
                            .store(true, Ordering::Relaxed);
                        health_state_clone
                            .last_session_count
                            .store(0, Ordering::Relaxed);
                        log::debug!("tmux server running but no sessions exist");
                    } else {
                        match classify_tmux_unresponsive(ever_seen_alive, &stderr) {
                            TmuxUnresponsiveKind::NeverStarted => {
                                log::warn!(
                                    "tmux server not responding (not yet started this run, likely a fresh boot with no sessions created yet): {stderr}"
                                );
                            }
                            TmuxUnresponsiveKind::IdleAfterAlive => {
                                // Normal tmux behavior: the server process
                                // exits once its last session closes. Log
                                // the transition once at INFO and stay quiet
                                // until the state changes again.
                                if !idle_logged {
                                    log::info!(
                                        "tmux server idle — 0 loom sessions (server exited after its last session closed)"
                                    );
                                    idle_logged = true;
                                }
                            }
                            TmuxUnresponsiveKind::Unresponsive => {
                                log::error!("🚨 tmux server not responding: {stderr}");
                            }
                        }
                        health_state_clone
                            .server_alive
                            .store(false, Ordering::Relaxed);
                    }
                }
                Err(e) => {
                    log::error!("Failed to check tmux health: {e}");
                    health_state_clone
                        .server_alive
                        .store(false, Ordering::Relaxed);
                }
            }
        }
    });

    (handle, health_state)
}

/// Check if health monitoring is enabled and get the interval
///
/// Returns the monitoring interval in seconds:
/// - If `LOOM_TMUX_HEALTH_MONITOR` is set to a number > 0, use that interval
/// - If `LOOM_TMUX_HEALTH_MONITOR` is set to 0, health monitoring is disabled
/// - If `LOOM_TMUX_HEALTH_MONITOR` is not set, default to 60 seconds (enabled by default)
///
/// # Returns
/// `Some(interval_secs)` if enabled, `None` if explicitly disabled
pub fn check_env_enabled() -> Option<u64> {
    if let Ok(val) = std::env::var("LOOM_TMUX_HEALTH_MONITOR") {
        match val.parse::<u64>() {
            Ok(0) => {
                // Explicitly disabled
                log::info!(
                    "tmux health monitoring explicitly disabled via LOOM_TMUX_HEALTH_MONITOR=0"
                );
                None
            }
            Ok(interval) => {
                // Custom interval
                log::info!("tmux health monitoring enabled with custom interval: {interval}s");
                Some(interval)
            }
            Err(_) => {
                // Invalid value, use default
                log::warn!("Invalid LOOM_TMUX_HEALTH_MONITOR value: '{val}', using default 60s");
                Some(60)
            }
        }
    } else {
        // Not set, use default (enabled by default)
        log::info!("tmux health monitoring enabled by default (60s interval)");
        Some(60)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::sync::atomic::Ordering;

    // ===== TmuxHealthState::default tests =====

    #[test]
    fn test_default_state_server_alive() {
        let state = TmuxHealthState::default();
        assert!(state.server_alive.load(Ordering::Relaxed));
    }

    #[test]
    fn test_default_state_session_count_zero() {
        let state = TmuxHealthState::default();
        assert_eq!(state.last_session_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_default_state_crash_count_zero() {
        let state = TmuxHealthState::default();
        assert_eq!(state.crash_count.load(Ordering::Relaxed), 0);
    }

    // ===== is_missing_socket_error tests =====

    #[test]
    fn test_is_missing_socket_error_detects_no_such_file() {
        assert!(is_missing_socket_error(
            "error connecting to /private/tmp/tmux-501/loom (No such file or directory)"
        ));
    }

    #[test]
    fn test_is_missing_socket_error_false_for_other_errors() {
        assert!(!is_missing_socket_error("permission denied"));
    }

    // ===== classify_tmux_unresponsive tests =====

    #[test]
    fn test_classify_never_started_before_any_session() {
        // Fresh boot: server has never been observed alive (e.g. the tmux
        // socket directory doesn't exist yet because no session has been
        // created this run). This is benign and should not be ERROR.
        assert_eq!(
            classify_tmux_unresponsive(
                false,
                "error connecting to /tmp/tmux-501/loom (No such file or directory)"
            ),
            TmuxUnresponsiveKind::NeverStarted
        );
    }

    #[test]
    fn test_classify_idle_after_alive_missing_socket() {
        // Server was previously observed alive and the socket is now
        // missing: tmux exits its server process once the last session
        // closes. This is normal idle behavior, not an error.
        assert_eq!(
            classify_tmux_unresponsive(
                true,
                "error connecting to /private/tmp/tmux-501/loom (No such file or directory)"
            ),
            TmuxUnresponsiveKind::IdleAfterAlive
        );
    }

    #[test]
    fn test_classify_unresponsive_when_alive_and_not_missing_socket() {
        // Server was previously observed alive and is failing to respond
        // for a reason other than a missing socket (e.g. a stale socket
        // that exists but doesn't answer). This remains ERROR.
        assert_eq!(
            classify_tmux_unresponsive(true, "permission denied"),
            TmuxUnresponsiveKind::Unresponsive
        );
    }

    // ===== check_env_enabled tests =====

    #[test]
    #[serial]
    fn test_check_env_enabled_unset_returns_default() {
        std::env::remove_var("LOOM_TMUX_HEALTH_MONITOR");
        assert_eq!(check_env_enabled(), Some(60));
    }

    #[test]
    #[serial]
    fn test_check_env_enabled_zero_disables() {
        std::env::set_var("LOOM_TMUX_HEALTH_MONITOR", "0");
        assert_eq!(check_env_enabled(), None);
        std::env::remove_var("LOOM_TMUX_HEALTH_MONITOR");
    }

    #[test]
    #[serial]
    fn test_check_env_enabled_custom_interval() {
        std::env::set_var("LOOM_TMUX_HEALTH_MONITOR", "30");
        assert_eq!(check_env_enabled(), Some(30));
        std::env::remove_var("LOOM_TMUX_HEALTH_MONITOR");
    }

    #[test]
    #[serial]
    fn test_check_env_enabled_invalid_value_returns_default() {
        std::env::set_var("LOOM_TMUX_HEALTH_MONITOR", "not_a_number");
        assert_eq!(check_env_enabled(), Some(60));
        std::env::remove_var("LOOM_TMUX_HEALTH_MONITOR");
    }
}
