//! Autonomous self-update loop — the daemon rebuilds and restarts itself onto
//! a fresher binary when its source checkout has advanced past the commit it
//! was built from (Issue #4055, Phase 3 of #4017).
//!
//! # Why
//!
//! [`crate::self_update`] already answers the read-only question "is this
//! running binary stale vs. its source checkout?" and surfaces it in
//! `loom-daemon status`. But acting on it still required an operator to run
//! `loom-daemon-update.sh` by hand — the exact standing manual step a
//! long-lived daemon should own itself. This loop closes the self-repair cycle:
//! it *decides* and *sequences* the roll, reusing `loom-daemon-update.sh` for
//! the rebuild/provision and #4090's drain primitive
//! ([`crate::ipc::handle_drain_request`]) for the restart — it reimplements
//! neither.
//!
//! # Safety gates (from #4017 — the loop never ships without them)
//!
//! 1. **Verify before swap** — delivered by #4053: `loom-daemon-update.sh`
//!    asserts the freshly-built binary's embedded commit equals source HEAD
//!    *before* provisioning and re-verifies the destination after. A
//!    commit-identity mismatch (script exit `4`/`5`) is **terminal**, surfaced,
//!    and never retried.
//! 2. **Clean-tree gate** — `CARGO_MANIFEST_DIR` points at the operator's live
//!    working checkout, so an unattended `cargo build --release` there would
//!    compile whatever is uncommitted into the running daemon. The loop refuses
//!    to build unless [`crate::self_update::source_tree_clean`] is `Some(true)`.
//!    It never runs `git pull` on the operator's behalf.
//! 3. **Backoff on failure** — a source tree that does not compile must not
//!    retry every tick forever. Retryable build failures back off exponentially
//!    with a ceiling; the terminal give-up state is surfaced in `--status`.
//! 4. **No build stampede** — a `cargo build --release` competes with every
//!    in-flight sweep's own build for CPU, so the loop defers the rebuild while
//!    [`crate::ipc::count_in_flight_sweeps`] reports any non-terminal sweep
//!    across every managed root (the "gated" policy).
//! 5. **In-flight sweeps survive** — the roll goes through #4090's **drain**
//!    path, not a bare restart, so dispatched sweeps finish first and stay in
//!    the registry rather than being orphaned as bare processes.
//! 6. **Flags replay exactly** — the restart exits into launchd
//!    `KeepAlive:SuccessfulExit`, which relaunches from the plist's persisted
//!    `ProgramArguments`/`EnvironmentVariables`, so the daemon comes back with
//!    exactly its prior autonomy flags, never wider.
//! 7. **Settle window** — the loop does not roll within `settleSecs` of first
//!    observing a stale commit, batching a burst of daemon commits into one
//!    roll (the timer resets whenever the source commit advances).
//!
//! # `None` is never "stale"
//!
//! [`crate::self_update::SelfUpdateStatus::update_available`] is a tri-state:
//! only `Some(true)` triggers a rebuild. `None` (a tarball install with no
//! source checkout, or `BUILT_COMMIT == "unknown"`) means "do nothing" — never
//! "stale".
//!
//! # Process-global, not per-workspace
//!
//! Unlike [`crate::work_finder`] / [`crate::main_health_gate`] /
//! [`crate::token_ranking_refresh`], whose subject *is* a workspace, this loop's
//! subject is the **daemon process itself**: one binary, one source checkout,
//! one restart. So exactly **one** task runs per daemon regardless of how many
//! workspaces are registered — it is spawned alongside, not inside, the
//! per-workspace fan-outs. Config is read from the daemon's default workspace
//! (`sweep_workspace`), and gate 4's count is inherently cross-root via
//! [`crate::ipc::count_in_flight_sweeps`].

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::event_bus::EventBus;
use crate::ipc::DrainState;
use crate::workspace_pool::WorkspacePool;

// ============================================================================
// Constants
// ============================================================================

/// Master on/off env override for the loop (precedence env > config >
/// default(**false**) — this loop has side effects on the running process, so
/// it is opt-in, like `workFinder`/`mainHealthGate`, not default-on).
pub const AUTO_UPDATE_ENABLE_ENV: &str = "LOOM_AUTO_UPDATE";

/// Env override for the staleness-check cadence (seconds).
pub const AUTO_UPDATE_INTERVAL_ENV: &str = "LOOM_AUTO_UPDATE_INTERVAL_SECS";

/// Env override for the settle window (seconds).
pub const AUTO_UPDATE_SETTLE_ENV: &str = "LOOM_AUTO_UPDATE_SETTLE_SECS";

/// Default cadence between staleness checks (15 minutes). A rebuild is far from
/// free, so this is deliberately coarse.
pub const DEFAULT_AUTO_UPDATE_INTERVAL_SECS: u64 = 900;

/// Default settle window (10 minutes): once a stale commit is first observed,
/// the loop waits this long — resetting on every further commit — before it
/// rolls, so a burst of merges collapses into a single roll.
pub const DEFAULT_AUTO_UPDATE_SETTLE_SECS: u64 = 600;

/// First backoff delay after a retryable build failure. Subsequent failures
/// double it, capped at [`BACKOFF_CEILING`].
const BACKOFF_BASE: Duration = Duration::from_secs(60);

/// Ceiling on the exponential backoff so a persistently-broken source tree
/// still retries hourly (a later commit may fix it) rather than never again.
const BACKOFF_CEILING: Duration = Duration::from_secs(3600);

/// How long to wait for `loom-daemon-update.sh` (which runs `cargo build
/// --release`) before killing it. A release build of the daemon plus a
/// provision step is minutes, not seconds; this is generous headroom without
/// letting a wedged build pin the loop forever.
const DEFAULT_REBUILD_TIMEOUT: Duration = Duration::from_secs(1800);

/// Poll granularity while waiting for the rebuild subprocess.
const REBUILD_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Max bytes of captured script output retained in a failure/roll log line.
const MAX_OUTPUT_TAIL_BYTES: usize = 2048;

// ============================================================================
// Config (.loom/config.json → autonomous.autoUpdate)
// ============================================================================

/// The subset of `.loom/config.json → autonomous.autoUpdate` this module
/// consumes. Each field is `Option` so an absent key falls through to the
/// env-var / built-in default — precedence **env > config > default** for every
/// knob, matching every other migrated `autonomous.*` surface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutoUpdateConfig {
    /// `autonomous.autoUpdate.enabled` — whether to run the loop. `None` when
    /// absent (falls through to env / default(**false**)).
    pub enabled: Option<bool>,
    /// `autonomous.autoUpdate.intervalSecs` — staleness-check cadence in
    /// seconds (a zero/invalid value is dropped to `None`).
    pub interval_secs: Option<u64>,
    /// `autonomous.autoUpdate.settleSecs` — settle window in seconds (a
    /// zero/invalid value is dropped to `None`; `0` is intentionally *not* a
    /// meaningful "no settle" here — use a small positive value).
    pub settle_secs: Option<u64>,
}

/// Read `.loom/config.json → autonomous.autoUpdate` through
/// [`crate::config_resolver`] (so the `.loom-project/` tier is honored like
/// every other migrated `autonomous.*` block, #4058), soft-failing every field
/// to `None` (env/default resolution) on a missing file, malformed JSON, or a
/// missing `autonomous` / `autoUpdate` block. Shape copied verbatim from
/// [`crate::token_ranking_refresh::read_token_ranking_refresh_config`].
#[must_use]
pub fn read_auto_update_config(repo_root: &Path) -> AutoUpdateConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(block) = crate::config_resolver::get_path(&effective, "autonomous.autoUpdate") else {
        return AutoUpdateConfig::default();
    };

    AutoUpdateConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        interval_secs: block
            .get("intervalSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
        settle_secs: block
            .get("settleSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
    }
}

/// Resolve whether the loop is enabled with precedence **env > config >
/// default(false)**. This loop is opt-in (side effects on the running process),
/// so an absent config leaves it **off**.
#[must_use]
pub fn resolve_enabled(config: &AutoUpdateConfig) -> bool {
    if let Ok(v) = std::env::var(AUTO_UPDATE_ENABLE_ENV) {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config.enabled.unwrap_or(false)
}

/// Resolve the check cadence with precedence **env > config > default**. A zero
/// or unparseable env value falls through to `config`/the default rather than
/// producing a busy loop.
#[must_use]
pub fn resolve_interval(config: &AutoUpdateConfig) -> Duration {
    std::env::var(AUTO_UPDATE_INTERVAL_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(config.interval_secs)
        .map_or_else(|| Duration::from_secs(DEFAULT_AUTO_UPDATE_INTERVAL_SECS), Duration::from_secs)
}

/// Resolve the settle window with precedence **env > config > default**.
#[must_use]
pub fn resolve_settle(config: &AutoUpdateConfig) -> Duration {
    std::env::var(AUTO_UPDATE_SETTLE_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(config.settle_secs)
        .map_or_else(|| Duration::from_secs(DEFAULT_AUTO_UPDATE_SETTLE_SECS), Duration::from_secs)
}

// ============================================================================
// Status (published to the process-global, read by build_daemon_status)
// ============================================================================

/// The publicly-observable auto-update state rendered by `loom-daemon status`
/// (mirrors the `auto_update_*` fields on [`crate::types::DaemonStatusReport`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutoUpdateStatusSnapshot {
    /// Whether the loop is enabled this process.
    pub enabled: bool,
    /// Wall-clock time of the most recent staleness check.
    pub last_check: Option<DateTime<Utc>>,
    /// Wall-clock time of the most recent successful roll.
    pub last_roll: Option<DateTime<Utc>>,
    /// Consecutive retryable build failures (resets on success / commit
    /// advance).
    pub consecutive_failures: u32,
    /// Current backoff delay in seconds, or `None` when not backing off.
    pub backoff_secs: Option<u64>,
    /// Terminal give-up reason (non-retryable failure), or `None`.
    pub terminal_reason: Option<String>,
    /// Short human-readable note about the most recent tick.
    pub note: Option<String>,
}

/// Shared, thread-safe handle the loop publishes to and
/// [`crate::ipc::build_daemon_status`] reads from.
#[derive(Debug, Default)]
pub struct AutoUpdateStatus {
    inner: Mutex<AutoUpdateStatusSnapshot>,
}

// Allow expect_used: a poisoned status mutex means another thread panicked
// while holding it — unrecoverable, matching the crash-on-poison policy used
// across ipc.rs / the drain state.
#[allow(clippy::expect_used)]
impl AutoUpdateStatus {
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self {
            inner: Mutex::new(AutoUpdateStatusSnapshot {
                enabled,
                ..AutoUpdateStatusSnapshot::default()
            }),
        }
    }

    /// A snapshot of the current status for rendering.
    #[must_use]
    pub fn snapshot(&self) -> AutoUpdateStatusSnapshot {
        self.inner
            .lock()
            .expect("auto-update status mutex poisoned")
            .clone()
    }

    /// Overwrite the published snapshot.
    fn publish(&self, snap: AutoUpdateStatusSnapshot) {
        *self
            .inner
            .lock()
            .expect("auto-update status mutex poisoned") = snap;
    }
}

/// Process-global status handle. The single spawned loop registers its handle
/// here so [`crate::ipc::build_daemon_status`] can read auto-update state
/// without threading an `Arc` through the whole IPC server. Unset (loop never
/// spawned) reads as the default disabled/never-checked snapshot.
static GLOBAL_STATUS: OnceLock<Arc<AutoUpdateStatus>> = OnceLock::new();

/// Register the loop's status handle as the process-global. Idempotent: only
/// the first registration wins (there is exactly one loop per process).
pub fn register_global_status(status: Arc<AutoUpdateStatus>) {
    let _ = GLOBAL_STATUS.set(status);
}

/// The process-global auto-update status snapshot, or the default
/// (disabled/never-checked) when the loop was never spawned.
#[must_use]
pub fn global_status_snapshot() -> AutoUpdateStatusSnapshot {
    GLOBAL_STATUS
        .get()
        .map_or_else(AutoUpdateStatusSnapshot::default, |s| s.snapshot())
}

// ============================================================================
// Probe + drain trigger (testable via traits, mirrors RankingRefreshRunner)
// ============================================================================

/// The tri-state staleness signal plus the source HEAD it compared against,
/// derived from [`crate::self_update::check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCheck {
    /// `Some(true)` stale, `Some(false)` current, `None` undecidable (no source
    /// checkout / `BUILT_COMMIT == "unknown"`).
    pub update_available: Option<bool>,
    /// The source checkout's current HEAD short commit, when resolvable.
    pub source_commit: Option<String>,
}

/// The outcome of one rebuild+provision invocation of `loom-daemon-update.sh`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebuildOutcome {
    /// Script exit `0` — rebuilt + provisioned (or already current).
    Success,
    /// Script exit `1` (or a spawn/timeout failure) — retryable; back off.
    Retryable(String),
    /// Script exit `4`/`5` — a build-verification / commit-identity mismatch
    /// (#4053). Retrying cannot fix it; terminal, surfaced, not backed off.
    Terminal(String),
}

/// The "world" the loop probes each tick — abstracted behind a trait so the
/// loop is testable with a scripted fake (no real `git`/`cargo`), exactly as
/// [`crate::token_ranking_refresh::RankingRefreshRunner`] makes that loop
/// testable.
pub trait AutoUpdateProbe: Send {
    /// The current staleness tri-state + source commit.
    fn check(&self) -> UpdateCheck;
    /// Whether the source working tree is clean. `None` ⇒ "cannot prove clean"
    /// (no checkout / `git` failed); the loop treats that as not-clean.
    fn is_tree_clean(&self) -> Option<bool>;
    /// Cross-root in-flight (non-terminal) sweep count (gate 4).
    fn in_flight_sweeps(&self) -> usize;
    /// Run the rebuild + provision step (`loom-daemon-update.sh --no-restart`)
    /// and map its exit code to a [`RebuildOutcome`]. Never panics.
    fn rebuild(&mut self) -> RebuildOutcome;
}

/// Triggers the roll through #4090's drain path — separated from
/// [`AutoUpdateProbe`] because in production it needs a tokio runtime handle to
/// spawn the drain supervisor. Returns `true` when the drain was accepted.
pub trait DrainTrigger: Send {
    fn trigger(&self) -> bool;
}

/// The production [`AutoUpdateProbe`]: reads [`crate::self_update`] for
/// staleness + clean-tree, [`crate::ipc::count_in_flight_sweeps`] for gate 4,
/// and shells out to `loom-daemon-update.sh --no-restart` for the rebuild.
pub struct ScriptAutoUpdateProbe {
    /// Cross-root sweep count needs the pool + a fallback root.
    workspace_pool: Arc<WorkspacePool>,
    fallback_root: PathBuf,
    /// The source checkout to rebuild in (resolved once at spawn). `None` when
    /// there is no checkout — but the loop never rebuilds in that case anyway
    /// (`update_available` is `None`).
    source_root: Option<PathBuf>,
    timeout: Duration,
}

impl ScriptAutoUpdateProbe {
    #[must_use]
    pub fn new(workspace_pool: Arc<WorkspacePool>, fallback_root: PathBuf) -> Self {
        Self {
            workspace_pool,
            fallback_root,
            source_root: crate::self_update::source_checkout_root(),
            timeout: DEFAULT_REBUILD_TIMEOUT,
        }
    }

    /// Resolve the `loom-daemon-update.sh` path inside the source checkout:
    /// prefer the installed `.loom/scripts/cli/` copy, else the in-repo
    /// `defaults/scripts/cli/` source.
    fn resolve_script(root: &Path) -> Option<PathBuf> {
        let installed = root.join(".loom/scripts/cli/loom-daemon-update.sh");
        if installed.exists() {
            return Some(installed);
        }
        let source = root.join("defaults/scripts/cli/loom-daemon-update.sh");
        if source.exists() {
            return Some(source);
        }
        None
    }
}

impl AutoUpdateProbe for ScriptAutoUpdateProbe {
    fn check(&self) -> UpdateCheck {
        let status = crate::self_update::check();
        UpdateCheck {
            update_available: status.update_available,
            source_commit: status.source_commit,
        }
    }

    fn is_tree_clean(&self) -> Option<bool> {
        crate::self_update::source_tree_clean()
    }

    fn in_flight_sweeps(&self) -> usize {
        crate::ipc::count_in_flight_sweeps(&self.workspace_pool, &self.fallback_root)
    }

    fn rebuild(&mut self) -> RebuildOutcome {
        let Some(root) = self.source_root.clone() else {
            return RebuildOutcome::Retryable(
                "no source checkout resolved — cannot rebuild".to_string(),
            );
        };
        let Some(script) = Self::resolve_script(&root) else {
            return RebuildOutcome::Retryable(format!(
                "loom-daemon-update.sh not found under {}",
                root.display()
            ));
        };
        run_update_script(&script, &root, self.timeout)
    }
}

/// The production [`DrainTrigger`]: calls [`crate::ipc::handle_drain_request`]
/// (the #4090 primitive) inside a captured runtime handle so the supervisor it
/// spawns resolves a runtime even when invoked from a blocking thread.
pub struct IpcDrainTrigger {
    drain: Arc<DrainState>,
    workspace_pool: Arc<WorkspacePool>,
    fallback_root: PathBuf,
    event_bus: Arc<EventBus>,
    handle: tokio::runtime::Handle,
}

impl IpcDrainTrigger {
    #[must_use]
    pub fn new(
        drain: Arc<DrainState>,
        workspace_pool: Arc<WorkspacePool>,
        fallback_root: PathBuf,
        event_bus: Arc<EventBus>,
        handle: tokio::runtime::Handle,
    ) -> Self {
        Self {
            drain,
            workspace_pool,
            fallback_root,
            event_bus,
            handle,
        }
    }
}

impl DrainTrigger for IpcDrainTrigger {
    fn trigger(&self) -> bool {
        // Enter the runtime so `handle_drain_request`'s internal `tokio::spawn`
        // of the drain supervisor resolves a runtime from a blocking thread.
        // `timeout_secs=None` uses the default drain deadline;
        // `force_after_timeout=false` is the fail-safe — if in-flight sweeps do
        // not drain by the deadline the roll is refused and dispatch resumes,
        // never killing a sweep.
        let _guard = self.handle.enter();
        let resp = crate::ipc::handle_drain_request(
            &self.drain,
            &self.workspace_pool,
            &self.fallback_root,
            &self.event_bus,
            None,
            false,
        );
        matches!(resp, crate::types::Response::DaemonDrain { accepted: true, .. })
    }
}

/// Run `loom-daemon-update.sh --no-restart` in `cwd`, capturing combined output
/// to a temp file (never a pipe — `cargo build --release` is long and chatty,
/// exactly the pipe-buffer-deadlock case) and killing it after `timeout`.
/// Maps the script's documented exit codes to a [`RebuildOutcome`]:
/// `0`→Success, `4`/`5`→Terminal, everything else (incl. spawn/timeout)
/// →Retryable.
fn run_update_script(script: &Path, cwd: &Path, timeout: Duration) -> RebuildOutcome {
    let log_path =
        std::env::temp_dir().join(format!("loom-auto-update-{}.log", uuid::Uuid::new_v4()));
    let out_file = match std::fs::File::create(&log_path) {
        Ok(f) => f,
        Err(e) => return RebuildOutcome::Retryable(format!("could not create output file: {e}")),
    };
    let stderr_file = match out_file.try_clone() {
        Ok(f) => f,
        Err(e) => {
            let _ = std::fs::remove_file(&log_path);
            return RebuildOutcome::Retryable(format!("could not clone output handle: {e}"));
        }
    };

    let mut child = match Command::new(script)
        .arg("--no-restart")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(stderr_file))
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&log_path);
            return RebuildOutcome::Retryable(format!(
                "could not spawn `{}`: {e}",
                script.display()
            ));
        }
    };

    let start = Instant::now();
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) => break classify_exit(status.code(), &log_path),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    break RebuildOutcome::Retryable(format!(
                        "`{}` timed out after {}s",
                        script.display(),
                        timeout.as_secs()
                    ));
                }
                std::thread::sleep(REBUILD_POLL_INTERVAL);
            }
            Err(e) => {
                break RebuildOutcome::Retryable(format!(
                    "could not poll `{}`: {e}",
                    script.display()
                ))
            }
        }
    };
    let _ = std::fs::remove_file(&log_path);
    outcome
}

/// Map a `loom-daemon-update.sh` exit code to a [`RebuildOutcome`], attaching a
/// tail of the captured output on any non-success.
fn classify_exit(code: Option<i32>, log_path: &Path) -> RebuildOutcome {
    let tail = || truncate_tail(&std::fs::read_to_string(log_path).unwrap_or_default());
    match code {
        Some(0) => RebuildOutcome::Success,
        // #4053: exit 4 (build-verification) and 5 (post-provision
        // verification) are commit-identity defects retrying cannot fix.
        Some(4) => {
            RebuildOutcome::Terminal(format!("build verification failed (exit 4): {}", tail()))
        }
        Some(5) => RebuildOutcome::Terminal(format!(
            "post-provision verification failed (exit 5): {}",
            tail()
        )),
        Some(other) => RebuildOutcome::Retryable(format!("exit {other}: {}", tail())),
        None => RebuildOutcome::Retryable(format!("killed by signal: {}", tail())),
    }
}

/// Keep only the last [`MAX_OUTPUT_TAIL_BYTES`] bytes of captured output,
/// trimmed, on a char boundary.
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
// Loop state + pure decision logic
// ============================================================================

/// The per-tick decision the loop reaches from the current state + probe
/// readings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickDecision {
    /// Do nothing this tick; the string is the human-readable reason surfaced in
    /// `--status`.
    Skip(String),
    /// All gates passed — run the rebuild.
    Rebuild,
}

/// The loop's mutable bookkeeping: settle-window tracking, backoff, and the
/// terminal give-up state. Kept separate from any I/O so the deciding logic is
/// unit-testable with plain values.
#[derive(Debug, Default)]
pub struct AutoUpdateState {
    /// The stale source commit currently being tracked for the settle window.
    tracked_commit: Option<String>,
    /// When the currently-tracked stale commit was first observed (settle
    /// timer origin). Monotonic — never wall-clock — for correct durations.
    stale_since: Option<Instant>,
    /// Consecutive retryable build failures for the tracked commit.
    consecutive_failures: u32,
    /// Instant until which the loop is backing off after a retryable failure.
    backoff_until: Option<Instant>,
    /// The current backoff delay (for status), or `None` when not backing off.
    backoff: Option<Duration>,
    /// A terminal give-up reason for the tracked commit (sticky until the
    /// source commit advances).
    terminal_reason: Option<String>,
    /// Wall-clock time of the last successful roll (for status).
    last_roll: Option<DateTime<Utc>>,
}

impl AutoUpdateState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Decide this tick from the probe readings. Mutates settle-window and
    /// commit-identity bookkeeping (resetting backoff/terminal when the source
    /// commit advances) but performs no I/O.
    pub fn decide(
        &mut self,
        now: Instant,
        check: &UpdateCheck,
        tree_clean: bool,
        in_flight: usize,
        settle: Duration,
    ) -> TickDecision {
        // Only `Some(true)` is actionable. `Some(false)` (current) and `None`
        // (undecidable — tarball / unknown built commit) both clear any pending
        // settle state and do nothing.
        if check.update_available != Some(true) {
            self.tracked_commit = None;
            self.stale_since = None;
            return TickDecision::Skip(match check.update_available {
                Some(false) => "up to date with source HEAD".to_string(),
                _ => "no source checkout / staleness undecidable — nothing to do".to_string(),
            });
        }

        // A new (or first) stale commit resets the settle timer AND clears any
        // backoff/terminal state — a later commit is a fresh attempt that may
        // well fix a previously-broken build.
        if self.tracked_commit != check.source_commit {
            self.tracked_commit = check.source_commit.clone();
            self.stale_since = Some(now);
            self.consecutive_failures = 0;
            self.backoff_until = None;
            self.backoff = None;
            self.terminal_reason = None;
        }

        if let Some(reason) = &self.terminal_reason {
            return TickDecision::Skip(format!(
                "terminal — not retrying until a new commit: {reason}"
            ));
        }

        if let Some(until) = self.backoff_until {
            if now < until {
                let secs = until.saturating_duration_since(now).as_secs();
                return TickDecision::Skip(format!(
                    "backing off after build failure (~{secs}s left)"
                ));
            }
        }

        // Clean-tree gate — refuse an unattended build of a dirty checkout.
        if !tree_clean {
            return TickDecision::Skip(
                "source tree is dirty — refusing an unattended rebuild (never `git pull`)"
                    .to_string(),
            );
        }

        // Settle window — batch a burst of commits into one roll.
        let settled = self
            .stale_since
            .is_some_and(|s| now.duration_since(s) >= settle);
        if !settled {
            return TickDecision::Skip(
                "within settle window — waiting for commits to settle".to_string(),
            );
        }

        // Gate 4 — do not stampede in-flight sweep builds.
        if in_flight > 0 {
            return TickDecision::Skip(format!(
                "{in_flight} in-flight sweep(s) — deferring rebuild to avoid a build stampede"
            ));
        }

        TickDecision::Rebuild
    }

    /// Record the outcome of a rebuild attempt (and, for a success, whether the
    /// subsequent drain was accepted). Updates backoff / terminal / last-roll.
    /// `now` is the same monotonic clock [`Self::decide`] reads, so backoff
    /// deadlines and the settle window share one time base. Returns the note to
    /// surface for this tick.
    pub fn record_rebuild(
        &mut self,
        now: Instant,
        outcome: &RebuildOutcome,
        drain_accepted: bool,
    ) -> String {
        match outcome {
            RebuildOutcome::Success => {
                if drain_accepted {
                    self.consecutive_failures = 0;
                    self.backoff_until = None;
                    self.backoff = None;
                    self.last_roll = Some(Utc::now());
                    "rebuilt + provisioned; drain-and-restart triggered".to_string()
                } else {
                    // The binary IS provisioned, but the drain was refused
                    // (e.g. no supervisor). Do not treat as a build failure —
                    // launchd will pick up the fresh binary on the next
                    // supervised restart. Surface it without backing off.
                    self.last_roll = Some(Utc::now());
                    "rebuilt + provisioned, but drain-and-restart was refused (no supervisor?) — \
                     restart manually to run the fresh binary"
                        .to_string()
                }
            }
            RebuildOutcome::Retryable(msg) => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                let delay = backoff_delay(self.consecutive_failures);
                self.backoff = Some(delay);
                self.backoff_until = Some(now + delay);
                format!(
                    "rebuild failed (attempt {}, backing off {}s): {msg}",
                    self.consecutive_failures,
                    delay.as_secs()
                )
            }
            RebuildOutcome::Terminal(msg) => {
                self.terminal_reason = Some(msg.clone());
                self.backoff_until = None;
                self.backoff = None;
                format!("rebuild TERMINALLY failed — not retrying until a new commit: {msg}")
            }
        }
    }

    /// Build the status snapshot published after this tick.
    fn snapshot(
        &self,
        enabled: bool,
        last_check: DateTime<Utc>,
        note: String,
    ) -> AutoUpdateStatusSnapshot {
        AutoUpdateStatusSnapshot {
            enabled,
            last_check: Some(last_check),
            last_roll: self.last_roll,
            consecutive_failures: self.consecutive_failures,
            backoff_secs: self.backoff.map(|d| d.as_secs()),
            terminal_reason: self.terminal_reason.clone(),
            note: Some(note),
        }
    }
}

/// Exponential backoff with a ceiling: `min(BASE * 2^(failures-1), CEILING)`.
/// Saturating — a large failure count never overflows the shift.
#[must_use]
fn backoff_delay(failures: u32) -> Duration {
    if failures <= 1 {
        return BACKOFF_BASE;
    }
    let shift = failures - 1;
    // Cap the shift so `BASE.as_secs() << shift` never overflows; the ceiling
    // clamps anything past it anyway.
    let base = BACKOFF_BASE.as_secs();
    let scaled = base.checked_shl(shift).unwrap_or(u64::MAX);
    Duration::from_secs(scaled.min(BACKOFF_CEILING.as_secs()))
}

// ============================================================================
// Runtime wiring
// ============================================================================

/// Run one full tick: publish `last_check`, probe, decide, and — on a `Rebuild`
/// decision — rebuild and (on success) trigger the drain-and-restart. Pure of
/// spawning concerns so tests can drive it directly. Returns the note surfaced.
fn run_tick<P: AutoUpdateProbe, T: DrainTrigger>(
    state: &mut AutoUpdateState,
    status: &AutoUpdateStatus,
    probe: &mut P,
    trigger: &T,
    settle: Duration,
) {
    let now = Instant::now();
    let last_check = Utc::now();
    let check = probe.check();
    let tree_clean = probe.is_tree_clean().unwrap_or(false);
    // Only pay for the in-flight count once staleness is actionable (it loads
    // the workspace registry from disk); a cheap pre-filter avoids that read on
    // every up-to-date tick.
    let in_flight = if check.update_available == Some(true) {
        probe.in_flight_sweeps()
    } else {
        0
    };

    let note = match state.decide(now, &check, tree_clean, in_flight, settle) {
        TickDecision::Skip(reason) => reason,
        TickDecision::Rebuild => {
            log::info!(
                "auto_update: source is stale and settled with 0 in-flight sweeps — rebuilding"
            );
            let outcome = probe.rebuild();
            let drain_accepted = matches!(outcome, RebuildOutcome::Success) && trigger.trigger();
            let note = state.record_rebuild(now, &outcome, drain_accepted);
            match &outcome {
                RebuildOutcome::Success => log::warn!("auto_update: {note}"),
                RebuildOutcome::Retryable(_) => log::warn!("auto_update: {note}"),
                RebuildOutcome::Terminal(_) => log::error!("auto_update: {note}"),
            }
            note
        }
    };

    status.publish(state.snapshot(true, last_check, note));
}

/// Spawn the **single** process-global auto-update loop on the shared daemon
/// runtime (Issue #4055). Registers `status` as the process-global so
/// `loom-daemon status` can render it, then ticks every `interval`, moving the
/// per-tick blocking work (git/cargo subprocesses, registry reads) onto
/// `spawn_blocking` so it never parks a runtime worker.
///
/// Unlike the sibling autonomous loops this is **not** a `spawn_multi_*`
/// per-workspace fan-out: the daemon has one binary and one source checkout, so
/// exactly one loop runs regardless of how many workspaces are registered.
pub fn spawn_auto_update_task<P, T>(
    mut probe: P,
    mut trigger: T,
    status: Arc<AutoUpdateStatus>,
    interval: Duration,
    settle: Duration,
) -> tokio::task::JoinHandle<()>
where
    P: AutoUpdateProbe + Send + 'static,
    T: DrainTrigger + Send + Sync + 'static,
{
    register_global_status(status.clone());
    log::info!(
        "auto_update: starting loop (interval={}s, settle={}s)",
        interval.as_secs(),
        settle.as_secs()
    );
    tokio::spawn(async move {
        let mut state = AutoUpdateState::new();
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let status_task = status.clone();
            let joined = tokio::task::spawn_blocking(move || {
                run_tick(&mut state, &status_task, &mut probe, &trigger, settle);
                (state, probe, trigger)
            })
            .await;
            match joined {
                Ok((s, p, t)) => {
                    state = s;
                    probe = p;
                    trigger = t;
                }
                Err(e) => {
                    log::error!(
                        "auto_update: tick task panicked ({e}); stopping loop (the running \
                         daemon is left untouched)"
                    );
                    return;
                }
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn write_config(root: &Path, contents: &str) {
        fs::create_dir_all(root.join(".loom")).unwrap();
        fs::write(root.join(".loom").join("config.json"), contents).unwrap();
    }

    fn write_project_config(root: &Path, contents: &str) {
        let full = root.join(crate::config_resolver::PROJECT_CONFIG_REL);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, contents).unwrap();
    }

    fn stale(commit: &str) -> UpdateCheck {
        UpdateCheck {
            update_available: Some(true),
            source_commit: Some(commit.to_string()),
        }
    }

    // ===================================================================
    // Config surface — autonomous.autoUpdate (soft-fail + happy path)
    // ===================================================================

    #[test]
    fn test_config_missing_file_is_default() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_auto_update_config(tmp.path()), AutoUpdateConfig::default());
    }

    #[test]
    fn test_config_malformed_json_is_default() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), "{not valid json");
        assert_eq!(read_auto_update_config(tmp.path()), AutoUpdateConfig::default());
    }

    #[test]
    fn test_config_missing_block_is_default() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"workFinder": {"enabled": true}}}"#);
        assert_eq!(read_auto_update_config(tmp.path()), AutoUpdateConfig::default());
    }

    #[test]
    fn test_config_reads_all_fields() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous": {"autoUpdate": {"enabled": true, "intervalSecs": 120, "settleSecs": 30}}}"#,
        );
        assert_eq!(
            read_auto_update_config(tmp.path()),
            AutoUpdateConfig {
                enabled: Some(true),
                interval_secs: Some(120),
                settle_secs: Some(30),
            }
        );
    }

    #[test]
    fn test_config_zero_values_dropped_to_none() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous": {"autoUpdate": {"intervalSecs": 0, "settleSecs": 0}}}"#,
        );
        let cfg = read_auto_update_config(tmp.path());
        assert_eq!(cfg.interval_secs, None);
        assert_eq!(cfg.settle_secs, None);
    }

    // ===================================================================
    // config_resolver migration (#4058) — .loom-project/ tier
    // ===================================================================

    #[test]
    #[serial(loom_config_env)]
    fn test_config_project_tier_only_is_honored() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_project_config(
            tmp.path(),
            r#"{"autonomous": {"autoUpdate": {"enabled": true, "intervalSecs": 120}}}"#,
        );
        let cfg = read_auto_update_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(cfg.enabled, Some(true));
        assert_eq!(cfg.interval_secs, Some(120));
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_project_tier_overrides_legacy() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous": {"autoUpdate": {"enabled": true, "settleSecs": 600}}}"#,
        );
        write_project_config(tmp.path(), r#"{"autonomous": {"autoUpdate": {"settleSecs": 30}}}"#);
        let cfg = read_auto_update_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        // Overlapping settleSecs -> project tier wins; non-overlapping enabled
        // still supplied by the legacy tier.
        assert_eq!(cfg.settle_secs, Some(30));
        assert_eq!(cfg.enabled, Some(true));
    }

    // ===================================================================
    // Precedence — env > config > default
    // ===================================================================

    #[test]
    #[serial]
    fn test_resolve_enabled_default_is_false() {
        std::env::remove_var(AUTO_UPDATE_ENABLE_ENV);
        assert!(
            !resolve_enabled(&AutoUpdateConfig::default()),
            "absent config + unset env ⇒ default OFF (opt-in loop)"
        );
    }

    #[test]
    #[serial]
    fn test_resolve_enabled_config_then_env() {
        std::env::remove_var(AUTO_UPDATE_ENABLE_ENV);
        assert!(resolve_enabled(&AutoUpdateConfig {
            enabled: Some(true),
            ..AutoUpdateConfig::default()
        }));
        // Env forces OFF over config-on.
        std::env::set_var(AUTO_UPDATE_ENABLE_ENV, "0");
        assert!(!resolve_enabled(&AutoUpdateConfig {
            enabled: Some(true),
            ..AutoUpdateConfig::default()
        }));
        // Env forces ON over config-off.
        std::env::set_var(AUTO_UPDATE_ENABLE_ENV, "1");
        assert!(resolve_enabled(&AutoUpdateConfig {
            enabled: Some(false),
            ..AutoUpdateConfig::default()
        }));
        std::env::remove_var(AUTO_UPDATE_ENABLE_ENV);
    }

    #[test]
    #[serial]
    fn test_resolve_interval_and_settle_precedence() {
        std::env::remove_var(AUTO_UPDATE_INTERVAL_ENV);
        std::env::remove_var(AUTO_UPDATE_SETTLE_ENV);

        // Defaults.
        assert_eq!(
            resolve_interval(&AutoUpdateConfig::default()),
            Duration::from_secs(DEFAULT_AUTO_UPDATE_INTERVAL_SECS)
        );
        assert_eq!(
            resolve_settle(&AutoUpdateConfig::default()),
            Duration::from_secs(DEFAULT_AUTO_UPDATE_SETTLE_SECS)
        );

        // Config alone.
        let cfg = AutoUpdateConfig {
            enabled: None,
            interval_secs: Some(300),
            settle_secs: Some(45),
        };
        assert_eq!(resolve_interval(&cfg), Duration::from_secs(300));
        assert_eq!(resolve_settle(&cfg), Duration::from_secs(45));

        // Env overrides config.
        std::env::set_var(AUTO_UPDATE_INTERVAL_ENV, "77");
        std::env::set_var(AUTO_UPDATE_SETTLE_ENV, "11");
        assert_eq!(resolve_interval(&cfg), Duration::from_secs(77));
        assert_eq!(resolve_settle(&cfg), Duration::from_secs(11));

        // Zero/garbage env falls through to config, not the default.
        std::env::set_var(AUTO_UPDATE_INTERVAL_ENV, "0");
        std::env::set_var(AUTO_UPDATE_SETTLE_ENV, "garbage");
        assert_eq!(resolve_interval(&cfg), Duration::from_secs(300));
        assert_eq!(resolve_settle(&cfg), Duration::from_secs(45));

        std::env::remove_var(AUTO_UPDATE_INTERVAL_ENV);
        std::env::remove_var(AUTO_UPDATE_SETTLE_ENV);
    }

    // ===================================================================
    // Backoff math
    // ===================================================================

    #[test]
    fn test_backoff_is_exponential_with_ceiling() {
        assert_eq!(backoff_delay(1), BACKOFF_BASE);
        assert_eq!(backoff_delay(2), Duration::from_secs(120));
        assert_eq!(backoff_delay(3), Duration::from_secs(240));
        // Eventually clamps at the ceiling and never overflows.
        assert_eq!(backoff_delay(10), BACKOFF_CEILING);
        assert_eq!(backoff_delay(u32::MAX), BACKOFF_CEILING);
    }

    // ===================================================================
    // Decision logic — the settle/clean/gate matrix
    // ===================================================================

    const SETTLE: Duration = Duration::from_secs(60);

    #[test]
    fn test_decide_up_to_date_is_skip() {
        let mut st = AutoUpdateState::new();
        let now = Instant::now();
        let check = UpdateCheck {
            update_available: Some(false),
            source_commit: Some("abc".into()),
        };
        assert!(matches!(st.decide(now, &check, true, 0, SETTLE), TickDecision::Skip(_)));
    }

    #[test]
    fn test_decide_undecidable_none_is_skip() {
        let mut st = AutoUpdateState::new();
        let now = Instant::now();
        let check = UpdateCheck {
            update_available: None,
            source_commit: None,
        };
        assert!(matches!(st.decide(now, &check, true, 0, SETTLE), TickDecision::Skip(_)));
    }

    #[test]
    fn test_decide_stale_dirty_tree_is_skip() {
        let mut st = AutoUpdateState::new();
        let base = Instant::now();
        // First observe (starts settle timer), then advance past settle.
        st.decide(base, &stale("c1"), false, 0, SETTLE);
        let later = base + SETTLE + Duration::from_secs(1);
        let d = st.decide(later, &stale("c1"), false, 0, SETTLE);
        assert!(matches!(d, TickDecision::Skip(reason) if reason.contains("dirty")));
    }

    #[test]
    fn test_decide_stale_clean_within_settle_is_skip() {
        let mut st = AutoUpdateState::new();
        let base = Instant::now();
        let d = st.decide(base, &stale("c1"), true, 0, SETTLE);
        assert!(matches!(d, TickDecision::Skip(reason) if reason.contains("settle")));
    }

    #[test]
    fn test_decide_stale_clean_settled_zero_inflight_is_rebuild() {
        let mut st = AutoUpdateState::new();
        let base = Instant::now();
        st.decide(base, &stale("c1"), true, 0, SETTLE);
        let later = base + SETTLE + Duration::from_secs(1);
        assert_eq!(st.decide(later, &stale("c1"), true, 0, SETTLE), TickDecision::Rebuild);
    }

    #[test]
    fn test_decide_gate4_inflight_sweeps_blocks_rebuild() {
        let mut st = AutoUpdateState::new();
        let base = Instant::now();
        st.decide(base, &stale("c1"), true, 3, SETTLE);
        let later = base + SETTLE + Duration::from_secs(1);
        let d = st.decide(later, &stale("c1"), true, 3, SETTLE);
        assert!(matches!(d, TickDecision::Skip(reason) if reason.contains("in-flight")));
    }

    #[test]
    fn test_new_commit_resets_settle_window() {
        let mut st = AutoUpdateState::new();
        let base = Instant::now();
        st.decide(base, &stale("c1"), true, 0, SETTLE);
        // Settled for c1...
        let later = base + SETTLE + Duration::from_secs(1);
        // ...but a NEW commit lands: the settle timer restarts, so this tick is
        // within-settle again (not a rebuild).
        let d = st.decide(later, &stale("c2"), true, 0, SETTLE);
        assert!(matches!(d, TickDecision::Skip(reason) if reason.contains("settle")));
    }

    // ===================================================================
    // Backoff + terminal state transitions
    // ===================================================================

    #[test]
    fn test_retryable_failures_back_off_then_reset_on_success() {
        let mut st = AutoUpdateState::new();
        let t0 = Instant::now();
        // Establish a tracked commit + settle so backoff_until is meaningful.
        st.decide(t0, &stale("c1"), true, 0, SETTLE);

        st.record_rebuild(t0, &RebuildOutcome::Retryable("boom".into()), false);
        assert_eq!(st.consecutive_failures, 1);
        assert_eq!(st.backoff, Some(backoff_delay(1)));

        st.record_rebuild(t0, &RebuildOutcome::Retryable("boom".into()), false);
        assert_eq!(st.consecutive_failures, 2);
        assert_eq!(st.backoff, Some(backoff_delay(2)));

        // A success (with an accepted drain) resets the counter + clears backoff.
        st.record_rebuild(t0, &RebuildOutcome::Success, true);
        assert_eq!(st.consecutive_failures, 0);
        assert_eq!(st.backoff, None);
        assert!(st.last_roll.is_some());
    }

    #[test]
    fn test_backing_off_blocks_rebuild_until_delay_elapses() {
        let mut st = AutoUpdateState::new();
        let t0 = Instant::now();
        st.decide(t0, &stale("c1"), true, 0, SETTLE);
        // A settled rebuild fails at t1; backoff_until = t1 + backoff_delay(1) (60s).
        let t1 = t0 + SETTLE;
        st.record_rebuild(t1, &RebuildOutcome::Retryable("boom".into()), false);
        // t2 is settled (past t0+SETTLE) but still inside the 60s backoff window.
        let t2 = t1 + Duration::from_secs(30);
        let d = st.decide(t2, &stale("c1"), true, 0, SETTLE);
        assert!(matches!(d, TickDecision::Skip(reason) if reason.contains("backing off")));
        // Once the backoff elapses, the same settled/clean/idle state rebuilds.
        let t3 = t1 + backoff_delay(1) + Duration::from_secs(1);
        assert_eq!(st.decide(t3, &stale("c1"), true, 0, SETTLE), TickDecision::Rebuild);
    }

    #[test]
    fn test_terminal_is_sticky_until_commit_changes() {
        let mut st = AutoUpdateState::new();
        let t0 = Instant::now();
        st.decide(t0, &stale("c1"), true, 0, SETTLE);
        st.record_rebuild(t0, &RebuildOutcome::Terminal("commit mismatch (exit 4)".into()), false);
        assert!(st.terminal_reason.is_some());

        // Same commit, fully settled + clean + idle: still skipped (terminal).
        let later = t0 + SETTLE + Duration::from_secs(1);
        let d = st.decide(later, &stale("c1"), true, 0, SETTLE);
        assert!(matches!(d, TickDecision::Skip(reason) if reason.contains("terminal")));

        // A NEW commit clears the terminal state (fresh attempt).
        st.decide(later, &stale("c2"), true, 0, SETTLE);
        assert!(st.terminal_reason.is_none());
        assert_eq!(st.consecutive_failures, 0);
    }

    // ===================================================================
    // Exit-code → RebuildOutcome mapping (#4053 exit 4/5 terminal)
    // ===================================================================

    fn write_fake_script(dir: &Path, body: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join("fake-update.sh");
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

    #[test]
    fn test_run_update_script_exit_0_is_success() {
        let tmp = tempfile::tempdir().unwrap();
        let s = write_fake_script(tmp.path(), "echo built; exit 0");
        assert_eq!(
            run_update_script(&s, tmp.path(), Duration::from_secs(10)),
            RebuildOutcome::Success
        );
    }

    #[test]
    fn test_run_update_script_exit_1_is_retryable() {
        let tmp = tempfile::tempdir().unwrap();
        let s = write_fake_script(tmp.path(), "echo compile error; exit 1");
        let o = run_update_script(&s, tmp.path(), Duration::from_secs(10));
        assert!(matches!(o, RebuildOutcome::Retryable(m) if m.contains("compile error")));
    }

    #[test]
    fn test_run_update_script_exit_4_is_terminal() {
        let tmp = tempfile::tempdir().unwrap();
        let s = write_fake_script(tmp.path(), "echo commit mismatch; exit 4");
        let o = run_update_script(&s, tmp.path(), Duration::from_secs(10));
        assert!(matches!(o, RebuildOutcome::Terminal(m) if m.contains("commit mismatch")));
    }

    #[test]
    fn test_run_update_script_exit_5_is_terminal() {
        let tmp = tempfile::tempdir().unwrap();
        let s = write_fake_script(tmp.path(), "exit 5");
        assert!(matches!(
            run_update_script(&s, tmp.path(), Duration::from_secs(10)),
            RebuildOutcome::Terminal(_)
        ));
    }

    #[test]
    fn test_run_update_script_timeout_is_retryable() {
        let tmp = tempfile::tempdir().unwrap();
        let s = write_fake_script(tmp.path(), "sleep 30");
        let o = run_update_script(&s, tmp.path(), Duration::from_millis(300));
        assert!(matches!(o, RebuildOutcome::Retryable(m) if m.contains("timed out")));
    }

    #[test]
    fn test_run_update_script_spawn_failure_is_retryable() {
        let tmp = tempfile::tempdir().unwrap();
        let bogus = tmp.path().join("does-not-exist.sh");
        assert!(matches!(
            run_update_script(&bogus, tmp.path(), Duration::from_secs(10)),
            RebuildOutcome::Retryable(_)
        ));
    }

    // ===================================================================
    // Loop wiring — fake probe + trigger drive run_tick end to end
    // ===================================================================

    struct FakeProbe {
        check: UpdateCheck,
        tree_clean: Option<bool>,
        in_flight: usize,
        rebuild_outcome: RebuildOutcome,
        rebuild_calls: Arc<AtomicUsize>,
    }

    impl AutoUpdateProbe for FakeProbe {
        fn check(&self) -> UpdateCheck {
            self.check.clone()
        }
        fn is_tree_clean(&self) -> Option<bool> {
            self.tree_clean
        }
        fn in_flight_sweeps(&self) -> usize {
            self.in_flight
        }
        fn rebuild(&mut self) -> RebuildOutcome {
            self.rebuild_calls.fetch_add(1, Ordering::SeqCst);
            self.rebuild_outcome.clone()
        }
    }

    struct FakeTrigger {
        accepted: bool,
        calls: Arc<AtomicUsize>,
    }

    impl DrainTrigger for FakeTrigger {
        fn trigger(&self) -> bool {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.accepted
        }
    }

    /// A settled, clean, idle, stale probe rolls exactly once and triggers a
    /// drain — the full happy path through `run_tick`.
    #[test]
    fn test_run_tick_rolls_and_triggers_drain_when_settled() {
        let rebuild_calls = Arc::new(AtomicUsize::new(0));
        let trigger_calls = Arc::new(AtomicUsize::new(0));
        let mut probe = FakeProbe {
            check: stale("c1"),
            tree_clean: Some(true),
            in_flight: 0,
            rebuild_outcome: RebuildOutcome::Success,
            rebuild_calls: rebuild_calls.clone(),
        };
        let trigger = FakeTrigger {
            accepted: true,
            calls: trigger_calls.clone(),
        };
        let status = AutoUpdateStatus::new(true);
        let mut state = AutoUpdateState::new();
        // A zero settle window makes the very first observed-stale tick settled.
        let settle = Duration::from_secs(0);

        run_tick(&mut state, &status, &mut probe, &trigger, settle);
        assert_eq!(rebuild_calls.load(Ordering::SeqCst), 1);
        assert_eq!(trigger_calls.load(Ordering::SeqCst), 1);
        let snap = status.snapshot();
        assert!(snap.last_roll.is_some());
        assert_eq!(snap.consecutive_failures, 0);
    }

    #[test]
    fn test_run_tick_none_never_rebuilds() {
        let rebuild_calls = Arc::new(AtomicUsize::new(0));
        let trigger_calls = Arc::new(AtomicUsize::new(0));
        let mut probe = FakeProbe {
            check: UpdateCheck {
                update_available: None,
                source_commit: None,
            },
            tree_clean: Some(true),
            in_flight: 0,
            rebuild_outcome: RebuildOutcome::Success,
            rebuild_calls: rebuild_calls.clone(),
        };
        let trigger = FakeTrigger {
            accepted: true,
            calls: trigger_calls.clone(),
        };
        let status = AutoUpdateStatus::new(true);
        let mut state = AutoUpdateState::new();
        run_tick(&mut state, &status, &mut probe, &trigger, Duration::from_secs(0));
        assert_eq!(rebuild_calls.load(Ordering::SeqCst), 0, "None must never rebuild");
        assert_eq!(trigger_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn test_run_tick_dirty_tree_never_rebuilds() {
        let rebuild_calls = Arc::new(AtomicUsize::new(0));
        let mut probe = FakeProbe {
            check: stale("c1"),
            tree_clean: Some(false),
            in_flight: 0,
            rebuild_outcome: RebuildOutcome::Success,
            rebuild_calls: rebuild_calls.clone(),
        };
        let trigger = FakeTrigger {
            accepted: true,
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let status = AutoUpdateStatus::new(true);
        let mut state = AutoUpdateState::new();
        run_tick(&mut state, &status, &mut probe, &trigger, Duration::from_secs(0));
        assert_eq!(rebuild_calls.load(Ordering::SeqCst), 0, "dirty tree must never rebuild");
    }

    #[test]
    fn test_run_tick_terminal_exit_not_retried_next_tick() {
        let rebuild_calls = Arc::new(AtomicUsize::new(0));
        let mut probe = FakeProbe {
            check: stale("c1"),
            tree_clean: Some(true),
            in_flight: 0,
            rebuild_outcome: RebuildOutcome::Terminal("commit mismatch (exit 4)".into()),
            rebuild_calls: rebuild_calls.clone(),
        };
        let trigger = FakeTrigger {
            accepted: true,
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let status = AutoUpdateStatus::new(true);
        let mut state = AutoUpdateState::new();
        let settle = Duration::from_secs(0);
        // Tick 1: rebuilds, hits terminal.
        run_tick(&mut state, &status, &mut probe, &trigger, settle);
        assert_eq!(rebuild_calls.load(Ordering::SeqCst), 1);
        assert!(status.snapshot().terminal_reason.is_some());
        // Tick 2 (same commit): must NOT rebuild again.
        run_tick(&mut state, &status, &mut probe, &trigger, settle);
        assert_eq!(rebuild_calls.load(Ordering::SeqCst), 1, "terminal must not retry same commit");
    }

    #[test]
    fn test_run_tick_gate4_defers_rebuild_with_inflight_sweeps() {
        let rebuild_calls = Arc::new(AtomicUsize::new(0));
        let mut probe = FakeProbe {
            check: stale("c1"),
            tree_clean: Some(true),
            in_flight: 2,
            rebuild_outcome: RebuildOutcome::Success,
            rebuild_calls: rebuild_calls.clone(),
        };
        let trigger = FakeTrigger {
            accepted: true,
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let status = AutoUpdateStatus::new(true);
        let mut state = AutoUpdateState::new();
        run_tick(&mut state, &status, &mut probe, &trigger, Duration::from_secs(0));
        assert_eq!(rebuild_calls.load(Ordering::SeqCst), 0, "in-flight sweeps must gate the build");
    }

    // ===================================================================
    // IpcDrainTrigger — the roll routes through #4090's drain primitive
    // ===================================================================

    /// The production trigger genuinely calls [`crate::ipc::handle_drain_request`]
    /// (the #4090 drain path), not a bare restart: on an **unsupervised** host it
    /// is refused (`accepted: false`) and — critically — dispatch is NOT paused
    /// (`is_draining()` stays false), exactly the drain primitive's contract.
    /// The supervised happy path (`is_draining()` true, `evaluate_drain_tick`
    /// completing only at 0 in-flight) is covered by #4090's own ipc.rs tests;
    /// exercising it here would `process::exit` the test runner.
    #[tokio::test]
    #[serial(loom_daemon_supervisor)]
    async fn test_ipc_drain_trigger_routes_through_drain_primitive() {
        std::env::remove_var("LOOM_DAEMON_SUPERVISOR");
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        // Force an empty workspace registry so count_in_flight_sweeps == 0 and
        // the only variable under test is the supervisor refusal.
        std::env::set_var(
            crate::workspace_registry::REGISTRY_PATH_ENV,
            root.join("no-such-workspaces.json"),
        );
        let bus = Arc::new(EventBus::new());
        let pool = Arc::new(WorkspacePool::new(bus.clone(), tokio::runtime::Handle::current()));
        let drain = Arc::new(DrainState::new());
        let trigger =
            IpcDrainTrigger::new(drain.clone(), pool, root, bus, tokio::runtime::Handle::current());

        let accepted = trigger.trigger();
        std::env::remove_var(crate::workspace_registry::REGISTRY_PATH_ENV);

        assert!(!accepted, "unsupervised host must refuse the drain (no bare restart fallback)");
        assert!(!drain.is_draining(), "a refused drain must not pause dispatch");
        assert_eq!(drain.generation(), 0, "a refused drain must not bump the drain generation");
    }

    // ===================================================================
    // Global status handle
    // ===================================================================

    #[test]
    fn test_global_status_defaults_when_unset() {
        // Not registering leaves the default (this may race other tests that
        // DO register, so only assert structural defaults on a fresh snapshot).
        let snap = AutoUpdateStatus::new(false).snapshot();
        assert!(!snap.enabled);
        assert!(snap.last_check.is_none());
    }
}
