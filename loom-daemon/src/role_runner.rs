//! Autonomous periodic support-role runner — dispatches the standalone
//! support roles (Champion, Curator, Judge, Auditor, Guide) host-side through
//! `spawn-claude.sh`, drawing from the same rotated, health-ranked token pool
//! sweeps already use, instead of GitHub Actions cron with a static
//! `CLAUDE_API_KEY` secret (issue #4015).
//!
//! # Why
//!
//! Before this module the periodic support roles ran ONLY as GitHub Actions
//! cron jobs (`.github/workflows/loom-*.yml`, Phase 2a of epic #3372/#3375),
//! authenticating with a single static `ANTHROPIC_API_KEY` secret with no
//! rotation and no health-awareness. Sweeps, by contrast, run host-side via
//! [`crate::sweep_registry`], which selects a token from the rotated pool
//! (`.loom/tokens/`, ranked via claude-monitor) and automatically skips
//! exhausted/blocked accounts. That split meant an operator had to provision
//! *two* separate token systems for the same underlying `claude -p "/role"`
//! invocation — and a deployment with no `CLAUDE_API_KEY` secret had its
//! entire backlog-grooming pipeline (Curator/Guide/Auditor/standalone
//! Champion) silently dead even though sweeps ran fine on the rotated pool
//! (the incident that filed #4015).
//!
//! Precise scope (per the issue's verified-history comment): the *per-sweep*
//! lifecycle roles (Judge/Doctor/Champion-merge dispatched **inside** a
//! `/loom:sweep`) already run host-side on the rotated pool via
//! [`crate::sweep_registry`] and are unaffected by this module. This module
//! targets the **standalone periodic** roles that only ever had the GitHub
//! Actions cron path: Champion, Curator, Judge, Auditor, Guide (mirroring the
//! table in `.github/workflows/loom-*.yml` / CLAUDE.md "Scheduled Support
//! Roles"). The GitHub Actions workflows remain a supported fallback for
//! deployments with no always-on daemon — this module does not remove them,
//! it gives an always-on daemon host a better primary path.
//!
//! # Shape (mirrors [`crate::token_ranking_refresh`] / [`crate::work_finder`])
//!
//! Per enabled role, on its own configurable cadence, the daemon shells out to
//! `spawn-claude.sh -p "/<role>" --dangerously-skip-permissions` in the target
//! workspace — the same launcher [`crate::sweep_registry`] uses for sweep
//! children, so the role draws a token via the identical 3-tier selection
//! (ranking -> allowlist -> random) and appears in the same
//! `.loom/tokens/.bad_tokens` / `.ranking` accounting as sweeps.
//!
//! - **Opt-in** ([`ROLE_RUNNER_ENABLE_ENV`], default OFF) — like
//!   [`crate::work_finder`] and [`crate::main_health_gate`], this loop has
//!   dispatch-affecting side effects (spawning a full `claude` session that
//!   can mutate issues/PRs on the forge), so an absent daemon config leaves
//!   the daemon's behavior byte-for-byte unchanged.
//! - **Config** read from `.loom/config.json` -> `autonomous.roleRunner` with
//!   the same soft-fail pattern as every other `autonomous.*` surface
//!   (missing file / malformed JSON / missing block all resolve to
//!   "env-var / built-in default").
//! - **Precedence env > config > default** for `enabled`, the role subset,
//!   and the cadence.
//! - **One task per role**, each with its own ticker at that role's resolved
//!   interval (defaults mirror the commented-out `cron:` schedules in
//!   `.github/workflows/loom-*.yml`: champion 10m, curator 5m, judge 5m,
//!   auditor 10m, guide 15m) — so a fast-cadence role (curator) is not forced
//!   onto a slow role's tick.
//! - **Multi-workspace** ([`spawn_multi_role_task`]): re-reads the workspace
//!   registry each tick and, for every registered repo that has this role
//!   enabled, runs one invocation — exactly like
//!   [`crate::token_ranking_refresh::spawn_multi_token_ranking_refresh_task`].
//!   An empty registry reduces to the single `fallback_root`.
//! - The invocation runs on a blocking thread via `tokio::task::spawn_blocking`
//!   (it shells out to a whole `claude -p` session) so it never parks a
//!   runtime worker.
//!
//! # Never fatal, first tick skipped
//!
//! A failed invocation (script missing, non-zero exit, timeout) is logged and
//! skipped — it never panics the loop or the daemon; the next tick tries
//! again. Unlike the read-only token-ranking refresh, this loop mirrors
//! [`crate::work_finder`] / [`crate::main_health_gate`] in skipping the first
//! tick: a role invocation has real dispatch side effects (it can flip
//! labels, comment, merge), so firing every enabled role's session
//! immediately at daemon boot would needlessly burst several concurrent
//! `claude` sessions at once rather than settling into the steady-state
//! cadence.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::sweep_registry::{self, SweepRegistryConfig};
use crate::workspace_registry::WorkspaceRegistry;

// ============================================================================
// Constants
// ============================================================================

/// Environment variable enabling the role-runner loop.
///
/// Opt-in — unset or a false-y value keeps it OFF (byte-for-byte unchanged
/// daemon behavior), because the loop spawns full `claude` sessions that can
/// mutate issues/PRs on the forge. Set to `1`/`true`/`yes`/`on`
/// (case-insensitive) to enable.
pub const ROLE_RUNNER_ENABLE_ENV: &str = "LOOM_ROLE_RUNNER";

/// Environment variable overriding EVERY enabled role's tick interval
/// (seconds), uniformly. Per-role cadence diversity still comes from
/// [`RoleSpec::default_interval_secs`] / `autonomous.roleRunner.intervalSecs`
/// when this is unset.
pub const ROLE_RUNNER_INTERVAL_ENV: &str = "LOOM_ROLE_RUNNER_INTERVAL_SECS";

/// How long to wait for one role invocation (a full `claude -p "/<role>"`
/// session) before killing it. Generous — a role tick can involve several
/// forge round-trips (list/enrich/label issues, review PRs) — but bounded so
/// a wedged session can't block that role's loop forever.
const DEFAULT_ROLE_TIMEOUT: Duration = Duration::from_secs(1800);

/// Poll granularity while waiting for a role invocation to finish.
const INVOCATION_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Grace period after SIGTERM before escalating to SIGKILL on timeout.
const TERMINATE_GRACE: Duration = Duration::from_secs(5);

/// Max bytes of captured invocation output retained in a failure log line.
const MAX_OUTPUT_TAIL_BYTES: usize = 2048;

/// A `Success` outcome faster than this is implausible for a real
/// `claude -p "/<role>"` session — starting the process, authenticating, and
/// making at least one forge round-trip (list/enrich/label an issue, review a
/// PR) takes longer than this in practice. The incident that filed #4034 was
/// a silent no-op (the prompt matched no real slash command) that still
/// exited 0 in ~1.4s and was logged as a healthy `Success`. A tick this fast
/// is logged at `WARN` instead of `INFO` so that failure mode is visible in
/// the log without inspecting forge state.
const IMPLAUSIBLY_FAST_TICK: Duration = Duration::from_secs(10);

/// One standalone support role this module knows how to dispatch: its name
/// (used for config/env lookups and the per-role log file), the `/role`
/// slash-command prompt passed to `claude -p`, and its default tick interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoleSpec {
    /// Short name (e.g. `"champion"`), matched against
    /// `autonomous.roleRunner.roles` entries.
    pub name: &'static str,
    /// The `/role` prompt passed to `claude -p`.
    pub prompt: &'static str,
    /// Default tick interval in seconds when no config/env override applies.
    pub default_interval_secs: u64,
}

/// The standalone periodic support roles this module dispatches, with
/// defaults mirroring the commented-out `cron:` schedules in
/// `.github/workflows/loom-*.yml` (CLAUDE.md "Scheduled Support Roles"
/// table). Deliberately excludes Builder/Doctor (never run standalone —
/// dispatched inside a sweep) and does not touch the per-sweep Judge/Champion
/// invocations `sweep_registry` already handles.
///
/// Each `prompt` is the **namespaced** slash command (`/loom:<role>`), not
/// the bare `/<role>` form — the installed commands live under
/// `.claude/commands/loom/<role>.md` and are only resolved under that
/// namespace (there are no top-level, unnamespaced command files). A bare
/// `/curator` etc. matches no real command, so `claude -p` falls back to
/// treating it as an ordinary prompt: it answers briefly and exits 0, which
/// the runner faithfully — and wrongly — logs as `Success` (issue #4034).
/// This mirrors the existing hardcoded-literal precedent in
/// `sweep_registry.rs` (`format!("/loom:sweep {issue}")`) rather than
/// deriving/configuring the namespace: it is a settled, deliberate install
/// layout, not a per-install variable.
pub const DEFAULT_ROLES: &[RoleSpec] = &[
    RoleSpec {
        name: "champion",
        prompt: "/loom:champion",
        default_interval_secs: 600,
    },
    RoleSpec {
        name: "curator",
        prompt: "/loom:curator",
        default_interval_secs: 300,
    },
    RoleSpec {
        name: "judge",
        prompt: "/loom:judge",
        default_interval_secs: 300,
    },
    RoleSpec {
        name: "auditor",
        prompt: "/loom:auditor",
        default_interval_secs: 600,
    },
    RoleSpec {
        name: "guide",
        prompt: "/loom:guide",
        default_interval_secs: 900,
    },
];

// ============================================================================
// Outcome + runner (testable via a trait, mirrors token_ranking_refresh)
// ============================================================================

/// The result of one role invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoleTickOutcome {
    /// The invocation ran to completion with a zero exit code.
    Success,
    /// The invocation could not be run, or ran and reported failure. Never
    /// fatal to the daemon — logged and skipped.
    Failure(String),
}

impl RoleTickOutcome {
    /// True for a completed, successful invocation.
    #[must_use]
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success)
    }
}

/// Runs one role invocation. Abstracted behind a trait so the loop is
/// testable with a scripted fake, exactly as
/// [`crate::token_ranking_refresh::RankingRefreshRunner`] makes its loop
/// testable.
pub trait RoleInvocationRunner {
    /// Invoke `role` (whose `/role` prompt is `prompt`) once and return the
    /// outcome. Never panics — a spawn failure, timeout, or non-zero exit is
    /// a [`RoleTickOutcome::Failure`], never a propagated error.
    fn invoke(&mut self, role: &str, prompt: &str) -> RoleTickOutcome;
}

/// The concrete [`RoleInvocationRunner`]: shells out to
/// `spawn-claude.sh -p "<prompt>" --dangerously-skip-permissions` in
/// `workspace_root` — the same launcher [`crate::sweep_registry`] uses for
/// sweep children, so role invocations draw from the identical rotated token
/// pool and appear in the same accounting.
pub struct ScriptRoleInvocationRunner {
    workspace_root: PathBuf,
    /// Explicit script override (tests point this at a fake executable).
    /// Production leaves this `None` and resolves via
    /// [`SweepRegistryConfig::resolve_spawn_bin`] — the same resolution
    /// sweeps use.
    spawn_bin: Option<PathBuf>,
    timeout: Duration,
}

impl ScriptRoleInvocationRunner {
    /// Construct a runner for `workspace_root` with the production timeout.
    #[must_use]
    pub fn new(workspace_root: PathBuf) -> Self {
        Self {
            workspace_root,
            spawn_bin: None,
            timeout: DEFAULT_ROLE_TIMEOUT,
        }
    }

    /// Override the spawn binary (tests only).
    #[must_use]
    pub fn with_spawn_bin(mut self, bin: PathBuf) -> Self {
        self.spawn_bin = Some(bin);
        self
    }

    /// Override the invocation timeout (tests only).
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn resolve_spawn_bin(&self) -> Result<PathBuf, String> {
        if let Some(p) = &self.spawn_bin {
            return Ok(p.clone());
        }
        let mut cfg = SweepRegistryConfig::new(self.workspace_root.clone());
        cfg.spawn_bin = None;
        cfg.resolve_spawn_bin().map_err(|e| e.to_string())
    }

    /// Directory holding per-role log files: `<workspace_root>/.loom/logs`.
    fn logs_dir(&self) -> PathBuf {
        self.workspace_root.join(".loom").join("logs")
    }
}

impl RoleInvocationRunner for ScriptRoleInvocationRunner {
    fn invoke(&mut self, role: &str, prompt: &str) -> RoleTickOutcome {
        let script = match self.resolve_spawn_bin() {
            Ok(p) => p,
            Err(e) => return RoleTickOutcome::Failure(e),
        };
        run_role_with_timeout(
            &script,
            &self.workspace_root,
            role,
            prompt,
            self.logs_dir(),
            self.timeout,
        )
    }
}

/// Run `spawn-claude.sh -p "<prompt>" --dangerously-skip-permissions` in
/// `workspace_root`, appending combined output to
/// `<logs_dir>/role-<role>.log` (never a pipe — avoids the pipe-buffer
/// deadlock pattern documented in [`crate::main_health_gate`] /
/// [`crate::token_ranking_refresh`]) and killing it after `timeout`.
#[allow(clippy::too_many_arguments)]
fn run_role_with_timeout(
    script: &Path,
    workspace_root: &Path,
    role: &str,
    prompt: &str,
    logs_dir: PathBuf,
    timeout: Duration,
) -> RoleTickOutcome {
    if let Err(e) = std::fs::create_dir_all(&logs_dir) {
        return RoleTickOutcome::Failure(format!(
            "could not create logs dir {}: {e}",
            logs_dir.display()
        ));
    }
    let log_path = logs_dir.join(format!("role-{role}.log"));

    {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            let _ = writeln!(
                f,
                "\n==== loom-daemon role_runner: {} role={role} ====",
                chrono::Utc::now().to_rfc3339()
            );
        }
    }

    let out_file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(f) => f,
        Err(e) => {
            return RoleTickOutcome::Failure(format!(
                "could not open log {}: {e}",
                log_path.display()
            ))
        }
    };
    let stderr_file = match out_file.try_clone() {
        Ok(f) => f,
        Err(e) => return RoleTickOutcome::Failure(format!("could not clone log handle: {e}")),
    };

    let mut cmd = Command::new(script);
    cmd.arg("-p")
        .arg(prompt)
        .arg("--dangerously-skip-permissions")
        .current_dir(workspace_root)
        .env(sweep_registry::WORKSPACE_ENV, workspace_root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(stderr_file));

    // Forward LOOM_PACKAGE_PATH so a consumer-repo dispatch can still locate
    // the `loom_tools` Python package for token selection (issue #3949) —
    // mirrors `sweep_registry::spawn_child`'s treatment exactly.
    if let Some(pkg_path) = sweep_registry::resolve_package_path_env() {
        cmd.env(sweep_registry::PACKAGE_PATH_ENV, pkg_path);
    }

    // Run the child as its own process-group leader so a timeout can tear
    // down the whole subtree (the `claude` session's tool-call
    // subprocesses), not just the top-level `spawn-claude.sh` PID — mirrors
    // `sweep_registry::spawn_child`'s `process_group(0)` treatment.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return RoleTickOutcome::Failure(format!("could not spawn `{}`: {e}", script.display()))
        }
    };
    let pid = child.id();

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return RoleTickOutcome::Success,
            Ok(Some(status)) => {
                let tail = tail_of_file(&log_path);
                return RoleTickOutcome::Failure(format!(
                    "`{}` exited with {status}: {tail}",
                    script.display()
                ));
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    return terminate_timed_out(&mut child, pid, script);
                }
                std::thread::sleep(INVOCATION_POLL_INTERVAL);
            }
            Err(e) => {
                return RoleTickOutcome::Failure(format!(
                    "could not poll `{}`: {e}",
                    script.display()
                ))
            }
        }
    }
}

/// SIGTERM the timed-out child's process group, give it [`TERMINATE_GRACE`]
/// to exit, then SIGKILL the group and reap. Never panics.
fn terminate_timed_out(child: &mut Child, pid: u32, script: &Path) -> RoleTickOutcome {
    send_group_signal(pid, 15);
    let grace_start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if grace_start.elapsed() >= TERMINATE_GRACE {
                    send_group_signal(pid, 9);
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                std::thread::sleep(INVOCATION_POLL_INTERVAL);
            }
            Err(_) => break,
        }
    }
    RoleTickOutcome::Failure(format!("`{}` timed out (pid {pid} terminated)", script.display()))
}

/// Send `sig` to the process GROUP led by `pgid` (mirrors
/// `sweep_registry::send_group_signal` — duplicated here in miniature rather
/// than exposed cross-module, since this module's only need is "best-effort
/// tear down a timed-out invocation", not the full cancel-lifecycle
/// bookkeeping `sweep_registry` owns). `pgid == 0` is rejected: `kill(0,
/// sig)` would target the *daemon's own* group.
#[cfg(unix)]
fn send_group_signal(pgid: u32, sig: i32) -> bool {
    if pgid == 0 {
        return false;
    }
    let Ok(pgid_t): Result<i32, _> = pgid.try_into() else {
        return false;
    };
    // SAFETY: kill(2) with a negative pid targets the process group; this is
    // a documented POSIX signal-delivery call with no memory-safety concerns.
    unsafe { extern_kill(-pgid_t, sig) == 0 }
}

#[cfg(not(unix))]
fn send_group_signal(_pgid: u32, _sig: i32) -> bool {
    false
}

#[cfg(unix)]
extern "C" {
    #[link_name = "kill"]
    fn extern_kill(pid: i32, sig: i32) -> i32;
}

/// Read the last [`MAX_OUTPUT_TAIL_BYTES`] of `path` for a failure log line.
fn tail_of_file(path: &Path) -> String {
    let s = std::fs::read_to_string(path).unwrap_or_default();
    truncate_tail(&s)
}

/// Truncate captured output to the last [`MAX_OUTPUT_TAIL_BYTES`] bytes (the
/// failure detail is usually last), trimmed of surrounding whitespace.
fn truncate_tail(s: &str) -> String {
    if s.len() <= MAX_OUTPUT_TAIL_BYTES {
        return s.trim().to_string();
    }
    let start = s.len() - MAX_OUTPUT_TAIL_BYTES;
    let start = (start..s.len())
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(s.len());
    s[start..].trim().to_string()
}

// ============================================================================
// Config (.loom/config.json -> autonomous.roleRunner)
// ============================================================================

/// The subset of `.loom/config.json -> autonomous.roleRunner` this module
/// consumes. Each field is `Option` so an absent key falls through to the
/// env-var / built-in-default resolution — precedence is **env > config >
/// default** for every knob, matching every other `autonomous.*` surface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoleRunnerConfig {
    /// `autonomous.roleRunner.enabled` — whether to run the loop at all.
    pub enabled: Option<bool>,
    /// `autonomous.roleRunner.roles` — the subset of [`DEFAULT_ROLES`] (by
    /// name) to dispatch. `None` (key absent) runs every default role;
    /// `Some(vec![])` (explicit empty array) runs none.
    pub roles: Option<Vec<String>>,
    /// `autonomous.roleRunner.intervalSecs` — a single override applied
    /// uniformly to every enabled role's cadence (a zero/invalid value is
    /// dropped to `None`, falling through to that role's own default).
    pub interval_secs: Option<u64>,
}

/// Read `.loom/config.json -> autonomous.roleRunner`, soft-failing every
/// field to `None` (env/default resolution) on any of: missing file,
/// malformed JSON, or a missing `autonomous` / `roleRunner` block. Mirrors
/// the soft-fail contract of
/// [`crate::token_ranking_refresh::read_token_ranking_refresh_config`].
#[must_use]
pub fn read_role_runner_config(repo_root: &Path) -> RoleRunnerConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(block) = crate::config_resolver::get_path(&effective, "autonomous.roleRunner") else {
        return RoleRunnerConfig::default();
    };

    let roles = block
        .get("roles")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        });

    RoleRunnerConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        roles,
        interval_secs: block
            .get("intervalSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
    }
}

/// Resolve whether the loop is enabled with precedence **env > config >
/// default(false)**. When [`ROLE_RUNNER_ENABLE_ENV`] is *set* (to any value)
/// it decides (truthy enables, anything else disables); when unset the
/// config `enabled` flag decides; absent config leaves it off (opt-in, zero
/// behavior change).
#[must_use]
pub fn resolve_enabled(config: &RoleRunnerConfig) -> bool {
    if let Ok(v) = std::env::var(ROLE_RUNNER_ENABLE_ENV) {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config.enabled.unwrap_or(false)
}

/// Resolve the set of roles to dispatch: `config.roles` (by name, matched
/// against [`DEFAULT_ROLES`], preserving [`DEFAULT_ROLES`] order and ignoring
/// unknown names with a warning) when present, else every entry in
/// [`DEFAULT_ROLES`].
#[must_use]
pub fn resolve_roles(config: &RoleRunnerConfig) -> Vec<RoleSpec> {
    let Some(names) = &config.roles else {
        return DEFAULT_ROLES.to_vec();
    };
    let mut out = Vec::new();
    for spec in DEFAULT_ROLES {
        if names.iter().any(|n| n == spec.name) {
            out.push(*spec);
        }
    }
    for name in names {
        if !DEFAULT_ROLES.iter().any(|s| s.name == name) {
            log::warn!(
                "role_runner: autonomous.roleRunner.roles entry {name:?} is not a known standalone \
                 role (expected one of {:?}) — ignored",
                DEFAULT_ROLES.iter().map(|s| s.name).collect::<Vec<_>>()
            );
        }
    }
    out
}

/// Resolve a single role's tick interval with precedence **env
/// ([`ROLE_RUNNER_INTERVAL_ENV`], applied uniformly to every role) > config
/// (`autonomous.roleRunner.intervalSecs`, also uniform) > that role's own
/// [`RoleSpec::default_interval_secs`]**.
#[must_use]
pub fn resolve_interval_for_role(spec: &RoleSpec, config: &RoleRunnerConfig) -> Duration {
    std::env::var(ROLE_RUNNER_INTERVAL_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(config.interval_secs)
        .map_or_else(|| Duration::from_secs(spec.default_interval_secs), Duration::from_secs)
}

// ============================================================================
// Runtime wiring
// ============================================================================

/// Spawn the role-runner loop for a single role on a single workspace on the
/// shared daemon runtime. Intended for tests; production uses
/// [`spawn_multi_role_task`] (the multi-workspace entry point wired into
/// `main.rs`).
///
/// Mirrors [`crate::work_finder::spawn_work_finder_task`] /
/// [`crate::main_health_gate`]: the **first tick is skipped** so several
/// role loops starting at daemon boot don't burst several `claude` sessions
/// at once — see the module docs.
pub fn spawn_role_task<R>(
    mut runner: R,
    spec: RoleSpec,
    interval: Duration,
    drain: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> tokio::task::JoinHandle<()>
where
    R: RoleInvocationRunner + Send + 'static,
{
    log::info!("role_runner: starting {} loop (interval={}s)", spec.name, interval.as_secs());
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // skip immediate first tick (see module docs)
        loop {
            ticker.tick().await;
            // Scheduled drain (#4090): role ticks have no sweep-registry entry to
            // await, so a drain cannot wait for an in-flight tick — but it MUST
            // stop new ticks from *starting* (e.g. a Champion mid-merge). Skip
            // the whole tick while draining.
            if drain.load(std::sync::atomic::Ordering::Relaxed) {
                log::debug!(
                    "role_runner: {} tick skipped — drain in progress (no new role dispatch)",
                    spec.name
                );
                continue;
            }
            let name = spec.name;
            let prompt = spec.prompt;
            let tick_start = Instant::now();
            let joined = tokio::task::spawn_blocking(move || {
                let outcome = runner.invoke(name, prompt);
                (outcome, runner)
            })
            .await;
            let elapsed = tick_start.elapsed();
            match joined {
                Ok((outcome, r)) => {
                    runner = r;
                    log_outcome(spec.name, &outcome, elapsed);
                }
                Err(e) => {
                    log::error!(
                        "role_runner: {} invocation task panicked ({e}); stopping this role's loop",
                        spec.name
                    );
                    return;
                }
            }
        }
    })
}

/// Spawn the **multi-workspace** role-runner loop for one role (mirrors
/// [`crate::token_ranking_refresh::spawn_multi_token_ranking_refresh_task`])
/// on the shared daemon runtime.
///
/// Every `interval` it re-reads [`WorkspaceRegistry::effective_roots`]
/// against `fallback_root` (an **empty** registry yields the single
/// `fallback_root`) and, for each registered root whose own
/// `.loom/config.json` has this role enabled (`resolve_enabled` AND the role
/// name present in `resolve_roles` — precedence env > config > default), runs
/// one invocation. Invocations run **sequentially** per tick (no shared
/// mutable state to leak across repos, and it avoids bursting concurrent
/// `claude` sessions across every registered repo at once).
pub fn spawn_multi_role_task(
    spec: RoleSpec,
    fallback_root: PathBuf,
    interval: Duration,
    drain: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    log::info!(
        "role_runner: starting {} multi-workspace loop (interval={}s)",
        spec.name,
        interval.as_secs()
    );
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // skip immediate first tick (see module docs)
        loop {
            ticker.tick().await;

            // Scheduled drain (#4090): stop starting new role ticks across every
            // workspace while a drain is in progress (Finding 2 — role ticks are
            // not in the sweep registry, so the drain cannot await them, but it
            // must not let a fresh Champion/Curator tick start mid-roll).
            if drain.load(std::sync::atomic::Ordering::Relaxed) {
                log::debug!(
                    "role_runner: {} multi-workspace tick skipped — drain in progress",
                    spec.name
                );
                continue;
            }

            let roots = WorkspaceRegistry::load_default()
                .unwrap_or_else(|e| {
                    log::warn!(
                        "role_runner: could not load workspace registry ({e}); using fallback"
                    );
                    WorkspaceRegistry::default()
                })
                .effective_roots(&fallback_root);

            for root in roots {
                let config = read_role_runner_config(&root);
                if !resolve_enabled(&config) {
                    log::debug!(
                        "role_runner: {} disabled for {} (autonomous.roleRunner.enabled=false or \
                         LOOM_ROLE_RUNNER unset-falsy) — skipping",
                        spec.name,
                        root.display()
                    );
                    continue;
                }
                if !resolve_roles(&config).iter().any(|r| r.name == spec.name) {
                    log::debug!(
                        "role_runner: {} not in autonomous.roleRunner.roles for {} — skipping",
                        spec.name,
                        root.display()
                    );
                    continue;
                }
                let root_for_task = root.clone();
                let name = spec.name;
                let prompt = spec.prompt;
                let tick_start = Instant::now();
                let joined = tokio::task::spawn_blocking(move || {
                    let mut runner = ScriptRoleInvocationRunner::new(root_for_task);
                    runner.invoke(name, prompt)
                })
                .await;
                let elapsed = tick_start.elapsed();
                match joined {
                    Ok(outcome) => log_outcome_for_root(spec.name, &root, &outcome, elapsed),
                    Err(e) => log::error!(
                        "role_runner: {} invocation task for {} panicked ({e}); continuing to the \
                         next repo",
                        spec.name,
                        root.display()
                    ),
                }
            }
        }
    })
}

/// True when `outcome` is a [`RoleTickOutcome::Success`] that completed
/// faster than [`IMPLAUSIBLY_FAST_TICK`] — the signal that distinguishes a
/// genuine no-op-that-reports-success (issue #4034: a slash-command prompt
/// that did not resolve, so `claude -p` answered a one-off prompt and exited
/// 0 in ~1.4s) from a healthy tick. A real `claude -p "/<role>"` session
/// cannot start, authenticate, and do real forge work that quickly. Pulled
/// out of the two `log_outcome*` functions so the threshold logic is
/// unit-testable without capturing `log` crate output.
#[must_use]
fn tick_is_implausibly_fast(outcome: &RoleTickOutcome, elapsed: Duration) -> bool {
    matches!(outcome, RoleTickOutcome::Success) && elapsed < IMPLAUSIBLY_FAST_TICK
}

/// Log a single-workspace invocation outcome, including elapsed tick
/// duration. Never escalates to `error!` — a role-invocation failure is never
/// fatal to the daemon. See [`tick_is_implausibly_fast`] for the `WARN`
/// escalation on a suspiciously-fast `Success`.
fn log_outcome(role: &str, outcome: &RoleTickOutcome, elapsed: Duration) {
    match outcome {
        RoleTickOutcome::Success if tick_is_implausibly_fast(outcome, elapsed) => {
            log::warn!(
                "role_runner: {role} tick completed in {elapsed:.1?} — implausibly fast for a \
                 real session (threshold {IMPLAUSIBLY_FAST_TICK:.0?}); this may be a no-op that \
                 exited 0 without doing real work (e.g. a slash-command prompt that did not \
                 resolve)"
            );
        }
        RoleTickOutcome::Success => {
            log::info!("role_runner: {role} tick completed in {elapsed:.1?}");
        }
        RoleTickOutcome::Failure(reason) => {
            log::warn!(
                "role_runner: {role} tick failed after {elapsed:.1?} (logged and skipped, never \
                 fatal): {reason}"
            );
        }
    }
}

/// Root-aware variant of [`log_outcome`] for the multi-workspace loop.
fn log_outcome_for_root(role: &str, root: &Path, outcome: &RoleTickOutcome, elapsed: Duration) {
    match outcome {
        RoleTickOutcome::Success if tick_is_implausibly_fast(outcome, elapsed) => {
            log::warn!(
                "role_runner: {role} tick completed for {} in {elapsed:.1?} — implausibly fast \
                 for a real session (threshold {IMPLAUSIBLY_FAST_TICK:.0?}); this may be a no-op \
                 that exited 0 without doing real work (e.g. a slash-command prompt that did not \
                 resolve)",
                root.display()
            );
        }
        RoleTickOutcome::Success => {
            log::info!(
                "role_runner: {role} tick completed for {} in {elapsed:.1?}",
                root.display()
            );
        }
        RoleTickOutcome::Failure(reason) => log::warn!(
            "role_runner: {role} tick failed for {} after {elapsed:.1?} (logged and skipped, \
             never fatal): {reason}",
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

    /// A fake script that just exits with a fixed code, optionally writing to
    /// stdout/stderr first. Written with a shebang so it's directly
    /// executable — mirrors `token_ranking_refresh`'s test helper.
    fn write_fake_script(dir: &Path, name: &str, body: &str) -> PathBuf {
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
    // ScriptRoleInvocationRunner — resolution + execution
    // ===================================================================

    #[test]
    fn test_resolve_spawn_bin_missing_is_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let mut runner = ScriptRoleInvocationRunner::new(tmp.path().to_path_buf());
        let outcome = runner.invoke("curator", "/curator");
        assert!(!outcome.is_success());
        let RoleTickOutcome::Failure(reason) = outcome else {
            panic!("expected Failure");
        };
        assert!(reason.contains("spawn-claude.sh not found"), "{reason}");
    }

    #[test]
    fn test_invoke_success_on_zero_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_fake_script(tmp.path(), "fake-spawn.sh", "echo ok; exit 0");
        let mut runner =
            ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
        assert_eq!(runner.invoke("curator", "/curator"), RoleTickOutcome::Success);
    }

    #[test]
    fn test_invoke_failure_on_nonzero_exit_includes_output_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_fake_script(tmp.path(), "fake-spawn.sh", "echo boom detail; exit 1");
        let mut runner =
            ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
        let outcome = runner.invoke("curator", "/curator");
        let RoleTickOutcome::Failure(reason) = outcome else {
            panic!("expected Failure");
        };
        assert!(reason.contains("boom detail"), "{reason}");
    }

    #[test]
    fn test_invoke_receives_prompt_and_skip_permissions_flag() {
        let tmp = tempfile::tempdir().unwrap();
        // Fail unless invoked with -p "/curator" --dangerously-skip-permissions.
        let script = write_fake_script(
            tmp.path(),
            "fake-spawn.sh",
            "[ \"$1\" = \"-p\" ] && [ \"$2\" = \"/curator\" ] && [ \"$3\" = \"--dangerously-skip-permissions\" ] && exit 0 || exit 1",
        );
        let mut runner =
            ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
        assert_eq!(runner.invoke("curator", "/curator"), RoleTickOutcome::Success);
    }

    #[test]
    fn test_invoke_writes_per_role_log_file() {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_fake_script(tmp.path(), "fake-spawn.sh", "echo hello-from-role; exit 0");
        let mut runner =
            ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
        assert_eq!(runner.invoke("curator", "/curator"), RoleTickOutcome::Success);
        let log_path = tmp
            .path()
            .join(".loom")
            .join("logs")
            .join("role-curator.log");
        let contents = fs::read_to_string(log_path).unwrap();
        assert!(contents.contains("hello-from-role"), "{contents}");
    }

    #[test]
    fn test_invoke_times_out_on_hung_script() {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_fake_script(tmp.path(), "fake-spawn.sh", "sleep 30");
        let mut runner = ScriptRoleInvocationRunner::new(tmp.path().to_path_buf())
            .with_spawn_bin(script)
            .with_timeout(Duration::from_millis(300));
        let outcome = runner.invoke("curator", "/curator");
        let RoleTickOutcome::Failure(reason) = outcome else {
            panic!("expected Failure");
        };
        assert!(reason.contains("timed out"), "{reason}");
    }

    #[test]
    fn test_invoke_spawn_failure_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let bogus = tmp.path().join("does-not-exist.sh");
        let mut runner =
            ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(bogus);
        let outcome = runner.invoke("curator", "/curator");
        assert!(!outcome.is_success());
    }

    // ===================================================================
    // Config surface — autonomous.roleRunner
    // ===================================================================

    #[test]
    fn test_config_missing_file_is_default() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_role_runner_config(tmp.path()), RoleRunnerConfig::default());
    }

    #[test]
    fn test_config_malformed_json_is_default() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), "{not valid json");
        assert_eq!(read_role_runner_config(tmp.path()), RoleRunnerConfig::default());
    }

    #[test]
    fn test_config_missing_block_is_default() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"workFinder": {"enabled": true}}}"#);
        assert_eq!(read_role_runner_config(tmp.path()), RoleRunnerConfig::default());
    }

    #[test]
    fn test_config_reads_enabled_roles_and_interval() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous": {"roleRunner": {"enabled": true, "roles": ["curator", "guide"], "intervalSecs": 120}}}"#,
        );
        assert_eq!(
            read_role_runner_config(tmp.path()),
            RoleRunnerConfig {
                enabled: Some(true),
                roles: Some(vec!["curator".to_string(), "guide".to_string()]),
                interval_secs: Some(120),
            }
        );
    }

    #[test]
    fn test_config_zero_interval_is_dropped_to_none() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"intervalSecs": 0}}}"#);
        assert_eq!(read_role_runner_config(tmp.path()).interval_secs, None);
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
            r#"{"autonomous": {"roleRunner": {"enabled": true, "roles": ["curator"], "intervalSecs": 60}}}"#,
        );
        let cfg = read_role_runner_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(
            cfg,
            RoleRunnerConfig {
                enabled: Some(true),
                roles: Some(vec!["curator".to_string()]),
                interval_secs: Some(60),
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
            r#"{"autonomous": {"roleRunner": {"enabled": true, "intervalSecs": 120}}}"#,
        );
        write_project_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"intervalSecs": 30}}}"#);
        let cfg = read_role_runner_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        // Overlapping `intervalSecs` -> project tier wins.
        assert_eq!(cfg.interval_secs, Some(30));
        // Non-overlapping `enabled` still supplied by legacy tier.
        assert_eq!(cfg.enabled, Some(true));
    }

    // ===================================================================
    // resolve_roles
    // ===================================================================

    #[test]
    fn test_resolve_roles_absent_is_all_defaults() {
        assert_eq!(resolve_roles(&RoleRunnerConfig::default()), DEFAULT_ROLES.to_vec());
    }

    #[test]
    fn test_resolve_roles_empty_array_is_none() {
        let config = RoleRunnerConfig {
            enabled: None,
            roles: Some(vec![]),
            interval_secs: None,
        };
        assert_eq!(resolve_roles(&config), Vec::new());
    }

    #[test]
    fn test_resolve_roles_filters_and_preserves_default_order() {
        let config = RoleRunnerConfig {
            enabled: None,
            roles: Some(vec!["guide".to_string(), "champion".to_string()]),
            interval_secs: None,
        };
        let roles = resolve_roles(&config);
        assert_eq!(roles.iter().map(|r| r.name).collect::<Vec<_>>(), vec!["champion", "guide"]);
    }

    #[test]
    fn test_resolve_roles_ignores_unknown_names() {
        let config = RoleRunnerConfig {
            enabled: None,
            roles: Some(vec!["curator".to_string(), "not-a-role".to_string()]),
            interval_secs: None,
        };
        let roles = resolve_roles(&config);
        assert_eq!(roles.iter().map(|r| r.name).collect::<Vec<_>>(), vec!["curator"]);
    }

    // ===================================================================
    // Precedence — env > config > default
    // ===================================================================

    #[test]
    #[serial]
    fn test_resolve_enabled_default_is_false() {
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        assert!(!resolve_enabled(&RoleRunnerConfig::default()));
    }

    #[test]
    #[serial]
    fn test_resolve_enabled_config_can_enable() {
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        assert!(resolve_enabled(&RoleRunnerConfig {
            enabled: Some(true),
            roles: None,
            interval_secs: None,
        }));
    }

    #[test]
    #[serial]
    fn test_resolve_enabled_env_overrides_config() {
        std::env::set_var(ROLE_RUNNER_ENABLE_ENV, "0");
        assert!(!resolve_enabled(&RoleRunnerConfig {
            enabled: Some(true),
            roles: None,
            interval_secs: None,
        }));
        std::env::set_var(ROLE_RUNNER_ENABLE_ENV, "1");
        assert!(resolve_enabled(&RoleRunnerConfig {
            enabled: Some(false),
            roles: None,
            interval_secs: None,
        }));
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
    }

    #[test]
    #[serial]
    fn test_resolve_interval_for_role_precedence() {
        std::env::remove_var(ROLE_RUNNER_INTERVAL_ENV);
        let spec = DEFAULT_ROLES[0];

        // Absent config + unset env => the role's own built-in default.
        assert_eq!(
            resolve_interval_for_role(&spec, &RoleRunnerConfig::default()),
            Duration::from_secs(spec.default_interval_secs)
        );

        // Config sets a uniform override.
        assert_eq!(
            resolve_interval_for_role(
                &spec,
                &RoleRunnerConfig {
                    enabled: None,
                    roles: None,
                    interval_secs: Some(42)
                }
            ),
            Duration::from_secs(42)
        );

        // Env overrides config.
        std::env::set_var(ROLE_RUNNER_INTERVAL_ENV, "7");
        assert_eq!(
            resolve_interval_for_role(
                &spec,
                &RoleRunnerConfig {
                    enabled: None,
                    roles: None,
                    interval_secs: Some(42)
                }
            ),
            Duration::from_secs(7)
        );
        std::env::remove_var(ROLE_RUNNER_INTERVAL_ENV);
    }

    // ===================================================================
    // Loop wiring — a scripted fake runner proves ticks + panics behave
    // ===================================================================

    struct FakeRunner {
        outcomes: Vec<RoleTickOutcome>,
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl RoleInvocationRunner for FakeRunner {
        fn invoke(&mut self, _role: &str, _prompt: &str) -> RoleTickOutcome {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.outcomes.get(n).cloned().unwrap_or_else(|| {
                self.outcomes
                    .last()
                    .cloned()
                    .unwrap_or(RoleTickOutcome::Success)
            })
        }
    }

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
    async fn test_loop_ticks_repeatedly_skipping_first_tick() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let runner = FakeRunner {
            outcomes: vec![RoleTickOutcome::Success; 3],
            calls: calls.clone(),
        };
        let spec = RoleSpec {
            name: "curator",
            prompt: "/loom:curator",
            default_interval_secs: 1,
        };
        let drain = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handle = spawn_role_task(runner, spec, Duration::from_millis(20), drain);

        wait_for_calls(&calls, 1, Duration::from_secs(2)).await;
        wait_for_calls(&calls, 3, Duration::from_secs(2)).await;

        handle.abort();
    }

    /// A drain in progress (#4090) stops role ticks from *starting*: with the
    /// drain flag set before the loop runs, `spawn_role_task` performs ZERO
    /// `invoke` calls even after several tick intervals elapse. This is the
    /// highest-value new role-runner coverage (Finding 2 — role ticks had no
    /// halt gate at all before this).
    #[tokio::test]
    async fn test_drain_stops_role_ticks_from_starting() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let runner = FakeRunner {
            outcomes: vec![RoleTickOutcome::Success; 3],
            calls: calls.clone(),
        };
        let spec = RoleSpec {
            name: "champion",
            prompt: "/loom:champion",
            default_interval_secs: 1,
        };
        // Drain already engaged before the loop starts.
        let drain = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let handle = spawn_role_task(runner, spec, Duration::from_millis(20), drain.clone());

        // Let several tick intervals elapse; not a single invoke may fire.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no role tick may start while draining"
        );

        // Clearing the drain resumes dispatch — proving the gate, not a dead loop.
        drain.store(false, std::sync::atomic::Ordering::SeqCst);
        wait_for_calls(&calls, 1, Duration::from_secs(2)).await;

        handle.abort();
    }

    #[tokio::test]
    async fn test_loop_stops_cleanly_when_runner_panics() {
        struct PanicOnceRunner;
        impl RoleInvocationRunner for PanicOnceRunner {
            fn invoke(&mut self, _role: &str, _prompt: &str) -> RoleTickOutcome {
                panic!("boom");
            }
        }
        let spec = RoleSpec {
            name: "curator",
            prompt: "/loom:curator",
            default_interval_secs: 1,
        };
        let drain = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handle = spawn_role_task(PanicOnceRunner, spec, Duration::from_millis(20), drain);
        let result = tokio::time::timeout(Duration::from_secs(5), handle).await;
        assert!(result.is_ok(), "loop task should finish (not hang) after the runner panics");
    }

    // ===================================================================
    // DEFAULT_ROLES prompts — regression guard for #4034 (bare `/curator`
    // matches no real command; the installed commands are namespaced).
    // ===================================================================

    #[test]
    fn test_default_roles_prompts_are_namespaced() {
        for spec in DEFAULT_ROLES {
            let expected = format!("/loom:{}", spec.name);
            assert_eq!(
                spec.prompt, expected,
                "RoleSpec {:?} prompt must be the namespaced `/loom:<role>` command, not a bare \
                 `/<role>` (see #4034 — a bare prompt matches no installed slash command and \
                 silently no-ops)",
                spec.name
            );
        }
    }

    // ===================================================================
    // tick_is_implausibly_fast — #4034 AC #4 (a no-op success must be
    // distinguishable in the log from a real, slower tick).
    // ===================================================================

    #[test]
    fn test_implausibly_fast_success_is_flagged() {
        assert!(tick_is_implausibly_fast(
            &RoleTickOutcome::Success,
            Duration::from_millis(1400) // the observed #4034 incident duration
        ));
    }

    #[test]
    fn test_success_at_or_above_threshold_is_not_flagged() {
        assert!(!tick_is_implausibly_fast(&RoleTickOutcome::Success, IMPLAUSIBLY_FAST_TICK));
        assert!(!tick_is_implausibly_fast(
            &RoleTickOutcome::Success,
            IMPLAUSIBLY_FAST_TICK + Duration::from_secs(60)
        ));
    }

    #[test]
    fn test_failure_is_never_flagged_regardless_of_duration() {
        assert!(!tick_is_implausibly_fast(
            &RoleTickOutcome::Failure("boom".to_string()),
            Duration::from_millis(1)
        ));
    }
}
