//! Declared-cadence daemon liveness heartbeat (issue #4011).
//!
//! # Why
//!
//! On 2026-07-26 the `loom-daemon` launchd job took a SIGTERM two seconds after
//! starting and was left `bootout`-ed (unloaded) from launchd. Autonomous
//! dispatch silently stopped, and **nothing surfaced it** — no log line, no
//! forge signal, no notification. It was discovered hours later only because
//! someone happened to run `loom-daemon status` by hand.
//!
//! The fix (this issue's AC #1 / AC #2) is a *host-side* detector that lives
//! **outside** the daemon process — a dead daemon cannot report its own death,
//! which is why #3971's in-daemon watch loop is not reusable as the reporter.
//! That detector (`loom-daemon-watchdog.sh`, run as a second `StartInterval`
//! launchd job) needs a liveness signal it can read without talking to the
//! daemon. This module emits it: a heartbeat file the running daemon touches on
//! a **declared cadence**.
//!
//! # Why an explicit heartbeat, not an existing mtime
//!
//! The token-ranking refresh loop (#3969) already rewrites
//! `.loom/tokens/.ranking` on a ~10-minute cadence, so its mtime is an
//! *accidental* liveness signal. We deliberately do **not** build on it: it is a
//! side effect rather than a contract, and it is config-disableable
//! (`autonomous.tokenRankingRefresh`), so a detector keyed to it would silently
//! stop working when an operator turned that loop off. This module writes a file
//! whose *only* purpose is liveness, with a documented cadence the watchdog's
//! staleness threshold is derived from.
//!
//! # Where
//!
//! The heartbeat lives at `<loom_dir>/daemon.heartbeat`, where `<loom_dir>` is
//! resolved exactly like the daemon's socket/log paths: the parent of
//! `LOOM_SOCKET_PATH` when set (so a test daemon pointed at a tempdir socket
//! writes its heartbeat into that tempdir, never the operator's real
//! `~/.loom/daemon.heartbeat`), else `~/.loom`. This mirrors `resolve_loom_dir`
//! / `resolve_log_path` in `main.rs` (#4010).
//!
//! # Default-on
//!
//! Like the token-ranking refresh (#3969) and watch-monitor (#3971) loops, and
//! unlike the dispatch-affecting work-finder / main-health-gate loops, this loop
//! is **default-on**: it only ever writes a small bookkeeping file with no
//! dispatch side effect, and an absent heartbeat is precisely the signal the
//! watchdog needs. An operator can still opt out
//! (`LOOM_DAEMON_HEARTBEAT=0` / `autonomous.heartbeat.enabled=false`); the
//! watchdog degrades to a liveness-only check when no heartbeat is present.

use std::path::{Path, PathBuf};
use std::time::Duration;

// ============================================================================
// Constants
// ============================================================================

/// Environment variable controlling the heartbeat loop. Default-on (see module
/// docs): set to `0`/`false`/`no`/`off` (case-insensitive) to disable;
/// `1`/`true`/`yes`/`on` to force-enable even when config disables it.
pub const HEARTBEAT_ENABLE_ENV: &str = "LOOM_DAEMON_HEARTBEAT";

/// Environment variable overriding the heartbeat cadence (seconds).
pub const HEARTBEAT_INTERVAL_ENV: &str = "LOOM_DAEMON_HEARTBEAT_INTERVAL_SECS";

/// Default heartbeat cadence. The watchdog's default staleness threshold
/// (`loom-daemon-watchdog.sh`) is a comfortable multiple of this (5×), so a
/// single missed write never trips a false report.
pub const DEFAULT_HEARTBEAT_INTERVAL_SECS: u64 = 60;

/// Basename of the heartbeat file under the resolved loom dir.
pub const HEARTBEAT_FILENAME: &str = "daemon.heartbeat";

// ============================================================================
// Path resolution
// ============================================================================

/// Resolve `<loom_dir>` the same way `main.rs::resolve_loom_dir` does: the
/// parent of `LOOM_SOCKET_PATH` when set, else `~/.loom`. Kept in sync with
/// `main.rs` deliberately (both are tiny) rather than exported from `main`,
/// which is a binary, not a library.
fn resolve_loom_dir() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("LOOM_SOCKET_PATH") {
        return PathBuf::from(path).parent().map(Path::to_path_buf);
    }
    dirs::home_dir().map(|h| h.join(".loom"))
}

/// Resolve the heartbeat file path (`<loom_dir>/daemon.heartbeat`), or `None`
/// when no loom dir can be resolved (no home directory and no
/// `LOOM_SOCKET_PATH`).
#[must_use]
pub fn resolve_heartbeat_path() -> Option<PathBuf> {
    resolve_loom_dir().map(|d| d.join(HEARTBEAT_FILENAME))
}

// ============================================================================
// Writing
// ============================================================================

/// Write one heartbeat record to `path` atomically (temp file + rename), so a
/// concurrent watchdog read never sees a torn line. The record is a single line
/// of `<unix_secs> pid=<pid> ts=<iso8601>` — the watchdog keys off the file's
/// **mtime** for staleness, but the content is human-legible for diagnostics.
///
/// Returns `Err` with a human-readable reason on any I/O failure; the caller
/// logs and continues (a failed heartbeat write is never fatal).
pub fn write_heartbeat(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return Err(format!("could not create {}: {e}", parent.display()));
        }
    }
    let now = chrono::Utc::now();
    let contents =
        format!("{} pid={} ts={}\n", now.timestamp(), std::process::id(), now.to_rfc3339());
    // Temp file in the same dir so the rename is atomic (same filesystem).
    let tmp = path.with_extension(format!("heartbeat.tmp.{}", uuid::Uuid::new_v4()));
    if let Err(e) = std::fs::write(&tmp, &contents) {
        return Err(format!("could not write temp heartbeat {}: {e}", tmp.display()));
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("could not rename heartbeat into place {}: {e}", path.display()));
    }
    Ok(())
}

// ============================================================================
// Config (.loom/config.json → autonomous.heartbeat)
// ============================================================================

/// The subset of `.loom/config.json → autonomous.heartbeat` this module
/// consumes. Each field is `Option` so an absent key falls through to the
/// env-var / built-in-default resolution — precedence **env > config >
/// default**, matching every other `autonomous.*` surface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeartbeatConfig {
    /// `autonomous.heartbeat.enabled` — whether to run the loop. `None` when the
    /// key is absent (falls through to env / default(**true**) — default-on).
    pub enabled: Option<bool>,
    /// `autonomous.heartbeat.intervalSecs` — cadence in seconds (a zero/invalid
    /// value is dropped to `None`).
    pub interval_secs: Option<u64>,
}

/// Read `.loom/config.json → autonomous.heartbeat`, soft-failing every field to
/// `None` (env/default resolution) on any of: missing file, malformed JSON, or
/// a missing `autonomous` / `heartbeat` block. Mirrors the soft-fail contract of
/// [`crate::token_ranking_refresh::read_token_ranking_refresh_config`].
#[must_use]
pub fn read_heartbeat_config(repo_root: &Path) -> HeartbeatConfig {
    let config_path = repo_root.join(".loom").join("config.json");

    let config_str = match std::fs::read_to_string(&config_path) {
        Ok(s) => s,
        Err(e) => {
            log::debug!(
                "daemon_heartbeat: could not read config at {}: {e}",
                config_path.display()
            );
            return HeartbeatConfig::default();
        }
    };

    let config: serde_json::Value = match serde_json::from_str(&config_str) {
        Ok(v) => v,
        Err(e) => {
            log::warn!(
                "daemon_heartbeat: could not parse config at {}: {e}",
                config_path.display()
            );
            return HeartbeatConfig::default();
        }
    };

    let Some(block) = config.get("autonomous").and_then(|a| a.get("heartbeat")) else {
        return HeartbeatConfig::default();
    };

    HeartbeatConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        interval_secs: block
            .get("intervalSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
    }
}

/// Resolve whether the loop is enabled with precedence **env > config >
/// default(true)** — default-on (see module docs).
#[must_use]
pub fn resolve_enabled(config: &HeartbeatConfig) -> bool {
    if let Ok(v) = std::env::var(HEARTBEAT_ENABLE_ENV) {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config.enabled.unwrap_or(true)
}

/// Resolve the heartbeat cadence with precedence **env > config > default**. A
/// zero or unparseable env value falls through to `config`/the default rather
/// than producing a busy loop.
#[must_use]
pub fn resolve_interval(config: &HeartbeatConfig) -> Duration {
    std::env::var(HEARTBEAT_INTERVAL_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(config.interval_secs)
        .map_or_else(|| Duration::from_secs(DEFAULT_HEARTBEAT_INTERVAL_SECS), Duration::from_secs)
}

// ============================================================================
// Runtime wiring
// ============================================================================

/// Spawn the heartbeat loop on the shared daemon runtime. The **first tick is
/// not skipped** — the heartbeat is written immediately at boot so the watchdog
/// has fresh data within one cadence of the daemon coming up, then every
/// `interval` thereafter. A write failure is logged and the loop continues (the
/// next tick retries); it is never fatal to the daemon.
///
/// Returns `None` (and logs a warning) when no heartbeat path can be resolved —
/// the daemon still runs, only without a heartbeat, and the watchdog degrades to
/// a liveness-only check.
#[must_use]
pub fn spawn_heartbeat_task(interval: Duration) -> Option<tokio::task::JoinHandle<()>> {
    let path = match resolve_heartbeat_path() {
        Some(p) => p,
        None => {
            log::warn!(
                "daemon_heartbeat: could not resolve a heartbeat path (no home dir / \
                 LOOM_SOCKET_PATH) — heartbeat disabled for this run"
            );
            return None;
        }
    };
    log::info!(
        "daemon_heartbeat: starting loop (path={}, interval={}s)",
        path.display(),
        interval.as_secs()
    );
    Some(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            match write_heartbeat(&path) {
                Ok(()) => log::debug!("daemon_heartbeat: wrote {}", path.display()),
                Err(reason) => log::warn!(
                    "daemon_heartbeat: write failed (logged, never fatal; will retry next tick): {reason}"
                ),
            }
        }
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::fs;
    use std::time::Instant;

    fn write_config(root: &Path, contents: &str) {
        fs::create_dir_all(root.join(".loom")).unwrap();
        fs::write(root.join(".loom").join("config.json"), contents).unwrap();
    }

    // ===================================================================
    // write_heartbeat
    // ===================================================================

    #[test]
    fn test_write_heartbeat_creates_file_with_expected_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon.heartbeat");
        write_heartbeat(&path).unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("pid="), "{contents:?}");
        assert!(contents.contains("ts="), "{contents:?}");
        // First token is a unix timestamp (all digits).
        let first = contents.split_whitespace().next().unwrap();
        assert!(first.chars().all(|c| c.is_ascii_digit()), "{first:?}");
    }

    #[test]
    fn test_write_heartbeat_creates_parent_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp
            .path()
            .join("nested")
            .join("dir")
            .join("daemon.heartbeat");
        write_heartbeat(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn test_write_heartbeat_overwrites_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon.heartbeat");
        write_heartbeat(&path).unwrap();
        let first = fs::read_to_string(&path).unwrap();
        // Sleep a hair so the timestamp differs, then rewrite.
        std::thread::sleep(Duration::from_millis(1100));
        write_heartbeat(&path).unwrap();
        let second = fs::read_to_string(&path).unwrap();
        assert_ne!(first, second, "second write should carry a newer timestamp");
        // No stray temp files left behind.
        let leftovers: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "temp files should be renamed away: {leftovers:?}");
    }

    // ===================================================================
    // Path resolution
    // ===================================================================

    #[test]
    #[serial]
    fn test_resolve_heartbeat_path_honors_socket_path_env() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("loom-daemon.sock");
        std::env::set_var("LOOM_SOCKET_PATH", &socket);
        let resolved = resolve_heartbeat_path().unwrap();
        assert_eq!(resolved, tmp.path().join(HEARTBEAT_FILENAME));
        std::env::remove_var("LOOM_SOCKET_PATH");
    }

    // ===================================================================
    // Config surface — autonomous.heartbeat
    // ===================================================================

    #[test]
    fn test_config_missing_file_is_default() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_heartbeat_config(tmp.path()), HeartbeatConfig::default());
    }

    #[test]
    fn test_config_malformed_json_is_default() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), "{not valid json");
        assert_eq!(read_heartbeat_config(tmp.path()), HeartbeatConfig::default());
    }

    #[test]
    fn test_config_missing_block_is_default() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"workFinder": {"enabled": true}}}"#);
        assert_eq!(read_heartbeat_config(tmp.path()), HeartbeatConfig::default());
    }

    #[test]
    fn test_config_reads_enabled_and_interval() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous": {"heartbeat": {"enabled": false, "intervalSecs": 30}}}"#,
        );
        assert_eq!(
            read_heartbeat_config(tmp.path()),
            HeartbeatConfig {
                enabled: Some(false),
                interval_secs: Some(30)
            }
        );
    }

    #[test]
    fn test_config_zero_interval_is_dropped_to_none() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"heartbeat": {"intervalSecs": 0}}}"#);
        assert_eq!(read_heartbeat_config(tmp.path()).interval_secs, None);
    }

    // ===================================================================
    // Precedence — env > config > default
    // ===================================================================

    #[test]
    #[serial]
    fn test_resolve_enabled_default_is_true() {
        std::env::remove_var(HEARTBEAT_ENABLE_ENV);
        assert!(
            resolve_enabled(&HeartbeatConfig::default()),
            "absent config + unset env ⇒ default ON (default-on loop)"
        );
    }

    #[test]
    #[serial]
    fn test_resolve_enabled_config_can_disable() {
        std::env::remove_var(HEARTBEAT_ENABLE_ENV);
        assert!(!resolve_enabled(&HeartbeatConfig {
            enabled: Some(false),
            interval_secs: None
        }));
        assert!(resolve_enabled(&HeartbeatConfig {
            enabled: Some(true),
            interval_secs: None
        }));
    }

    #[test]
    #[serial]
    fn test_resolve_enabled_env_overrides_config() {
        std::env::set_var(HEARTBEAT_ENABLE_ENV, "0");
        assert!(!resolve_enabled(&HeartbeatConfig {
            enabled: Some(true),
            interval_secs: None
        }));
        std::env::set_var(HEARTBEAT_ENABLE_ENV, "1");
        assert!(resolve_enabled(&HeartbeatConfig {
            enabled: Some(false),
            interval_secs: None
        }));
        std::env::remove_var(HEARTBEAT_ENABLE_ENV);
    }

    #[test]
    #[serial]
    fn test_resolve_interval_default_config_and_env_precedence() {
        std::env::remove_var(HEARTBEAT_INTERVAL_ENV);
        assert_eq!(
            resolve_interval(&HeartbeatConfig::default()),
            Duration::from_secs(DEFAULT_HEARTBEAT_INTERVAL_SECS)
        );
        assert_eq!(
            resolve_interval(&HeartbeatConfig {
                enabled: None,
                interval_secs: Some(30)
            }),
            Duration::from_secs(30)
        );
        std::env::set_var(HEARTBEAT_INTERVAL_ENV, "15");
        assert_eq!(
            resolve_interval(&HeartbeatConfig {
                enabled: None,
                interval_secs: Some(30)
            }),
            Duration::from_secs(15)
        );
        std::env::set_var(HEARTBEAT_INTERVAL_ENV, "0");
        assert_eq!(
            resolve_interval(&HeartbeatConfig {
                enabled: None,
                interval_secs: Some(30)
            }),
            Duration::from_secs(30)
        );
        std::env::remove_var(HEARTBEAT_INTERVAL_ENV);
    }

    // ===================================================================
    // Loop wiring
    // ===================================================================

    #[tokio::test]
    async fn test_loop_writes_immediately_and_refreshes_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("loom-daemon.sock");
        std::env::set_var("LOOM_SOCKET_PATH", &socket);
        let path = tmp.path().join(HEARTBEAT_FILENAME);

        let handle = spawn_heartbeat_task(Duration::from_millis(30)).unwrap();

        // Written within one interval (first tick is immediate).
        let deadline = Instant::now() + Duration::from_secs(2);
        while !path.exists() {
            assert!(Instant::now() < deadline, "heartbeat file never appeared");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let first = fs::metadata(&path).unwrap().modified().unwrap();

        // A later tick refreshes the mtime.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let now = fs::metadata(&path).unwrap().modified().unwrap();
            if now > first {
                break;
            }
            assert!(Instant::now() < deadline, "heartbeat mtime never advanced");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        handle.abort();
        std::env::remove_var("LOOM_SOCKET_PATH");
    }
}
