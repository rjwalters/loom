//! Optional background health monitoring for tmux server
//!
//! This module provides a background thread that periodically checks tmux server health.
//! Enable by setting the `LOOM_TMUX_HEALTH_MONITOR` environment variable.
//!
//! Example:
//! ```bash
//! LOOM_TMUX_HEALTH_MONITOR=30 pnpm daemon:preview  # Check every 30 seconds
//! ```

use std::process::Command;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Start a background thread that monitors tmux server health
///
/// # Arguments
/// * `interval_secs` - How often to check tmux server health (in seconds)
///
/// # Returns
/// A `JoinHandle` to the monitoring thread (can be ignored or used to stop monitoring)
///
/// # Example
/// ```
/// use loom_daemon::health_monitor;
///
/// // Start monitoring every 30 seconds
/// let _monitor = health_monitor::start_tmux_health_monitor(30);
/// ```
pub fn start_tmux_health_monitor(interval_secs: u64) -> JoinHandle<()> {
    log::info!("🏥 Starting tmux health monitor (checking every {interval_secs} seconds)");

    thread::spawn(move || loop {
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

                log::info!("📊 tmux health check: {} loom sessions active", sessions.len());

                // Alert on session count anomalies
                if sessions.is_empty() {
                    log::warn!(
                        "⚠️  No loom sessions found - server may have crashed or not started"
                    );
                }
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);

                // Distinguish failure modes
                if stderr.contains("no server running") {
                    log::error!(
                        "🚨 TMUX SERVER DEAD (health monitor) - Socket should be at /private/tmp/tmux-$UID/loom"
                    );
                } else if stderr.contains("no sessions") {
                    log::debug!("No tmux sessions exist: {stderr}");
                } else {
                    log::error!("🚨 tmux server not responding: {stderr}");
                }
            }
            Err(e) => {
                log::error!("Failed to check tmux health: {e}");
            }
        }
    })
}

/// Check if health monitoring is enabled via environment variable
///
/// Returns `Some(interval)` if enabled, `None` if disabled
pub fn check_env_enabled() -> Option<u64> {
    std::env::var("LOOM_TMUX_HEALTH_MONITOR")
        .ok()
        .and_then(|val| val.parse::<u64>().ok())
}
