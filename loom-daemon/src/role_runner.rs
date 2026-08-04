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
//! **Doctor is the one exception to "standalone vs. per-sweep" above
//! (issue #5272).** Before #5272, a `loom:changes-requested` PR was owned
//! *only* by the Doctor a live `/loom:sweep`'s judge-rejection loop
//! dispatches — so a PR left in that state after its sweep ended (crash,
//! token exhaustion, retry budget, or a judge rejection landing after the
//! sweep's own retry budget was spent) had no role left to pick it up,
//! ever. Doctor is therefore also in [`DEFAULT_ROLES`], invoked with **no**
//! PR number (`/loom:doctor`'s own "Finding Work" section, not "PR Fix
//! Mode") so a tick scans the live `loom:changes-requested` queue itself —
//! reusing the claim (`loom:treating`) + staleness (`LOOM_STALE_TREATING_MINUTES`)
//! discipline `doctor.md` already implements for the per-sweep case, so this
//! adds no new claim mechanism. This makes Doctor dual-mode: still dispatched
//! per-sweep by `sweep_registry` for a PR *currently* in a live sweep, and
//! now also dispatched standalone by this module as the queue's periodic
//! owner once a sweep is gone. The two can never race on the same PR: this
//! module's own in-progress guard ([`InProgressGuard`]) serializes standalone
//! `(root, "doctor")` ticks, and `doctor.md`'s `loom:treating` claim check
//! serializes against a *concurrent* per-sweep Doctor the same way it already
//! serializes against a concurrent standalone one.
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

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use crate::script_helpers::log_filter::strip_ansi;
use crate::sweep_registry::{self, SweepRegistryConfig};
use crate::types::RoleTickRecord;
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

/// Max characters of failure detail retained after ANSI-stripping and
/// cleanup (issue #5024). Bounds `RoleTickOutcome::Failure`'s reason string —
/// and therefore `RoleTickRecord.detail` — so a single failing invocation's
/// raw log tail (which can still carry ANSI escapes, banners, and multi-line
/// stderr even after [`MAX_OUTPUT_TAIL_BYTES`] truncation) cannot blow up the
/// `roles.summary` health line downstream. See `health::assess_roles`, which
/// folds every persistent failure's detail into one line.
const MAX_FAILURE_DETAIL_CHARS: usize = 500;

/// ANSI-strip and length-cap `text` for use as a `RoleTickOutcome::Failure`
/// reason. Reuses [`strip_ansi`] rather than reimplementing ANSI stripping
/// (issue #5024).
fn clean_and_cap_detail(text: &str) -> String {
    let cleaned = strip_ansi(text).trim().to_string();
    if cleaned.chars().count() <= MAX_FAILURE_DETAIL_CHARS {
        return cleaned;
    }
    let capped: String = cleaned.chars().take(MAX_FAILURE_DETAIL_CHARS).collect();
    format!("{capped}… [truncated]")
}

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

/// Process-wide count of ticks skipped with [`RoleTickOutcome::NoTokenPool`]
/// (#4642) — a distinct, independently-attributable tally, deliberately never
/// folded into the generic [`RoleTickOutcome::Failure`] count a real
/// invocation failure increments (mirrors the named per-reason skip counters
/// in `sweep_registry.rs`, e.g. `OpenPrDispatchError`/`DispatchBackoffError`).
static NO_TOKEN_POOL_SKIP_COUNT: AtomicU64 = AtomicU64::new(0);

/// Total number of role-runner ticks skipped so far for having no available
/// token pool (see [`RoleTickOutcome::NoTokenPool`]). Exposed for tests and
/// future status surfacing; the daemon does not reset this across its
/// lifetime.
#[must_use]
pub fn no_token_pool_skip_count() -> u64 {
    NO_TOKEN_POOL_SKIP_COUNT.load(Ordering::Relaxed)
}

/// Process-wide count of ticks skipped with
/// [`RoleTickOutcome::ModelRuntimeMismatch`] (#5028, follow-up to #5001 AC2/
/// AC3) — a distinct, independently-attributable tally, deliberately never
/// folded into the generic [`RoleTickOutcome::Failure`] count a real
/// invocation failure increments, exactly like [`NO_TOKEN_POOL_SKIP_COUNT`]:
/// this is a permanent config conflict, not a transient failure worth
/// retrying identically forever.
static MODEL_RUNTIME_MISMATCH_SKIP_COUNT: AtomicU64 = AtomicU64::new(0);

/// Total number of role-runner ticks skipped so far for a provable
/// model/runtime mismatch (see [`RoleTickOutcome::ModelRuntimeMismatch`]).
/// Exposed for tests and future status surfacing; the daemon does not reset
/// this across its lifetime.
#[must_use]
pub fn model_runtime_mismatch_skip_count() -> u64 {
    MODEL_RUNTIME_MISMATCH_SKIP_COUNT.load(Ordering::Relaxed)
}

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
/// table). Deliberately excludes Builder (never run standalone — always
/// dispatched with an issue number, either inside a sweep or by the work
/// finder) and does not touch the per-sweep Judge/Champion invocations
/// `sweep_registry` already handles.
///
/// `doctor` is the one role here that is *also* dispatched per-sweep (see the
/// module-level "Doctor is the one exception" doc above, issue #5272) — its
/// standalone tick here runs `/loom:doctor` with no PR number, so it exercises
/// the role's own "Finding Work" queue scan rather than "PR Fix Mode".
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
        // Standalone owner of the `loom:changes-requested` queue once a PR's
        // sweep is gone (#5272) — see the module-level doc comment. Same
        // 300s cadence as `judge`, its paired stage in the PR lifecycle.
        name: "doctor",
        prompt: "/loom:doctor",
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
    /// Fail-closed scheduling rejection with machine-readable provenance.
    RuntimeRejected(crate::runtime_admission::RuntimeRejection),
    /// No available token pool for this workspace (issue #4642): neither a
    /// per-repo `.loom/tokens/` pool nor a provisioned shared pool
    /// (`LOOM_SHARED_TOKENS_DIR` / `~/.loom/tokens`) exists, so
    /// `spawn-claude.sh`'s own token-selection preflight is guaranteed to
    /// exit `78` (`EX_CONFIG`). A distinct variant — never folded into the
    /// generic [`RoleTickOutcome::Failure`] tally a real invocation failure
    /// increments — because this is a permanent config state until an
    /// operator provisions a pool, not a transient failure worth retrying
    /// identically forever.
    NoTokenPool,
    /// A provable model/runtime mismatch (#5028, follow-up to #5001 AC2/AC3):
    /// the admitted runtime and the resolved model are confidently-known,
    /// differing provider families (e.g. a Claude-shaped model resolved for a
    /// role admitted onto the Codex runtime) — see
    /// [`crate::sweep_registry::model_runtime_mismatch`]. A distinct variant,
    /// deliberately never folded into the generic [`Self::Failure`] tally: it
    /// is detected BEFORE any spawn, is a permanent config conflict rather
    /// than a transient invocation failure, and self-heals the moment the
    /// conflicting config is corrected (no restart, no one-shot disable).
    ModelRuntimeMismatch(ModelRuntimeMismatch),
}

impl RoleTickOutcome {
    /// True for a completed, successful invocation.
    #[must_use]
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success)
    }
}

/// The detail carried by [`RoleTickOutcome::ModelRuntimeMismatch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRuntimeMismatch {
    /// The role that was ticked (e.g. `"judge"`).
    pub role: String,
    /// The admitted runtime (e.g. `"codex"`).
    pub runtime: String,
    /// The model resolved by [`resolve_role_runner_model`] (or the test-only
    /// `with_model` override) for this role.
    pub model: String,
    /// The config/env tier label [`resolve_role_runner_model`] attributes the
    /// model to (e.g. `"default"`, `"autonomous.roleRunner.model"`), unchanged
    /// from what a successful spawn's log header would have recorded.
    pub model_source: String,
    /// The [`crate::sweep_registry::model_runtime_mismatch`] reason string
    /// naming the two conflicting families.
    pub reason: String,
}

impl ModelRuntimeMismatch {
    /// One-line, operator-facing detail. `record_role_tick` stores this
    /// verbatim on the ring record, and `assess_roles` in `health.rs` already
    /// renders a persistent failure's `detail` as-is — so `loom-daemon
    /// health` names the broken config key without an operator reading a
    /// spawn transcript (#5028 AC2).
    #[must_use]
    pub fn detail(&self) -> String {
        format!(
            "model/runtime mismatch: {} (model source={}); set \
             autonomous.roleRunner.roleModels.{} to a model the {} runtime accepts, or point \
             this role back at a Claude runtime",
            self.reason, self.model_source, self.role, self.runtime
        )
    }
}

// ============================================================================
// Role-tick health ring (Issue #4761)
// ============================================================================

/// How many `(root, role)` tick outcomes the process-global ring retains.
///
/// The ring is carried verbatim over IPC in
/// [`crate::types::DaemonStatusReport::role_tick_records`], so the bound is
/// really a *payload* bound: at ~150 bytes a record this is well under 20 KB
/// even when full, which a 5s-interval dashboard poll can afford. It is also
/// generously larger than any window `loom-daemon health --since` would ask
/// for in practice (5 roles × a 5-minute cadence fills ~60 entries an hour).
pub const ROLE_TICK_RING_CAPACITY: usize = 128;

/// Process-global newest-last ring of role-runner tick outcomes.
///
/// Same "loop publishes, status reads" discipline as
/// [`crate::work_finder::last_tick_summary`]: the role-runner loop appends one
/// record per completed `(root, role)` invocation, and `build_daemon_status`
/// hands the window to clients, which apply their own
/// transient-vs-persistent classifier ([`crate::health::summarize_role_ticks`]).
/// The daemon deliberately stores *raw outcomes*, not a verdict — the window an
/// operator cares about is a client-side choice.
static ROLE_TICKS: OnceLock<Mutex<VecDeque<RoleTickRecord>>> = OnceLock::new();

fn role_tick_ring() -> &'static Mutex<VecDeque<RoleTickRecord>> {
    ROLE_TICKS.get_or_init(|| Mutex::new(VecDeque::with_capacity(ROLE_TICK_RING_CAPACITY)))
}

/// Append one `(root, role)` tick outcome to the process-global ring, stamped
/// at `at` (Issue #4761). Oldest entries are evicted past
/// [`ROLE_TICK_RING_CAPACITY`].
pub fn record_role_tick_at(
    role: &str,
    root: &Path,
    outcome: &RoleTickOutcome,
    at: chrono::DateTime<chrono::Utc>,
) {
    let (ok, detail) = match outcome {
        RoleTickOutcome::Success => (true, None),
        RoleTickOutcome::Failure(reason) => (false, Some(reason.clone())),
        RoleTickOutcome::RuntimeRejected(rejection) => {
            (false, Some(format!("runtime-rejected: {}", rejection.reason)))
        }
        // #4642's permanent no-pool state is recorded as NOT ok on purpose: a
        // role that cannot run at all is exactly what a health check must
        // surface, and the persistent-vs-transient classifier will (correctly)
        // never clear it until a pool is provisioned.
        RoleTickOutcome::NoTokenPool => (false, Some("no-token-pool".to_string())),
        // #5028: same reasoning as `NoTokenPool` — a permanent config
        // conflict is exactly what a health check must surface, and the
        // operator-facing `detail()` names the broken config key directly
        // (AC2), so `assess_roles`'s verbatim rendering needs no special case.
        RoleTickOutcome::ModelRuntimeMismatch(mismatch) => (false, Some(mismatch.detail())),
    };
    let mut ring = role_tick_ring()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if ring.len() >= ROLE_TICK_RING_CAPACITY {
        ring.pop_front();
    }
    ring.push_back(RoleTickRecord {
        root: root.to_path_buf(),
        role: role.to_string(),
        at,
        ok,
        detail,
    });
}

/// [`record_role_tick_at`] stamped with the current wall clock.
///
/// The tick loop ([`spawn_multi_role_task`]) records *every* raw outcome here —
/// including the identical repeat failures its own log-dedup (#4349) downgrades
/// to `DEBUG`. That completeness is what lets the client-side classifier detect
/// a config-shaped failure that can never self-recover: N consecutive failures
/// for the same `(root, role)` pair with a byte-identical `detail` escalate from
/// ordinary "persistent" to a loud, distinct verdict via
/// [`crate::health::summarize_role_ticks`] /
/// [`crate::health::ROLE_TICK_ESCALATION_THRESHOLD`] (#5023) — rather than
/// retrying identically forever, silently burning a token slot each tick.
pub fn record_role_tick(role: &str, root: &Path, outcome: &RoleTickOutcome) {
    record_role_tick_at(role, root, outcome, chrono::Utc::now());
}

/// Snapshot the role-tick ring, oldest first (Issue #4761).
#[must_use]
pub fn role_tick_records() -> Vec<RoleTickRecord> {
    role_tick_ring()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .cloned()
        .collect()
}

/// Test-only reset of the process-global role-tick ring.
#[cfg(test)]
fn reset_role_tick_ring() {
    role_tick_ring()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
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
        // Pre-spawn token-pool preflight (issue #4642): a workspace with
        // neither a per-repo `.loom/tokens/` pool nor a provisioned shared
        // pool is guaranteed to fail `spawn-claude.sh`'s own token-selection
        // preflight (`EX_CONFIG`, exit 78) — checking here, before spawning
        // anything, means the role runner skips the doomed spawn instead of
        // burning a tick on a guaranteed exit-78 failure every single time.
        // Gated the same way as the admission check just below: only the
        // real production path (`spawn_bin` unset) checks this — tests that
        // point `spawn_bin` at a fake script opt out, exactly like
        // `resolve_and_admit` below.
        if self.spawn_bin.is_none() && crate::tokens::token_pool_size(&self.workspace_root) == 0 {
            NO_TOKEN_POOL_SKIP_COUNT.fetch_add(1, Ordering::Relaxed);
            return RoleTickOutcome::NoTokenPool;
        }
        // Issue #5028 (follow-up to #5001 AC2/AC3): runtime admission now
        // resolves BEFORE the model, because the runtime is a per-role INPUT
        // to the model/runtime mismatch check just below — a Claude-shaped
        // model can only be judged wrong once the admitted runtime is known.
        let admission = if self.spawn_bin.is_none() {
            match crate::runtime_admission::resolve_and_admit(&self.workspace_root, role, None) {
                Ok(value) => Some(value),
                Err(e) => return RoleTickOutcome::RuntimeRejected(e),
            }
        } else {
            None
        };
        // Issue #4501: pin the child's model instead of inheriting the account's
        // interactive CLI default (`fable` on the host that filed the issue,
        // where every role child burned the most constrained quota tier and then
        // died on "You've reached your Fable 5 limit").
        let (model, model_source) = match &self.model {
            Some(m) => (m.clone(), "override".to_string()),
            None => resolve_role_runner_model(&self.workspace_root, role),
        };
        // Issue #5028: refuse a launch whose resolved model is a provable
        // conflict with the just-admitted runtime — e.g.
        // `runtimes.roles.judge = "codex"` with no matching
        // `autonomous.roleRunner.roleModels.judge` override still resolves the
        // Claude-shaped default (`sonnet`), which the Codex adapter rejects
        // with an HTTP 400. Detected here, before any spawn, so the role
        // runner skips the doomed launch instead of burning a tick (and a
        // token draw) on a guaranteed failure every time (#5001 AC2/AC3).
        // Gated on `admission` being `Some` — tests that opt out of admission
        // via `spawn_bin` have no resolved runtime to check against, and are
        // unaffected (mirrors the token-pool preflight's `spawn_bin.is_none()`
        // gate above).
        if let Some(admitted) = &admission {
            if let Some(reason) =
                crate::sweep_registry::model_runtime_mismatch(&admitted.runtime, &model)
            {
                MODEL_RUNTIME_MISMATCH_SKIP_COUNT.fetch_add(1, Ordering::Relaxed);
                return RoleTickOutcome::ModelRuntimeMismatch(ModelRuntimeMismatch {
                    role: role.to_string(),
                    runtime: admitted.runtime.clone(),
                    model,
                    model_source,
                    reason,
                });
            }
        }
        run_role_with_timeout(
            &script,
            &self.workspace_root,
            role,
            prompt,
            self.logs_dir(),
            self.timeout,
            &model,
            &model_source,
            admission.as_ref(),
        )
    }
}

/// Issue #4501 / #5001: resolve the model a role-runner child must run with,
/// joining the SAME precedence chain sweep dispatch uses
/// ([`sweep_registry::resolve_dispatch_model`]) with a per-role override and the
/// role-runner-specific global `autonomous.roleRunner.model` occupying the
/// "explicit request" tier:
///
/// **`autonomous.roleRunner.roleModels.<role>` >
/// `autonomous.roleRunner.model` > `autonomous.model` > shipped
/// [`sweep_registry::DEFAULT_DISPATCH_MODEL`] (`sonnet`)**
///
/// Empty/whitespace values are treated as unset at every tier, so the resolved
/// model is never the empty string and never the CLI-inherited interactive
/// default. Returns the model plus a label naming the tier that supplied it (for
/// the per-role log header).
///
/// # Why the per-role tier (#5001)
///
/// `LOOM_RUNTIME_<ROLE>` gives each role its own **runtime** axis (Claude vs
/// Codex etc.), but before #5001 the model was a single global value shared by
/// every role. The moment one role (e.g. Judge) was pointed at a different
/// provider via `LOOM_RUNTIME_JUDGE=codex`, the globally-pinned Claude alias
/// (`sonnet`) was forwarded verbatim to the Codex adapter, which rejected it with
/// an HTTP 400 — so every Judge tick failed silently, fleet-wide. The per-role
/// override closes that gap: a repo can run Judge on Codex with a Codex-valid
/// model while Curator/Champion keep a Claude alias, all from config.
///
/// Before #4501, `run_role_with_timeout` emitted **no** `--model` argument at
/// all, so every scheduled curator/champion/judge/auditor/guide child inherited
/// whatever the selected account's interactive `claude` default happened to be —
/// the live defect this resolution exists to prevent.
#[must_use]
pub fn resolve_role_runner_model(repo_root: &Path, role: &str) -> (String, String) {
    let config = read_role_runner_config(repo_root);
    let role_key = role.trim().to_ascii_lowercase();
    // Per-role override (#5001) wins over the single global
    // `autonomous.roleRunner.model`; both occupy `resolve_dispatch_model`'s
    // "explicit request" (`Param`) tier, so a `per_role` flag disambiguates the
    // log label. A blank per-role value never reaches here — blanks are dropped
    // at parse time in `read_role_runner_config`, so it falls through to the
    // global tier just like an absent key.
    let (configured, per_role) = match config.role_models.get(&role_key) {
        Some(m) => (Some(m.clone()), true),
        None => (config.model.clone(), false),
    };
    let (model, source) = sweep_registry::resolve_dispatch_model(repo_root, configured.as_deref());
    let label = match source {
        sweep_registry::ModelSource::Param if per_role => {
            format!("autonomous.roleRunner.roleModels.{role_key}")
        }
        // `Param` without `per_role` can only arise from the global
        // `autonomous.roleRunner.model` — this function is its only caller.
        sweep_registry::ModelSource::Param => "autonomous.roleRunner.model".to_string(),
        sweep_registry::ModelSource::Config => "autonomous.model".to_string(),
        sweep_registry::ModelSource::Default => "default".to_string(),
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
    admission: Option<&crate::runtime_admission::ResolvedRuntime>,
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
    if let Some(admission) = admission {
        // Pin the already-admitted choice so spawn-worker cannot re-resolve a
        // different runtime after the pre-spawn decision.
        cmd.env("LOOM_RUNTIME", &admission.runtime);
        // Issue #4768: pin the admitted role too, mirroring
        // `sweep_registry::spawn_child`. Without it, a Codex-runtime role
        // child (e.g. `LOOM_ROLE` unset for a champion/curator/judge/auditor/
        // guide tick) reaches `spawn-codex.sh` with no role signal at all,
        // which is indistinguishable from an unrecognized role there.
        cmd.env("LOOM_ROLE", &admission.role);
        log::info!(
            "role_runner: admitted role={} runtime={} source={}",
            admission.role,
            admission.runtime,
            admission.source
        );
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
                let tail = clean_and_cap_detail(&tail_of_file(&log_path));
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
    /// `autonomous.roleRunner.roleModels` — per-role model overrides keyed by
    /// role name (issue #5001), each occupying a tier **above** the global
    /// [`model`](Self::model). This is the config axis that lets a repo run one
    /// role on a different runtime (e.g. `LOOM_RUNTIME_JUDGE=codex`) while giving
    /// that role a model its provider accepts, without forcing the other roles
    /// (still on Claude) onto the same alias. Keys are lower-cased and trimmed;
    /// blank keys and blank/non-string values are dropped, so an entry never
    /// emits `--model ""`. Absent / malformed / non-object soft-fails to an empty
    /// map (every role falls through to the global chain). Resolved by
    /// [`resolve_role_runner_model`].
    pub role_models: BTreeMap<String, String>,
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

    // `roleModels` (#5001): a `{ "<role>": "<model>" }` object of per-role
    // overrides. Keys are lower-cased + trimmed (matching how the resolver looks
    // them up); a blank key, or a blank / non-string value, is dropped — an
    // override must never emit `--model ""`. Absent / non-object soft-fails to an
    // empty map, so every role falls through to the global `model` chain
    // unchanged (zero behavior change when the key is not configured).
    let role_models = block
        .get("roleModels")
        .and_then(serde_json::Value::as_object)
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| {
                    let key = k.trim().to_ascii_lowercase();
                    if key.is_empty() {
                        return None;
                    }
                    let val = v.as_str().map(str::trim).filter(|m| !m.is_empty())?;
                    Some((key, val.to_string()))
                })
                .collect::<BTreeMap<String, String>>()
        })
        .unwrap_or_default();

    RoleRunnerConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        roles,
        interval_secs: block
            .get("intervalSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
        on_idle,
        model,
        role_models,
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
        // The idle path has no ticker of its own, so the collision probe's
        // lookback window (#4623) defaults to this role's *interval* cadence —
        // the same span a peer's interval-driven pass would write within.
        let interval = resolve_interval_for_role(&spec, &config);
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
                let mut runner = ScriptRoleInvocationRunner::new(run_root.clone());
                // Cross-host collision detection (#4623) — detection only.
                invoke_with_collision_probe(&mut runner, &run_root, name, prompt, interval)
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

/// Run one role invocation wrapped in cross-host collision **detection**
/// (#4623): a pre-tick probe of the role's own forge queue, then self-run
/// window bookkeeping so the *next* probe can tell this process's own writes
/// apart from a peer daemon's.
///
/// Ordering matters and is load-bearing:
/// 1. **probe first** — [`crate::role_collision::probe_before_tick`] reads the
///    baseline left by our *previous* completed run; starting a new run first
///    would clear it.
/// 2. `record_run_started` opens this run's window (suppressing attribution
///    while it is in flight — under-count, never over-count).
/// 3. `record_run_finished` closes it, becoming the next probe's baseline.
///
/// The probe is a **no-op with no forge call** when detection is disabled for
/// `root` (default), so the disabled path costs one config read; the tick's
/// behavior is identical either way — detection never suppresses, delays, or
/// reorders an invocation.
///
/// **Must run on a blocking thread** (every call site is already inside
/// `spawn_blocking`): the probe shells out to `gh`.
fn invoke_with_collision_probe<R: RoleInvocationRunner + ?Sized>(
    runner: &mut R,
    root: &Path,
    role: &'static str,
    prompt: &str,
    interval: Duration,
) -> RoleTickOutcome {
    crate::role_collision::probe_before_tick(root, role, interval);
    crate::role_collision::record_run_started(root, role, chrono::Utc::now());
    let outcome = runner.invoke(role, prompt);
    crate::role_collision::record_run_finished(root, role, chrono::Utc::now());
    outcome
}

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
            let probe_root = root.clone();
            let joined = tokio::task::spawn_blocking(move || {
                // Cross-host collision detection (#4623) — detection only; the
                // invocation itself is unchanged.
                let outcome =
                    invoke_with_collision_probe(&mut runner, &probe_root, name, prompt, interval);
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
        // Per-root no-token-pool state (#4642), tracked completely
        // independently of `failing_roots` so a permanent missing-pool skip
        // is never conflated with (or silences the WARN for) a genuine
        // invocation failure — see `RootTickLogAction::is_no_token_pool`.
        let mut no_token_pool_roots: HashMap<PathBuf, bool> = HashMap::new();
        // Per-root model/runtime-mismatch state (#5028), tracked completely
        // independently of both `failing_roots` and `no_token_pool_roots` so a
        // permanent config-conflict skip is never conflated with (or silences
        // the WARN for) either — see `RootTickLogAction::is_model_mismatch`.
        let mut model_mismatch_roots: HashMap<PathBuf, bool> = HashMap::new();
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
                    let mut runner = ScriptRoleInvocationRunner::new(root_for_task.clone());
                    // Cross-host collision detection (#4623) — detection only;
                    // the invocation itself is unchanged.
                    invoke_with_collision_probe(&mut runner, &root_for_task, name, prompt, interval)
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
                        &mut no_token_pool_roots,
                        &mut model_mismatch_roots,
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
        RoleTickOutcome::RuntimeRejected(rejection) => {
            log::warn!("role_runner: {role} runtime admission rejected: {rejection}");
        }
        RoleTickOutcome::NoTokenPool => {
            log::warn!(
                "role_runner: {role} tick skipped after {elapsed:.1?} — no token pool available \
                 (neither a per-repo .loom/tokens/ pool nor a provisioned shared pool at \
                 ~/.loom/tokens; run `loom-daemon tokens bootstrap` for a per-repo pool, or \
                 `loom-daemon tokens bootstrap --shared` for the machine-level pool — see \
                 .loom/docs/token-pool.md, #4642)"
            );
        }
        RoleTickOutcome::ModelRuntimeMismatch(mismatch) => {
            log::warn!(
                "role_runner: {role} tick skipped after {elapsed:.1?} — {} (#5028)",
                mismatch.detail()
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
        RoleTickOutcome::RuntimeRejected(rejection) => log::warn!(
            "role_runner: {role} runtime admission rejected for {} after {elapsed:.1?}: {rejection}",
            root.display()
        ),
        RoleTickOutcome::NoTokenPool => log::warn!(
            "role_runner: {role} tick for {} skipped after {elapsed:.1?} — no token pool \
             available (neither a per-repo .loom/tokens/ pool nor a provisioned shared pool at \
             ~/.loom/tokens; run `loom-daemon tokens bootstrap` for a per-repo pool, or \
             `loom-daemon tokens bootstrap --shared` for the machine-level pool — see \
             .loom/docs/token-pool.md, #4642)",
            root.display()
        ),
        RoleTickOutcome::ModelRuntimeMismatch(mismatch) => log::warn!(
            "role_runner: {role} tick for {} skipped after {elapsed:.1?} — {} (#5028)",
            root.display(),
            mismatch.detail()
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
    /// First tick with no available token pool (edge into this state, #4642):
    /// log at `WARN`. Distinct from [`Self::FailureEdge`] — a missing token
    /// pool is a permanent config state, not an invocation failure, and must
    /// never be tallied as one.
    NoTokenPoolEdge,
    /// Repeat tick with no available token pool (already warned, #4642):
    /// downgrade to `DEBUG`, mirroring [`Self::FailureRepeat`]'s dedup shape
    /// but tracked completely independently of the Failure/RuntimeRejected
    /// state.
    NoTokenPoolRepeat,
    /// First tick with a model/runtime mismatch (edge into this state, #5028):
    /// log at `WARN`. Distinct from [`Self::FailureEdge`] and
    /// [`Self::NoTokenPoolEdge`] — a provable model/runtime conflict is a
    /// permanent config state detected before any spawn, never tallied as an
    /// invocation failure.
    ModelMismatchEdge,
    /// Repeat tick with a model/runtime mismatch (already warned, #5028):
    /// downgrade to `DEBUG`, mirroring [`Self::FailureRepeat`] /
    /// [`Self::NoTokenPoolRepeat`]'s dedup shape but tracked completely
    /// independently of both.
    ModelMismatchRepeat,
}

impl RootTickLogAction {
    /// Whether this action should mark the root as failing for the *next*
    /// tick's edge/repeat decision.
    #[must_use]
    fn is_failing(self) -> bool {
        matches!(self, Self::FailureEdge | Self::FailureRepeat)
    }

    /// Whether this action should mark the root as no-token-pool for the
    /// *next* tick's edge/repeat decision (#4642) — tracked independently of
    /// [`Self::is_failing`] so the two conditions never bleed into each
    /// other's dedup state.
    #[must_use]
    fn is_no_token_pool(self) -> bool {
        matches!(self, Self::NoTokenPoolEdge | Self::NoTokenPoolRepeat)
    }

    /// Whether this action should mark the root as model-mismatched for the
    /// *next* tick's edge/repeat decision (#5028) — tracked independently of
    /// both [`Self::is_failing`] and [`Self::is_no_token_pool`] so none of the
    /// three axes bleed into each other's dedup state.
    #[must_use]
    fn is_model_mismatch(self) -> bool {
        matches!(self, Self::ModelMismatchEdge | Self::ModelMismatchRepeat)
    }
}

#[must_use]
fn classify_root_tick_log(
    outcome: &RoleTickOutcome,
    elapsed: Duration,
    was_failing: bool,
    was_no_token_pool: bool,
    was_model_mismatch: bool,
) -> RootTickLogAction {
    match outcome {
        RoleTickOutcome::NoTokenPool if was_no_token_pool => RootTickLogAction::NoTokenPoolRepeat,
        RoleTickOutcome::NoTokenPool => RootTickLogAction::NoTokenPoolEdge,
        RoleTickOutcome::ModelRuntimeMismatch(_) if was_model_mismatch => {
            RootTickLogAction::ModelMismatchRepeat
        }
        RoleTickOutcome::ModelRuntimeMismatch(_) => RootTickLogAction::ModelMismatchEdge,
        RoleTickOutcome::Failure(_) | RoleTickOutcome::RuntimeRejected(_) if was_failing => {
            RootTickLogAction::FailureRepeat
        }
        RoleTickOutcome::Failure(_) | RoleTickOutcome::RuntimeRejected(_) => {
            RootTickLogAction::FailureEdge
        }
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
/// *previous* tick for that root ended in [`RoleTickOutcome::Failure`] (or
/// [`RoleTickOutcome::RuntimeRejected`]); `no_token_pool` tracks, per root and
/// completely independently, whether the previous tick ended in
/// [`RoleTickOutcome::NoTokenPool`] (#4642); `model_mismatch` tracks, per root
/// and completely independently of both, whether the previous tick ended in
/// [`RoleTickOutcome::ModelRuntimeMismatch`] (#5028) — see [`RootTickLogAction`]
/// for the per-transition logging rules.
#[allow(clippy::too_many_arguments)]
fn log_outcome_for_root_deduped(
    role: &str,
    root: &Path,
    outcome: &RoleTickOutcome,
    elapsed: Duration,
    failing: &mut HashMap<PathBuf, bool>,
    no_token_pool: &mut HashMap<PathBuf, bool>,
    model_mismatch: &mut HashMap<PathBuf, bool>,
) {
    // Record the raw outcome BEFORE the log-dedup decision (#4761): the
    // edge/repeat dedup exists to keep the *log* quiet, but a health check needs
    // every tick — a persistently-failing root logs at DEBUG after its first
    // WARN, which is exactly the case that must still surface as degraded.
    record_role_tick(role, root, outcome);
    let was_failing = failing.get(root).copied().unwrap_or(false);
    let was_no_token_pool = no_token_pool.get(root).copied().unwrap_or(false);
    let was_model_mismatch = model_mismatch.get(root).copied().unwrap_or(false);
    let action = classify_root_tick_log(
        outcome,
        elapsed,
        was_failing,
        was_no_token_pool,
        was_model_mismatch,
    );
    let reason = match outcome {
        RoleTickOutcome::Failure(reason) => reason.as_str(),
        RoleTickOutcome::RuntimeRejected(rejection) => rejection.reason.as_str(),
        RoleTickOutcome::Success | RoleTickOutcome::NoTokenPool => "",
        RoleTickOutcome::ModelRuntimeMismatch(_) => "",
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
        RootTickLogAction::NoTokenPoolEdge => {
            log::warn!(
                "role_runner: {role} tick for {} skipped after {elapsed:.1?} — no token pool \
                 available (neither a per-repo .loom/tokens/ pool nor a provisioned shared pool \
                 at ~/.loom/tokens; run `loom-daemon tokens bootstrap` for a per-repo pool, or \
                 `loom-daemon tokens bootstrap --shared` for the machine-level pool — see \
                 .loom/docs/token-pool.md; further identical skips for this root are logged at \
                 DEBUG until a pool becomes available, #4642)",
                root.display()
            );
        }
        RootTickLogAction::NoTokenPoolRepeat => {
            log::debug!(
                "role_runner: {role} tick for {} skipped again after {elapsed:.1?} — no token \
                 pool available (repeat of an already-logged skip; not re-warned every tick — \
                 see the skip-edge WARN above, #4642)",
                root.display()
            );
        }
        RootTickLogAction::ModelMismatchEdge => {
            if let RoleTickOutcome::ModelRuntimeMismatch(mismatch) = outcome {
                log::warn!(
                    "role_runner: {role} tick for {} skipped after {elapsed:.1?} — {} (further \
                     identical skips for this root are logged at DEBUG until the config is \
                     corrected, #5028)",
                    root.display(),
                    mismatch.detail()
                );
            }
        }
        RootTickLogAction::ModelMismatchRepeat => {
            if let RoleTickOutcome::ModelRuntimeMismatch(mismatch) = outcome {
                log::debug!(
                    "role_runner: {role} tick for {} skipped again after {elapsed:.1?} — repeat \
                     of an already-logged model/runtime mismatch (see the mismatch-edge WARN \
                     above, #5028): {}",
                    root.display(),
                    mismatch.detail()
                );
            }
        }
    }
    failing.insert(root.to_path_buf(), action.is_failing());
    no_token_pool.insert(root.to_path_buf(), action.is_no_token_pool());
    model_mismatch.insert(root.to_path_buf(), action.is_model_mismatch());
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

    // -- clean_and_cap_detail (#5024) ---------------------------------------

    #[test]
    fn clean_and_cap_detail_strips_ansi_and_trims() {
        let raw = "\x1b[31merror:\x1b[0m something failed\n";
        assert_eq!(clean_and_cap_detail(raw), "error: something failed");
    }

    #[test]
    fn clean_and_cap_detail_round_trips_short_clean_text_unchanged() {
        let raw = "exit code 1: connection refused";
        assert_eq!(clean_and_cap_detail(raw), raw);
    }

    #[test]
    fn clean_and_cap_detail_caps_oversized_text() {
        let raw = "x".repeat(MAX_FAILURE_DETAIL_CHARS * 4);
        let cleaned = clean_and_cap_detail(&raw);
        // Capped body + a short "… [truncated]" marker — bound generously
        // above MAX_FAILURE_DETAIL_CHARS so the assertion doesn't hardcode
        // the marker's exact byte/char width.
        assert!(
            cleaned.chars().count() <= MAX_FAILURE_DETAIL_CHARS + 32,
            "cleaned detail was not capped: {} chars",
            cleaned.chars().count()
        );
        assert!(cleaned.ends_with("[truncated]"));
    }

    /// RAII guard that clears the ambient `LOOM_RUNTIME` env var for the
    /// scope of a test and restores whatever value (if any) it previously
    /// had — including across a mid-test assertion panic, since Rust
    /// unwinds through `Drop`. Some host/dev-container shells export
    /// `LOOM_RUNTIME` (as the `spawn-worker.sh` runtime selector), and
    /// without this guard that ambient value silently outranks the
    /// `runtimes.roles` config precedence this test exercises (#4739).
    struct ClearedLoomRuntimeEnv(Option<String>);

    impl ClearedLoomRuntimeEnv {
        fn new() -> Self {
            let prior = std::env::var("LOOM_RUNTIME").ok();
            std::env::remove_var("LOOM_RUNTIME");
            Self(prior)
        }
    }

    impl Drop for ClearedLoomRuntimeEnv {
        fn drop(&mut self) {
            match self.0.take() {
                Some(v) => std::env::set_var("LOOM_RUNTIME", v),
                None => std::env::remove_var("LOOM_RUNTIME"),
            }
        }
    }

    #[test]
    #[serial]
    fn mixed_runtime_role_launch_is_admitted_and_pinned_before_spawn() {
        use std::os::unix::fs::PermissionsExt;

        let _env_guard = ClearedLoomRuntimeEnv::new();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for sub in [
            ".loom/roles",
            ".loom/runtimes",
            ".loom/scripts",
            ".loom/tokens",
        ] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        // #4642: a per-repo token pool so the new pre-spawn token-pool check
        // does not short-circuit this test's runtime-admission scenario.
        fs::write(root.join(".loom/tokens/fake.token"), "sk-ant-oat01-fake").unwrap();
        // #5028: without a matching `roleModels.curator` override, curator
        // admitted onto `codex` would resolve the Claude-shaped default model
        // (`sonnet`) and now get refused as a `ModelRuntimeMismatch` BEFORE
        // this test's runtime-admission/pinning scenario ever reaches the
        // adapter — supplying the override keeps this test's scope on
        // admission/pinning, not the (separately tested) mismatch refusal.
        write_config(
            root,
            r#"{"runtimes":{"roles":{"curator":"codex"}},"autonomous":{"roleRunner":{"roleModels":{"curator":"gpt-5-codex"}}}}"#,
        );
        fs::write(root.join(".loom/roles/curator.json"), r#"{"runtimeRequirements":["mcp"]}"#)
            .unwrap();
        fs::write(
            root.join(".loom/runtimes/codex.json"),
            r#"{"runtime":"codex","capabilities":{"mcp":"yes","worktreeIsolation":"partial"}}"#,
        )
        .unwrap();
        let adapter = root.join(".loom/scripts/spawn-codex.sh");
        fs::write(&adapter, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&adapter, fs::Permissions::from_mode(0o755)).unwrap();
        let observed = root.join("observed-runtime");
        let worker = root.join(".loom/scripts/spawn-worker.sh");
        fs::write(
            &worker,
            format!("#!/bin/sh\nprintf '%s' \"$LOOM_RUNTIME\" > '{}'\n", observed.display()),
        )
        .unwrap();
        fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).unwrap();

        let mut runner = ScriptRoleInvocationRunner::new(root.to_path_buf())
            .with_timeout(Duration::from_secs(5));
        assert_eq!(runner.invoke("curator", "/loom:curator"), RoleTickOutcome::Success);
        assert_eq!(fs::read_to_string(observed).unwrap(), "codex");
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

    /// #4642: a workspace with a resolvable `spawn-worker.sh` but NO token
    /// pool (neither per-repo nor shared) must short-circuit to
    /// `NoTokenPool` — proving the pre-spawn check fires *before*
    /// `run_role_with_timeout` ever runs the script — by asserting a marker
    /// file the script would write is never created.
    #[test]
    #[serial(loom_shared_tokens_dir_env)]
    fn test_invoke_short_circuits_with_no_token_pool_before_running_the_script() {
        use std::os::unix::fs::PermissionsExt;

        // Force a deterministic "no shared pool" resolution regardless of a
        // real `~/.loom/tokens` on the machine running this test.
        let prev_shared = std::env::var("LOOM_SHARED_TOKENS_DIR").ok();
        std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".loom/scripts")).unwrap();
        // A real, resolvable spawn-worker.sh that proves whether it ran by
        // writing a marker file.
        let marker = root.join("script-ran");
        let worker = root.join(".loom/scripts/spawn-worker.sh");
        fs::write(&worker, format!("#!/bin/sh\ntouch '{}'\nexit 0\n", marker.display())).unwrap();
        fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).unwrap();

        let before = no_token_pool_skip_count();
        let mut runner = ScriptRoleInvocationRunner::new(root.to_path_buf());
        let outcome = runner.invoke("curator", "/loom:curator");

        assert_eq!(outcome, RoleTickOutcome::NoTokenPool);
        assert!(!outcome.is_success());
        assert!(!marker.exists(), "the doomed script must never actually run");
        assert_eq!(no_token_pool_skip_count(), before + 1);

        match prev_shared {
            Some(v) => std::env::set_var("LOOM_SHARED_TOKENS_DIR", v),
            None => std::env::remove_var("LOOM_SHARED_TOKENS_DIR"),
        }
    }

    /// #4642: the SAME workspace with a per-repo `.loom/tokens/` pool
    /// populated proceeds past the check and actually runs the script —
    /// proving the gate re-checks live state rather than caching a verdict.
    #[test]
    #[serial(loom_shared_tokens_dir_env)]
    fn test_invoke_proceeds_once_a_per_repo_token_pool_exists() {
        use std::os::unix::fs::PermissionsExt;

        let prev_shared = std::env::var("LOOM_SHARED_TOKENS_DIR").ok();
        std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".loom/scripts")).unwrap();
        fs::create_dir_all(root.join(".loom/tokens")).unwrap();
        fs::write(root.join(".loom/tokens/fake.token"), "sk-ant-oat01-fake").unwrap();
        let worker = root.join(".loom/scripts/spawn-worker.sh");
        fs::write(&worker, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).unwrap();

        let mut runner = ScriptRoleInvocationRunner::new(root.to_path_buf());
        let outcome = runner.invoke("curator", "/loom:curator");
        // Not asserting `Success` specifically: with no
        // `.loom/roles`/`.loom/runtimes` manifests in this minimal fixture,
        // the runtime-admission step below the token check is expected to
        // reject the (unconfigured) default runtime — the point of this test
        // is only that the token-pool gate itself let the tick past, i.e.
        // the outcome is never `NoTokenPool` once a pool exists.
        assert_ne!(outcome, RoleTickOutcome::NoTokenPool);

        match prev_shared {
            Some(v) => std::env::set_var("LOOM_SHARED_TOKENS_DIR", v),
            None => std::env::remove_var("LOOM_SHARED_TOKENS_DIR"),
        }
    }

    /// Shared fixture for the #5028 end-to-end mismatch tests: a workspace
    /// admitted onto the `codex` runtime for `judge`, with a real per-repo
    /// token pool (so the #4642 preflight does not short-circuit first) and a
    /// fake `spawn-worker.sh` (the actual script `resolve_spawn_bin` resolves
    /// and `invoke` runs — mirrors `mixed_runtime_role_launch_is_admitted_and_pinned_before_spawn`)
    /// that writes a marker file if it is ever actually invoked — proving a
    /// refused launch never reaches the spawn.
    fn setup_codex_judge_fixture(root: &Path, config_extra: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        for sub in [
            ".loom/roles",
            ".loom/runtimes",
            ".loom/scripts",
            ".loom/tokens",
        ] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        fs::write(root.join(".loom/tokens/fake.token"), "sk-ant-oat01-fake").unwrap();
        write_config(
            root,
            &format!(r#"{{"runtimes":{{"roles":{{"judge":"codex"}}}}{}}}"#, config_extra),
        );
        fs::write(root.join(".loom/roles/judge.json"), r#"{"runtimeRequirements":[]}"#).unwrap();
        fs::write(
            root.join(".loom/runtimes/codex.json"),
            r#"{"runtime":"codex","capabilities":{}}"#,
        )
        .unwrap();
        // Admission (`resolve_and_admit`) validates that the `codex` adapter
        // file exists on disk before admitting the runtime at all — it is
        // never actually exec'd in this fixture (that's `spawn-worker.sh`
        // below), but its mere absence would itself refuse the launch with a
        // `RuntimeRejected`, which is not what these tests are exercising.
        let adapter = root.join(".loom/scripts/spawn-codex.sh");
        fs::write(&adapter, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&adapter, fs::Permissions::from_mode(0o755)).unwrap();

        let marker = root.join("spawn-ran");
        let worker = root.join(".loom/scripts/spawn-worker.sh");
        fs::write(&worker, format!("#!/bin/sh\ntouch '{}'\nexit 0\n", marker.display())).unwrap();
        fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).unwrap();
        marker
    }

    /// Issue #5028 (#5001 AC2/AC3): `runtimes.roles.judge = "codex"` with NO
    /// `autonomous.roleRunner.roleModels.judge` override resolves the
    /// Claude-shaped default model (`sonnet`) for a role admitted onto Codex —
    /// a provable, doomed launch. `invoke` must refuse it as
    /// `ModelRuntimeMismatch` BEFORE the spawn, never create the adapter's
    /// marker file, and increment the dedicated skip counter — never a bare
    /// `Failure`/`RuntimeRejected`.
    #[test]
    #[serial]
    fn test_invoke_refuses_a_provable_model_runtime_mismatch_before_spawning() {
        let _env_guard = ClearedLoomRuntimeEnv::new();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let marker = setup_codex_judge_fixture(root, "");

        let before = model_runtime_mismatch_skip_count();
        let mut runner = ScriptRoleInvocationRunner::new(root.to_path_buf())
            .with_timeout(Duration::from_secs(5));
        let outcome = runner.invoke("judge", "/loom:judge");

        let RoleTickOutcome::ModelRuntimeMismatch(mismatch) = outcome else {
            panic!("expected ModelRuntimeMismatch, got {outcome:?}");
        };
        assert_eq!(mismatch.role, "judge");
        assert_eq!(mismatch.runtime, "codex");
        assert_eq!(mismatch.model, "sonnet", "the unfixed Claude-shaped default");
        assert!(!marker.exists(), "a doomed launch must never actually spawn the adapter");
        assert_eq!(model_runtime_mismatch_skip_count(), before + 1);
    }

    /// Issue #5028: the SAME fixture with `roleModels.judge` pointed at a
    /// Codex-valid model spawns successfully — proving the check is a
    /// targeted refusal, not a blanket block on Judge-on-Codex, and that it
    /// self-heals the moment the config is corrected (no restart needed).
    #[test]
    #[serial]
    fn test_invoke_succeeds_once_role_models_supplies_a_matching_model() {
        let _env_guard = ClearedLoomRuntimeEnv::new();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let marker = setup_codex_judge_fixture(
            root,
            r#","autonomous":{"roleRunner":{"roleModels":{"judge":"gpt-5-codex"}}}"#,
        );

        let mut runner = ScriptRoleInvocationRunner::new(root.to_path_buf())
            .with_timeout(Duration::from_secs(5));
        let outcome = runner.invoke("judge", "/loom:judge");

        assert_eq!(outcome, RoleTickOutcome::Success);
        assert!(marker.exists(), "a matching model must let the launch actually spawn");
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

    /// Issue #5001: end-to-end, a `roleModels.<role>` override reaches the actual
    /// `--model` argv for that role while a peer role (no override) still gets the
    /// global `autonomous.roleRunner.model`. This is the argv-level proof of the
    /// mixed-runtime fix: the Codex-bound Judge pins a Codex-valid model while the
    /// Claude-bound Curator keeps the Claude alias — from one config block.
    #[test]
    fn test_invoke_per_role_model_override_reaches_argv() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous": {"roleRunner": {
                "enabled": true,
                "model": "sonnet",
                "roleModels": {"judge": "gpt-5-codex"}
            }}}"#,
        );
        let script = write_fake_script(
            tmp.path(),
            "fake-spawn.sh",
            "printf '%s\\n' \"$@\" > argv-last.txt; exit 0",
        );

        // Judge gets its per-role Codex model.
        let mut judge = ScriptRoleInvocationRunner::new(tmp.path().to_path_buf())
            .with_spawn_bin(script.clone());
        assert_eq!(judge.invoke("judge", "/loom:judge"), RoleTickOutcome::Success);
        let judge_argv = fs::read_to_string(tmp.path().join("argv-last.txt")).unwrap();
        assert!(
            judge_argv.contains("--model\ngpt-5-codex\n"),
            "judge must pin its per-role model; argv: {judge_argv}"
        );

        // Curator (no override) still gets the global roleRunner.model.
        let mut curator =
            ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
        assert_eq!(curator.invoke("curator", "/loom:curator"), RoleTickOutcome::Success);
        let curator_argv = fs::read_to_string(tmp.path().join("argv-last.txt")).unwrap();
        assert!(
            curator_argv.contains("--model\nsonnet\n"),
            "curator must keep the global roleRunner.model; argv: {curator_argv}"
        );
    }

    /// Issue #4501: with only `autonomous.model` set, the role runner joins the
    /// SAME chain sweep dispatch uses rather than keeping a private default.
    //
    // NOTE: see the comment above `test_config_missing_file_is_default` —
    // `resolve_role_runner_model` reads `read_role_runner_config` internally
    // (and this test also calls it directly for the `blank` case), so it needs
    // the same private-defaults-tier guard + `#[serial(loom_config_env)]`
    // (#4593, discovered during review of #4590 / #4538).
    #[test]
    #[serial(loom_config_env)]
    fn test_resolve_role_runner_model_precedence_chain() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");

        // No config at all -> shipped default, labelled `default`.
        let bare = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_role_runner_model(bare.path(), "curator"),
            (sweep_registry::DEFAULT_DISPATCH_MODEL.to_string(), "default".to_string())
        );

        // `autonomous.model` only -> that value, labelled `autonomous.model`.
        // Routing through `resolve_dispatch_model` also means the role runner
        // inherits the #3982 logical-tier alias resolution for free
        // (`opus` -> `claude-opus-5`), exactly as sweep dispatch does.
        let shared = tempfile::tempdir().unwrap();
        write_config(shared.path(), r#"{"autonomous": {"model": "opus"}}"#);
        assert_eq!(
            resolve_role_runner_model(shared.path(), "curator"),
            ("claude-opus-5".to_string(), "autonomous.model".to_string())
        );

        // Both -> the role-runner-specific value, labelled as such.
        let both = tempfile::tempdir().unwrap();
        write_config(
            both.path(),
            r#"{"autonomous": {"model": "opus", "roleRunner": {"model": "haiku"}}}"#,
        );
        assert_eq!(
            resolve_role_runner_model(both.path(), "curator"),
            ("haiku".to_string(), "autonomous.roleRunner.model".to_string())
        );

        // A blank override is treated as unset at every tier (never `--model ""`).
        let blank = tempfile::tempdir().unwrap();
        write_config(blank.path(), r#"{"autonomous": {"roleRunner": {"model": "   "}}}"#);
        assert_eq!(read_role_runner_config(blank.path()).model, None);
        assert_eq!(
            resolve_role_runner_model(blank.path(), "curator"),
            (sweep_registry::DEFAULT_DISPATCH_MODEL.to_string(), "default".to_string())
        );

        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    }

    /// Issue #5001: `autonomous.roleRunner.roleModels.<role>` is a tier ABOVE the
    /// global `autonomous.roleRunner.model` — a repo can point one role (Judge,
    /// on Codex) at a provider-valid model while the other roles
    /// (Curator/Champion, on Claude) keep a Claude alias, all from config. This
    /// is the config-only fix for the `LOOM_RUNTIME_JUDGE=codex` -> `sonnet` 400
    /// incident: the per-role and global model axes can finally disagree.
    #[test]
    #[serial(loom_config_env)]
    fn test_resolve_role_runner_model_per_role_override() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");

        // Judge gets a Codex-valid model; curator/champion keep the global
        // Claude alias — the exact mixed-runtime shape the incident needed.
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            r#"{"autonomous": {"roleRunner": {
                "model": "sonnet",
                "roleModels": {"judge": "gpt-5-codex"}
            }}}"#,
        );
        assert_eq!(
            resolve_role_runner_model(dir.path(), "judge"),
            ("gpt-5-codex".to_string(), "autonomous.roleRunner.roleModels.judge".to_string())
        );
        // A role with no per-role entry falls through to the global tier.
        assert_eq!(
            resolve_role_runner_model(dir.path(), "curator"),
            ("sonnet".to_string(), "autonomous.roleRunner.model".to_string())
        );
        assert_eq!(
            resolve_role_runner_model(dir.path(), "champion"),
            ("sonnet".to_string(), "autonomous.roleRunner.model".to_string())
        );

        // Per-role override with NO global model set: the overridden role uses
        // its override; every other role falls all the way through to the
        // shipped default (not the override).
        let no_global = tempfile::tempdir().unwrap();
        write_config(
            no_global.path(),
            r#"{"autonomous": {"roleRunner": {"roleModels": {"judge": "gpt-5-codex"}}}}"#,
        );
        assert_eq!(
            resolve_role_runner_model(no_global.path(), "judge"),
            ("gpt-5-codex".to_string(), "autonomous.roleRunner.roleModels.judge".to_string())
        );
        assert_eq!(
            resolve_role_runner_model(no_global.path(), "guide"),
            (sweep_registry::DEFAULT_DISPATCH_MODEL.to_string(), "default".to_string())
        );

        // The lookup is case-insensitive: a `Judge` config key matches the
        // lower-cased `judge` role name the runner dispatches under.
        let cased = tempfile::tempdir().unwrap();
        write_config(
            cased.path(),
            r#"{"autonomous": {"roleRunner": {"roleModels": {"Judge": "gpt-5-codex"}}}}"#,
        );
        assert_eq!(resolve_role_runner_model(cased.path(), "judge").0, "gpt-5-codex".to_string());

        // A per-role override that is a logical Claude alias still resolves
        // through the #3982 tier map (`opus` -> `claude-opus-5`), exactly like
        // the other tiers.
        let alias = tempfile::tempdir().unwrap();
        write_config(
            alias.path(),
            r#"{"autonomous": {"roleRunner": {"roleModels": {"judge": "opus"}}}}"#,
        );
        assert_eq!(resolve_role_runner_model(alias.path(), "judge").0, "claude-opus-5");

        // A blank per-role value is dropped at parse time and falls through to
        // the global tier — never `--model ""`.
        let blank = tempfile::tempdir().unwrap();
        write_config(
            blank.path(),
            r#"{"autonomous": {"roleRunner": {"model": "sonnet", "roleModels": {"judge": "   "}}}}"#,
        );
        assert!(read_role_runner_config(blank.path()).role_models.is_empty());
        assert_eq!(
            resolve_role_runner_model(blank.path(), "judge"),
            ("sonnet".to_string(), "autonomous.roleRunner.model".to_string())
        );

        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    }

    /// Issue #5001: `read_role_runner_config` soft-fails a malformed / absent /
    /// non-object `roleModels` to an empty map (every role falls through to the
    /// global chain), and drops blank keys — mirroring the soft-fail contract of
    /// every other `autonomous.roleRunner.*` field.
    #[test]
    #[serial(loom_config_env)]
    fn test_read_role_models_soft_fails_and_normalizes() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");

        // Absent key -> empty map.
        let absent = tempfile::tempdir().unwrap();
        write_config(absent.path(), r#"{"autonomous": {"roleRunner": {"enabled": true}}}"#);
        assert!(read_role_runner_config(absent.path())
            .role_models
            .is_empty());

        // Non-object value -> empty map (no panic).
        let non_object = tempfile::tempdir().unwrap();
        write_config(
            non_object.path(),
            r#"{"autonomous": {"roleRunner": {"roleModels": "sonnet"}}}"#,
        );
        assert!(read_role_runner_config(non_object.path())
            .role_models
            .is_empty());

        // Blank keys and blank/non-string values are dropped; good entries are
        // kept, lower-cased, and trimmed.
        let mixed = tempfile::tempdir().unwrap();
        write_config(
            mixed.path(),
            r#"{"autonomous": {"roleRunner": {"roleModels": {
                "  Judge  ": "  gpt-5-codex  ",
                "curator": "",
                "   ": "sonnet",
                "guide": 42
            }}}}"#,
        );
        let models = read_role_runner_config(mixed.path()).role_models;
        assert_eq!(models.get("judge").map(String::as_str), Some("gpt-5-codex"));
        assert_eq!(models.len(), 1, "blank/non-string entries must be dropped: {models:?}");

        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
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

    // NOTE: these tests read `read_role_runner_config`, which merges the
    // private-defaults tier (`config_resolver::private_defaults_path()`) ahead
    // of the tempdir-scoped config under test. That tier resolves off
    // `$LOOM_CONFIG_DEFAULTS_FILE` / `$HOME` — independent of `tmp.path()` — so
    // a host's real `~/.local/share/loom/config/defaults.json` can leak into
    // the result. Neutralize it for the duration of each test (#4538), and use
    // the same named serial group (`loom_config_env`) as the other tests below
    // that mutate this exact env var — a bare `#[serial]` would not serialize
    // against it, since `serial_test` locks are per-key.
    #[test]
    #[serial(loom_config_env)]
    fn test_config_missing_file_is_default() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        let cfg = read_role_runner_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(cfg, RoleRunnerConfig::default());
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_malformed_json_is_default() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), "{not valid json");
        let cfg = read_role_runner_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(cfg, RoleRunnerConfig::default());
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_missing_block_is_default() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"workFinder": {"enabled": true}}}"#);
        let cfg = read_role_runner_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(cfg, RoleRunnerConfig::default());
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_reads_enabled_roles_and_interval() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous": {"roleRunner": {"enabled": true, "roles": ["curator", "guide"], "intervalSecs": 120}}}"#,
        );
        let cfg = read_role_runner_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(
            cfg,
            RoleRunnerConfig {
                enabled: Some(true),
                roles: Some(vec!["curator".to_string(), "guide".to_string()]),
                interval_secs: Some(120),
                on_idle: None,
                model: None,
                role_models: BTreeMap::new(),
            }
        );
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_zero_interval_is_dropped_to_none() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"intervalSecs": 0}}}"#);
        let interval_secs = read_role_runner_config(tmp.path()).interval_secs;
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(interval_secs, None);
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
                role_models: BTreeMap::new(),
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
            role_models: BTreeMap::new(),
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
            role_models: BTreeMap::new(),
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
            role_models: BTreeMap::new(),
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
            role_models: BTreeMap::new(),
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
            role_models: BTreeMap::new(),
        }));
        std::env::set_var(ROLE_RUNNER_ENABLE_ENV, "1");
        assert!(resolve_enabled(&RoleRunnerConfig {
            enabled: Some(false),
            roles: None,
            interval_secs: None,
            on_idle: None,
            model: None,
            role_models: BTreeMap::new(),
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
                    role_models: BTreeMap::new(),
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
                    role_models: BTreeMap::new(),
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
    // Doctor in DEFAULT_ROLES — regression guard for #5272 (before this,
    // a `loom:changes-requested` PR whose sweep ended had no role left to
    // pick it up standalone, ever).
    // ===================================================================

    #[test]
    fn test_default_roles_includes_doctor_with_no_pr_number() {
        let doctor = DEFAULT_ROLES
            .iter()
            .find(|s| s.name == "doctor")
            .expect("#5272: DEFAULT_ROLES must include doctor as a standalone role");
        assert_eq!(
            doctor.prompt, "/loom:doctor",
            "must invoke Doctor's own Finding Work queue scan, not PR Fix Mode \
             (no PR number appended to the prompt)"
        );
        // Same cadence as `judge` — its paired stage in the PR lifecycle: a
        // fresh Judge rejection should not sit unaddressed materially longer
        // than a fresh Judge review sits unclaimed.
        let judge = DEFAULT_ROLES
            .iter()
            .find(|s| s.name == "judge")
            .expect("judge is default");
        assert_eq!(doctor.default_interval_secs, judge.default_interval_secs);
    }

    #[test]
    fn test_resolve_roles_can_select_doctor_alone() {
        let config = RoleRunnerConfig {
            roles: Some(vec!["doctor".to_string()]),
            ..Default::default()
        };
        let resolved = resolve_roles(&config);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "doctor");
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

    // NOTE: see the comment above `test_config_missing_file_is_default` — these
    // tests read `read_role_runner_config` too, so they need the same
    // private-defaults-tier guard + `#[serial(loom_config_env)]` (#4538).
    #[test]
    #[serial(loom_config_env)]
    fn test_config_on_idle_absent_is_none() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"enabled": true}}}"#);
        let on_idle = read_role_runner_config(tmp.path()).on_idle;
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(on_idle, None);
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_on_idle_parses_array() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"onIdle": ["champion"]}}}"#);
        let on_idle = read_role_runner_config(tmp.path()).on_idle;
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(on_idle, Some(vec!["champion".to_string()]));
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_on_idle_non_array_soft_fails_to_none() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        // A non-array (string) value must not panic — it soft-fails to `None`,
        // matching the `roles` contract.
        write_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"onIdle": "champion"}}}"#);
        let on_idle = read_role_runner_config(tmp.path()).on_idle;
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(on_idle, None);
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_on_idle_drops_non_string_entries() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        // Non-string entries are dropped; string entries survive (unknown
        // *names* are filtered later in `resolve_on_idle_roles`).
        write_config(
            tmp.path(),
            r#"{"autonomous": {"roleRunner": {"onIdle": ["champion", 7, true]}}}"#,
        );
        let on_idle = read_role_runner_config(tmp.path()).on_idle;
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(on_idle, Some(vec!["champion".to_string()]));
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
            role_models: BTreeMap::new(),
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
            role_models: BTreeMap::new(),
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
            role_models: BTreeMap::new(),
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
    // invoke_with_collision_probe — cross-host collision detection (#4623)
    // ===================================================================

    /// A runner that records every `(role, prompt)` it was asked to invoke and
    /// returns a scripted outcome.
    struct RecordingRunner {
        calls: Vec<(String, String)>,
        outcome: RoleTickOutcome,
    }

    impl RoleInvocationRunner for RecordingRunner {
        fn invoke(&mut self, role: &str, prompt: &str) -> RoleTickOutcome {
            self.calls.push((role.to_string(), prompt.to_string()));
            self.outcome.clone()
        }
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_collision_probe_wrapper_is_transparent_to_the_invocation() {
        // Detection is opt-in and default-off: with it disabled the wrapper
        // must pass the invocation through byte-for-byte (same role, same
        // prompt, same outcome) and make no forge call.
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        std::env::remove_var(crate::role_collision::ROLE_COLLISION_DETECT_ENV);
        std::env::remove_var(crate::sweep_registry::COLLISION_DETECT_ENV);
        let tmp = tempfile::tempdir().unwrap();
        let mut runner = RecordingRunner {
            calls: Vec::new(),
            outcome: RoleTickOutcome::Failure("boom".into()),
        };
        let outcome = invoke_with_collision_probe(
            &mut runner,
            tmp.path(),
            "champion",
            "/loom:champion",
            Duration::from_secs(600),
        );
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(outcome, RoleTickOutcome::Failure("boom".into()));
        assert_eq!(runner.calls, vec![("champion".to_string(), "/loom:champion".to_string())]);
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_collision_probe_wrapper_records_the_self_run_window() {
        // The baseline the NEXT tick attributes foreign forge activity
        // against: the wrapper must open and close a self-run window around
        // every invocation, even a failing one, and even with detection off
        // (so enabling it mid-run has a baseline immediately).
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        std::env::remove_var(crate::role_collision::ROLE_COLLISION_DETECT_ENV);
        std::env::remove_var(crate::sweep_registry::COLLISION_DETECT_ENV);
        let tmp = tempfile::tempdir().unwrap();
        let mut runner = RecordingRunner {
            calls: Vec::new(),
            outcome: RoleTickOutcome::Failure("boom".into()),
        };
        let before = chrono::Utc::now();
        let _ = invoke_with_collision_probe(
            &mut runner,
            tmp.path(),
            "guide",
            "/loom:guide",
            Duration::from_secs(900),
        );
        let after = chrono::Utc::now();
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        let window = crate::role_collision::last_self_run(tmp.path(), "guide")
            .expect("a self-run window must be recorded");
        assert!(window.started >= before && window.started <= after);
        let ended = window
            .ended
            .expect("the window must be closed after the invocation");
        assert!(ended >= window.started && ended <= after);
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
            role_models: BTreeMap::new(),
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
            role_models: BTreeMap::new(),
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
    // NOTE: see the comment above `test_config_missing_file_is_default` — this
    // test's `observe_and_fire_idle` calls read the private-defaults tier via
    // `read_role_runner_config` too, so it needs the same guard +
    // `#[serial(loom_config_env)]` (#4593, discovered during review of #4590 /
    // #4538).
    #[test]
    #[serial(loom_config_env)]
    fn test_observe_and_fire_idle_cross_config_disabled_target_root_warns_and_suppresses() {
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
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
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
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
            classify_root_tick_log(
                &RoleTickOutcome::Failure("boom".into()),
                NORMAL_TICK,
                false,
                false,
                false
            ),
            RootTickLogAction::FailureEdge
        );
    }

    #[test]
    fn test_classify_repeat_failure_is_downgraded() {
        assert_eq!(
            classify_root_tick_log(
                &RoleTickOutcome::Failure("boom".into()),
                NORMAL_TICK,
                true,
                false,
                false
            ),
            RootTickLogAction::FailureRepeat
        );
    }

    #[test]
    fn test_classify_success_after_failure_is_recovery() {
        assert_eq!(
            classify_root_tick_log(&RoleTickOutcome::Success, NORMAL_TICK, true, false, false),
            RootTickLogAction::Recovered
        );
    }

    #[test]
    fn test_classify_steady_state_success_is_plain() {
        assert_eq!(
            classify_root_tick_log(&RoleTickOutcome::Success, NORMAL_TICK, false, false, false),
            RootTickLogAction::Success
        );
    }

    #[test]
    fn test_classify_implausibly_fast_variants() {
        assert_eq!(
            classify_root_tick_log(
                &RoleTickOutcome::Success,
                Duration::from_millis(100),
                false,
                false,
                false
            ),
            RootTickLogAction::SuccessImplausiblyFast
        );
        assert_eq!(
            classify_root_tick_log(
                &RoleTickOutcome::Success,
                Duration::from_millis(100),
                true,
                false,
                false
            ),
            RootTickLogAction::RecoveredImplausiblyFast
        );
    }

    // ---- no-token-pool classification (#4642) -------------------------

    #[test]
    fn test_classify_first_no_token_pool_is_edge() {
        assert_eq!(
            classify_root_tick_log(&RoleTickOutcome::NoTokenPool, NORMAL_TICK, false, false, false),
            RootTickLogAction::NoTokenPoolEdge
        );
    }

    #[test]
    fn test_classify_repeat_no_token_pool_is_downgraded() {
        assert_eq!(
            classify_root_tick_log(&RoleTickOutcome::NoTokenPool, NORMAL_TICK, false, true, false),
            RootTickLogAction::NoTokenPoolRepeat
        );
    }

    #[test]
    fn test_classify_no_token_pool_is_independent_of_failing_state() {
        // A root that was previously `Failure`-failing must not have its
        // no-token-pool skip demoted to `Repeat` just because `was_failing`
        // is true — the two conditions are tracked on separate axes.
        assert_eq!(
            classify_root_tick_log(&RoleTickOutcome::NoTokenPool, NORMAL_TICK, true, false, false),
            RootTickLogAction::NoTokenPoolEdge
        );
    }

    #[test]
    fn test_root_tick_log_action_no_token_pool_is_not_failing() {
        // #4642: a no-token-pool skip must never contribute to the
        // Failure/RuntimeRejected tally.
        assert!(!RootTickLogAction::NoTokenPoolEdge.is_failing());
        assert!(!RootTickLogAction::NoTokenPoolRepeat.is_failing());
        assert!(RootTickLogAction::NoTokenPoolEdge.is_no_token_pool());
        assert!(RootTickLogAction::NoTokenPoolRepeat.is_no_token_pool());
        assert!(!RootTickLogAction::FailureEdge.is_no_token_pool());
        assert!(!RootTickLogAction::FailureRepeat.is_no_token_pool());
    }

    // ---- model/runtime mismatch classification (#5028) -----------------

    fn mismatch_outcome() -> RoleTickOutcome {
        RoleTickOutcome::ModelRuntimeMismatch(ModelRuntimeMismatch {
            role: "judge".to_string(),
            runtime: "codex".to_string(),
            model: "sonnet".to_string(),
            model_source: "default".to_string(),
            reason: "runtime \"codex\" only accepts an OpenAI/Codex model but got \"sonnet\""
                .to_string(),
        })
    }

    #[test]
    fn test_classify_first_model_mismatch_is_edge() {
        assert_eq!(
            classify_root_tick_log(&mismatch_outcome(), NORMAL_TICK, false, false, false),
            RootTickLogAction::ModelMismatchEdge
        );
    }

    #[test]
    fn test_classify_repeat_model_mismatch_is_downgraded() {
        assert_eq!(
            classify_root_tick_log(&mismatch_outcome(), NORMAL_TICK, false, false, true),
            RootTickLogAction::ModelMismatchRepeat
        );
    }

    #[test]
    fn test_classify_model_mismatch_is_independent_of_failing_and_no_token_pool_state() {
        // A root previously `Failure`-failing OR previously no-token-pool must
        // not have its model-mismatch skip demoted to `Repeat` just because
        // one of the OTHER two axes is `true` — all three are tracked
        // independently.
        assert_eq!(
            classify_root_tick_log(&mismatch_outcome(), NORMAL_TICK, true, false, false),
            RootTickLogAction::ModelMismatchEdge
        );
        assert_eq!(
            classify_root_tick_log(&mismatch_outcome(), NORMAL_TICK, false, true, false),
            RootTickLogAction::ModelMismatchEdge
        );
    }

    #[test]
    fn test_root_tick_log_action_model_mismatch_is_not_failing_or_no_token_pool() {
        // #5028: a model-mismatch skip must never contribute to the
        // Failure/RuntimeRejected tally, nor to the NoTokenPool tally.
        assert!(!RootTickLogAction::ModelMismatchEdge.is_failing());
        assert!(!RootTickLogAction::ModelMismatchRepeat.is_failing());
        assert!(!RootTickLogAction::ModelMismatchEdge.is_no_token_pool());
        assert!(!RootTickLogAction::ModelMismatchRepeat.is_no_token_pool());
        assert!(RootTickLogAction::ModelMismatchEdge.is_model_mismatch());
        assert!(RootTickLogAction::ModelMismatchRepeat.is_model_mismatch());
        assert!(!RootTickLogAction::FailureEdge.is_model_mismatch());
        assert!(!RootTickLogAction::NoTokenPoolEdge.is_model_mismatch());
    }

    #[test]
    #[serial(role_tick_ring)]
    fn test_log_outcome_for_root_deduped_tracks_failing_state_across_ticks() {
        let root = PathBuf::from("/tmp/does-not-need-to-exist-for-this-test");
        let mut failing: HashMap<PathBuf, bool> = HashMap::new();
        let mut no_token_pool: HashMap<PathBuf, bool> = HashMap::new();
        let mut model_mismatch: HashMap<PathBuf, bool> = HashMap::new();

        // Tick 1: failure -> edge, marks failing.
        log_outcome_for_root_deduped(
            "champion",
            &root,
            &RoleTickOutcome::Failure("MCP_PREFLIGHT_FAILED".into()),
            NORMAL_TICK,
            &mut failing,
            &mut no_token_pool,
            &mut model_mismatch,
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
                &mut no_token_pool,
                &mut model_mismatch,
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
            &mut no_token_pool,
            &mut model_mismatch,
        );
        assert_eq!(failing.get(&root), Some(&false));

        // Tick 6: steady-state success keeps it healthy.
        log_outcome_for_root_deduped(
            "champion",
            &root,
            &RoleTickOutcome::Success,
            NORMAL_TICK,
            &mut failing,
            &mut no_token_pool,
            &mut model_mismatch,
        );
        assert_eq!(failing.get(&root), Some(&false));
    }

    #[test]
    #[serial(role_tick_ring)]
    fn test_log_outcome_for_root_deduped_is_independent_per_root() {
        // A failure on one registered root must not affect another root's
        // failing state (each workspace's health is tracked independently).
        let root_a = PathBuf::from("/tmp/root-a");
        let root_b = PathBuf::from("/tmp/root-b");
        let mut failing: HashMap<PathBuf, bool> = HashMap::new();
        let mut no_token_pool: HashMap<PathBuf, bool> = HashMap::new();
        let mut model_mismatch: HashMap<PathBuf, bool> = HashMap::new();

        log_outcome_for_root_deduped(
            "curator",
            &root_a,
            &RoleTickOutcome::Failure("boom".into()),
            NORMAL_TICK,
            &mut failing,
            &mut no_token_pool,
            &mut model_mismatch,
        );
        log_outcome_for_root_deduped(
            "curator",
            &root_b,
            &RoleTickOutcome::Success,
            NORMAL_TICK,
            &mut failing,
            &mut no_token_pool,
            &mut model_mismatch,
        );

        assert_eq!(failing.get(&root_a), Some(&true));
        assert_eq!(failing.get(&root_b), Some(&false));
    }

    #[test]
    #[serial(role_tick_ring)]
    fn test_log_outcome_for_root_deduped_no_token_pool_tracked_independently_of_failing() {
        // #4642: a NoTokenPool tick must never mark `failing` true, and a
        // real Failure tick must never mark `no_token_pool` true — the two
        // maps are independent axes even for the SAME root.
        let root = PathBuf::from("/tmp/does-not-need-to-exist-for-this-test-2");
        let mut failing: HashMap<PathBuf, bool> = HashMap::new();
        let mut no_token_pool: HashMap<PathBuf, bool> = HashMap::new();
        let mut model_mismatch: HashMap<PathBuf, bool> = HashMap::new();

        log_outcome_for_root_deduped(
            "auditor",
            &root,
            &RoleTickOutcome::NoTokenPool,
            NORMAL_TICK,
            &mut failing,
            &mut no_token_pool,
            &mut model_mismatch,
        );
        assert_eq!(no_token_pool.get(&root), Some(&true));
        assert_eq!(failing.get(&root), Some(&false));

        // A subsequent real failure must still log as a fresh `FailureEdge`
        // (not `FailureRepeat`) even though the root was just skipped for no
        // token pool — proving the two states never cross-contaminate.
        log_outcome_for_root_deduped(
            "auditor",
            &root,
            &RoleTickOutcome::Failure("boom".into()),
            NORMAL_TICK,
            &mut failing,
            &mut no_token_pool,
            &mut model_mismatch,
        );
        assert_eq!(failing.get(&root), Some(&true));
        assert_eq!(no_token_pool.get(&root), Some(&false));
    }

    #[test]
    #[serial(role_tick_ring)]
    fn test_log_outcome_for_root_deduped_model_mismatch_tracked_independently() {
        // #5028: a ModelRuntimeMismatch tick must never mark `failing` or
        // `no_token_pool` true, and must not itself be marked by either of
        // those two axes — all three maps are independent even for the SAME
        // root.
        let root = PathBuf::from("/tmp/does-not-need-to-exist-for-this-test-3");
        let mut failing: HashMap<PathBuf, bool> = HashMap::new();
        let mut no_token_pool: HashMap<PathBuf, bool> = HashMap::new();
        let mut model_mismatch: HashMap<PathBuf, bool> = HashMap::new();

        log_outcome_for_root_deduped(
            "judge",
            &root,
            &mismatch_outcome(),
            NORMAL_TICK,
            &mut failing,
            &mut no_token_pool,
            &mut model_mismatch,
        );
        assert_eq!(model_mismatch.get(&root), Some(&true));
        assert_eq!(failing.get(&root), Some(&false));
        assert_eq!(no_token_pool.get(&root), Some(&false));

        // A subsequent real failure must still log as a fresh `FailureEdge`
        // even though the root was just skipped for a model/runtime mismatch.
        log_outcome_for_root_deduped(
            "judge",
            &root,
            &RoleTickOutcome::Failure("boom".into()),
            NORMAL_TICK,
            &mut failing,
            &mut no_token_pool,
            &mut model_mismatch,
        );
        assert_eq!(failing.get(&root), Some(&true));
        assert_eq!(model_mismatch.get(&root), Some(&false));
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

    // ===================================================================
    // Role-tick health ring (#4761)
    // ===================================================================

    #[test]
    #[serial(role_tick_ring)]
    fn recording_a_tick_makes_it_readable_cross_process() {
        reset_role_tick_ring();
        let at = chrono::Utc::now();
        record_role_tick_at("curator", Path::new("/r/loom"), &RoleTickOutcome::Success, at);

        let records = role_tick_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].role, "curator");
        assert_eq!(records[0].root, PathBuf::from("/r/loom"));
        assert_eq!(records[0].at, at);
        assert!(records[0].ok);
        assert_eq!(records[0].detail, None);
    }

    #[test]
    #[serial(role_tick_ring)]
    fn a_failure_records_its_reason() {
        reset_role_tick_ring();
        record_role_tick(
            "champion",
            Path::new("/r/loom"),
            &RoleTickOutcome::Failure("mcp preflight failed".to_string()),
        );
        let records = role_tick_records();
        assert!(!records[0].ok);
        assert_eq!(records[0].detail.as_deref(), Some("mcp preflight failed"));
    }

    /// #4642's permanent no-pool state must surface as NOT-ok — a role that
    /// cannot run at all is precisely what a health check exists to report.
    #[test]
    #[serial(role_tick_ring)]
    fn a_missing_token_pool_records_as_not_ok() {
        reset_role_tick_ring();
        record_role_tick("guide", Path::new("/r/loom"), &RoleTickOutcome::NoTokenPool);
        let records = role_tick_records();
        assert!(!records[0].ok);
        assert_eq!(records[0].detail.as_deref(), Some("no-token-pool"));
    }

    #[test]
    #[serial(role_tick_ring)]
    fn the_ring_is_bounded_and_keeps_the_newest_entries() {
        reset_role_tick_ring();
        for _ in 0..(ROLE_TICK_RING_CAPACITY + 10) {
            record_role_tick("curator", Path::new("/r/loom"), &RoleTickOutcome::Success);
        }
        record_role_tick(
            "curator",
            Path::new("/r/loom"),
            &RoleTickOutcome::Failure("newest".to_string()),
        );
        let records = role_tick_records();
        assert_eq!(records.len(), ROLE_TICK_RING_CAPACITY);
        assert_eq!(records.last().unwrap().detail.as_deref(), Some("newest"));
    }

    /// The log-dedup path (#4349) downgrades a *repeat* failure to DEBUG, but
    /// the health ring must still see every one of them — otherwise a
    /// persistently-broken root would look quiet to a health check.
    #[test]
    #[serial(role_tick_ring)]
    fn repeat_failures_are_all_recorded_even_though_the_log_dedups_them() {
        reset_role_tick_ring();
        let mut failing = HashMap::new();
        let mut no_pool = HashMap::new();
        let mut model_mismatch = HashMap::new();
        let root = PathBuf::from("/r/loom");
        for _ in 0..3 {
            log_outcome_for_root_deduped(
                "curator",
                &root,
                &RoleTickOutcome::Failure("boom".to_string()),
                Duration::from_secs(30),
                &mut failing,
                &mut no_pool,
                &mut model_mismatch,
            );
        }
        let records = role_tick_records();
        assert_eq!(records.len(), 3, "every tick is recorded, not just the fail edge");
        assert!(records.iter().all(|r| !r.ok));
    }

    /// #5028 AC2: a `ModelRuntimeMismatch` outcome records as NOT-ok with an
    /// operator-facing `detail()` string that names the broken config key —
    /// exactly what `assess_roles` in `health.rs` renders verbatim into
    /// `loom-daemon health`, so an operator learns the fix without reading a
    /// spawn transcript.
    #[test]
    #[serial(role_tick_ring)]
    fn a_model_runtime_mismatch_records_its_operator_facing_detail() {
        reset_role_tick_ring();
        record_role_tick("judge", Path::new("/r/loom"), &mismatch_outcome());
        let records = role_tick_records();
        assert!(!records[0].ok);
        let detail = records[0].detail.as_deref().unwrap();
        assert!(
            detail.contains("autonomous.roleRunner.roleModels.judge"),
            "detail must name the broken config key: {detail}"
        );
        assert!(detail.contains("model/runtime mismatch"), "detail: {detail}");
    }
}
