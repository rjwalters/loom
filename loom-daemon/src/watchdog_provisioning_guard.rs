//! Periodic re-provisioning of a missing watchdog job/timer onto an
//! **already-running** host (issue #5405).
//!
//! # Why this exists
//!
//! `heal_watchdog_provisioning_gap()` (`defaults/scripts/cli/loom-daemon-start.sh`,
//! issue #5343) is correct and idempotent, but it only ever runs as a SIDE
//! EFFECT of re-running `loom-daemon-start.sh` — an operator running it by
//! hand, `loom-daemon fleet add-worker`, or `loom-daemon-update.sh`'s
//! non-launchd restart path. A host that was provisioned before #5343 landed
//! and simply keeps running (the daemon never restarts, never rebuilds) is
//! therefore never healed: nothing host-resident ever notices the gap. #5391's
//! bounded watchdog recovery can't close this either — it re-runs
//! `loom-daemon-start.sh` too, but only a host that ALREADY has a working
//! watchdog can trigger that recovery in the first place. An unprotected host
//! has no watchdog and so has no trigger.
//!
//! # The fix: the daemon itself is the host-resident, always-running thing
//!
//! The one thing that IS host-resident and already runs on a cadence — on
//! every host, protected or not — is the daemon process itself. This module
//! adds one more periodic loop (same runtime-wiring shape as
//! [`crate::daemon_heartbeat`] / [`crate::token_ranking_refresh`]) that
//! periodically re-invokes `loom-daemon-start.sh --heal-watchdog-only`
//! (`defaults/scripts/cli/loom-daemon-start.sh`, issue #5405) — a narrow,
//! side-effect-scoped entry point that performs ONLY the
//! `heal_watchdog_provisioning_gap()` heal and exits, reusing that function
//! **verbatim** rather than reimplementing launchd/systemd provisioning logic
//! in Rust. Critically, `--heal-watchdog-only` never reaches the script's
//! "already-running guard" or the daemon-start path at all, so it cannot ever
//! attempt to start a second daemon — even if the PID file this script itself
//! manages were stale, missing, or wrong.
//!
//! # Constraints (deliberate, mirrors [`crate::autonomy_marker`] / #5343)
//!
//! - **Cheap when there is nothing to do.** The autonomy-desired marker is
//!   checked with a plain filesystem read BEFORE ever spawning the script —
//!   an unsupervised / non-autonomous host pays only that one `Path::exists`
//!   call per tick, never a subprocess.
//! - **Idempotent underlying heal.** `heal_watchdog_provisioning_gap()`
//!   already guarantees repeated calls are a no-op once provisioned (#5343) —
//!   this loop leans on that guarantee rather than tracking its own
//!   "already provisioned" state.
//! - **Never disturbs the running daemon.** `--heal-watchdog-only` structurally
//!   cannot reach the daemon-start path (see the script's own doc comment).
//! - **Never fatal.** A failed pass (spawn error, non-zero exit, timeout) is
//!   logged and retried on the next tick — never taken as a reason to stop the
//!   daemon or the loop itself.
//! - **Default-on**, like [`crate::daemon_heartbeat`] and
//!   [`crate::autonomy_marker`]'s startup healing: this is the same
//!   crash-protection class of loop (an absent watchdog is a silent,
//!   unattended-failure-mode gap), not a dispatch-affecting autonomous loop —
//!   so it does not follow the FLAGS-OFF opt-in convention those use. An
//!   operator can still opt out ([`WATCHDOG_PROVISIONING_GUARD_ENABLE_ENV`] /
//!   `autonomous.watchdogProvisioningGuard.enabled=false`).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

// ============================================================================
// Constants (env overrides + built-in defaults)
// ============================================================================

/// Master on/off env override. Default-ON (see module docs — this is the
/// crash-protection class of loop, not a FLAGS-OFF autonomous one): unset ⇒
/// enabled. `0`/`false`/`no`/`off` (case-insensitive) disables;
/// `1`/`true`/`yes`/`on` force-enables even when config disables it.
pub const WATCHDOG_PROVISIONING_GUARD_ENABLE_ENV: &str = "LOOM_WATCHDOG_PROVISIONING_GUARD";

/// Env override for the check cadence (seconds).
pub const WATCHDOG_PROVISIONING_GUARD_INTERVAL_ENV: &str =
    "LOOM_WATCHDOG_PROVISIONING_GUARD_INTERVAL_SECS";

/// Default cadence (10 minutes) — the same "periodic support" slot as
/// [`crate::token_ranking_refresh`] (#3969): frequent enough that a host which
/// loses its watchdog provisioning is re-healed promptly, cheap enough
/// (one filesystem read most ticks; the underlying heal is already idempotent)
/// that running it unconditionally is negligible next to normal daemon load.
pub const DEFAULT_INTERVAL_SECS: u64 = 600;

/// Timeout for one `--heal-watchdog-only` subprocess pass. Generous headroom
/// over the script's real runtime (a few `launchctl`/`systemctl` calls) without
/// letting a hung pass wedge the loop indefinitely.
const DEFAULT_HEAL_TIMEOUT: Duration = Duration::from_secs(60);

/// Poll granularity while waiting for the heal subprocess to finish.
const PROBE_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Max bytes of captured subprocess output retained in a failure log line.
const MAX_OUTPUT_TAIL_BYTES: usize = 2048;

/// The default heal-pass timeout, exposed for callers that wire the loop
/// (mirrors [`DEFAULT_INTERVAL_SECS`]'s role for the tick cadence).
#[must_use]
pub const fn default_heal_timeout() -> Duration {
    DEFAULT_HEAL_TIMEOUT
}

// ============================================================================
// Config (.loom/config.json → autonomous.watchdogProvisioningGuard)
// ============================================================================

/// The subset of `.loom/config.json → autonomous.watchdogProvisioningGuard`
/// this module consumes. Each field is `Option` so an absent key falls
/// through to the env-var / built-in-default resolution — precedence
/// **env > config > default**, matching every other `autonomous.*` surface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchdogProvisioningGuardConfig {
    /// `autonomous.watchdogProvisioningGuard.enabled` — `None` when the key is
    /// absent (falls through to env / default(**true**) — default-on).
    pub enabled: Option<bool>,
    /// `autonomous.watchdogProvisioningGuard.intervalSecs` — cadence in
    /// seconds (a zero/invalid value is dropped to `None`).
    pub interval_secs: Option<u64>,
}

/// Read `.loom/config.json → autonomous.watchdogProvisioningGuard` through
/// [`crate::config_resolver`] (so the `.loom-project/` tier is honored like
/// every other migrated `autonomous.*` block), soft-failing every field to
/// `None` (env/default resolution) on a missing file, malformed JSON, or a
/// missing `autonomous` / `watchdogProvisioningGuard` block.
#[must_use]
pub fn read_config(repo_root: &Path) -> WatchdogProvisioningGuardConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(block) =
        crate::config_resolver::get_path(&effective, "autonomous.watchdogProvisioningGuard")
    else {
        return WatchdogProvisioningGuardConfig::default();
    };

    WatchdogProvisioningGuardConfig {
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
pub fn resolve_enabled(config: &WatchdogProvisioningGuardConfig) -> bool {
    if let Ok(v) = std::env::var(WATCHDOG_PROVISIONING_GUARD_ENABLE_ENV) {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config.enabled.unwrap_or(true)
}

/// Resolve the check cadence with precedence **env > config > default**. A
/// zero or unparseable env value falls through to `config`/the default rather
/// than producing a busy loop.
#[must_use]
pub fn resolve_interval(config: &WatchdogProvisioningGuardConfig) -> Duration {
    std::env::var(WATCHDOG_PROVISIONING_GUARD_INTERVAL_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(config.interval_secs)
        .map_or_else(|| Duration::from_secs(DEFAULT_INTERVAL_SECS), Duration::from_secs)
}

// ============================================================================
// Script resolution
// ============================================================================

/// Resolve `loom-daemon-start.sh` under `repo_root`: prefer the installed
/// `.loom/scripts/cli/` copy, else the in-repo `defaults/scripts/cli/` source
/// — the exact precedence [`crate::auto_update::ScriptAutoUpdateProbe`] uses
/// for `loom-daemon-update.sh`. `None` when neither exists (a repo whose
/// `.loom/` install predates this script, or a non-Loom-managed checkout).
#[must_use]
pub fn resolve_heal_script(repo_root: &Path) -> Option<PathBuf> {
    let installed = repo_root.join(".loom/scripts/cli/loom-daemon-start.sh");
    if installed.is_file() {
        return Some(installed);
    }
    let source = repo_root.join("defaults/scripts/cli/loom-daemon-start.sh");
    if source.is_file() {
        return Some(source);
    }
    None
}

// ============================================================================
// One pass
// ============================================================================

/// The result of one `run_pass` — one variant per branch, so the runtime
/// wiring can log precisely and tests can assert on the decision without
/// inspecting subprocess output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealPassOutcome {
    /// No loom dir could be resolved at all (no `LOOM_SOCKET_PATH` and no home
    /// directory) — cannot even check for a marker.
    NoLoomDir,
    /// No `<loom_dir>/autonomy-desired` marker: nothing was ever "desired" on
    /// this host, so the heal script is never even spawned (cheap fs read
    /// only, matches `heal_watchdog_provisioning_gap()`'s own first check).
    MarkerAbsent,
    /// Marker present, but no `loom-daemon-start.sh` could be located under
    /// this repo root's `.loom/scripts/cli/` or `defaults/scripts/cli/`.
    ScriptMissing,
    /// `--heal-watchdog-only` ran and exited 0 (a no-op when already
    /// provisioned, a real provision when not — indistinguishable from here,
    /// by design: the underlying heal is idempotent either way).
    Healed,
    /// The pass could not run to a successful completion (spawn error,
    /// non-zero exit, or timeout). Never fatal — logged and retried on the
    /// next tick.
    Failed(String),
}

/// Run one heal pass against `repo_root`: resolve the loom dir + marker via
/// the SAME resolution [`crate::autonomy_marker`] uses (so this loop and the
/// startup marker-healing agree on what "autonomy desired" means), then —
/// only if the marker is present — spawn `--heal-watchdog-only` with
/// `timeout`.
#[must_use]
pub fn run_pass(repo_root: &Path, timeout: Duration) -> HealPassOutcome {
    let Some(loom_dir) = crate::autonomy_marker::resolve_loom_dir() else {
        return HealPassOutcome::NoLoomDir;
    };
    let marker_path = crate::autonomy_marker::resolve_marker_path(&loom_dir);
    if !marker_path.exists() {
        return HealPassOutcome::MarkerAbsent;
    }

    let Some(script) = resolve_heal_script(repo_root) else {
        return HealPassOutcome::ScriptMissing;
    };

    match run_heal_watchdog_only(&script, repo_root, timeout) {
        Ok(()) => HealPassOutcome::Healed,
        Err(e) => HealPassOutcome::Failed(e),
    }
}

/// Spawn `<script> --heal-watchdog-only` with `cwd` as the working directory,
/// capturing combined output to a temp file (never a pipe — avoids the
/// pipe-buffer deadlock) and killing it after `timeout`. `Ok(())` on a zero
/// exit; `Err(reason)` (with an output tail) otherwise. Shape mirrors
/// `install_self_check::run_with_timeout` / `auto_update::run_update_script`.
fn run_heal_watchdog_only(script: &Path, cwd: &Path, timeout: Duration) -> Result<(), String> {
    let log_path = std::env::temp_dir()
        .join(format!("loom-watchdog-provisioning-guard-{}.log", uuid::Uuid::new_v4()));
    let out_file = std::fs::File::create(&log_path)
        .map_err(|e| format!("could not create output file: {e}"))?;
    let stderr_file = match out_file.try_clone() {
        Ok(f) => f,
        Err(e) => {
            let _ = std::fs::remove_file(&log_path);
            return Err(format!("could not clone output handle: {e}"));
        }
    };

    let mut child = match Command::new(script)
        .arg("--heal-watchdog-only")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(stderr_file))
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&log_path);
            return Err(format!("could not spawn `{}`: {e}", script.display()));
        }
    };

    let start = Instant::now();
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break Ok(()),
            Ok(Some(status)) => {
                let tail = std::fs::read_to_string(&log_path).unwrap_or_default();
                break Err(format!(
                    "`{}` exited with {status}: {}",
                    script.display(),
                    truncate_tail(&tail)
                ));
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(format!(
                        "`{}` timed out after {}s",
                        script.display(),
                        timeout.as_secs()
                    ));
                }
                std::thread::sleep(PROBE_POLL_INTERVAL);
            }
            Err(e) => break Err(format!("could not poll `{}`: {e}", script.display())),
        }
    };
    let _ = std::fs::remove_file(&log_path);
    result
}

/// Truncate captured output to the last [`MAX_OUTPUT_TAIL_BYTES`] bytes,
/// trimmed, on a char boundary.
fn truncate_tail(s: &str) -> String {
    if s.len() <= MAX_OUTPUT_TAIL_BYTES {
        return s.trim().to_string();
    }
    let start = s.len() - MAX_OUTPUT_TAIL_BYTES;
    let boundary = (start..s.len())
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(s.len());
    format!("…{}", s[boundary..].trim())
}

// ============================================================================
// Runtime wiring
// ============================================================================

/// Spawn the watchdog-provisioning-guard loop on the shared daemon runtime
/// (mirrors [`crate::daemon_heartbeat::spawn_heartbeat_task`] /
/// [`crate::token_ranking_refresh`]'s single-workspace shape — this daemon
/// process protects the ONE host/workspace it runs on, so there is no
/// multi-workspace fan-out to do, unlike [`crate::install_self_check`]).
/// Every `interval` it runs one [`run_pass`] against `repo_root`, moved onto
/// `spawn_blocking` since a pass may shell out — so a tick never parks a
/// runtime worker.
#[must_use]
pub fn spawn_watchdog_provisioning_guard_task(
    repo_root: PathBuf,
    interval: Duration,
    timeout: Duration,
) -> tokio::task::JoinHandle<()> {
    log::info!("watchdog_provisioning_guard: starting loop (interval={}s)", interval.as_secs());
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let root = repo_root.clone();
            let joined = tokio::task::spawn_blocking(move || run_pass(&root, timeout)).await;
            match joined {
                Ok(HealPassOutcome::NoLoomDir) => log::debug!(
                    "watchdog_provisioning_guard: could not resolve a loom dir (no \
                     LOOM_SOCKET_PATH / home) — skipping this pass"
                ),
                Ok(HealPassOutcome::MarkerAbsent) => log::debug!(
                    "watchdog_provisioning_guard: no autonomy-desired marker — nothing to heal \
                     (#5405)"
                ),
                Ok(HealPassOutcome::ScriptMissing) => log::debug!(
                    "watchdog_provisioning_guard: marker present but loom-daemon-start.sh not \
                     found under this repo root — skipping this pass"
                ),
                Ok(HealPassOutcome::Healed) => log::debug!(
                    "watchdog_provisioning_guard: --heal-watchdog-only pass completed (#5405)"
                ),
                Ok(HealPassOutcome::Failed(reason)) => log::warn!(
                    "watchdog_provisioning_guard: pass failed (logged, never fatal; will retry \
                     next tick): {reason}"
                ),
                Err(e) => log::error!(
                    "watchdog_provisioning_guard: pass task panicked ({e}); continuing to the \
                     next tick"
                ),
            }
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::Instant as StdInstant;

    fn write_config(root: &Path, contents: &str) {
        fs::create_dir_all(root.join(".loom")).unwrap();
        fs::write(root.join(".loom").join("config.json"), contents).unwrap();
    }

    fn write_executable_script(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    /// Hermetic against ambient `LOOM_AUTONOMY_MARKER` (#6538): a real
    /// loom-daemon dogfooding host sets this permanently in its process
    /// environment, and `resolve_marker_path()` — which `run_pass()` calls —
    /// checks it BEFORE falling back to `<loom_dir>/autonomy-desired`, so on
    /// such a host it resolves to the real (present) marker instead of the
    /// tempdir fixture these "marker absent" tests are built around. Save/clear
    /// it for the duration of the test and restore whatever the host had on
    /// exit (even on panic), the same guard-on-Drop idiom `safehouse.rs` uses
    /// for the analogous `LOOM_SAFEHOUSE_ROOM` ambient-state hazard (#5805,
    /// commit a1428779).
    struct RestoreAmbientAutonomyMarker(Option<String>);
    impl RestoreAmbientAutonomyMarker {
        fn clear() -> Self {
            let saved = std::env::var("LOOM_AUTONOMY_MARKER").ok();
            std::env::remove_var("LOOM_AUTONOMY_MARKER");
            Self(saved)
        }
    }
    impl Drop for RestoreAmbientAutonomyMarker {
        fn drop(&mut self) {
            match &self.0 {
                Some(value) => std::env::set_var("LOOM_AUTONOMY_MARKER", value),
                None => std::env::remove_var("LOOM_AUTONOMY_MARKER"),
            }
        }
    }

    // ===================================================================
    // Config surface — autonomous.watchdogProvisioningGuard
    // ===================================================================

    #[test]
    fn test_config_missing_file_is_default() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_config(tmp.path()), WatchdogProvisioningGuardConfig::default());
    }

    #[test]
    fn test_config_malformed_json_is_default() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), "{not valid json");
        assert_eq!(read_config(tmp.path()), WatchdogProvisioningGuardConfig::default());
    }

    #[test]
    fn test_config_missing_block_is_default() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"heartbeat": {"enabled": true}}}"#);
        assert_eq!(read_config(tmp.path()), WatchdogProvisioningGuardConfig::default());
    }

    #[test]
    fn test_config_reads_enabled_and_interval() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous": {"watchdogProvisioningGuard": {"enabled": false, "intervalSecs": 30}}}"#,
        );
        assert_eq!(
            read_config(tmp.path()),
            WatchdogProvisioningGuardConfig {
                enabled: Some(false),
                interval_secs: Some(30)
            }
        );
    }

    #[test]
    fn test_config_zero_interval_is_dropped_to_none() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous": {"watchdogProvisioningGuard": {"intervalSecs": 0}}}"#,
        );
        assert_eq!(read_config(tmp.path()).interval_secs, None);
    }

    // ===================================================================
    // Precedence — env > config > default
    // ===================================================================

    #[test]
    #[serial]
    fn test_resolve_enabled_default_is_true() {
        std::env::remove_var(WATCHDOG_PROVISIONING_GUARD_ENABLE_ENV);
        assert!(
            resolve_enabled(&WatchdogProvisioningGuardConfig::default()),
            "absent config + unset env ⇒ default ON (crash-protection class, #5405)"
        );
    }

    #[test]
    #[serial]
    fn test_resolve_enabled_config_can_disable() {
        std::env::remove_var(WATCHDOG_PROVISIONING_GUARD_ENABLE_ENV);
        assert!(!resolve_enabled(&WatchdogProvisioningGuardConfig {
            enabled: Some(false),
            interval_secs: None
        }));
        assert!(resolve_enabled(&WatchdogProvisioningGuardConfig {
            enabled: Some(true),
            interval_secs: None
        }));
    }

    #[test]
    #[serial]
    fn test_resolve_enabled_env_overrides_config() {
        std::env::set_var(WATCHDOG_PROVISIONING_GUARD_ENABLE_ENV, "0");
        assert!(!resolve_enabled(&WatchdogProvisioningGuardConfig {
            enabled: Some(true),
            interval_secs: None
        }));
        std::env::set_var(WATCHDOG_PROVISIONING_GUARD_ENABLE_ENV, "1");
        assert!(resolve_enabled(&WatchdogProvisioningGuardConfig {
            enabled: Some(false),
            interval_secs: None
        }));
        std::env::remove_var(WATCHDOG_PROVISIONING_GUARD_ENABLE_ENV);
    }

    #[test]
    #[serial]
    fn test_resolve_interval_default_config_and_env_precedence() {
        std::env::remove_var(WATCHDOG_PROVISIONING_GUARD_INTERVAL_ENV);
        assert_eq!(
            resolve_interval(&WatchdogProvisioningGuardConfig::default()),
            Duration::from_secs(DEFAULT_INTERVAL_SECS)
        );
        assert_eq!(
            resolve_interval(&WatchdogProvisioningGuardConfig {
                enabled: None,
                interval_secs: Some(30)
            }),
            Duration::from_secs(30)
        );
        std::env::set_var(WATCHDOG_PROVISIONING_GUARD_INTERVAL_ENV, "15");
        assert_eq!(
            resolve_interval(&WatchdogProvisioningGuardConfig {
                enabled: None,
                interval_secs: Some(30)
            }),
            Duration::from_secs(15)
        );
        std::env::set_var(WATCHDOG_PROVISIONING_GUARD_INTERVAL_ENV, "0");
        assert_eq!(
            resolve_interval(&WatchdogProvisioningGuardConfig {
                enabled: None,
                interval_secs: Some(30)
            }),
            Duration::from_secs(30)
        );
        std::env::remove_var(WATCHDOG_PROVISIONING_GUARD_INTERVAL_ENV);
    }

    // ===================================================================
    // Script resolution
    // ===================================================================

    #[test]
    fn test_resolve_heal_script_prefers_installed_over_source() {
        let tmp = tempfile::tempdir().unwrap();
        write_executable_script(
            &tmp.path().join(".loom/scripts/cli/loom-daemon-start.sh"),
            "#!/bin/sh\nexit 0\n",
        );
        write_executable_script(
            &tmp.path().join("defaults/scripts/cli/loom-daemon-start.sh"),
            "#!/bin/sh\nexit 1\n",
        );
        assert_eq!(
            resolve_heal_script(tmp.path()),
            Some(tmp.path().join(".loom/scripts/cli/loom-daemon-start.sh"))
        );
    }

    #[test]
    fn test_resolve_heal_script_falls_back_to_source() {
        let tmp = tempfile::tempdir().unwrap();
        write_executable_script(
            &tmp.path().join("defaults/scripts/cli/loom-daemon-start.sh"),
            "#!/bin/sh\nexit 0\n",
        );
        assert_eq!(
            resolve_heal_script(tmp.path()),
            Some(tmp.path().join("defaults/scripts/cli/loom-daemon-start.sh"))
        );
    }

    #[test]
    fn test_resolve_heal_script_none_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(resolve_heal_script(tmp.path()), None);
    }

    // ===================================================================
    // run_pass
    // ===================================================================

    #[test]
    #[serial(loom_socket_path_env)]
    fn test_run_pass_marker_absent_is_a_pure_fs_check() {
        let _restore_ambient_marker = RestoreAmbientAutonomyMarker::clear();
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("loom-daemon.sock");
        std::env::set_var("LOOM_SOCKET_PATH", &socket);
        // Deliberately no autonomy-desired marker and no start script — if the
        // marker check did not short-circuit, ScriptMissing would fire
        // instead, so asserting MarkerAbsent proves the fs-read-only path.
        let outcome = run_pass(tmp.path(), Duration::from_secs(5));
        std::env::remove_var("LOOM_SOCKET_PATH");
        assert_eq!(outcome, HealPassOutcome::MarkerAbsent);
    }

    #[test]
    #[serial(loom_socket_path_env)]
    fn test_run_pass_script_missing_when_marker_present() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("loom-daemon.sock");
        std::env::set_var("LOOM_SOCKET_PATH", &socket);
        fs::write(tmp.path().join("autonomy-desired"), "started_at=x\n").unwrap();
        let outcome = run_pass(tmp.path(), Duration::from_secs(5));
        std::env::remove_var("LOOM_SOCKET_PATH");
        assert_eq!(outcome, HealPassOutcome::ScriptMissing);
    }

    #[test]
    #[serial(loom_socket_path_env)]
    fn test_run_pass_healed_on_successful_script() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("loom-daemon.sock");
        std::env::set_var("LOOM_SOCKET_PATH", &socket);
        fs::write(tmp.path().join("autonomy-desired"), "started_at=x\n").unwrap();
        write_executable_script(
            &tmp.path().join(".loom/scripts/cli/loom-daemon-start.sh"),
            "#!/bin/sh\n[ \"$1\" = \"--heal-watchdog-only\" ] || exit 9\nexit 0\n",
        );
        let outcome = run_pass(tmp.path(), Duration::from_secs(5));
        std::env::remove_var("LOOM_SOCKET_PATH");
        assert_eq!(outcome, HealPassOutcome::Healed);
    }

    #[test]
    #[serial(loom_socket_path_env)]
    fn test_run_pass_failed_on_nonzero_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("loom-daemon.sock");
        std::env::set_var("LOOM_SOCKET_PATH", &socket);
        fs::write(tmp.path().join("autonomy-desired"), "started_at=x\n").unwrap();
        write_executable_script(
            &tmp.path().join(".loom/scripts/cli/loom-daemon-start.sh"),
            "#!/bin/sh\necho boom >&2\nexit 3\n",
        );
        let outcome = run_pass(tmp.path(), Duration::from_secs(5));
        std::env::remove_var("LOOM_SOCKET_PATH");
        match outcome {
            HealPassOutcome::Failed(reason) => assert!(reason.contains("boom")),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    #[serial(loom_socket_path_env)]
    fn test_run_pass_failed_on_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("loom-daemon.sock");
        std::env::set_var("LOOM_SOCKET_PATH", &socket);
        fs::write(tmp.path().join("autonomy-desired"), "started_at=x\n").unwrap();
        write_executable_script(
            &tmp.path().join(".loom/scripts/cli/loom-daemon-start.sh"),
            "#!/bin/sh\nsleep 5\nexit 0\n",
        );
        let start = StdInstant::now();
        let outcome = run_pass(tmp.path(), Duration::from_millis(100));
        std::env::remove_var("LOOM_SOCKET_PATH");
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "should time out promptly, not wait for sleep 5"
        );
        match outcome {
            HealPassOutcome::Failed(reason) => assert!(reason.contains("timed out")),
            other => panic!("expected Failed(timed out), got {other:?}"),
        }
    }

    #[test]
    #[serial(loom_socket_path_env)]
    fn test_run_pass_never_spawns_when_marker_absent_even_if_script_exists() {
        // If the marker check did not short-circuit BEFORE resolving/spawning
        // the script, this fixture (script present, marker absent) would run
        // the script and write the sentinel below. Asserting the sentinel is
        // ABSENT proves the marker check gates the spawn, not just the outcome.
        let _restore_ambient_marker = RestoreAmbientAutonomyMarker::clear();
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("loom-daemon.sock");
        std::env::set_var("LOOM_SOCKET_PATH", &socket);
        let sentinel = tmp.path().join("ran.sentinel");
        write_executable_script(
            &tmp.path().join(".loom/scripts/cli/loom-daemon-start.sh"),
            &format!("#!/bin/sh\ntouch {}\nexit 0\n", sentinel.display()),
        );
        let outcome = run_pass(tmp.path(), Duration::from_secs(5));
        std::env::remove_var("LOOM_SOCKET_PATH");
        assert_eq!(outcome, HealPassOutcome::MarkerAbsent);
        assert!(!sentinel.exists(), "script must never be spawned when the marker is absent");
    }

    // ===================================================================
    // Loop wiring
    // ===================================================================

    #[tokio::test]
    #[serial(loom_socket_path_env)]
    async fn test_loop_runs_a_pass_promptly() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("loom-daemon.sock");
        std::env::set_var("LOOM_SOCKET_PATH", &socket);
        fs::write(tmp.path().join("autonomy-desired"), "started_at=x\n").unwrap();
        let sentinel = tmp.path().join("ran.sentinel");
        write_executable_script(
            &tmp.path().join(".loom/scripts/cli/loom-daemon-start.sh"),
            &format!("#!/bin/sh\ntouch {}\nexit 0\n", sentinel.display()),
        );

        let handle = spawn_watchdog_provisioning_guard_task(
            tmp.path().to_path_buf(),
            Duration::from_millis(30),
            Duration::from_secs(5),
        );

        let deadline = StdInstant::now() + Duration::from_secs(5);
        while !sentinel.exists() {
            assert!(StdInstant::now() < deadline, "heal pass never ran");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        handle.abort();
        std::env::remove_var("LOOM_SOCKET_PATH");
    }
}
