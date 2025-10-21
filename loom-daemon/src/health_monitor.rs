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

/// Start a background thread that monitors tmux server health
///
/// # Arguments
/// * `interval_secs` - How often to check tmux server health (in seconds)
///
/// # Returns
/// A tuple of (JoinHandle, Arc<TmuxHealthState>) for monitoring and querying health status
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
                        health_state_clone
                            .server_alive
                            .store(true, Ordering::Relaxed);
                        health_state_clone
                            .last_session_count
                            .store(0, Ordering::Relaxed);
                        log::debug!("tmux server running but no sessions exist");
                    } else {
                        log::error!("🚨 tmux server not responding: {stderr}");
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
/// - If LOOM_TMUX_HEALTH_MONITOR is set to a number > 0, use that interval
/// - If LOOM_TMUX_HEALTH_MONITOR is set to 0, health monitoring is disabled
/// - If LOOM_TMUX_HEALTH_MONITOR is not set, default to 60 seconds (enabled by default)
///
/// # Returns
/// `Some(interval_secs)` if enabled, `None` if explicitly disabled
pub fn check_env_enabled() -> Option<u64> {
    match std::env::var("LOOM_TMUX_HEALTH_MONITOR") {
        Ok(val) => {
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
                    log::warn!(
                        "Invalid LOOM_TMUX_HEALTH_MONITOR value: '{val}', using default 60s"
                    );
                    Some(60)
                }
            }
        }
        Err(_) => {
            // Not set, use default (enabled by default)
            log::info!("tmux health monitoring enabled by default (60s interval)");
            Some(60)
        }
    }
}
