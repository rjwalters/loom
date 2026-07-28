//! Autonomous token-ranking refresh — the daemon self-refreshes the
//! multi-account token pool's `.ranking` file instead of depending on an
//! operator-managed cron for `probe-tokens.sh --ranking` (issue #3969).
//!
//! # Why
//!
//! Token selection ([`crate::tokens`] / `loom_tools.tokens.select`) prefers
//! `.loom/tokens/.ranking` over the allowlist/random fallback tiers, but
//! nothing wrote that file unless an operator set up an external cron
//! (`.loom/scripts/probe-tokens.sh --ranking`, documented in CLAUDE.md). During
//! the 2026-07-26 canary run the operator was refreshing it by hand — exactly
//! the kind of standing manual step the daemon (a long-lived process that
//! already owns dispatch cadence and consumes the ranking via
//! `spawn-claude.sh`) should own itself.
//!
//! # What it does
//!
//! On a configurable cadence (default 10 minutes) the daemon shells out to
//! **its own binary** (`std::env::current_exe()`) with `tokens check
//! --ranking --workspace <repo_root>` (issue #4080, epic #4081 Phase 2 —
//! historically this shelled out to `probe-tokens.sh --ranking`; see
//! [`ScriptRankingRefreshRunner::resolve_bin`]). Invoking `current_exe()`
//! rather than re-resolving a script sidesteps the "stale binary predates the
//! `tokens` subcommand" hazard entirely — the running daemon by construction
//! supports its own subcommands — and drops a process layer (daemon → bash →
//! daemon). The subcommand probes every bootstrapped account for its current
//! rate-limit headers and atomically rewrites `.ranking` in whichever pool
//! [`crate::tokens_pool::paths`] resolves for that repo (per-repo
//! `.loom/tokens/`, or the shared machine-level pool per #3938 when the
//! per-repo pool is absent/empty).
//!
//! # Never fatal, never double-writes unsafely
//!
//! A probe failure (network hiccup, every account exhausted, no tokens
//! bootstrapped at all) is logged and skipped — it never panics the loop or
//! the daemon. Because `tokens check --ranking` writes `.ranking` atomically
//! (temp file + rename, matching every other Loom bookkeeping writer), an
//! operator's cron running `probe-tokens.sh --ranking` concurrently is
//! harmless: the two refreshers can race to *schedule* a write but never to a
//! *torn* file, and whichever write lands last simply wins — both are
//! measuring the same live rate-limit state, so there is no correctness cost
//! to the redundancy, only (rare) duplicated API calls.
//!
//! # Default-on (unlike the work-finder / main-health-gate loops)
//!
//! [`crate::work_finder`] and [`crate::main_health_gate`] are opt-in because
//! they have dispatch-affecting side effects (spawning sweeps, halting
//! dispatch on a red `main`). This loop only ever *reads* rate-limit headers
//! and rewrites a bookkeeping file nothing else in the daemon consults
//! synchronously — so leaving it on by default costs nothing beyond a periodic
//! probe API call, while an absent daemon-side refresher silently regresses
//! every install back to the stale-ranking failure mode this issue exists to
//! fix. An operator who wants it off (e.g. no tokens bootstrapped at all, or a
//! read-only/dry-run daemon) still has a full opt-out knob — see
//! [`TOKEN_RANKING_REFRESH_ENABLE_ENV`] / `autonomous.tokenRankingRefresh.enabled`.
//!
//! # Shape (mirrors [`crate::main_health_gate`] / [`crate::work_finder`])
//!
//! - **Config** read from `.loom/config.json` → `autonomous.tokenRankingRefresh`
//!   with the same soft-fail pattern as [`crate::main_health_gate::read_build_gate_config`]
//!   (missing file / malformed JSON / missing block all resolve to "use the env
//!   var / built-in default").
//! - **Precedence env > config > default** for both `enabled` and the cadence,
//!   exactly like every other `autonomous.*` knob (CLAUDE.md → "Daemon
//!   Configuration (Tier 2)").
//! - **Multi-workspace** ([`spawn_multi_token_ranking_refresh_task`]): re-reads
//!   the workspace registry each tick and refreshes every registered repo's own
//!   pool, exactly like [`crate::main_health_gate::spawn_multi_main_health_gate_task`].
//!   An empty registry reduces to the single `fallback_root`.
//! - The probe runs on a blocking thread via `tokio::task::spawn_blocking` (it
//!   shells out to a subprocess and makes network calls) so it never parks a
//!   runtime worker, mirroring the main-health gate's command execution.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::workspace_registry::WorkspaceRegistry;

// ============================================================================
// Constants
// ============================================================================

/// Environment variable controlling the token-ranking refresh loop.
///
/// Unlike [`crate::work_finder::WORK_FINDER_ENABLE_ENV`] /
/// [`crate::main_health_gate::MAIN_HEALTH_GATE_ENABLE_ENV`] (opt-in, unset ⇒
/// off), this loop is **default-on** — see the module docs. Set to
/// `0`/`false`/`no`/`off` (case-insensitive) to disable; `1`/`true`/`yes`/`on`
/// to force-enable even when config disables it.
pub const TOKEN_RANKING_REFRESH_ENABLE_ENV: &str = "LOOM_TOKEN_RANKING_REFRESH";

/// Environment variable overriding the refresh cadence (seconds).
pub const TOKEN_RANKING_REFRESH_INTERVAL_ENV: &str = "LOOM_TOKEN_RANKING_REFRESH_INTERVAL_SECS";

/// Default refresh cadence — matches the operator cron example this loop
/// retires (`*/10 * * * *` in the historical CLAUDE.md crontab).
pub const DEFAULT_TOKEN_RANKING_REFRESH_INTERVAL_SECS: u64 = 600;

/// How long to wait for `probe-tokens.sh --ranking` before killing it. A probe
/// is a handful of short HTTP requests (one per bootstrapped account); this is
/// generous headroom for a slow network without letting a hung probe wedge the
/// loop indefinitely.
const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(120);

/// Poll granularity while waiting for the probe to finish.
const PROBE_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Max bytes of captured probe output retained in a failure log line.
const MAX_OUTPUT_TAIL_BYTES: usize = 2048;

// ============================================================================
// Outcome + runner (testable via a trait, mirrors GateRunner)
// ============================================================================

/// The result of one refresh tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// The probe ran to completion and reported success (`.ranking` rewritten,
    /// or at least one account was probed — see `loom-daemon tokens check`'s
    /// own exit code contract, mirrored from `probe-tokens.sh`).
    Success,
    /// The probe could not be run, or ran and reported failure. `reason`
    /// explains what happened. Never fatal to the daemon — logged and skipped.
    Failure(String),
}

impl RefreshOutcome {
    /// True for a completed, successful probe.
    #[must_use]
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success)
    }
}

/// Runs the ranking probe once. Abstracted behind a trait so the loop is
/// testable with a scripted fake, exactly as
/// [`crate::main_health_gate::GateRunner`] makes the health-gate loop testable.
pub trait RankingRefreshRunner {
    /// Refresh the ranking once and return the outcome. Never panics — a
    /// spawn failure, timeout, or non-zero exit is a [`RefreshOutcome::Failure`],
    /// never a propagated error.
    fn refresh(&mut self) -> RefreshOutcome;
}

/// The concrete [`RankingRefreshRunner`]: shells out to **its own binary**
/// (`std::env::current_exe()`) with `tokens check --ranking --workspace
/// <repo_root>` (issue #4080). Historically this resolved and spawned
/// `probe-tokens.sh --ranking`; invoking `current_exe()` instead sidesteps the
/// "stale binary predates the `tokens` subcommand" hazard entirely (the
/// running daemon by construction supports its own subcommands) and drops a
/// process layer (daemon → bash → daemon).
///
/// Pool-location resolution (per-repo vs. the #3938 shared machine-level pool)
/// is deliberately **not** reimplemented here — `tokens check` delegates to
/// [`crate::tokens_pool::paths`], which already owns that resolution and the
/// atomic `.ranking` write. This runner's only job is resolving the binary to
/// invoke and running it with a timeout.
pub struct ScriptRankingRefreshRunner {
    repo_root: PathBuf,
    /// Explicit binary override (tests point this at a fake executable).
    /// Production leaves this `None` and resolves via [`Self::resolve_bin`].
    bin: Option<PathBuf>,
    timeout: Duration,
}

impl ScriptRankingRefreshRunner {
    /// Construct a runner for `repo_root` with the production timeout.
    #[must_use]
    pub fn new(repo_root: PathBuf) -> Self {
        Self {
            repo_root,
            bin: None,
            timeout: DEFAULT_PROBE_TIMEOUT,
        }
    }

    /// Override the daemon binary to invoke (tests only — production always
    /// resolves via [`Self::resolve_bin`] to `std::env::current_exe()`).
    #[must_use]
    pub fn with_bin(mut self, bin: PathBuf) -> Self {
        self.bin = Some(bin);
        self
    }

    /// Override the probe timeout (tests only).
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Resolve the binary to invoke: [`Self::bin`] explicit override (tests),
    /// else `std::env::current_exe()` — the currently-running `loom-daemon`
    /// binary, which by construction supports the `tokens check` subcommand.
    fn resolve_bin(&self) -> Result<PathBuf, String> {
        if let Some(p) = &self.bin {
            return Ok(p.clone());
        }
        std::env::current_exe().map_err(|e| format!("could not resolve current_exe(): {e}"))
    }
}

impl RankingRefreshRunner for ScriptRankingRefreshRunner {
    fn refresh(&mut self) -> RefreshOutcome {
        let bin = match self.resolve_bin() {
            Ok(p) => p,
            Err(e) => return RefreshOutcome::Failure(e),
        };
        run_probe_with_timeout(&bin, &self.repo_root, self.timeout)
    }
}

/// Run `bin tokens check --ranking --workspace <repo_root>` with `repo_root`
/// as both the `--workspace` argument and the child's cwd, capturing combined
/// output to a temp file (never a pipe — avoids the pipe-buffer deadlock the
/// health gate's equivalent helper documents) and killing it after `timeout`.
fn run_probe_with_timeout(bin: &Path, repo_root: &Path, timeout: Duration) -> RefreshOutcome {
    let log_path = std::env::temp_dir()
        .join(format!("loom-token-ranking-refresh-{}.log", uuid::Uuid::new_v4()));
    let out_file = match std::fs::File::create(&log_path) {
        Ok(f) => f,
        Err(e) => {
            return RefreshOutcome::Failure(format!("could not create probe output file: {e}"))
        }
    };
    let stderr_file = match out_file.try_clone() {
        Ok(f) => f,
        Err(e) => {
            let _ = std::fs::remove_file(&log_path);
            return RefreshOutcome::Failure(format!("could not clone probe output handle: {e}"));
        }
    };

    let mut child = match Command::new(bin)
        .arg("tokens")
        .arg("check")
        .arg("--ranking")
        .arg("--workspace")
        .arg(repo_root)
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(stderr_file))
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&log_path);
            return RefreshOutcome::Failure(format!("could not spawn `{}`: {e}", bin.display()));
        }
    };

    let start = Instant::now();
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break RefreshOutcome::Success,
            Ok(Some(status)) => {
                let tail = std::fs::read_to_string(&log_path).unwrap_or_default();
                break RefreshOutcome::Failure(format!(
                    "`{}` exited with {status}: {}",
                    bin.display(),
                    truncate_tail(&tail)
                ));
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    break RefreshOutcome::Failure(format!(
                        "`{}` timed out after {}s",
                        bin.display(),
                        timeout.as_secs()
                    ));
                }
                std::thread::sleep(PROBE_POLL_INTERVAL);
            }
            Err(e) => {
                break RefreshOutcome::Failure(format!("could not poll `{}`: {e}", bin.display()))
            }
        }
    };
    let _ = std::fs::remove_file(&log_path);
    result
}

/// Truncate captured probe output to the last [`MAX_OUTPUT_TAIL_BYTES`] bytes
/// (the failure detail is usually last), trimmed of surrounding whitespace.
fn truncate_tail(s: &str) -> String {
    if s.len() <= MAX_OUTPUT_TAIL_BYTES {
        return s.trim().to_string();
    }
    // Slice on a char boundary at-or-before the byte cutoff.
    let start = s.len() - MAX_OUTPUT_TAIL_BYTES;
    let start = (start..s.len())
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(s.len());
    s[start..].trim().to_string()
}

// ============================================================================
// Config (.loom/config.json → autonomous.tokenRankingRefresh)
// ============================================================================

/// The subset of `.loom/config.json → autonomous.tokenRankingRefresh` this
/// module consumes. Each field is `Option` so an absent key falls through to
/// the env-var / built-in-default resolution — precedence is **env > config >
/// default** for every knob, matching every other `autonomous.*` surface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenRankingRefreshConfig {
    /// `autonomous.tokenRankingRefresh.enabled` — whether to run the loop.
    /// `None` when the key is absent (falls through to env / default(**true**)
    /// — this loop is default-**on**, unlike the work-finder / gate).
    pub enabled: Option<bool>,
    /// `autonomous.tokenRankingRefresh.intervalSecs` — refresh cadence in
    /// seconds (a zero/invalid value is dropped to `None`).
    pub interval_secs: Option<u64>,
}

/// Read `.loom/config.json → autonomous.tokenRankingRefresh`, soft-failing
/// every field to `None` (env/default resolution) on any of: missing file,
/// malformed JSON, or a missing `autonomous` / `tokenRankingRefresh` block.
///
/// Mirrors the soft-fail contract of
/// [`crate::main_health_gate::read_build_gate_config`] — a repo with no
/// `autonomous` block gets the env-only default behavior (enabled, 10-minute
/// cadence).
#[must_use]
pub fn read_token_ranking_refresh_config(repo_root: &Path) -> TokenRankingRefreshConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(block) =
        crate::config_resolver::get_path(&effective, "autonomous.tokenRankingRefresh")
    else {
        return TokenRankingRefreshConfig::default();
    };

    TokenRankingRefreshConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        interval_secs: block
            .get("intervalSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
    }
}

/// Resolve whether the loop is enabled with precedence **env > config >
/// default(true)**. When [`TOKEN_RANKING_REFRESH_ENABLE_ENV`] is *set* (to any
/// value) it decides (truthy enables, anything else disables); when unset the
/// config `enabled` flag decides; absent config leaves it **on** — the
/// default-on decision documented at the module level.
#[must_use]
pub fn resolve_enabled(config: &TokenRankingRefreshConfig) -> bool {
    if let Ok(v) = std::env::var(TOKEN_RANKING_REFRESH_ENABLE_ENV) {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config.enabled.unwrap_or(true)
}

/// Resolve the refresh cadence with precedence **env > config > default**. A
/// zero or unparseable env value falls through to `config`/the default rather
/// than producing a busy loop.
#[must_use]
pub fn resolve_interval(config: &TokenRankingRefreshConfig) -> Duration {
    std::env::var(TOKEN_RANKING_REFRESH_INTERVAL_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(config.interval_secs)
        .map_or_else(
            || Duration::from_secs(DEFAULT_TOKEN_RANKING_REFRESH_INTERVAL_SECS),
            Duration::from_secs,
        )
}

// ============================================================================
// Runtime wiring
// ============================================================================

/// Spawn the token-ranking refresh loop for a single workspace on the shared
/// daemon runtime. Intended for tests; production uses
/// [`spawn_multi_token_ranking_refresh_task`] (the multi-workspace entry
/// point wired into `main.rs`).
///
/// Unlike [`crate::main_health_gate::spawn_main_health_gate_task`] /
/// [`crate::work_finder`]'s loops, the **first tick is not skipped**: a ranking
/// probe is a handful of cheap read-only HTTP calls with no dispatch side
/// effect, so refreshing immediately at boot (rather than waiting a full
/// cadence for the first data) is strictly better — there is no "churn on
/// startup" cost to avoid here.
pub fn spawn_token_ranking_refresh_task<R>(
    mut runner: R,
    interval: Duration,
) -> tokio::task::JoinHandle<()>
where
    R: RankingRefreshRunner + Send + 'static,
{
    log::info!("token_ranking_refresh: starting loop (interval={}s)", interval.as_secs());
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let joined = tokio::task::spawn_blocking(move || {
                let outcome = runner.refresh();
                (outcome, runner)
            })
            .await;
            match joined {
                Ok((outcome, r)) => {
                    runner = r;
                    log_outcome(&outcome);
                }
                Err(e) => {
                    log::error!(
                        "token_ranking_refresh: refresh task panicked ({e}); stopping loop \
                         (the last-written .ranking, if any, is left as-is)"
                    );
                    return;
                }
            }
        }
    })
}

/// Spawn the **multi-workspace** token-ranking refresh loop (mirrors
/// [`crate::main_health_gate::spawn_multi_main_health_gate_task`]) on the
/// shared daemon runtime.
///
/// Every `interval` it re-reads [`WorkspaceRegistry::effective_roots`] against
/// `fallback_root` (an **empty** registry ⇒ the single `fallback_root`) and
/// refreshes **one ranking per registered root**, each gated by that root's own
/// `.loom/config.json` (`autonomous.tokenRankingRefresh.enabled`, precedence
/// env > config > default(true) — the env var, when set, applies uniformly to
/// every root). Probes run **sequentially** per tick (matches the health
/// gate's sequencing rationale — no shared mutable state to leak across repos,
/// and it avoids bursting several accounts' rate-limit probes at once).
pub fn spawn_multi_token_ranking_refresh_task(
    fallback_root: PathBuf,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    log::info!(
        "token_ranking_refresh: starting multi-workspace loop (interval={}s)",
        interval.as_secs()
    );
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;

            let roots = WorkspaceRegistry::load_default()
                .unwrap_or_else(|e| {
                    log::warn!(
                        "token_ranking_refresh: could not load workspace registry ({e}); using fallback"
                    );
                    WorkspaceRegistry::default()
                })
                .effective_roots(&fallback_root);

            for root in roots {
                if !resolve_enabled(&read_token_ranking_refresh_config(&root)) {
                    log::debug!(
                        "token_ranking_refresh: {} disabled (autonomous.tokenRankingRefresh.enabled=false \
                         or LOOM_TOKEN_RANKING_REFRESH unset-falsy) — skipping",
                        root.display()
                    );
                    continue;
                }
                let root_for_task = root.clone();
                let joined = tokio::task::spawn_blocking(move || {
                    let mut runner = ScriptRankingRefreshRunner::new(root_for_task);
                    runner.refresh()
                })
                .await;
                match joined {
                    Ok(outcome) => log_outcome_for_root(&root, &outcome),
                    Err(e) => log::error!(
                        "token_ranking_refresh: refresh task for {} panicked ({e}); continuing to the \
                         next repo",
                        root.display()
                    ),
                }
            }
        }
    })
}

/// Log a single-workspace refresh outcome at a severity matching its
/// significance. Never escalates to `error!` — a probe failure is never fatal.
fn log_outcome(outcome: &RefreshOutcome) {
    match outcome {
        RefreshOutcome::Success => log::debug!("token_ranking_refresh: ranking refreshed"),
        RefreshOutcome::Failure(reason) => log::warn!(
            "token_ranking_refresh: probe failed (logged and skipped, never fatal): {reason}"
        ),
    }
}

/// Root-aware variant of [`log_outcome`] for the multi-workspace loop.
fn log_outcome_for_root(root: &Path, outcome: &RefreshOutcome) {
    match outcome {
        RefreshOutcome::Success => {
            log::debug!("token_ranking_refresh: {} ranking refreshed", root.display());
        }
        RefreshOutcome::Failure(reason) => log::warn!(
            "token_ranking_refresh: {} probe failed (logged and skipped, never fatal): {reason}",
            root.display()
        ),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::fs;

    fn write_config(root: &Path, contents: &str) {
        fs::create_dir_all(root.join(".loom")).unwrap();
        fs::write(root.join(".loom").join("config.json"), contents).unwrap();
    }

    /// A fake binary that just exits with a fixed code, optionally writing to
    /// stdout/stderr first. Written with a shebang so it's directly executable.
    fn write_fake_bin(dir: &Path, name: &str, body: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&path, perms).unwrap();
        }
        path
    }

    // ===================================================================
    // ScriptRankingRefreshRunner — binary resolution + execution
    // ===================================================================

    #[test]
    fn test_resolve_bin_defaults_to_current_exe() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = ScriptRankingRefreshRunner::new(tmp.path().to_path_buf());
        let resolved = runner.resolve_bin().unwrap();
        assert_eq!(resolved, std::env::current_exe().unwrap());
    }

    #[test]
    fn test_resolve_bin_honors_override() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = write_fake_bin(tmp.path(), "fake-daemon.sh", "exit 0");
        let runner =
            ScriptRankingRefreshRunner::new(tmp.path().to_path_buf()).with_bin(fake.clone());
        assert_eq!(runner.resolve_bin().unwrap(), fake);
    }

    #[test]
    fn test_refresh_success_on_zero_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = write_fake_bin(tmp.path(), "fake-daemon.sh", "echo ok; exit 0");
        let mut runner = ScriptRankingRefreshRunner::new(tmp.path().to_path_buf()).with_bin(bin);
        assert_eq!(runner.refresh(), RefreshOutcome::Success);
    }

    #[test]
    fn test_refresh_failure_on_nonzero_exit_includes_output_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let bin =
            write_fake_bin(tmp.path(), "fake-daemon.sh", "echo tokens directory missing; exit 1");
        let mut runner = ScriptRankingRefreshRunner::new(tmp.path().to_path_buf()).with_bin(bin);
        let outcome = runner.refresh();
        let RefreshOutcome::Failure(reason) = outcome else {
            panic!("expected Failure");
        };
        assert!(reason.contains("tokens directory missing"), "{reason}");
    }

    #[test]
    fn test_refresh_invokes_tokens_check_ranking_argv() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().to_path_buf();
        let expected_workspace = repo_root.display().to_string();
        // Fail unless invoked with the exact `tokens check --ranking
        // --workspace <repo_root>` argv, proving the runner passes the
        // arguments the module docs promise (issue #4080).
        let body = format!(
            "[ \"$1\" = tokens ] && [ \"$2\" = check ] && [ \"$3\" = --ranking ] && \
             [ \"$4\" = --workspace ] && [ \"$5\" = \"{expected_workspace}\" ] && exit 0 || exit 1"
        );
        let bin = write_fake_bin(tmp.path(), "fake-daemon.sh", &body);
        let mut runner = ScriptRankingRefreshRunner::new(repo_root).with_bin(bin);
        assert_eq!(runner.refresh(), RefreshOutcome::Success);
    }

    #[test]
    fn test_refresh_times_out_on_hung_bin() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = write_fake_bin(tmp.path(), "fake-daemon.sh", "sleep 30");
        let mut runner = ScriptRankingRefreshRunner::new(tmp.path().to_path_buf())
            .with_bin(bin)
            .with_timeout(Duration::from_millis(300));
        let outcome = runner.refresh();
        let RefreshOutcome::Failure(reason) = outcome else {
            panic!("expected Failure");
        };
        assert!(reason.contains("timed out"), "{reason}");
    }

    #[test]
    fn test_refresh_spawn_failure_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        // A binary path that does not exist and was never `.exists()`-checked
        // because it's an explicit override — spawn itself must fail cleanly.
        let bogus = tmp.path().join("does-not-exist.sh");
        let mut runner = ScriptRankingRefreshRunner::new(tmp.path().to_path_buf()).with_bin(bogus);
        let outcome = runner.refresh();
        assert!(!outcome.is_success());
    }

    // ===================================================================
    // Config surface — autonomous.tokenRankingRefresh
    // ===================================================================

    #[test]
    fn test_config_missing_file_is_default() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            read_token_ranking_refresh_config(tmp.path()),
            TokenRankingRefreshConfig::default()
        );
    }

    #[test]
    fn test_config_malformed_json_is_default() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), "{not valid json");
        assert_eq!(
            read_token_ranking_refresh_config(tmp.path()),
            TokenRankingRefreshConfig::default()
        );
    }

    #[test]
    fn test_config_missing_block_is_default() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"workFinder": {"enabled": true}}}"#);
        assert_eq!(
            read_token_ranking_refresh_config(tmp.path()),
            TokenRankingRefreshConfig::default()
        );
    }

    #[test]
    fn test_config_reads_enabled_and_interval() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous": {"tokenRankingRefresh": {"enabled": false, "intervalSecs": 120}}}"#,
        );
        assert_eq!(
            read_token_ranking_refresh_config(tmp.path()),
            TokenRankingRefreshConfig {
                enabled: Some(false),
                interval_secs: Some(120),
            }
        );
    }

    #[test]
    fn test_config_zero_interval_is_dropped_to_none() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"tokenRankingRefresh": {"intervalSecs": 0}}}"#);
        assert_eq!(read_token_ranking_refresh_config(tmp.path()).interval_secs, None);
    }

    // ===================================================================
    // config_resolver migration (#4058) — tier precedence
    // ===================================================================

    fn write_project_config(root: &Path, contents: &str) {
        let full = root.join(crate::config_resolver::PROJECT_CONFIG_REL);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, contents).unwrap();
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_project_tier_only_is_honored_like_legacy() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_project_config(
            tmp.path(),
            r#"{"autonomous": {"tokenRankingRefresh": {"enabled": false, "intervalSecs": 120}}}"#,
        );
        let cfg = read_token_ranking_refresh_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(
            cfg,
            TokenRankingRefreshConfig {
                enabled: Some(false),
                interval_secs: Some(120),
            }
        );
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_project_tier_overrides_legacy_overlap_and_supplies_non_overlap() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous": {"tokenRankingRefresh": {"enabled": true, "intervalSecs": 600}}}"#,
        );
        write_project_config(
            tmp.path(),
            r#"{"autonomous": {"tokenRankingRefresh": {"intervalSecs": 300}}}"#,
        );
        let cfg = read_token_ranking_refresh_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        // Overlapping `intervalSecs` -> project tier wins.
        assert_eq!(cfg.interval_secs, Some(300));
        // Non-overlapping `enabled` still supplied by legacy tier.
        assert_eq!(cfg.enabled, Some(true));
    }

    // ===================================================================
    // Precedence — env > config > default
    // ===================================================================

    #[test]
    #[serial]
    fn test_resolve_enabled_default_is_true() {
        std::env::remove_var(TOKEN_RANKING_REFRESH_ENABLE_ENV);
        assert!(
            resolve_enabled(&TokenRankingRefreshConfig::default()),
            "absent config + unset env ⇒ default ON (this loop is default-on, unlike work-finder/gate)"
        );
    }

    #[test]
    #[serial]
    fn test_resolve_enabled_config_can_disable() {
        std::env::remove_var(TOKEN_RANKING_REFRESH_ENABLE_ENV);
        assert!(!resolve_enabled(&TokenRankingRefreshConfig {
            enabled: Some(false),
            interval_secs: None,
        }));
        assert!(resolve_enabled(&TokenRankingRefreshConfig {
            enabled: Some(true),
            interval_secs: None,
        }));
    }

    #[test]
    #[serial]
    fn test_resolve_enabled_env_overrides_config() {
        // Env forces OFF even when config says on.
        std::env::set_var(TOKEN_RANKING_REFRESH_ENABLE_ENV, "0");
        assert!(!resolve_enabled(&TokenRankingRefreshConfig {
            enabled: Some(true),
            interval_secs: None,
        }));
        // Env forces ON even when config says off.
        std::env::set_var(TOKEN_RANKING_REFRESH_ENABLE_ENV, "1");
        assert!(resolve_enabled(&TokenRankingRefreshConfig {
            enabled: Some(false),
            interval_secs: None,
        }));
        std::env::remove_var(TOKEN_RANKING_REFRESH_ENABLE_ENV);
    }

    #[test]
    #[serial]
    fn test_resolve_enabled_truthy_and_falsy_values() {
        for v in ["1", "true", "yes", "on", "TRUE", " On "] {
            std::env::set_var(TOKEN_RANKING_REFRESH_ENABLE_ENV, v);
            assert!(resolve_enabled(&TokenRankingRefreshConfig::default()), "{v:?} should enable");
        }
        for v in ["0", "false", "no", "off", "", "garbage"] {
            std::env::set_var(TOKEN_RANKING_REFRESH_ENABLE_ENV, v);
            assert!(
                !resolve_enabled(&TokenRankingRefreshConfig::default()),
                "{v:?} should disable"
            );
        }
        std::env::remove_var(TOKEN_RANKING_REFRESH_ENABLE_ENV);
    }

    #[test]
    #[serial]
    fn test_resolve_interval_default_config_and_env_precedence() {
        std::env::remove_var(TOKEN_RANKING_REFRESH_INTERVAL_ENV);

        // Absent config + unset env ⇒ built-in default.
        assert_eq!(
            resolve_interval(&TokenRankingRefreshConfig::default()),
            Duration::from_secs(DEFAULT_TOKEN_RANKING_REFRESH_INTERVAL_SECS)
        );

        // Config alone sets the cadence.
        assert_eq!(
            resolve_interval(&TokenRankingRefreshConfig {
                enabled: None,
                interval_secs: Some(300),
            }),
            Duration::from_secs(300)
        );

        // Env overrides config.
        std::env::set_var(TOKEN_RANKING_REFRESH_INTERVAL_ENV, "45");
        assert_eq!(
            resolve_interval(&TokenRankingRefreshConfig {
                enabled: None,
                interval_secs: Some(300),
            }),
            Duration::from_secs(45)
        );

        // Zero/unparseable env falls through to config, not the default.
        std::env::set_var(TOKEN_RANKING_REFRESH_INTERVAL_ENV, "0");
        assert_eq!(
            resolve_interval(&TokenRankingRefreshConfig {
                enabled: None,
                interval_secs: Some(300),
            }),
            Duration::from_secs(300)
        );
        std::env::set_var(TOKEN_RANKING_REFRESH_INTERVAL_ENV, "garbage");
        assert_eq!(
            resolve_interval(&TokenRankingRefreshConfig {
                enabled: None,
                interval_secs: Some(300),
            }),
            Duration::from_secs(300)
        );

        std::env::remove_var(TOKEN_RANKING_REFRESH_INTERVAL_ENV);
    }

    // ===================================================================
    // Loop wiring — a scripted fake runner proves ticks + panics behave
    // ===================================================================

    /// A fake [`RankingRefreshRunner`] that records call count and returns a
    /// scripted sequence of outcomes (repeating the last entry once exhausted).
    struct FakeRunner {
        outcomes: Vec<RefreshOutcome>,
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl RankingRefreshRunner for FakeRunner {
        fn refresh(&mut self) -> RefreshOutcome {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.outcomes.get(n).cloned().unwrap_or_else(|| {
                self.outcomes
                    .last()
                    .cloned()
                    .unwrap_or(RefreshOutcome::Success)
            })
        }
    }

    /// Issue #4044: these tests wait on a genuine `spawn_blocking` OS thread
    /// (not process exec), but a saturated host scheduler can still starve
    /// that thread past a short deadline — a false red, not a code fault.
    /// The polling helpers below return the instant the condition is met, so
    /// a generous ceiling costs nothing on a healthy host.
    const SCHEDULER_WAIT: Duration = Duration::from_secs(90);

    /// Poll `calls` (real wall-clock time — `refresh()` runs on a genuine
    /// `spawn_blocking` OS thread, so a paused tokio clock can't stand in for
    /// waiting on it) until it reaches `target`, or panic after `timeout`.
    async fn wait_for_calls(
        calls: &std::sync::atomic::AtomicUsize,
        target: usize,
        timeout: Duration,
    ) {
        let deadline = Instant::now() + timeout;
        loop {
            if calls.load(std::sync::atomic::Ordering::SeqCst) >= target {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for call count to reach {target} (saw {})",
                calls.load(std::sync::atomic::Ordering::SeqCst)
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn test_loop_refreshes_immediately_without_skipping_first_tick() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let runner = FakeRunner {
            outcomes: vec![RefreshOutcome::Success; 3],
            calls: calls.clone(),
        };
        // A short real interval so the test completes quickly without needing
        // a paused virtual clock (which can't observe a genuine spawn_blocking
        // OS thread completing).
        let handle = spawn_token_ranking_refresh_task(runner, Duration::from_millis(20));

        wait_for_calls(&calls, 1, SCHEDULER_WAIT).await;
        wait_for_calls(&calls, 3, SCHEDULER_WAIT).await;

        handle.abort();
    }

    #[tokio::test]
    async fn test_loop_stops_cleanly_when_runner_panics() {
        struct PanicOnceRunner;
        impl RankingRefreshRunner for PanicOnceRunner {
            fn refresh(&mut self) -> RefreshOutcome {
                panic!("boom");
            }
        }
        let handle = spawn_token_ranking_refresh_task(PanicOnceRunner, Duration::from_millis(20));
        // The loop should observe the panicked blocking task and return
        // (rather than hang or propagate the panic to the caller).
        let result = tokio::time::timeout(SCHEDULER_WAIT, handle).await;
        assert!(result.is_ok(), "loop task should finish (not hang) after the runner panics");
    }
}
