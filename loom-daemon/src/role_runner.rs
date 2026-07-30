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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::sweep_registry::{self, SweepRegistryConfig};
use crate::workspace_registry::{filter_missing_roots, WorkspaceRegistry};

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

/// Minimum time between idle-edge-triggered runs of the **same** `(root, role)`
/// (#4364). The idle edge itself only fires on a non-idle → idle transition, so
/// a queue that stays empty never re-fires; this debounce is the second-line
/// guard against rapid idle/busy *flapping* (a queue that empties, refills, and
/// empties again within seconds) hot-looping a role. A constant, deliberately
/// not a config knob — the interval cadence is the tunable backstop.
const IDLE_TRIGGER_DEBOUNCE: Duration = Duration::from_secs(60);

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
    /// Explicit model override (tests only). Production leaves this `None` and
    /// resolves per invocation via [`resolve_role_runner_model`] — the same
    /// precedence chain sweep dispatch uses (issue #4501).
    model: Option<String>,
}

impl ScriptRoleInvocationRunner {
    /// Construct a runner for `workspace_root` with the production timeout.
    #[must_use]
    pub fn new(workspace_root: PathBuf) -> Self {
        Self {
            workspace_root,
            spawn_bin: None,
            timeout: DEFAULT_ROLE_TIMEOUT,
            model: None,
        }
    }

    /// Override the spawn binary (tests only).
    #[must_use]
    pub fn with_spawn_bin(mut self, bin: PathBuf) -> Self {
        self.spawn_bin = Some(bin);
        self
    }

    /// Override the resolved model (tests only) — bypasses
    /// [`resolve_role_runner_model`].
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
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
        // Issue #4501: pin the child's model instead of inheriting the account's
        // interactive CLI default (`fable` on the host that filed the issue,
        // where every role child burned the most constrained quota tier and then
        // died on "You've reached your Fable 5 limit").
        let (model, model_source) = match &self.model {
            Some(m) => (m.clone(), "override"),
            None => resolve_role_runner_model(&self.workspace_root),
        };
        run_role_with_timeout(
            &script,
            &self.workspace_root,
            role,
            prompt,
            self.logs_dir(),
            self.timeout,
            &model,
            model_source,
        )
    }
}

/// Issue #4501: resolve the model a role-runner child must run with, joining the
/// SAME precedence chain sweep dispatch uses
/// ([`sweep_registry::resolve_dispatch_model`]) with the role-runner-specific
/// `autonomous.roleRunner.model` occupying the "explicit request" tier:
///
/// **`autonomous.roleRunner.model` > `autonomous.model` > shipped
/// [`sweep_registry::DEFAULT_DISPATCH_MODEL`] (`sonnet`)**
///
/// Empty/whitespace values are treated as unset at every tier, so the resolved
/// model is never the empty string and never the CLI-inherited interactive
/// default. Returns the model plus a label naming the tier that supplied it (for
/// the per-role log header).
///
/// Before this, `run_role_with_timeout` emitted **no** `--model` argument at
/// all, so every scheduled curator/champion/judge/auditor/guide child inherited
/// whatever the selected account's interactive `claude` default happened to be —
/// the live defect this resolution exists to prevent.
#[must_use]
pub fn resolve_role_runner_model(repo_root: &Path) -> (String, &'static str) {
    let configured = read_role_runner_config(repo_root).model;
    let (model, source) = sweep_registry::resolve_dispatch_model(repo_root, configured.as_deref());
    let label = match source {
        // `Param` can only arise from `autonomous.roleRunner.model` here — this
        // function is the only caller and it passes exactly that value.
        sweep_registry::ModelSource::Param => "autonomous.roleRunner.model",
        sweep_registry::ModelSource::Config => "autonomous.model",
        sweep_registry::ModelSource::Default => "default",
    };
    (model, label)
}

/// Run `spawn-claude.sh -p "<prompt>" --model <model>
/// --dangerously-skip-permissions` in `workspace_root`, appending combined
/// output to `<logs_dir>/role-<role>.log` (never a pipe — avoids the pipe-buffer
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
    model: &str,
    model_source: &str,
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
            // The resolved model + the tier that supplied it are recorded in the
            // per-role log header (#4501) so an operator can confirm from
            // `role-<role>.log` alone which model a scheduled child ran with —
            // the manual verification this fix needs on a live host.
            let _ = writeln!(
                f,
                "\n==== loom-daemon role_runner: {} role={role} model={model} \
                 (source={model_source}) ====",
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
    cmd.arg("-p").arg(prompt);
    // Model pin (issue #4501): appended immediately after the prompt, exactly as
    // `sweep_registry::spawn_child` does, so a role child never inherits the
    // account's interactive CLI default (`fable` on the affected host — the most
    // constrained quota tier, and the escalation ceiling rather than the floor).
    // An empty value is treated as unset — `--model ""` must never be emitted —
    // mirroring the same guard on the sweep-dispatch path; `resolve_role_runner_model`
    // already filters blanks at every tier, so this is belt-and-braces.
    if !model.is_empty() {
        cmd.arg("--model").arg(model);
    }
    cmd.arg("--dangerously-skip-permissions");
    // Transient-error recovery (issue #4255): scheduled role spawns are the
    // same unattended class as daemon-dispatched sweeps, so route them through
    // `claude-wrapper.sh` (retry/backoff/classification, bounded by
    // `LOOM_MAX_RETRIES`) instead of running bare `claude` that dies on the
    // first transient API failure. `spawn-claude.sh` consumes `--use-wrapper`
    // (not forwarded to `claude`) and execs the wrapper. Operators can force
    // the legacy single-shot path with `LOOM_USE_WRAPPER=0`.
    if sweep_registry::wrapper_dispatch_enabled() {
        cmd.arg("--use-wrapper");
    }
    cmd.current_dir(workspace_root)
        .env(sweep_registry::WORKSPACE_ENV, workspace_root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(stderr_file));

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
    /// `autonomous.roleRunner.onIdle` — the subset of [`DEFAULT_ROLES`] (by
    /// name) to fire on the work-finder **idle edge** (#4364), in addition to
    /// (never replacing) the interval cadence. Unlike [`roles`](Self::roles),
    /// `None` (key absent) means **no** idle triggering — the opposite default,
    /// because idle firing is a distinct opt-in surface. Resolved by
    /// [`resolve_on_idle_roles`].
    pub on_idle: Option<Vec<String>>,
    /// `autonomous.roleRunner.model` — the model every role child is pinned to
    /// (issue #4501). `None` (key absent, blank, or non-string) falls through to
    /// `autonomous.model` and then the shipped
    /// [`sweep_registry::DEFAULT_DISPATCH_MODEL`]; it never falls through to the
    /// account's interactive CLI default. Resolved by
    /// [`resolve_role_runner_model`].
    pub model: Option<String>,
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

    // `onIdle` parses exactly like `roles` (array of strings; absent /
    // non-array soft-fails to `None`); non-string entries are dropped. Unknown
    // *names* are warned-and-ignored later, in `resolve_on_idle_roles`.
    let on_idle = block
        .get("onIdle")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        });

    // `model` (#4501): a blank / whitespace-only / non-string value soft-fails to
    // `None` so it falls through to `autonomous.model` -> the shipped default
    // rather than emitting `--model ""` or an inherited interactive default.
    let model = block
        .get("model")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(String::from);

    RoleRunnerConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        roles,
        interval_secs: block
            .get("intervalSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
        on_idle,
        model,
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

/// Resolve the set of roles to fire on the work-finder **idle edge** (#4364):
/// `config.on_idle` (by name, matched against [`DEFAULT_ROLES`], preserving
/// [`DEFAULT_ROLES`] order and ignoring unknown names with a warning) when
/// present, else **empty**.
///
/// This mirrors [`resolve_roles`] except for the absent-key default: `None`
/// resolves to no roles (not every default), because idle triggering is a
/// distinct opt-in — a repo that never sets `onIdle` gets the interval-only
/// behavior byte-for-byte.
#[must_use]
pub fn resolve_on_idle_roles(config: &RoleRunnerConfig) -> Vec<RoleSpec> {
    let Some(names) = &config.on_idle else {
        return Vec::new();
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
                "role_runner: autonomous.roleRunner.onIdle entry {name:?} is not a known \
                 standalone role (expected one of {:?}) — ignored",
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
// Idle-edge triggering (#4364) — shared in-progress guard + edge/debounce state
// ============================================================================

/// Shared "a role invocation is currently running" set, keyed by
/// `(workspace_root, role_name)`.
///
/// Shared (one instance, cloned) between the interval role loops
/// ([`spawn_multi_role_task`]) and the idle-edge-triggered path
/// ([`plan_idle_runs`]) so the two never overlap for the same `(root, role)`:
/// an interval tick holds the entry for the duration of its `invoke`, and the
/// idle path refuses to fire while the entry is present (and vice versa). This
/// is **in-process shared state only** — deliberately not an event-bus topic
/// (the taxonomy is frozen, #4364).
pub type InProgressGuard = Arc<Mutex<HashSet<(PathBuf, &'static str)>>>;

static ROLE_RUN_START_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Construct an empty [`InProgressGuard`]. One instance is created in `main.rs`
/// and cloned into every interval role loop and the work-finder's idle path so
/// they share a single view.
#[must_use]
pub fn new_in_progress_guard() -> InProgressGuard {
    Arc::new(Mutex::new(HashSet::new()))
}

/// Number of role invocations active across all managed workspaces.
#[must_use]
pub fn active_run_count(set: &InProgressGuard) -> usize {
    set.lock().unwrap_or_else(PoisonError::into_inner).len()
}

/// Monotonic process-wide count of successfully started role invocations.
///
/// Unlike an active-count sample, a generation change cannot miss a short role
/// that starts and finishes between idle-exit polling ticks.
#[must_use]
pub fn role_run_start_generation() -> u64 {
    ROLE_RUN_START_GENERATION.load(Ordering::Relaxed)
}

/// RAII guard: [`try_acquire`](Self::try_acquire) inserts `(root, role)` into
/// the shared [`InProgressGuard`]; [`Drop`] removes it.
///
/// Because removal runs in `Drop`, the entry is cleared on **every** exit path
/// of the invocation it guards — success, failure, timeout, or a panic
/// unwinding the task — so a wedged run can never leave a stale entry that
/// permanently blocks that role from ever running again.
pub struct RoleRunGuard {
    set: InProgressGuard,
    key: (PathBuf, &'static str),
}

impl RoleRunGuard {
    /// Try to mark `(root, role)` in progress. Returns `None` when it is
    /// already marked (another interval or idle run holds it) — the caller then
    /// skips rather than overlapping.
    #[must_use]
    pub fn try_acquire(set: InProgressGuard, root: PathBuf, role: &'static str) -> Option<Self> {
        let key = (root, role);
        {
            let mut guard = set.lock().unwrap_or_else(PoisonError::into_inner);
            if guard.contains(&key) {
                return None;
            }
            guard.insert(key.clone());
        }
        ROLE_RUN_START_GENERATION.fetch_add(1, Ordering::Relaxed);
        Some(Self { set, key })
    }
}

impl Drop for RoleRunGuard {
    fn drop(&mut self) {
        let mut guard = self.set.lock().unwrap_or_else(PoisonError::into_inner);
        guard.remove(&self.key);
    }
}

/// Per-workspace idle-edge + debounce state for the idle-triggered role runs
/// (#4364). Owned by the work-finder task (one per daemon) and fed one idle
/// observation per root per tick.
///
/// * **Edge, not level.** [`observe_edge`](Self::observe_edge) returns `true`
///   only on the per-root transition from non-idle to idle, so a queue that
///   stays empty across many ticks triggers at most once (on the entering
///   edge).
/// * **Boot counts as already-idle.** A root with no prior observation is
///   treated as already idle, so a daemon that boots on an empty queue does not
///   fire at startup — the same first-tick-skip discipline the interval loops
///   use.
/// * **Debounce.** [`debounce_ok`](Self::debounce_ok) enforces a minimum
///   [`IDLE_TRIGGER_DEBOUNCE`] between idle-triggered runs per `(root, role)`.
#[derive(Debug, Default)]
pub struct IdleTrigger {
    prev_idle: HashMap<PathBuf, bool>,
    last_fired: HashMap<(PathBuf, &'static str), Instant>,
    /// Roots for which a "disabled but onIdle configured" warning has
    /// already been emitted (#4377) — the idle-path equivalent of the
    /// interval loop's `missing_roots_warned` (#4326) dedup. Cleared for a
    /// root the moment its role runner resolves enabled again, so a later
    /// re-disable warns once more rather than staying silent forever.
    disabled_warned: HashSet<PathBuf>,
}

impl IdleTrigger {
    /// Construct an empty tracker (every root starts treated as already-idle).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record this tick's idle observation for `root` and return whether the
    /// idle EDGE (non-idle → idle) just fired. The first observation for a root
    /// treats the prior state as idle, so booting idle never fires.
    pub fn observe_edge(&mut self, root: &Path, idle_now: bool) -> bool {
        let prev = self.prev_idle.get(root).copied().unwrap_or(true);
        self.prev_idle.insert(root.to_path_buf(), idle_now);
        !prev && idle_now
    }

    /// Whether `(root, role)` is outside its debounce window — never fired, or
    /// the last idle-triggered run was at least [`IDLE_TRIGGER_DEBOUNCE`] ago.
    #[must_use]
    pub fn debounce_ok(&self, root: &Path, role: &'static str, now: Instant) -> bool {
        match self.last_fired.get(&(root.to_path_buf(), role)) {
            Some(&last) => now.duration_since(last) >= IDLE_TRIGGER_DEBOUNCE,
            None => true,
        }
    }

    /// Record that an idle-triggered run for `(root, role)` fired at `now`,
    /// starting its debounce window.
    pub fn record_fired(&mut self, root: &Path, role: &'static str, now: Instant) {
        self.last_fired.insert((root.to_path_buf(), role), now);
    }

    /// Whether a "disabled but onIdle configured" warning has already been
    /// recorded for `root` (#4377) — test-observable dedup state; also the
    /// hook a status/diagnostic surface could use without re-deriving it.
    #[must_use]
    pub fn disabled_warned(&self, root: &Path) -> bool {
        self.disabled_warned.contains(root)
    }
}

/// Decide which on-idle roles should fire for `root` right now, given this
/// tick's idle observation. Pure of any claude spawning (the caller does the
/// fire-and-forget invocation), so the edge / debounce / guard logic is
/// unit-testable without a real `claude` session.
///
/// Steps, in order:
/// 1. Record the idle edge (always — so the level state stays accurate even on
///    a tick that ends up not firing).
/// 2. Bail on no edge, or on an active scheduled drain (#4090).
/// 3. Bail when the role runner is disabled for this root
///    ([`resolve_enabled`], precedence env > config > default) — this is the
///    **per-root** gate (#4377): it is resolved from `root`'s own
///    `.loom/config.json`, independent of the daemon workspace's own master
///    switch, which only decides whether the loops start at all. When
///    `onIdle` roles are configured for `root` but the gate is off, this is
///    the silent-no-op the issue exists to fix — see
///    [`warn_if_idle_configured_but_disabled`].
/// 4. Per configured on-idle role ([`resolve_on_idle_roles`]): skip if inside
///    the debounce window, or if an interval / idle run already holds the
///    in-progress guard; else record the fire and acquire the guard.
///
/// The returned [`RoleRunGuard`]s must be held by the caller for the duration
/// of each fire-and-forget invocation (they clear the in-progress entry on
/// drop).
#[must_use]
pub fn plan_idle_runs(
    trigger: &mut IdleTrigger,
    in_progress: &InProgressGuard,
    root: &Path,
    config: &RoleRunnerConfig,
    idle_now: bool,
    draining: bool,
    now: Instant,
) -> Vec<(RoleSpec, RoleRunGuard)> {
    let edge = trigger.observe_edge(root, idle_now);
    if !edge {
        return Vec::new();
    }
    if draining {
        log::debug!(
            "role_runner: idle edge for {} suppressed — drain in progress (#4090)",
            root.display()
        );
        return Vec::new();
    }
    if !resolve_enabled(config) {
        warn_if_idle_configured_but_disabled(trigger, root, config);
        return Vec::new();
    }
    // The root is enabled again — clear any stale disabled-warning so a
    // later disable re-warns instead of staying silent forever (#4377).
    trigger.disabled_warned.remove(root);
    let mut out = Vec::new();
    for spec in resolve_on_idle_roles(config) {
        if !trigger.debounce_ok(root, spec.name, now) {
            log::debug!(
                "role_runner: idle edge for {} — {} within {}s debounce, skipping",
                root.display(),
                spec.name,
                IDLE_TRIGGER_DEBOUNCE.as_secs()
            );
            continue;
        }
        let Some(guard) =
            RoleRunGuard::try_acquire(in_progress.clone(), root.to_path_buf(), spec.name)
        else {
            log::debug!(
                "role_runner: idle edge for {} — {} run already in progress, skipping",
                root.display(),
                spec.name
            );
            continue;
        };
        trigger.record_fired(root, spec.name, now);
        out.push((spec, guard));
    }
    out
}

/// Emit a warn-once-per-root line (#4377) when an idle edge fires for `root`
/// while `onIdle` roles are configured there but the role runner is disabled
/// for that root (`resolve_enabled` false). Before this the idle path bailed
/// with **no log at any level** — every neighboring bail (drain, debounce,
/// in-progress guard) already logs at `debug!`, so this was the fully-silent
/// gap: a registered workspace with `onIdle` set but no
/// `autonomous.roleRunner.enabled: true` in its own `.loom/config.json` got
/// zero ticks and zero diagnostics.
///
/// A root with **no** `onIdle` roles configured stays silent here — disabled
/// is that root's normal, unconfigured state, not a misconfiguration worth
/// flagging on every idle edge. Dedup state lives on [`IdleTrigger`] (see
/// [`IdleTrigger::disabled_warned`]) and is cleared the moment the root
/// resolves enabled again ([`plan_idle_runs`]), so a later re-disable warns
/// once more rather than staying silent forever.
fn warn_if_idle_configured_but_disabled(
    trigger: &mut IdleTrigger,
    root: &Path,
    config: &RoleRunnerConfig,
) {
    let on_idle = resolve_on_idle_roles(config);
    if on_idle.is_empty() {
        return;
    }
    if !trigger.disabled_warned.insert(root.to_path_buf()) {
        return; // already warned for this root; stay quiet until it re-enables
    }
    log::warn!(
        "role_runner: idle edge fired for {} with onIdle roles {:?} configured, but the role \
         runner is disabled for this root (autonomous.roleRunner.enabled is false or absent in \
         {}'s own .loom/config.json) — these roles will never fire here until \
         autonomous.roleRunner.enabled=true is set in that root's own config; enablement is \
         resolved per registered root, not inherited from the daemon workspace's master switch \
         (#4377). This is a one-time warning for this root — see `loom-daemon status` for the \
         current per-root state.",
        root.display(),
        on_idle.iter().map(|r| r.name).collect::<Vec<_>>(),
        root.display(),
    );
}

/// Observe `root`'s post-tick idle state and, on the idle edge, fire-and-forget
/// each configured on-idle role (#4364) — the entry point the work-finder loop
/// calls once per root per tick.
///
/// Reads `root`'s own `.loom/config.json` (hot-apply, like the interval loops)
/// each tick and delegates the edge / debounce / guard decision to
/// [`plan_idle_runs`]. Each fired role runs as a detached `tokio::spawn` +
/// `spawn_blocking`, so this returns immediately — the work-finder tick NEVER
/// awaits a multi-minute role session. The in-progress guard for each run is
/// held for the whole invocation and cleared on every exit path.
pub fn observe_and_fire_idle(
    trigger: &mut IdleTrigger,
    in_progress: &InProgressGuard,
    root: &Path,
    idle_now: bool,
    draining: bool,
) {
    let config = read_role_runner_config(root);
    let plans =
        plan_idle_runs(trigger, in_progress, root, &config, idle_now, draining, Instant::now());
    for (spec, guard) in plans {
        let root_owned = root.to_path_buf();
        let name = spec.name;
        let prompt = spec.prompt;
        log::info!(
            "role_runner: idle edge for {} — firing idle-triggered {} run (#4364)",
            root.display(),
            name
        );
        tokio::spawn(async move {
            // Held for the whole invocation; the in-progress entry clears when
            // this guard drops (every exit path — success/failure/panic).
            let _guard = guard;
            let run_root = root_owned.clone();
            let tick_start = Instant::now();
            let joined = tokio::task::spawn_blocking(move || {
                ScriptRoleInvocationRunner::new(run_root).invoke(name, prompt)
            })
            .await;
            let elapsed = tick_start.elapsed();
            match joined {
                Ok(outcome) => log_outcome_for_root(name, &root_owned, &outcome, elapsed),
                Err(e) => log::error!(
                    "role_runner: idle-triggered {name} run for {} panicked ({e})",
                    root_owned.display()
                ),
            }
        });
    }
}

/// Whether the interval loop ([`spawn_multi_role_task`]) should log a `WARN`
/// (vs. a quieter, already-warned `DEBUG`) for `root` being disabled on this
/// tick (#4377): `true` the first time `root` is newly inserted into
/// `warned`, `false` on every subsequent tick until the caller removes it
/// (which it does once `root` resolves enabled again). Pulled out as a pure
/// function — mirroring [`classify_root_tick_log`] — so the warn-once dedup
/// is unit-testable without a running loop or captured log output.
#[must_use]
fn should_warn_disabled_root(warned: &mut HashSet<PathBuf>, root: &Path) -> bool {
    warned.insert(root.to_path_buf())
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
    root: PathBuf,
    in_progress: InProgressGuard,
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
            // Shared GitHub rate limit exhausted (#4429): a role session
            // spawned now would burn a token slot just to fail its own gh
            // calls against the same wall — skip until the window resets.
            if crate::rate_limit_breaker::global_is_suppressed() {
                log::debug!(
                    "role_runner: {} tick skipped — rate-limit cooldown (#4429)",
                    spec.name
                );
                continue;
            }
            let name = spec.name;
            let prompt = spec.prompt;
            // Shared in-progress guard (#4364): skip this interval tick if an
            // idle-triggered (or overlapping) run for the same (root, role) is
            // already active. Held for the whole invocation; cleared on drop.
            let Some(_run_guard) =
                RoleRunGuard::try_acquire(in_progress.clone(), root.clone(), name)
            else {
                log::debug!(
                    "role_runner: {} tick for {} skipped — a run is already in progress (#4364)",
                    name,
                    root.display()
                );
                continue;
            };
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
/// `fallback_root`), drops any root whose directory no longer exists on disk
/// via the shared [`filter_missing_roots`] hygiene (#4326/#4349 — warn once
/// per missing period, never auto-remove), and, for each surviving root
/// whose own `.loom/config.json` has this role enabled (`resolve_enabled`
/// AND the role name present in `resolve_roles` — precedence env > config >
/// default), runs one invocation. Invocations run **sequentially** per tick
/// (no shared mutable state to leak across repos, and it avoids bursting
/// concurrent `claude` sessions across every registered repo at once).
///
/// A repeatedly-failing root (e.g. a broken MCP preflight, #4349) logs once
/// on the fail edge and once on recovery — not once per tick — via a
/// per-root failing-state map tracked across ticks (mirrors the
/// `was_halted`/`was_pressured` state-change-dedup discipline in
/// [`crate::work_finder`]).
pub fn spawn_multi_role_task(
    spec: RoleSpec,
    fallback_root: PathBuf,
    interval: Duration,
    drain: std::sync::Arc<std::sync::atomic::AtomicBool>,
    in_progress: InProgressGuard,
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
                             // Missing-root warn-once-per-period state (#4326), shared discipline
                             // with `work_finder` via `filter_missing_roots`.
        let mut missing_roots_warned: HashSet<PathBuf> = HashSet::new();
        // Per-root failing state (#4349), so a persistently failing tick logs
        // only on the fail edge and on recovery, not every tick.
        let mut failing_roots: HashMap<PathBuf, bool> = HashMap::new();
        // Disabled-root warn-once state (#4377): the per-tick disabled-skip
        // below is otherwise only a `debug!` — invisible at the default `info`
        // level, so a registered root left disabled gets zero diagnostics.
        // Same warn-once-then-dedup shape as `missing_roots_warned`, but
        // without `filter_missing_roots`'s reset-every-tick semantics: an
        // entry here is cleared only when its root resolves enabled again
        // (see below), so re-disabling re-warns instead of staying silent.
        let mut disabled_roots_warned: HashSet<PathBuf> = HashSet::new();
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
            // Shared GitHub rate limit exhausted (#4429): a role session
            // spawned now would burn a token slot just to fail its own gh
            // calls against the same wall — skip until the window resets.
            if crate::rate_limit_breaker::global_is_suppressed() {
                log::debug!(
                    "role_runner: {} multi-workspace tick skipped — rate-limit cooldown (#4429)",
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
            // Skip registered roots whose directory no longer exists on disk
            // (#4326) so a dangling entry cannot burn every tick forever —
            // warn-and-skip, never auto-remove (`loom-daemon status` flags it,
            // `workspace remove` clears it).
            let roots = filter_missing_roots(roots, &mut missing_roots_warned);

            for root in roots {
                let config = read_role_runner_config(&root);
                if !resolve_enabled(&config) {
                    // Per-root gate (#4377): `enabled` is resolved from this
                    // root's own `.loom/config.json`, independent of the
                    // daemon workspace's master switch (which only decided
                    // whether this loop started at all). First sighting warns
                    // at `info`-visible `warn!`; repeats downgrade to
                    // `debug!` so a persistently-disabled root does not spam
                    // the log every tick forever.
                    if should_warn_disabled_root(&mut disabled_roots_warned, &root) {
                        log::warn!(
                            "role_runner: {} disabled for {} — autonomous.roleRunner.enabled is \
                             false or absent in that root's own .loom/config.json (enablement is \
                             resolved per registered root, not inherited from the daemon \
                             workspace's master switch, #4377); this root will receive zero {} \
                             ticks until autonomous.roleRunner.enabled=true is set there (see \
                             `loom-daemon status` for the current per-root state; further \
                             identical skips for this root are logged at DEBUG until it \
                             re-enables)",
                            spec.name,
                            root.display(),
                            spec.name
                        );
                    } else {
                        log::debug!(
                            "role_runner: {} disabled for {} (autonomous.roleRunner.enabled=false \
                             or LOOM_ROLE_RUNNER unset-falsy) — skipping (already warned above)",
                            spec.name,
                            root.display()
                        );
                    }
                    continue;
                }
                // The root resolved enabled again — clear any stale
                // disabled-warning so a later disable re-warns (#4377).
                disabled_roots_warned.remove(&root);
                if !resolve_roles(&config).iter().any(|r| r.name == spec.name) {
                    log::debug!(
                        "role_runner: {} not in autonomous.roleRunner.roles for {} — skipping",
                        spec.name,
                        root.display()
                    );
                    continue;
                }
                let name = spec.name;
                let prompt = spec.prompt;
                // Shared in-progress guard (#4364): skip this root's interval
                // tick when an idle-triggered (or overlapping) run for the same
                // (root, role) is already active. Held across the invocation;
                // cleared on drop (every exit path).
                let Some(_run_guard) =
                    RoleRunGuard::try_acquire(in_progress.clone(), root.clone(), name)
                else {
                    log::debug!(
                        "role_runner: {} tick for {} skipped — a run is already in progress \
                         (#4364)",
                        name,
                        root.display()
                    );
                    continue;
                };
                let root_for_task = root.clone();
                let tick_start = Instant::now();
                let joined = tokio::task::spawn_blocking(move || {
                    let mut runner = ScriptRoleInvocationRunner::new(root_for_task);
                    runner.invoke(name, prompt)
                })
                .await;
                let elapsed = tick_start.elapsed();
                match joined {
                    Ok(outcome) => log_outcome_for_root_deduped(
                        spec.name,
                        &root,
                        &outcome,
                        elapsed,
                        &mut failing_roots,
                    ),
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

/// Root-aware variant of [`log_outcome`] for the **fire-and-forget idle path**
/// ([`observe_and_fire_idle`], #4364). Unlike the repeating multi-workspace
/// interval loop — which uses [`log_outcome_for_root_deduped`] to suppress a
/// persistently-failing root's per-tick WARN noise (#4349) — an idle-triggered
/// run fires exactly once on a busy→idle *edge* and is dispatched as a detached
/// `tokio::spawn`. There is no repeating tick and no natural place to thread
/// the per-root `failing` dedup state through the detached task, so a single
/// plain (un-deduped) log line with root context is the correct, minimal fit
/// here. See #4376 for the design rationale.
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

/// The classified log action for one root's tick outcome, given whether that
/// root was already failing on the *previous* tick. Pulled out of
/// [`log_outcome_for_root_deduped`] as a pure function so the state-change
/// dedup logic (#4349) is unit-testable without capturing `log` crate output
/// — mirrors why [`tick_is_implausibly_fast`] was extracted the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootTickLogAction {
    /// Steady-state success: log at `INFO`, same as always.
    Success,
    /// Success, but implausibly fast: log at `WARN`, same as always.
    SuccessImplausiblyFast,
    /// Success immediately after a failing period: log once at `INFO` with
    /// an explicit "recovered" message (the edge back to healthy).
    Recovered,
    /// Success immediately after a failing period, but implausibly fast:
    /// log once at `WARN` combining both signals.
    RecoveredImplausiblyFast,
    /// First failure (edge into a failing period): log at `WARN`, same as
    /// always.
    FailureEdge,
    /// Repeat failure (already failing on the previous tick): downgrade to
    /// `DEBUG` — the identical failure no longer re-logs at `WARN` every
    /// tick forever (the #4349 symptom: a broken worktree's MCP preflight
    /// failing every 5-minute champion/curator tick with ERROR-level noise).
    FailureRepeat,
}

impl RootTickLogAction {
    /// Whether this action should mark the root as failing for the *next*
    /// tick's edge/repeat decision.
    #[must_use]
    fn is_failing(self) -> bool {
        matches!(self, Self::FailureEdge | Self::FailureRepeat)
    }
}

#[must_use]
fn classify_root_tick_log(
    outcome: &RoleTickOutcome,
    elapsed: Duration,
    was_failing: bool,
) -> RootTickLogAction {
    match outcome {
        RoleTickOutcome::Failure(_) if was_failing => RootTickLogAction::FailureRepeat,
        RoleTickOutcome::Failure(_) => RootTickLogAction::FailureEdge,
        RoleTickOutcome::Success if tick_is_implausibly_fast(outcome, elapsed) && was_failing => {
            RootTickLogAction::RecoveredImplausiblyFast
        }
        RoleTickOutcome::Success if tick_is_implausibly_fast(outcome, elapsed) => {
            RootTickLogAction::SuccessImplausiblyFast
        }
        RoleTickOutcome::Success if was_failing => RootTickLogAction::Recovered,
        RoleTickOutcome::Success => RootTickLogAction::Success,
    }
}

/// Root-aware, **state-change-deduped** variant of [`log_outcome`] for the
/// multi-workspace loop (#4349). `failing` tracks, per root, whether the
/// *previous* tick for that root ended in [`RoleTickOutcome::Failure`] — see
/// [`RootTickLogAction`] for the per-transition logging rules.
fn log_outcome_for_root_deduped(
    role: &str,
    root: &Path,
    outcome: &RoleTickOutcome,
    elapsed: Duration,
    failing: &mut HashMap<PathBuf, bool>,
) {
    let was_failing = failing.get(root).copied().unwrap_or(false);
    let action = classify_root_tick_log(outcome, elapsed, was_failing);
    let reason = match outcome {
        RoleTickOutcome::Failure(reason) => reason.as_str(),
        RoleTickOutcome::Success => "",
    };
    match action {
        RootTickLogAction::Success => {
            log::info!(
                "role_runner: {role} tick completed for {} in {elapsed:.1?}",
                root.display()
            );
        }
        RootTickLogAction::SuccessImplausiblyFast => {
            log::warn!(
                "role_runner: {role} tick completed for {} in {elapsed:.1?} — implausibly fast \
                 for a real session (threshold {IMPLAUSIBLY_FAST_TICK:.0?}); this may be a no-op \
                 that exited 0 without doing real work (e.g. a slash-command prompt that did not \
                 resolve)",
                root.display()
            );
        }
        RootTickLogAction::Recovered => {
            log::info!(
                "role_runner: {role} recovered for {} — tick completed in {elapsed:.1?} after a \
                 prior failing period",
                root.display()
            );
        }
        RootTickLogAction::RecoveredImplausiblyFast => {
            log::warn!(
                "role_runner: {role} tick for {} recovered from a failing period but completed \
                 in {elapsed:.1?} — implausibly fast for a real session (threshold \
                 {IMPLAUSIBLY_FAST_TICK:.0?}); this may be a no-op that exited 0 without doing \
                 real work",
                root.display()
            );
        }
        RootTickLogAction::FailureEdge => {
            log::warn!(
                "role_runner: {role} tick failed for {} after {elapsed:.1?} (logged and \
                 skipped, never fatal; further identical failures for this root are logged at \
                 DEBUG until it recovers): {reason}",
                root.display()
            );
        }
        RootTickLogAction::FailureRepeat => {
            log::debug!(
                "role_runner: {role} tick failed for {} again after {elapsed:.1?} (repeat of an \
                 already-logged failure; not re-warned every tick — see the fail-edge WARN \
                 above, or the eventual recovery INFO): {reason}",
                root.display()
            );
        }
    }
    failing.insert(root.to_path_buf(), action.is_failing());
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
        assert!(reason.contains("spawn-worker.sh not found"), "{reason}");
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
        // Fail unless invoked with
        //   -p "/curator" --model <m> --dangerously-skip-permissions
        // (the `--model` pin was inserted after the prompt by #4501, mirroring
        // `sweep_registry::spawn_child`'s argv order).
        let script = write_fake_script(
            tmp.path(),
            "fake-spawn.sh",
            "[ \"$1\" = \"-p\" ] && [ \"$2\" = \"/curator\" ] && [ \"$3\" = \"--model\" ] && [ -n \"$4\" ] && [ \"$5\" = \"--dangerously-skip-permissions\" ] && exit 0 || exit 1",
        );
        let mut runner =
            ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
        assert_eq!(runner.invoke("curator", "/curator"), RoleTickOutcome::Success);
    }

    /// Issue #4501: a role spawn pins the model explicitly — a role child must
    /// never inherit the account's interactive CLI default (`fable` on the host
    /// that filed the issue, where every child instantly died on "You've reached
    /// your Fable 5 limit"). With no config the pin is the shipped
    /// `DEFAULT_DISPATCH_MODEL` (`sonnet`).
    #[test]
    fn test_invoke_appends_resolved_model_defaulting_to_sonnet() {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_fake_script(
            tmp.path(),
            "fake-spawn.sh",
            "printf '%s\\n' \"$@\" > argv.txt; exit 0",
        );
        let mut runner =
            ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
        assert_eq!(runner.invoke("curator", "/loom:curator"), RoleTickOutcome::Success);
        let argv = fs::read_to_string(tmp.path().join("argv.txt")).unwrap();
        let args: Vec<&str> = argv.lines().collect();
        let idx = args
            .iter()
            .position(|a| *a == "--model")
            .expect("role spawn argv must contain --model");
        assert_eq!(
            args[idx + 1],
            sweep_registry::DEFAULT_DISPATCH_MODEL,
            "default role-runner model must be the shipped dispatch default; argv: {args:?}"
        );
        assert_ne!(args[idx + 1], "fable", "role children must never run fable by default");
    }

    /// Issue #4501: `autonomous.roleRunner.model` wins over the shipped default
    /// (and over `autonomous.model`) — the explicit-request tier of the shared
    /// `resolve_dispatch_model` chain.
    #[test]
    fn test_invoke_config_model_override_wins() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous": {"model": "opus", "roleRunner": {"enabled": true, "model": "claude-sonnet-4-6"}}}"#,
        );
        let script = write_fake_script(
            tmp.path(),
            "fake-spawn.sh",
            "printf '%s\\n' \"$@\" > argv.txt; exit 0",
        );
        let mut runner =
            ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
        assert_eq!(runner.invoke("curator", "/loom:curator"), RoleTickOutcome::Success);
        let argv = fs::read_to_string(tmp.path().join("argv.txt")).unwrap();
        assert!(
            argv.contains("--model\nclaude-sonnet-4-6\n"),
            "autonomous.roleRunner.model must win; argv: {argv}"
        );
    }

    /// Issue #4501: with only `autonomous.model` set, the role runner joins the
    /// SAME chain sweep dispatch uses rather than keeping a private default.
    #[test]
    fn test_resolve_role_runner_model_precedence_chain() {
        // No config at all -> shipped default, labelled `default`.
        let bare = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_role_runner_model(bare.path()),
            (sweep_registry::DEFAULT_DISPATCH_MODEL.to_string(), "default")
        );

        // `autonomous.model` only -> that value, labelled `autonomous.model`.
        // Routing through `resolve_dispatch_model` also means the role runner
        // inherits the #3982 logical-tier alias resolution for free
        // (`opus` -> `claude-opus-5`), exactly as sweep dispatch does.
        let shared = tempfile::tempdir().unwrap();
        write_config(shared.path(), r#"{"autonomous": {"model": "opus"}}"#);
        assert_eq!(
            resolve_role_runner_model(shared.path()),
            ("claude-opus-5".to_string(), "autonomous.model")
        );

        // Both -> the role-runner-specific value, labelled as such.
        let both = tempfile::tempdir().unwrap();
        write_config(
            both.path(),
            r#"{"autonomous": {"model": "opus", "roleRunner": {"model": "haiku"}}}"#,
        );
        assert_eq!(
            resolve_role_runner_model(both.path()),
            ("haiku".to_string(), "autonomous.roleRunner.model")
        );

        // A blank override is treated as unset at every tier (never `--model ""`).
        let blank = tempfile::tempdir().unwrap();
        write_config(blank.path(), r#"{"autonomous": {"roleRunner": {"model": "   "}}}"#);
        assert_eq!(read_role_runner_config(blank.path()).model, None);
        assert_eq!(
            resolve_role_runner_model(blank.path()),
            (sweep_registry::DEFAULT_DISPATCH_MODEL.to_string(), "default")
        );
    }

    /// Issue #4501: the per-role log header records the pinned model and the tier
    /// that supplied it, so an operator can verify the pin from
    /// `role-<role>.log` alone on a live host.
    #[test]
    fn test_invoke_log_header_records_pinned_model() {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_fake_script(tmp.path(), "fake-spawn.sh", "exit 0");
        let mut runner =
            ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
        assert_eq!(runner.invoke("guide", "/loom:guide"), RoleTickOutcome::Success);
        let log = fs::read_to_string(tmp.path().join(".loom").join("logs").join("role-guide.log"))
            .unwrap();
        assert!(
            log.contains(&format!(
                "model={} (source=default)",
                sweep_registry::DEFAULT_DISPATCH_MODEL
            )),
            "{log}"
        );
    }

    /// Issue #4255: a scheduled role spawn routes through `claude-wrapper.sh` by
    /// appending `--use-wrapper` after `--dangerously-skip-permissions`, so a
    /// transient API death is retried instead of killing the unattended role run
    /// on the first failure. Serialized on a named lock shared with the opt-out
    /// test so the `LOOM_USE_WRAPPER` env mutation cannot race it.
    #[test]
    #[serial(loom_use_wrapper_env)]
    fn test_invoke_appends_use_wrapper_flag() {
        std::env::remove_var("LOOM_USE_WRAPPER");
        let tmp = tempfile::tempdir().unwrap();
        // Succeeds only when --use-wrapper directly follows
        // --dangerously-skip-permissions (argv is now
        // `-p <prompt> --model <m> --dangerously-skip-permissions --use-wrapper`
        // since the #4501 model pin).
        let script = write_fake_script(
            tmp.path(),
            "fake-spawn.sh",
            "[ \"$5\" = \"--dangerously-skip-permissions\" ] && [ \"$6\" = \"--use-wrapper\" ] && exit 0 || exit 1",
        );
        let mut runner =
            ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
        assert_eq!(runner.invoke("curator", "/curator"), RoleTickOutcome::Success);
    }

    /// Issue #4255: the `LOOM_USE_WRAPPER=0` debug opt-out restores the legacy
    /// single-shot argv — argv ends at `--dangerously-skip-permissions` with no
    /// `--use-wrapper` token.
    #[test]
    #[serial(loom_use_wrapper_env)]
    fn test_invoke_opt_out_omits_use_wrapper_flag() {
        std::env::set_var("LOOM_USE_WRAPPER", "0");
        let tmp = tempfile::tempdir().unwrap();
        // Succeeds only when nothing follows --dangerously-skip-permissions
        // (argv ends there; the #4501 model pin shifted it to $5).
        let script = write_fake_script(
            tmp.path(),
            "fake-spawn.sh",
            "[ \"$5\" = \"--dangerously-skip-permissions\" ] && [ -z \"$6\" ] && exit 0 || exit 1",
        );
        let mut runner =
            ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
        let outcome = runner.invoke("curator", "/curator");
        std::env::remove_var("LOOM_USE_WRAPPER");
        assert_eq!(outcome, RoleTickOutcome::Success);
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
                on_idle: None,
                model: None,
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
                on_idle: None,
                model: None,
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
            on_idle: None,
            model: None,
        };
        assert_eq!(resolve_roles(&config), Vec::new());
    }

    #[test]
    fn test_resolve_roles_filters_and_preserves_default_order() {
        let config = RoleRunnerConfig {
            enabled: None,
            roles: Some(vec!["guide".to_string(), "champion".to_string()]),
            interval_secs: None,
            on_idle: None,
            model: None,
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
            on_idle: None,
            model: None,
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
            on_idle: None,
            model: None,
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
            on_idle: None,
            model: None,
        }));
        std::env::set_var(ROLE_RUNNER_ENABLE_ENV, "1");
        assert!(resolve_enabled(&RoleRunnerConfig {
            enabled: Some(false),
            roles: None,
            interval_secs: None,
            on_idle: None,
            model: None,
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
                    interval_secs: Some(42),
                    on_idle: None,
                    model: None,
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
                    interval_secs: Some(42),
                    on_idle: None,
                    model: None,
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
        let handle = spawn_role_task(
            runner,
            spec,
            Duration::from_millis(20),
            drain,
            PathBuf::from("/tmp/loom-test-root"),
            new_in_progress_guard(),
        );

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
        let handle = spawn_role_task(
            runner,
            spec,
            Duration::from_millis(20),
            drain.clone(),
            PathBuf::from("/tmp/loom-test-root"),
            new_in_progress_guard(),
        );

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
        let handle = spawn_role_task(
            PanicOnceRunner,
            spec,
            Duration::from_millis(20),
            drain,
            PathBuf::from("/tmp/loom-test-root"),
            new_in_progress_guard(),
        );
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

    // ===================================================================
    // onIdle config parsing (#4364)
    // ===================================================================

    #[test]
    fn test_config_on_idle_absent_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"enabled": true}}}"#);
        assert_eq!(read_role_runner_config(tmp.path()).on_idle, None);
    }

    #[test]
    fn test_config_on_idle_parses_array() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"onIdle": ["champion"]}}}"#);
        assert_eq!(read_role_runner_config(tmp.path()).on_idle, Some(vec!["champion".to_string()]));
    }

    #[test]
    fn test_config_on_idle_non_array_soft_fails_to_none() {
        let tmp = tempfile::tempdir().unwrap();
        // A non-array (string) value must not panic — it soft-fails to `None`,
        // matching the `roles` contract.
        write_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"onIdle": "champion"}}}"#);
        assert_eq!(read_role_runner_config(tmp.path()).on_idle, None);
    }

    #[test]
    fn test_config_on_idle_drops_non_string_entries() {
        let tmp = tempfile::tempdir().unwrap();
        // Non-string entries are dropped; string entries survive (unknown
        // *names* are filtered later in `resolve_on_idle_roles`).
        write_config(
            tmp.path(),
            r#"{"autonomous": {"roleRunner": {"onIdle": ["champion", 7, true]}}}"#,
        );
        assert_eq!(read_role_runner_config(tmp.path()).on_idle, Some(vec!["champion".to_string()]));
    }

    // ===================================================================
    // resolve_on_idle_roles (#4364)
    // ===================================================================

    #[test]
    fn test_resolve_on_idle_roles_absent_is_empty() {
        // Opposite default from `roles`: absent key means NO idle triggering.
        assert_eq!(resolve_on_idle_roles(&RoleRunnerConfig::default()), Vec::new());
    }

    #[test]
    fn test_resolve_on_idle_roles_parses_and_preserves_order() {
        let config = RoleRunnerConfig {
            enabled: None,
            roles: None,
            interval_secs: None,
            on_idle: Some(vec!["guide".to_string(), "champion".to_string()]),
            model: None,
        };
        let roles = resolve_on_idle_roles(&config);
        assert_eq!(roles.iter().map(|r| r.name).collect::<Vec<_>>(), vec!["champion", "guide"]);
    }

    #[test]
    fn test_resolve_on_idle_roles_ignores_unknown_names() {
        let config = RoleRunnerConfig {
            enabled: None,
            roles: None,
            interval_secs: None,
            on_idle: Some(vec![
                "champion".to_string(),
                "builder".to_string(),
                "nope".to_string(),
            ]),
            model: None,
        };
        let roles = resolve_on_idle_roles(&config);
        assert_eq!(roles.iter().map(|r| r.name).collect::<Vec<_>>(), vec!["champion"]);
    }

    #[test]
    fn test_resolve_on_idle_roles_empty_array_is_empty() {
        let config = RoleRunnerConfig {
            enabled: None,
            roles: None,
            interval_secs: None,
            on_idle: Some(vec![]),
            model: None,
        };
        assert_eq!(resolve_on_idle_roles(&config), Vec::new());
    }

    // ===================================================================
    // IdleTrigger — edge detection + debounce (#4364)
    // ===================================================================

    #[test]
    fn test_idle_trigger_boot_idle_does_not_fire() {
        let mut t = IdleTrigger::new();
        let root = Path::new("/tmp/loom-root-a");
        // First-ever observation is idle: boot on an empty queue must NOT fire.
        assert!(!t.observe_edge(root, true));
    }

    #[test]
    fn test_idle_trigger_fires_on_non_idle_to_idle_edge() {
        let mut t = IdleTrigger::new();
        let root = Path::new("/tmp/loom-root-b");
        // Boot idle (no fire), then busy, then idle => the edge fires exactly on
        // the busy → idle transition.
        assert!(!t.observe_edge(root, true));
        assert!(!t.observe_edge(root, false));
        assert!(t.observe_edge(root, true));
    }

    #[test]
    fn test_idle_trigger_does_not_refire_on_sustained_idle() {
        let mut t = IdleTrigger::new();
        let root = Path::new("/tmp/loom-root-c");
        assert!(!t.observe_edge(root, false)); // busy
        assert!(t.observe_edge(root, true)); // edge
                                             // Staying idle across N further ticks must not re-fire.
        assert!(!t.observe_edge(root, true));
        assert!(!t.observe_edge(root, true));
    }

    #[test]
    fn test_idle_trigger_no_fire_while_in_flight_then_fires_when_drained() {
        let mut t = IdleTrigger::new();
        let root = Path::new("/tmp/loom-root-d");
        // A tick that dispatched nothing but still has in-flight sweeps is
        // non-idle (not empty) — no edge; the edge fires on the later tick where
        // in-flight reaches zero.
        assert!(!t.observe_edge(root, false));
        assert!(!t.observe_edge(root, false));
        assert!(t.observe_edge(root, true));
    }

    #[test]
    fn test_idle_trigger_edge_is_per_root() {
        let mut t = IdleTrigger::new();
        let a = Path::new("/tmp/loom-root-e1");
        let b = Path::new("/tmp/loom-root-e2");
        // Drive root a busy→idle (edge) while b stays idle from boot (no edge).
        assert!(!t.observe_edge(a, false));
        assert!(!t.observe_edge(b, true));
        assert!(t.observe_edge(a, true)); // a fires
        assert!(!t.observe_edge(b, true)); // b never fired
    }

    #[test]
    fn test_idle_trigger_debounce_window() {
        let mut t = IdleTrigger::new();
        let root = Path::new("/tmp/loom-root-f");
        let t0 = Instant::now();
        // Never fired => outside the window.
        assert!(t.debounce_ok(root, "champion", t0));
        t.record_fired(root, "champion", t0);
        // Within 60s => debounced.
        assert!(!t.debounce_ok(root, "champion", t0 + Duration::from_secs(30)));
        assert!(!t.debounce_ok(root, "champion", t0 + Duration::from_secs(59)));
        // At/after 60s => allowed again.
        assert!(t.debounce_ok(root, "champion", t0 + IDLE_TRIGGER_DEBOUNCE));
        assert!(t.debounce_ok(root, "champion", t0 + Duration::from_secs(61)));
        // Debounce is per-role: a different role is unaffected.
        assert!(t.debounce_ok(root, "curator", t0 + Duration::from_secs(1)));
    }

    // ===================================================================
    // RoleRunGuard — in-progress overlap protection (#4364)
    // ===================================================================

    #[test]
    fn test_role_run_guard_blocks_second_acquire_then_releases_on_drop() {
        let set = new_in_progress_guard();
        let root = PathBuf::from("/tmp/loom-root-g");
        let g1 = RoleRunGuard::try_acquire(set.clone(), root.clone(), "champion");
        assert!(g1.is_some(), "first acquire should succeed");
        // Second acquire of the same (root, role) is refused while held.
        assert!(
            RoleRunGuard::try_acquire(set.clone(), root.clone(), "champion").is_none(),
            "second acquire of the same key must be refused"
        );
        // A different role on the same root is independent.
        assert!(RoleRunGuard::try_acquire(set.clone(), root.clone(), "curator").is_some());
        // Dropping the first guard clears the entry — a later acquire succeeds.
        drop(g1);
        assert!(
            RoleRunGuard::try_acquire(set, root, "champion").is_some(),
            "guard must clear its entry on drop"
        );
    }

    // ===================================================================
    // plan_idle_runs — the composed edge/drain/enabled/debounce/guard decision
    // ===================================================================

    fn on_idle_config(enabled: Option<bool>, roles: Vec<&str>) -> RoleRunnerConfig {
        RoleRunnerConfig {
            enabled,
            roles: None,
            interval_secs: None,
            on_idle: Some(roles.into_iter().map(str::to_string).collect()),
            model: None,
        }
    }

    #[test]
    #[serial]
    fn test_plan_idle_runs_fires_on_edge_when_enabled() {
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        let mut t = IdleTrigger::new();
        let set = new_in_progress_guard();
        let root = Path::new("/tmp/loom-plan-a");
        let cfg = on_idle_config(Some(true), vec!["champion"]);
        let now = Instant::now();
        // Boot idle: no edge, so no plan.
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, true, false, now).is_empty());
        // Go busy: no edge.
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, false, false, now).is_empty());
        // Busy → idle edge: champion fires (and its guard is now held).
        let plan = plan_idle_runs(&mut t, &set, root, &cfg, true, false, now);
        assert_eq!(plan.iter().map(|(s, _)| s.name).collect::<Vec<_>>(), vec!["champion"]);
    }

    #[test]
    #[serial]
    fn test_plan_idle_runs_drain_suppresses() {
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        let mut t = IdleTrigger::new();
        let set = new_in_progress_guard();
        let root = Path::new("/tmp/loom-plan-b");
        let cfg = on_idle_config(Some(true), vec!["champion"]);
        let now = Instant::now();
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, false, false, now).is_empty());
        // Edge present, but draining => suppressed.
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, true, true, now).is_empty());
    }

    #[test]
    #[serial]
    fn test_plan_idle_runs_disabled_suppresses() {
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        let mut t = IdleTrigger::new();
        let set = new_in_progress_guard();
        let root = Path::new("/tmp/loom-plan-c");
        let cfg = on_idle_config(Some(false), vec!["champion"]);
        let now = Instant::now();
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, false, false, now).is_empty());
        // Edge present, but role runner disabled => no fire.
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, true, false, now).is_empty());
        // #4377: onIdle is configured for this root, so the disabled-suppression
        // must be observable, not silent.
        assert!(t.disabled_warned(root), "onIdle configured + disabled must record a warning");
    }

    // ===================================================================
    // #4377 — idle-path disabled-suppression is visible, not silent
    // ===================================================================

    #[test]
    #[serial]
    fn test_plan_idle_runs_disabled_without_on_idle_does_not_warn() {
        // A root with no `onIdle` roles configured is disabled in its normal,
        // unconfigured state — not a misconfiguration, so no warning.
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        let mut t = IdleTrigger::new();
        let set = new_in_progress_guard();
        let root = Path::new("/tmp/loom-plan-no-onidle");
        let cfg = RoleRunnerConfig {
            enabled: Some(false),
            roles: None,
            interval_secs: None,
            on_idle: None,
            model: None,
        };
        let now = Instant::now();
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, false, false, now).is_empty());
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, true, false, now).is_empty());
        assert!(
            !t.disabled_warned(root),
            "no onIdle configured => disabled is normal, must not warn"
        );
    }

    #[test]
    #[serial]
    fn test_plan_idle_runs_disabled_warning_dedupes_across_repeated_edges() {
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        let mut t = IdleTrigger::new();
        let set = new_in_progress_guard();
        let root = Path::new("/tmp/loom-plan-dedupe");
        let cfg = on_idle_config(Some(false), vec!["champion"]);
        let t0 = Instant::now();
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, false, false, t0).is_empty());
        // First edge: disabled, onIdle configured => warns.
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, true, false, t0).is_empty());
        assert!(t.disabled_warned(root));
        // Flap busy -> idle again: still disabled; the warning stays deduped
        // (no observable way to detect a re-warn other than the state not
        // regressing — the log line itself is the thing that must not repeat).
        assert!(plan_idle_runs(
            &mut t,
            &set,
            root,
            &cfg,
            false,
            false,
            t0 + Duration::from_secs(5)
        )
        .is_empty());
        assert!(plan_idle_runs(
            &mut t,
            &set,
            root,
            &cfg,
            true,
            false,
            t0 + Duration::from_secs(10)
        )
        .is_empty());
        assert!(t.disabled_warned(root), "still deduped on the second edge");
    }

    #[test]
    #[serial]
    fn test_plan_idle_runs_disabled_warning_clears_once_enabled() {
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        let mut t = IdleTrigger::new();
        let set = new_in_progress_guard();
        let root = Path::new("/tmp/loom-plan-clears");
        let disabled_cfg = on_idle_config(Some(false), vec!["champion"]);
        let enabled_cfg = on_idle_config(Some(true), vec!["champion"]);
        let t0 = Instant::now();
        assert!(plan_idle_runs(&mut t, &set, root, &disabled_cfg, false, false, t0).is_empty());
        assert!(plan_idle_runs(&mut t, &set, root, &disabled_cfg, true, false, t0).is_empty());
        assert!(t.disabled_warned(root));

        // Root flips to enabled (hot-apply) well outside the debounce window.
        assert!(plan_idle_runs(
            &mut t,
            &set,
            root,
            &enabled_cfg,
            false,
            false,
            t0 + Duration::from_secs(70)
        )
        .is_empty());
        let fire = plan_idle_runs(
            &mut t,
            &set,
            root,
            &enabled_cfg,
            true,
            false,
            t0 + Duration::from_secs(80),
        );
        assert_eq!(fire.len(), 1, "enabled root must fire normally");
        assert!(
            !t.disabled_warned(root),
            "warned flag must clear once the root resolves enabled"
        );
    }

    /// Cross-config case (#4377 curated AC): a target root has `onIdle` set
    /// but its own per-root `enabled` is absent (resolves `false`) —
    /// independent of whatever the daemon's own workspace's master switch is
    /// set to (the master switch only decides whether these loops start at
    /// all, never a target root's own gate). `observe_and_fire_idle` is the
    /// real entry point the work-finder loop calls, reading the root's own
    /// on-disk config each tick — exercised here end-to-end rather than via
    /// the already-parsed `RoleRunnerConfig` the other tests use.
    #[test]
    #[serial]
    fn test_observe_and_fire_idle_cross_config_disabled_target_root_warns_and_suppresses() {
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"onIdle": ["champion"]}}}"#);
        let mut trigger = IdleTrigger::new();
        let in_progress = new_in_progress_guard();

        observe_and_fire_idle(&mut trigger, &in_progress, tmp.path(), true, false); // boot idle: no edge
        observe_and_fire_idle(&mut trigger, &in_progress, tmp.path(), false, false); // go busy: no edge
        observe_and_fire_idle(&mut trigger, &in_progress, tmp.path(), true, false); // busy -> idle edge

        assert!(
            trigger.disabled_warned(tmp.path()),
            "idle edge on a disabled-but-onIdle-configured root must record the warning"
        );
        assert!(
            in_progress.lock().unwrap().is_empty(),
            "a disabled root must never acquire/fire a run"
        );

        // A second flap must stay deduped — no panic, no re-fire, warned state
        // holds (this is the "second edge does not re-warn" acceptance case).
        observe_and_fire_idle(&mut trigger, &in_progress, tmp.path(), false, false);
        observe_and_fire_idle(&mut trigger, &in_progress, tmp.path(), true, false);
        assert!(trigger.disabled_warned(tmp.path()));
        assert!(in_progress.lock().unwrap().is_empty());
    }

    // ===================================================================
    // #4377 — interval-path disabled-root warn-once dedup
    // ===================================================================

    #[test]
    fn test_should_warn_disabled_root_warns_once_then_dedupes_until_reenable() {
        let mut warned: HashSet<PathBuf> = HashSet::new();
        let root = PathBuf::from("/tmp/loom-interval-disabled-root");
        assert!(
            should_warn_disabled_root(&mut warned, &root),
            "first sighting of a disabled root must warn"
        );
        assert!(
            !should_warn_disabled_root(&mut warned, &root),
            "repeat sighting must be deduped (downgraded to DEBUG by the caller)"
        );
        assert!(
            !should_warn_disabled_root(&mut warned, &root),
            "stays deduped across further ticks"
        );
        // Caller clears the entry once the root resolves enabled again.
        warned.remove(&root);
        assert!(
            should_warn_disabled_root(&mut warned, &root),
            "a re-disable after a re-enable must warn again"
        );
    }

    #[test]
    #[serial]
    fn test_plan_idle_runs_debounced_second_edge_then_fires_after_window() {
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        let mut t = IdleTrigger::new();
        let set = new_in_progress_guard();
        let root = Path::new("/tmp/loom-plan-d");
        let cfg = on_idle_config(Some(true), vec!["champion"]);
        let t0 = Instant::now();
        // First edge fires and records the debounce timestamp.
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, false, false, t0).is_empty());
        let first = plan_idle_runs(&mut t, &set, root, &cfg, true, false, t0);
        assert_eq!(first.len(), 1);
        drop(first); // release the guard so only debounce can block the next edge
                     // Flap busy→idle again within 60s: edge present but debounced.
        assert!(plan_idle_runs(
            &mut t,
            &set,
            root,
            &cfg,
            false,
            false,
            t0 + Duration::from_secs(10)
        )
        .is_empty());
        let debounced =
            plan_idle_runs(&mut t, &set, root, &cfg, true, false, t0 + Duration::from_secs(20));
        assert!(debounced.is_empty(), "second edge within 60s must be debounced");
        // Flap again after the window: fires.
        assert!(plan_idle_runs(
            &mut t,
            &set,
            root,
            &cfg,
            false,
            false,
            t0 + Duration::from_secs(70)
        )
        .is_empty());
        let after =
            plan_idle_runs(&mut t, &set, root, &cfg, true, false, t0 + Duration::from_secs(80));
        assert_eq!(after.len(), 1, "edge after the debounce window must fire");
    }

    #[test]
    #[serial]
    fn test_plan_idle_runs_skips_when_guard_already_held() {
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        let mut t = IdleTrigger::new();
        let set = new_in_progress_guard();
        let root = Path::new("/tmp/loom-plan-e");
        let cfg = on_idle_config(Some(true), vec!["champion"]);
        let now = Instant::now();
        // Simulate an interval run already holding the guard for (root, champion).
        let _held = RoleRunGuard::try_acquire(set.clone(), root.to_path_buf(), "champion");
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, false, false, now).is_empty());
        // Edge present, but the guard is held by the interval run => idle skips.
        assert!(
            plan_idle_runs(&mut t, &set, root, &cfg, true, false, now).is_empty(),
            "idle trigger must skip while an interval run holds the guard"
        );
    }

    // ===================================================================
    // Interval loop honors the shared in-progress guard (#4364)
    // ===================================================================

    /// A pre-held guard for (root, role) makes the interval loop skip every
    /// tick (0 invokes); clearing it resumes dispatch — proving the interval
    /// path also respects the shared guard, so an idle-triggered run in
    /// progress cannot be overlapped by an interval tick.
    #[tokio::test]
    async fn test_interval_loop_skips_while_guard_held() {
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
        let root = PathBuf::from("/tmp/loom-interval-guard");
        let in_progress = new_in_progress_guard();
        // Pre-hold the guard for (root, champion) so the loop cannot acquire it.
        in_progress
            .lock()
            .unwrap()
            .insert((root.clone(), "champion"));
        let drain = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handle = spawn_role_task(
            runner,
            spec,
            Duration::from_millis(20),
            drain,
            root.clone(),
            in_progress.clone(),
        );

        // Several intervals elapse; not a single invoke may fire.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "interval tick must skip while the shared guard is held"
        );

        // Release the guard — dispatch resumes, proving the gate (not a dead loop).
        in_progress.lock().unwrap().remove(&(root, "champion"));
        wait_for_calls(&calls, 1, Duration::from_secs(2)).await;

        handle.abort();
    }

    // ===================================================================
    // classify_root_tick_log / log_outcome_for_root_deduped — #4349 state-
    // change log dedup: a repeatedly failing root logs once on the fail
    // edge and once on recovery, not once per tick.
    // ===================================================================

    const NORMAL_TICK: Duration = Duration::from_secs(90);

    #[test]
    fn test_classify_first_failure_is_edge() {
        assert_eq!(
            classify_root_tick_log(&RoleTickOutcome::Failure("boom".into()), NORMAL_TICK, false),
            RootTickLogAction::FailureEdge
        );
    }

    #[test]
    fn test_classify_repeat_failure_is_downgraded() {
        assert_eq!(
            classify_root_tick_log(&RoleTickOutcome::Failure("boom".into()), NORMAL_TICK, true),
            RootTickLogAction::FailureRepeat
        );
    }

    #[test]
    fn test_classify_success_after_failure_is_recovery() {
        assert_eq!(
            classify_root_tick_log(&RoleTickOutcome::Success, NORMAL_TICK, true),
            RootTickLogAction::Recovered
        );
    }

    #[test]
    fn test_classify_steady_state_success_is_plain() {
        assert_eq!(
            classify_root_tick_log(&RoleTickOutcome::Success, NORMAL_TICK, false),
            RootTickLogAction::Success
        );
    }

    #[test]
    fn test_classify_implausibly_fast_variants() {
        assert_eq!(
            classify_root_tick_log(&RoleTickOutcome::Success, Duration::from_millis(100), false),
            RootTickLogAction::SuccessImplausiblyFast
        );
        assert_eq!(
            classify_root_tick_log(&RoleTickOutcome::Success, Duration::from_millis(100), true),
            RootTickLogAction::RecoveredImplausiblyFast
        );
    }

    #[test]
    fn test_log_outcome_for_root_deduped_tracks_failing_state_across_ticks() {
        let root = PathBuf::from("/tmp/does-not-need-to-exist-for-this-test");
        let mut failing: HashMap<PathBuf, bool> = HashMap::new();

        // Tick 1: failure -> edge, marks failing.
        log_outcome_for_root_deduped(
            "champion",
            &root,
            &RoleTickOutcome::Failure("MCP_PREFLIGHT_FAILED".into()),
            NORMAL_TICK,
            &mut failing,
        );
        assert_eq!(failing.get(&root), Some(&true));

        // Ticks 2-4: identical repeat failures -> still marked failing (the
        // dedup happens in the log call, not observable here directly, but
        // the state must remain `true` without ever clearing).
        for _ in 0..3 {
            log_outcome_for_root_deduped(
                "champion",
                &root,
                &RoleTickOutcome::Failure("MCP_PREFLIGHT_FAILED".into()),
                NORMAL_TICK,
                &mut failing,
            );
            assert_eq!(failing.get(&root), Some(&true));
        }

        // Tick 5: recovers -> state flips back to healthy.
        log_outcome_for_root_deduped(
            "champion",
            &root,
            &RoleTickOutcome::Success,
            NORMAL_TICK,
            &mut failing,
        );
        assert_eq!(failing.get(&root), Some(&false));

        // Tick 6: steady-state success keeps it healthy.
        log_outcome_for_root_deduped(
            "champion",
            &root,
            &RoleTickOutcome::Success,
            NORMAL_TICK,
            &mut failing,
        );
        assert_eq!(failing.get(&root), Some(&false));
    }

    #[test]
    fn test_log_outcome_for_root_deduped_is_independent_per_root() {
        // A failure on one registered root must not affect another root's
        // failing state (each workspace's health is tracked independently).
        let root_a = PathBuf::from("/tmp/root-a");
        let root_b = PathBuf::from("/tmp/root-b");
        let mut failing: HashMap<PathBuf, bool> = HashMap::new();

        log_outcome_for_root_deduped(
            "curator",
            &root_a,
            &RoleTickOutcome::Failure("boom".into()),
            NORMAL_TICK,
            &mut failing,
        );
        log_outcome_for_root_deduped(
            "curator",
            &root_b,
            &RoleTickOutcome::Success,
            NORMAL_TICK,
            &mut failing,
        );

        assert_eq!(failing.get(&root_a), Some(&true));
        assert_eq!(failing.get(&root_b), Some(&false));
    }

    // ===================================================================
    // spawn_multi_role_task missing-root hygiene (#4326/#4349) — a
    // registered root whose directory no longer exists is skipped, not
    // spawned against, mirroring work_finder's filter_missing_roots.
    // ===================================================================

    #[tokio::test]
    #[serial]
    async fn test_multi_role_task_skips_missing_registered_root() {
        let tmp = tempfile::tempdir().unwrap();
        let existing_root = tmp.path().join("existing");
        let missing_root = tmp.path().join("gone");
        std::fs::create_dir_all(&existing_root).unwrap();
        write_config(&existing_root, r#"{"autonomous":{"roleRunner":{"enabled":true}}}"#);
        // `add` validates the path exists at registration time, so create the
        // "missing" root first, register it, then delete it — reproducing a
        // registered-but-later-deleted worktree (#4349's #4188 scenario).
        std::fs::create_dir_all(&missing_root).unwrap();

        let registry_path = tmp.path().join("workspaces.json");
        std::env::set_var(
            crate::workspace_registry::REGISTRY_PATH_ENV,
            registry_path.to_str().unwrap(),
        );
        let mut registry = WorkspaceRegistry::default();
        registry.add(&existing_root, None).unwrap();
        registry.add(&missing_root, None).unwrap();
        registry.save_default().unwrap();
        std::fs::remove_dir_all(&missing_root).unwrap();

        let spec = RoleSpec {
            name: "curator",
            prompt: "/loom:curator",
            default_interval_secs: 1,
        };
        let drain = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let in_progress = new_in_progress_guard();
        let handle = spawn_multi_role_task(
            spec,
            tmp.path().to_path_buf(),
            Duration::from_millis(20),
            drain,
            in_progress,
        );

        // Let a couple of ticks fire. The missing root must never be spawned
        // against (there is no script at its `.loom/config.json`/spawn path
        // to invoke, so a spawn attempt would either fail loudly or panic
        // the resolve step; the assertion here is simply that the loop
        // survives several ticks without erroring the test process, which
        // it would if the missing root were not filtered before dispatch).
        tokio::time::sleep(Duration::from_millis(80)).await;
        handle.abort();

        std::env::remove_var(crate::workspace_registry::REGISTRY_PATH_ENV);
    }
}
