//! Reactive main-health backstop — `buildGate`-on-`main` + halt-on-red
//! (Phase C of epic #3809).
//!
//! This module is the daemon-native, always-on safety net for **autonomous**
//! (non-`/loom:sweep`) dispatch. It implements the epic's **git-based reactive
//! safety** design principle (operator decision 2026-07-23): git already
//! catches textual conflicts at merge time; this catches the recoverable
//! *semantic / cross-file* breakage that a clean merge can still introduce —
//! **reactively**, after the fact, never by dispatch-time collision prevention.
//!
//! # What it does
//!
//! On a configurable cadence the gate runs the repo's configured
//! `buildGate.command` (schema shipped in #3749) against `main`. On a **red**
//! run (non-zero exit or timeout) it sets a shared halt flag; the
//! [`crate::work_finder`] loop consults that flag and dispatches **zero** new
//! sweeps while halted (existing in-flight sweeps are never killed — halting
//! only stops making a red `main` worse). The next **green** run clears the
//! flag and dispatch resumes on the following work-finder tick.
//!
//! # Shape (mirrors [`crate::work_finder`])
//!
//! - **Opt-in** via [`MAIN_HEALTH_GATE_ENABLE_ENV`] — unset / false-y keeps it
//!   OFF, so the daemon's behavior is byte-for-byte unchanged when absent.
//! - **Config** read from `.loom/config.json` → `buildGate` with the same
//!   soft-fail pattern as [`crate::worktree_root`]'s `read_config_worktree_root`
//!   (missing file / missing key / malformed JSON / `enabled: false` all resolve
//!   to "gate disabled"), matching #3749's opt-in contract.
//! - **Cadence loop** [`spawn_main_health_gate_task`] runs as a plain
//!   `tokio::spawn` interval task on the shared daemon runtime, mirroring the
//!   work-finder. The (potentially minutes-long) gate command is executed on a
//!   blocking thread via `tokio::task::spawn_blocking` so it never parks a
//!   runtime worker.
//!
//! # Surfacing (scope-limited)
//!
//! A red `main` is surfaced by **loud logging** (daemon log): the offending
//! command, its exit reason, and a tail of its captured output. Auto-revert of
//! the offending PR (via `merge-pr.sh` / the Auditor cron) is an explicit
//! non-goal for this issue — halting + surfacing is the hard requirement.
//! No new event-bus topic is introduced (the six-topic taxonomy is frozen and
//! has no home for a non-sweep-triggered health event — a follow-up issue would
//! be required to add one).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ============================================================================
// Constants
// ============================================================================

/// Environment variable enabling the main-health gate loop.
///
/// The gate is **opt-in** — unset or a false-y value keeps it OFF so the
/// daemon's behavior is unchanged when the variable is absent. Set to `1` /
/// `true` / `yes` / `on` (case-insensitive) to enable.
pub const MAIN_HEALTH_GATE_ENABLE_ENV: &str = "LOOM_MAIN_HEALTH_GATE";

/// Environment variable overriding the gate cadence (seconds).
pub const MAIN_HEALTH_GATE_INTERVAL_ENV: &str = "LOOM_MAIN_HEALTH_GATE_INTERVAL_SECS";

/// Default gate cadence. Tighter than the work-finder's 60s default — a red
/// `main` should be caught (and dispatch halted) promptly — while still keeping
/// build volume low.
pub const DEFAULT_MAIN_HEALTH_GATE_INTERVAL_SECS: u64 = 30;

/// Default `buildGate.timeoutSeconds` when the config omits it (matches the
/// #3749 schema example).
pub const DEFAULT_BUILD_GATE_TIMEOUT_SECS: u64 = 600;

/// Poll granularity while waiting for the gate command to finish.
const GATE_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Max bytes of captured gate-command output retained for the red-detail log
/// line (the *tail* is kept — the failing assertion is usually last).
const MAX_OUTPUT_TAIL_BYTES: usize = 4096;

// ============================================================================
// Shared halt state
// ============================================================================

/// Cheaply-checked halt flag shared between the gate loop (writer) and the
/// [`crate::work_finder`] loop (reader).
///
/// Modeled on [`crate::health_monitor::TmuxHealthState`]'s `Arc<Atomic*>`
/// idiom: safe under concurrent access from the gate-check thread and the
/// work-finder tick with no mutex. `halted == true` means "a `buildGate` run
/// against `main` most recently failed — do not dispatch new work."
pub struct MainHealthState {
    /// Whether autonomous dispatch is currently halted due to a red `main`.
    halted: AtomicBool,
}

impl MainHealthState {
    /// A fresh state — **not** halted (dispatch allowed) until a gate run proves
    /// otherwise. This default means a daemon with the gate *disabled* never
    /// halts (nothing ever flips the flag), so work-finder behavior is
    /// unchanged when the gate is off.
    #[must_use]
    pub fn new() -> Self {
        Self {
            halted: AtomicBool::new(false),
        }
    }

    /// Whether autonomous dispatch is currently halted.
    #[must_use]
    pub fn is_halted(&self) -> bool {
        self.halted.load(Ordering::SeqCst)
    }

    /// Set the halt flag directly (primarily for tests / explicit control).
    pub fn set_halted(&self, halted: bool) {
        self.halted.store(halted, Ordering::SeqCst);
    }
}

impl Default for MainHealthState {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Config
// ============================================================================

/// The subset of the `.loom/config.json` `buildGate` block this module consumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildGateConfig {
    /// The command to run against `main` (executed via `sh -c`).
    pub command: String,
    /// Timeout for a single gate run.
    pub timeout: Duration,
}

/// Read `.loom/config.json` → `buildGate`, soft-failing to `None` (gate
/// disabled) on any of: missing file, malformed JSON, missing `buildGate` block,
/// `buildGate.enabled` not `true`, or a missing/empty `buildGate.command`.
///
/// Mirrors the soft-fail contract of
/// [`crate::worktree_root`]'s `read_config_worktree_root` — a repo with no
/// `buildGate` block (or `enabled: false`) gets zero behavior change.
#[must_use]
pub fn read_build_gate_config(repo_root: &Path) -> Option<BuildGateConfig> {
    let config_path = repo_root.join(".loom").join("config.json");

    let config_str = match std::fs::read_to_string(&config_path) {
        Ok(s) => s,
        Err(e) => {
            log::debug!(
                "main_health_gate: could not read config at {}: {e}",
                config_path.display()
            );
            return None;
        }
    };

    let config: serde_json::Value = match serde_json::from_str(&config_str) {
        Ok(v) => v,
        Err(e) => {
            log::warn!(
                "main_health_gate: could not parse config at {}: {e}",
                config_path.display()
            );
            return None;
        }
    };

    let gate = config.get("buildGate")?;

    // `enabled` must be explicitly true — absent or false ⇒ disabled.
    if !gate
        .get("enabled")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        log::debug!("main_health_gate: buildGate.enabled is not true — gate disabled");
        return None;
    }

    let command = gate
        .get("command")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .trim();
    if command.is_empty() {
        log::warn!("main_health_gate: buildGate.enabled is true but buildGate.command is missing/empty — gate disabled");
        return None;
    }

    let timeout_secs = gate
        .get("timeoutSeconds")
        .and_then(serde_json::Value::as_u64)
        .filter(|&s| s > 0)
        .unwrap_or(DEFAULT_BUILD_GATE_TIMEOUT_SECS);

    Some(BuildGateConfig {
        command: command.to_string(),
        timeout: Duration::from_secs(timeout_secs),
    })
}

// ============================================================================
// Gate outcome + runner
// ============================================================================

/// The result of one gate run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    /// `buildGate.command` exited 0 — `main` is healthy.
    Green,
    /// `buildGate.command` failed (non-zero exit, timeout, or spawn error).
    /// `detail` is a human-readable reason + a tail of captured output.
    Red { detail: String },
}

impl GateOutcome {
    /// Convenience constructor for a red outcome.
    #[must_use]
    pub fn red(detail: impl Into<String>) -> Self {
        Self::Red {
            detail: detail.into(),
        }
    }

    /// True when the run was green.
    #[must_use]
    pub fn is_green(&self) -> bool {
        matches!(self, Self::Green)
    }

    /// The red-detail string, or empty for a green outcome.
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Self::Green => "",
            Self::Red { detail } => detail,
        }
    }
}

/// Runs the configured `buildGate` command once and classifies the result.
///
/// Abstracted behind a trait so [`spawn_main_health_gate_task`] is testable with
/// a scripted fake runner, exactly as [`crate::work_finder::WorkSource`] /
/// [`crate::work_finder::WorkDispatcher`] make `tick` testable.
pub trait GateRunner {
    /// Run the gate once and return its classified outcome. Never errors — a
    /// spawn failure or timeout is itself a [`GateOutcome::Red`].
    fn run_gate(&mut self) -> GateOutcome;
}

/// The concrete [`GateRunner`]: shells out to `buildGate.command` (via `sh -c`)
/// against `main`, honoring `buildGate.timeoutSeconds`.
///
/// The command runs in `repo_root` — the daemon's workspace, which is a `main`
/// checkout. This module is the reactive backstop for autonomous dispatch, so
/// it verifies `main` as the daemon sees it; it deliberately does not provision
/// a throwaway worktree (extra complexity for no safety gain in the daemon's own
/// checkout).
pub struct CommandGateRunner {
    config: BuildGateConfig,
    repo_root: PathBuf,
}

impl CommandGateRunner {
    /// Construct a runner for `config`, executing in `repo_root`.
    #[must_use]
    pub fn new(config: BuildGateConfig, repo_root: PathBuf) -> Self {
        Self { config, repo_root }
    }
}

impl GateRunner for CommandGateRunner {
    fn run_gate(&mut self) -> GateOutcome {
        run_command_with_timeout(&self.config.command, &self.repo_root, self.config.timeout)
    }
}

/// Run `command` (via `sh -c`) in `cwd`, killing it if it exceeds `timeout`.
///
/// Child stdout+stderr are redirected to a single temp file (not a pipe) so a
/// chatty build can never dead-lock us on a full pipe buffer while we poll for
/// completion. The tail of that file is folded into the red-detail string.
fn run_command_with_timeout(command: &str, cwd: &Path, timeout: Duration) -> GateOutcome {
    use std::fs::File;

    let log_path =
        std::env::temp_dir().join(format!("loom-main-health-gate-{}.log", uuid::Uuid::new_v4()));
    let out_file = match File::create(&log_path) {
        Ok(f) => f,
        Err(e) => {
            return GateOutcome::red(format!(
                "failed to create gate output file {}: {e}",
                log_path.display()
            ));
        }
    };
    let err_file = match out_file.try_clone() {
        Ok(f) => f,
        Err(e) => {
            let _ = std::fs::remove_file(&log_path);
            return GateOutcome::red(format!("failed to clone gate output file handle: {e}"));
        }
    };

    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(err_file))
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&log_path);
            return GateOutcome::red(format!("failed to spawn gate command '{command}': {e}"));
        }
    };

    let start = Instant::now();
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    break GateOutcome::Green;
                }
                let tail = read_output_tail(&log_path);
                break GateOutcome::red(format!(
                    "gate command '{command}' exited with {status}{}",
                    format_tail(&tail)
                ));
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    let tail = read_output_tail(&log_path);
                    break GateOutcome::red(format!(
                        "gate command '{command}' timed out after {}s{}",
                        timeout.as_secs(),
                        format_tail(&tail)
                    ));
                }
                std::thread::sleep(GATE_POLL_INTERVAL);
            }
            Err(e) => {
                break GateOutcome::red(format!("failed to poll gate command '{command}': {e}"));
            }
        }
    };

    let _ = std::fs::remove_file(&log_path);
    outcome
}

/// Read the last [`MAX_OUTPUT_TAIL_BYTES`] of the gate's captured output.
fn read_output_tail(log_path: &Path) -> String {
    let bytes = std::fs::read(log_path).unwrap_or_default();
    let start = bytes.len().saturating_sub(MAX_OUTPUT_TAIL_BYTES);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

/// Format a captured-output tail for inclusion in a red-detail log line.
fn format_tail(tail: &str) -> String {
    let trimmed = tail.trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("; last output:\n{trimmed}")
    }
}

// ============================================================================
// Halt-state transitions
// ============================================================================

/// The health-state change a single gate outcome produced — returned so the
/// loop (and tests) can log/assert on transitions rather than steady state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthTransition {
    /// Green → Red: dispatch was allowed, now halted.
    EnteredHalt,
    /// Red → Red: still halted (no change).
    RemainedHalted,
    /// Red → Green: was halted, dispatch now resumes.
    Recovered,
    /// Green → Green: healthy, no change.
    RemainedHealthy,
}

/// Apply a gate `outcome` to the shared `state`, returning the transition.
///
/// Atomic `swap` makes the read-modify-write safe against a concurrent
/// work-finder read (which only ever *loads*). This is the single point that
/// mutates the halt flag.
#[must_use]
pub fn apply_gate_outcome(state: &MainHealthState, outcome: &GateOutcome) -> HealthTransition {
    match outcome {
        GateOutcome::Green => {
            let was_halted = state.halted.swap(false, Ordering::SeqCst);
            if was_halted {
                HealthTransition::Recovered
            } else {
                HealthTransition::RemainedHealthy
            }
        }
        GateOutcome::Red { .. } => {
            let was_halted = state.halted.swap(true, Ordering::SeqCst);
            if was_halted {
                HealthTransition::RemainedHalted
            } else {
                HealthTransition::EnteredHalt
            }
        }
    }
}

/// Log a health transition at a severity matching its significance.
fn log_transition(transition: HealthTransition, outcome: &GateOutcome) {
    match transition {
        HealthTransition::EnteredHalt => log::error!(
            "main_health_gate: main is RED — HALTING autonomous dispatch. {}",
            outcome.detail()
        ),
        HealthTransition::RemainedHalted => log::warn!(
            "main_health_gate: main still RED — dispatch remains halted. {}",
            outcome.detail()
        ),
        HealthTransition::Recovered => log::info!(
            "main_health_gate: main GREEN again — RESUMING autonomous dispatch on the next work-finder tick"
        ),
        HealthTransition::RemainedHealthy => {
            log::debug!("main_health_gate: main green — dispatch unaffected");
        }
    }
}

// ============================================================================
// Env-var configuration helpers
// ============================================================================

/// Whether the main-health gate loop is enabled, per
/// [`MAIN_HEALTH_GATE_ENABLE_ENV`]. Off by default (opt-in); parsing mirrors
/// [`crate::work_finder::enabled`]. This is the **env-only** primitive; the
/// config-aware entry point the daemon uses is [`resolve_enabled`] (precedence
/// env > config > default).
#[must_use]
pub fn enabled() -> bool {
    std::env::var(MAIN_HEALTH_GATE_ENABLE_ENV).is_ok_and(|v| {
        matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    })
}

/// The subset of `.loom/config.json → autonomous.mainHealthGate` this module
/// consumes. Today it carries only the enablement flag; future tuning knobs
/// (cadence, timeout) can be added here without touching the call site.
///
/// The gate's *behavior* (which command runs against `main`, its timeout) still
/// comes from the separate `buildGate` block via [`read_build_gate_config`] —
/// `autonomous.mainHealthGate` is purely the on/off (and future tuning) surface,
/// so Phase C's already-tested `buildGate` semantics are untouched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutonomousGateConfig {
    /// `autonomous.mainHealthGate.enabled` — whether to run the gate loop.
    /// `None` when the key is absent (falls through to env / default).
    pub enabled: Option<bool>,
}

/// Read `.loom/config.json → autonomous.mainHealthGate`, soft-failing to an
/// all-`None` config on any of: missing file, malformed JSON, or a missing
/// `autonomous` / `mainHealthGate` block. Mirrors [`read_build_gate_config`]'s
/// soft-fail contract — a repo with no `autonomous` block gets zero behavior
/// change (env-only enablement, exactly like Phase C shipped).
#[must_use]
pub fn read_autonomous_gate_config(repo_root: &Path) -> AutonomousGateConfig {
    let config_path = repo_root.join(".loom").join("config.json");

    let config_str = match std::fs::read_to_string(&config_path) {
        Ok(s) => s,
        Err(e) => {
            log::debug!(
                "main_health_gate: could not read config at {}: {e}",
                config_path.display()
            );
            return AutonomousGateConfig::default();
        }
    };

    let config: serde_json::Value = match serde_json::from_str(&config_str) {
        Ok(v) => v,
        Err(e) => {
            log::warn!(
                "main_health_gate: could not parse config at {}: {e}",
                config_path.display()
            );
            return AutonomousGateConfig::default();
        }
    };

    let Some(gate) = config
        .get("autonomous")
        .and_then(|a| a.get("mainHealthGate"))
    else {
        return AutonomousGateConfig::default();
    };

    AutonomousGateConfig {
        enabled: gate.get("enabled").and_then(serde_json::Value::as_bool),
    }
}

/// Resolve whether the gate loop is enabled with precedence **env > config >
/// default(false)**. When [`MAIN_HEALTH_GATE_ENABLE_ENV`] is *set* (to any
/// value) it decides (truthy enables, anything else disables); when unset the
/// config `enabled` flag decides; absent config leaves it off.
///
/// Keeping `LOOM_MAIN_HEALTH_GATE` as the master on/off preserves Phase C's
/// opt-in contract byte-for-byte when no `autonomous` block is present, while
/// letting a repo enable the gate entirely from committed config.
#[must_use]
pub fn resolve_enabled(config: &AutonomousGateConfig) -> bool {
    if let Ok(v) = std::env::var(MAIN_HEALTH_GATE_ENABLE_ENV) {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config.enabled.unwrap_or(false)
}

/// Resolve the gate cadence from [`MAIN_HEALTH_GATE_INTERVAL_ENV`], falling back
/// to [`DEFAULT_MAIN_HEALTH_GATE_INTERVAL_SECS`]. A zero or unparseable value
/// falls back to the default.
#[must_use]
pub fn resolve_interval() -> Duration {
    std::env::var(MAIN_HEALTH_GATE_INTERVAL_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .map_or_else(
            || Duration::from_secs(DEFAULT_MAIN_HEALTH_GATE_INTERVAL_SECS),
            Duration::from_secs,
        )
}

// ============================================================================
// Runtime wiring — the loop runs on the shared daemon runtime
// ============================================================================

/// Spawn the main-health gate loop on the shared daemon runtime and return its
/// task handle so the daemon can keep it alive for the process lifetime.
///
/// Every `interval` the loop runs one gate command (on a blocking thread via
/// `spawn_blocking`, since it may take minutes), applies the outcome to the
/// shared `health_state`, and logs the transition. The work-finder loop reads
/// `health_state` each of its own ticks and dispatches nothing while halted.
///
/// A plain `tokio::spawn` is correct here (unlike the epic supervisor's
/// dedicated OS thread) because the blocking command runs inside `spawn_blocking`
/// — the interval task itself never parks a runtime worker.
pub fn spawn_main_health_gate_task<R>(
    mut runner: R,
    health_state: Arc<MainHealthState>,
    interval: Duration,
) -> tokio::task::JoinHandle<()>
where
    R: GateRunner + Send + 'static,
{
    log::info!("main_health_gate: starting loop (interval={}s)", interval.as_secs());
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // First tick fires immediately; skip it so we don't churn at boot.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            // Run the (potentially minutes-long) gate command off the runtime.
            // Move the runner in and back out so it survives across ticks.
            let joined = tokio::task::spawn_blocking(move || {
                let outcome = runner.run_gate();
                (outcome, runner)
            })
            .await;
            let outcome = match joined {
                Ok((outcome, r)) => {
                    runner = r;
                    outcome
                }
                Err(e) => {
                    // The blocking task panicked; we can't recover the runner.
                    // Clear the halt flag so a panic here never wedges dispatch
                    // in a permanently-halted state, then stop the loop.
                    log::error!("main_health_gate: gate run task panicked ({e}); clearing halt and stopping loop");
                    health_state.set_halted(false);
                    return;
                }
            };
            let transition = apply_gate_outcome(&health_state, &outcome);
            log_transition(transition, &outcome);
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::collections::VecDeque;

    fn write_config(dir: &Path, body: &str) {
        let loom_dir = dir.join(".loom");
        std::fs::create_dir_all(&loom_dir).unwrap();
        std::fs::write(loom_dir.join("config.json"), body).unwrap();
    }

    // ===================================================================
    // Config soft-fail
    // ===================================================================

    #[test]
    fn test_config_missing_file_is_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_build_gate_config(tmp.path()), None);
    }

    #[test]
    fn test_config_malformed_json_is_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), "{not valid json");
        assert_eq!(read_build_gate_config(tmp.path()), None);
    }

    #[test]
    fn test_config_missing_build_gate_key_is_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"terminals": []}"#);
        assert_eq!(read_build_gate_config(tmp.path()), None);
    }

    #[test]
    fn test_config_enabled_false_is_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"buildGate": {"enabled": false, "command": "true"}}"#);
        assert_eq!(read_build_gate_config(tmp.path()), None);
    }

    #[test]
    fn test_config_enabled_missing_command_is_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"buildGate": {"enabled": true}}"#);
        assert_eq!(read_build_gate_config(tmp.path()), None);
    }

    #[test]
    fn test_config_enabled_empty_command_is_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"buildGate": {"enabled": true, "command": "   "}}"#);
        assert_eq!(read_build_gate_config(tmp.path()), None);
    }

    #[test]
    fn test_config_valid_uses_default_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"buildGate": {"enabled": true, "command": "bash .loom/scripts/build-gate.sh"}}"#,
        );
        let cfg = read_build_gate_config(tmp.path()).unwrap();
        assert_eq!(cfg.command, "bash .loom/scripts/build-gate.sh");
        assert_eq!(cfg.timeout, Duration::from_secs(DEFAULT_BUILD_GATE_TIMEOUT_SECS));
    }

    #[test]
    fn test_config_valid_honors_timeout_seconds() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"buildGate": {"enabled": true, "command": "true", "timeoutSeconds": 42}}"#,
        );
        let cfg = read_build_gate_config(tmp.path()).unwrap();
        assert_eq!(cfg.timeout, Duration::from_secs(42));
    }

    #[test]
    fn test_config_zero_timeout_seconds_falls_back_to_default() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"buildGate": {"enabled": true, "command": "true", "timeoutSeconds": 0}}"#,
        );
        let cfg = read_build_gate_config(tmp.path()).unwrap();
        assert_eq!(cfg.timeout, Duration::from_secs(DEFAULT_BUILD_GATE_TIMEOUT_SECS));
    }

    // ===================================================================
    // Halt-state transitions (the reactive core)
    // ===================================================================

    #[test]
    fn test_default_state_not_halted() {
        assert!(!MainHealthState::new().is_halted());
        assert!(!MainHealthState::default().is_halted());
    }

    #[test]
    fn test_green_then_red_enters_halt() {
        let state = MainHealthState::new();
        assert_eq!(
            apply_gate_outcome(&state, &GateOutcome::Green),
            HealthTransition::RemainedHealthy
        );
        assert!(!state.is_halted());

        assert_eq!(
            apply_gate_outcome(&state, &GateOutcome::red("boom")),
            HealthTransition::EnteredHalt
        );
        assert!(state.is_halted(), "a red run must halt dispatch");
    }

    #[test]
    fn test_red_then_red_remains_halted() {
        let state = MainHealthState::new();
        assert_eq!(
            apply_gate_outcome(&state, &GateOutcome::red("boom")),
            HealthTransition::EnteredHalt
        );
        assert_eq!(
            apply_gate_outcome(&state, &GateOutcome::red("still broken")),
            HealthTransition::RemainedHalted
        );
        assert!(state.is_halted());
    }

    #[test]
    fn test_red_then_green_recovers() {
        let state = MainHealthState::new();
        let _ = apply_gate_outcome(&state, &GateOutcome::red("boom"));
        assert!(state.is_halted());

        assert_eq!(apply_gate_outcome(&state, &GateOutcome::Green), HealthTransition::Recovered);
        assert!(!state.is_halted(), "a green run must clear the halt");
    }

    #[test]
    fn test_full_red_then_green_sequence_via_fake_runner() {
        // A scripted runner: red, red, green — asserting the halt flag tracks
        // the sequence exactly (halt on first red, stay halted, clear on green).
        struct FakeGateRunner {
            outcomes: VecDeque<GateOutcome>,
        }
        impl GateRunner for FakeGateRunner {
            fn run_gate(&mut self) -> GateOutcome {
                self.outcomes.pop_front().unwrap_or(GateOutcome::Green)
            }
        }

        let mut runner = FakeGateRunner {
            outcomes: VecDeque::from([
                GateOutcome::red("first failure"),
                GateOutcome::red("second failure"),
                GateOutcome::Green,
            ]),
        };
        let state = MainHealthState::new();

        // Tick 1: red ⇒ halted.
        let t1 = apply_gate_outcome(&state, &runner.run_gate());
        assert_eq!(t1, HealthTransition::EnteredHalt);
        assert!(state.is_halted());

        // Tick 2: red ⇒ still halted.
        let t2 = apply_gate_outcome(&state, &runner.run_gate());
        assert_eq!(t2, HealthTransition::RemainedHalted);
        assert!(state.is_halted());

        // Tick 3: green ⇒ recovered, dispatch resumes.
        let t3 = apply_gate_outcome(&state, &runner.run_gate());
        assert_eq!(t3, HealthTransition::Recovered);
        assert!(!state.is_halted());
    }

    // ===================================================================
    // Command runner (real subprocess — green + red + timeout)
    // ===================================================================

    #[test]
    fn test_command_runner_green_on_zero_exit() {
        let cfg = BuildGateConfig {
            command: "exit 0".to_string(),
            timeout: Duration::from_secs(30),
        };
        let mut runner = CommandGateRunner::new(cfg, std::env::temp_dir());
        assert_eq!(runner.run_gate(), GateOutcome::Green);
    }

    #[test]
    fn test_command_runner_red_on_nonzero_exit_captures_output() {
        let cfg = BuildGateConfig {
            command: "echo build-failed-marker >&2; exit 1".to_string(),
            timeout: Duration::from_secs(30),
        };
        let mut runner = CommandGateRunner::new(cfg, std::env::temp_dir());
        let outcome = runner.run_gate();
        assert!(!outcome.is_green());
        assert!(
            outcome.detail().contains("build-failed-marker"),
            "red detail should include captured output, got: {}",
            outcome.detail()
        );
    }

    #[test]
    fn test_command_runner_red_on_timeout() {
        let cfg = BuildGateConfig {
            command: "sleep 10".to_string(),
            timeout: Duration::from_secs(1),
        };
        let mut runner = CommandGateRunner::new(cfg, std::env::temp_dir());
        let outcome = runner.run_gate();
        assert!(!outcome.is_green());
        assert!(
            outcome.detail().contains("timed out"),
            "timeout detail expected, got: {}",
            outcome.detail()
        );
    }

    // ===================================================================
    // Env-var configuration
    // ===================================================================

    #[test]
    #[serial]
    fn test_enabled_off_by_default() {
        std::env::remove_var(MAIN_HEALTH_GATE_ENABLE_ENV);
        assert!(!enabled(), "unset ⇒ disabled (zero behavior change)");
    }

    #[test]
    #[serial]
    fn test_enabled_truthy_and_falsy() {
        for v in ["1", "true", "yes", "on", "TRUE", "On", " Yes "] {
            std::env::set_var(MAIN_HEALTH_GATE_ENABLE_ENV, v);
            assert!(enabled(), "{v:?} should enable");
        }
        for v in ["0", "false", "no", "off", "", "maybe"] {
            std::env::set_var(MAIN_HEALTH_GATE_ENABLE_ENV, v);
            assert!(!enabled(), "{v:?} should not enable");
        }
        std::env::remove_var(MAIN_HEALTH_GATE_ENABLE_ENV);
    }

    #[test]
    #[serial]
    fn test_resolve_interval_default_and_override() {
        std::env::remove_var(MAIN_HEALTH_GATE_INTERVAL_ENV);
        assert_eq!(resolve_interval(), Duration::from_secs(DEFAULT_MAIN_HEALTH_GATE_INTERVAL_SECS));

        std::env::set_var(MAIN_HEALTH_GATE_INTERVAL_ENV, "15");
        assert_eq!(resolve_interval(), Duration::from_secs(15));

        // Zero and unparseable fall back to the default.
        std::env::set_var(MAIN_HEALTH_GATE_INTERVAL_ENV, "0");
        assert_eq!(resolve_interval(), Duration::from_secs(DEFAULT_MAIN_HEALTH_GATE_INTERVAL_SECS));
        std::env::set_var(MAIN_HEALTH_GATE_INTERVAL_ENV, "garbage");
        assert_eq!(resolve_interval(), Duration::from_secs(DEFAULT_MAIN_HEALTH_GATE_INTERVAL_SECS));
        std::env::remove_var(MAIN_HEALTH_GATE_INTERVAL_ENV);
    }

    // ===================================================================
    // Autonomous config surface — autonomous.mainHealthGate (#3813)
    // ===================================================================

    #[test]
    fn test_autonomous_config_missing_file_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_autonomous_gate_config(tmp.path()), AutonomousGateConfig::default());
    }

    #[test]
    fn test_autonomous_config_malformed_json_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), "{not valid json");
        assert_eq!(read_autonomous_gate_config(tmp.path()), AutonomousGateConfig::default());
    }

    #[test]
    fn test_autonomous_config_missing_block_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"workFinder": {"enabled": true}}}"#);
        assert_eq!(read_autonomous_gate_config(tmp.path()), AutonomousGateConfig::default());
    }

    #[test]
    fn test_autonomous_config_enabled_true_and_false() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"mainHealthGate": {"enabled": true}}}"#);
        assert_eq!(
            read_autonomous_gate_config(tmp.path()),
            AutonomousGateConfig {
                enabled: Some(true)
            }
        );
        write_config(tmp.path(), r#"{"autonomous": {"mainHealthGate": {"enabled": false}}}"#);
        assert_eq!(
            read_autonomous_gate_config(tmp.path()),
            AutonomousGateConfig {
                enabled: Some(false)
            }
        );
    }

    #[test]
    #[serial]
    fn test_resolve_enabled_precedence() {
        std::env::remove_var(MAIN_HEALTH_GATE_ENABLE_ENV);

        // Absent config + unset env ⇒ default off (Phase C opt-in preserved).
        assert!(!resolve_enabled(&AutonomousGateConfig::default()));

        // Config alone enables/disables when env is unset.
        assert!(resolve_enabled(&AutonomousGateConfig {
            enabled: Some(true)
        }));
        assert!(!resolve_enabled(&AutonomousGateConfig {
            enabled: Some(false)
        }));

        // Env overrides config in both directions (env is the master switch).
        std::env::set_var(MAIN_HEALTH_GATE_ENABLE_ENV, "1");
        assert!(resolve_enabled(&AutonomousGateConfig {
            enabled: Some(false)
        }));
        std::env::set_var(MAIN_HEALTH_GATE_ENABLE_ENV, "0");
        assert!(!resolve_enabled(&AutonomousGateConfig {
            enabled: Some(true)
        }));
        std::env::remove_var(MAIN_HEALTH_GATE_ENABLE_ENV);
    }
}
