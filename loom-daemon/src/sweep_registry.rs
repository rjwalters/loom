//! Sweep registry — in-memory tracking of dispatched `/loom:sweep` children
//! (Issue #3452, Phase A of epic #3449).
//!
//! # Overview
//!
//! This module implements the daemon-side surface for dispatching and
//! tracking `/loom:sweep` runs. It is the foundation for the v0.10.0 daemon
//! rebuild — Phase A delivers:
//!
//! - A `Sweep` resource type (see [`crate::types::SweepInfo`]).
//! - In-memory `BTreeMap<SweepId, SweepInfo>` storage.
//! - `dispatch_sweep` primitive that shells out to
//!   `defaults/scripts/spawn-claude.sh` (NOT a Rust re-implementation of
//!   token rotation) and detaches a `claude -p "/loom:sweep N"` child.
//! - `list_sweeps` query with optional state filtering.
//! - Atomic `mkdir`-based claim locks under `.loom/locks/issue-<N>/`,
//!   matching the spawn-loop primitive at
//!   `defaults/scripts/spawn-loop.sh:293-309`.
//! - A reaper task that polls live PIDs on a 30s interval (env-overridable
//!   via `LOOM_SWEEP_REAPER_INTERVAL_SECS`, matching the spawn-loop
//!   `POLL_INTERVAL` default at `spawn-loop.sh:110`).
//! - Registry reconstruction on startup from live processes + checkpoints.
//!
//! # Idempotency
//!
//! When `idempotency_key` is provided and a `Running` sweep already holds
//! it, dispatch returns the existing `sweep_id` with no new spawn. Exited
//! or crashed entries with a matching key do NOT block re-dispatch — the
//! dedup window is the lifetime of the *running* entry.
//!
//! # Forge as source of truth
//!
//! Per the parent epic, the daemon does NOT persist sweep state to disk.
//! Recovery on restart relies on:
//!
//! - Live process detection (`kill(pid, 0)`).
//! - Sweep checkpoints under `.loom/sweep-checkpoint/issue-<N>.json` (#3373),
//!   but **only for daemon-owned sweeps**: `.loom/sweep-checkpoint/` is shared
//!   with the in-session `/loom:sweep` path, so a checkpoint is recovered only
//!   when a daemon-owned lock (`.loom/locks/issue-<N>/`, written exclusively by
//!   `dispatch`) also existed for that issue. This keeps the daemon from
//!   ingesting phantom entries for in-session sweeps it never dispatched
//!   (#3808). See [`SweepRegistry::reconstruct`].
//! - Forge labels (`loom:issue` vs `loom:building`).

use crate::event_bus::EventBus;
use crate::types::{Event, SweepId, SweepInfo, SweepKind, SweepOutcome, SweepState};

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ============================================================================
// Constants
// ============================================================================

/// Default reaper polling interval in seconds. Matches
/// `defaults/scripts/spawn-loop.sh:110` `POLL_INTERVAL`.
pub const DEFAULT_REAPER_INTERVAL_SECS: u64 = 30;

/// Environment variable for overriding the reaper interval. Naming follows
/// the existing `LOOM_*` conventions in `main.rs` (e.g., `LOOM_CLAIM_TTL_SECS`,
/// `LOOM_WORKSPACE`, `LOOM_SOCKET_PATH`).
pub const REAPER_INTERVAL_ENV: &str = "LOOM_SWEEP_REAPER_INTERVAL_SECS";

/// Environment variable for overriding the dispatch entry point used by
/// the registry. Defaults to `defaults/scripts/spawn-claude.sh` relative to
/// the workspace. Used by integration tests to substitute a fake child.
pub const SPAWN_BIN_ENV: &str = "LOOM_SWEEP_SPAWN_BIN";

/// Environment variable for overriding the workspace root used by the
/// registry. Falls back to `LOOM_WORKSPACE`, then current dir.
pub const WORKSPACE_ENV: &str = "LOOM_WORKSPACE";

/// Retention window after a sweep terminates before it is garbage-collected
/// from the in-memory map. One hour matches the operator intuition that
/// "recently exited sweeps should still show up in `list_sweeps`".
pub const TERMINAL_RETENTION_SECS: i64 = 3600;

/// Issue #3730: experiment-related env vars forwarded to the detached sweep
/// child via an EXPLICIT ALLOWLIST (never a blanket env_clear/copy). Byte-exact
/// names verified against `loom_tools/sweep_experiment.py` (`LOOM_MODEL_EXPERIMENT`,
/// `LOOM_MODEL_EXPERIMENT_CANARY`) and `.loom/scripts/archive-transcripts.sh`
/// (`LOOM_TRANSCRIPT_ARCHIVE`). Forwarding these makes env-based experiment
/// enablement reliable regardless of how the daemon itself was launched — an
/// operator can export them right before dispatching and have them reach the
/// child. Each is forwarded only when set to a non-empty value (see
/// `spawn_child`), so the spawn is a no-op when none are set.
pub const EXPERIMENT_ENV_ALLOWLIST: &[&str] = &[
    "LOOM_MODEL_EXPERIMENT",
    "LOOM_MODEL_EXPERIMENT_CANARY",
    "LOOM_TRANSCRIPT_ARCHIVE",
];

/// Issue #3802: fallback `token_name` recorded when `spawn-claude.sh`'s
/// account selection cannot be captured (e.g. the `LOOM_SPAWN_NO_EXPORT`
/// bypass path, where the caller pre-exported `CLAUDE_CODE_OAUTH_TOKEN` and no
/// account is selected at all — nothing to report, not a bug).
pub const UNKNOWN_TOKEN_NAME: &str = "unknown";

/// Issue #3802: the marker substring `spawn-claude.sh` logs immediately after
/// selecting an OAuth account — `spawn-claude: using OAuth account '<name>'
/// (mode=<mode>)` (see `defaults/scripts/spawn-claude.sh`). The daemon already
/// captures the child's stderr into the per-sweep log, so `spawn_child` reads
/// the log back and extracts `<name>` from the text following this marker. The
/// account name itself carries no ANSI colour codes (only the timestamp prefix
/// does), so a plain substring scan is sufficient — no ANSI stripping needed.
const TOKEN_NAME_LOG_MARKER: &str = "using OAuth account '";

/// Issue #3802: how long `spawn_child` polls the per-sweep log for the
/// `TOKEN_NAME_LOG_MARKER` line before giving up and recording
/// `UNKNOWN_TOKEN_NAME`. `spawn-claude.sh` selects and logs the account early
/// (well before it `exec`s `claude`), so the line normally appears within a
/// second. The poll also short-circuits the moment the child exits without
/// having logged a selection (keeps no-selection fixtures fast), so this full
/// window is only ever waited out in the pathological "child alive but never
/// logged" case — capture then degrades gracefully to `UNKNOWN_TOKEN_NAME`
/// and never blocks or fails dispatch.
const TOKEN_NAME_CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);

/// Issue #3802: poll cadence for the `TOKEN_NAME_LOG_MARKER` scan.
const TOKEN_NAME_CAPTURE_POLL_INTERVAL: Duration = Duration::from_millis(25);

// ----------------------------------------------------------------------------
// Startup-race mitigation: dispatch stagger + watchdog (Issue #3887)
// ----------------------------------------------------------------------------
//
// # Root cause (0-HTTPS MCP-init race)
//
// When `loom-daemon` dispatches several sweeps back-to-back (the autonomous
// work-finder drains a `loom:issue` backlog in a single tick), each spawned
// `claude -p "/loom:sweep N"` child immediately forks its own `mcp-loom` node
// child and performs the MCP stdio handshake plus Claude Code's local startup
// (config + keychain read) BEFORE its first API call. When many of those
// startups run *simultaneously* (all within ~1s), some children wedge in that
// pre-API phase: the sweep log shows only the spawn header + the
// `spawn-claude: using OAuth account` line, no worktree is ever created, the
// process sits at ~0% CPU with **zero** open HTTPS connections, and the issue
// never leaves `loom:building`. Re-dispatching the same issue as a fresh
// process reliably clears it — the smoking gun that it is a *startup* race, not
// a rate-limit or a bad token.
//
// The token-selection files (`.loom/tokens/.ranking` / `.bad_tokens` /
// `index.json`) are NOT the culprit: `select.py` only *reads* them at spawn
// time (concurrent reads are safe), and the one writer path (`.bad_tokens`)
// is already `mkdir`-lock guarded and atomic. A read race would mis-select a
// token, never hang — and the hang is observed *after* the account line is
// already logged. The contention is the simultaneous MCP-init / local-startup
// itself.
//
// # Two-layer mitigation
//
// 1. **Dispatch stagger (prevention)** — the registry serializes child
//    startups by enforcing a minimum wall-clock gap between consecutive
//    `spawn`s (`apply_dispatch_stagger`). Spacing the spawns out of the
//    simultaneous window is what actually prevents the race; a burst of K
//    dispatches becomes K spawns spaced `stagger` apart instead of K
//    near-simultaneous ones.
// 2. **Startup watchdog (self-heal backstop)** — a background task probes each
//    running sweep for *progress* (worktree created / checkpoint written / log
//    output past the spawn header). A sweep that shows none within
//    `timeout` (default 120s) is auto-cancelled and re-dispatched **exactly
//    once** (bounded — never a loop), so a hang that slips past the stagger
//    self-heals instead of silently wedging an issue.

/// Default minimum wall-clock gap the registry enforces between consecutive
/// child spawns (Issue #3887). Chosen to comfortably exceed the
/// simultaneous-startup window in which the MCP-init race is observed (~1s)
/// while adding only a small, bounded latency to a burst dispatch.
pub const DEFAULT_DISPATCH_STAGGER_MS: u64 = 2000;

/// Env var overriding the dispatch stagger, in milliseconds. `0` disables the
/// stagger entirely (spawns are not spaced). Precedence: env > config > default.
pub const DISPATCH_STAGGER_ENV: &str = "LOOM_SWEEP_DISPATCH_STAGGER_MS";

/// Env var toggling the startup watchdog (Issue #3887). `0`/`false`/`no`/`off`
/// disables; `1`/`true`/`yes`/`on` forces on. Overrides config.
pub const WATCHDOG_ENABLE_ENV: &str = "LOOM_SWEEP_WATCHDOG";

/// Env var overriding the watchdog no-progress timeout, in seconds.
pub const WATCHDOG_TIMEOUT_ENV: &str = "LOOM_SWEEP_WATCHDOG_TIMEOUT_SECS";

/// Env var overriding the watchdog probe interval, in seconds.
pub const WATCHDOG_INTERVAL_ENV: &str = "LOOM_SWEEP_WATCHDOG_INTERVAL_SECS";

/// Default watchdog no-progress timeout: a sweep that has created no worktree,
/// written no checkpoint, and produced no log output past the spawn header
/// within this window is treated as hung. Generous enough that a healthy sweep
/// (which emits Curator-phase output well inside two minutes) never trips it.
pub const DEFAULT_WATCHDOG_TIMEOUT_SECS: u64 = 120;

/// Default watchdog probe interval — matches the reaper cadence.
pub const DEFAULT_WATCHDOG_INTERVAL_SECS: u64 = 30;

/// Grace period the watchdog gives a hung child to exit after SIGTERM before
/// escalating to SIGKILL, when it auto-cancels for re-dispatch.
const WATCHDOG_CANCEL_GRACE: Duration = Duration::from_secs(3);

/// The dispatch-header marker `spawn_child` writes before each spawn. Reused by
/// the watchdog's progress probe to anchor its scan to the current dispatch.
const DISPATCH_HEADER_MARKER: &str = "==== loom-daemon dispatch:";

/// Compute how long a spawn must wait so that consecutive spawns are separated
/// by at least `stagger` (Issue #3887). Pure function of the last spawn instant,
/// the configured gap, and the current instant — unit-tested in isolation.
///
/// Returns `Duration::ZERO` when the stagger is disabled (zero), when no prior
/// spawn has happened, or when at least `stagger` has already elapsed.
#[must_use]
pub fn stagger_wait(last_spawn_at: Option<Instant>, stagger: Duration, now: Instant) -> Duration {
    if stagger.is_zero() {
        return Duration::ZERO;
    }
    match last_spawn_at {
        None => Duration::ZERO,
        Some(last) => {
            let elapsed = now.saturating_duration_since(last);
            stagger.checked_sub(elapsed).unwrap_or(Duration::ZERO)
        }
    }
}

/// The watchdog's per-sweep decision (Issue #3887). Pure state machine —
/// [`watchdog_decision`] maps `(elapsed, timeout, made_progress,
/// already_retried)` onto exactly one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogDecision {
    /// The sweep is making progress, or is still inside the grace window —
    /// leave it alone.
    Healthy,
    /// No progress past the deadline and this issue has not been auto-restarted
    /// yet — cancel it and re-dispatch once.
    Restart,
    /// No progress past the deadline but this issue was already auto-restarted
    /// once — give up (bounded: never loop). Left for the operator.
    GiveUp,
}

/// Pure watchdog state machine (Issue #3887).
///
/// - Any observed progress ⇒ [`WatchdogDecision::Healthy`] (regardless of
///   elapsed time), so a slow-but-live sweep is never disturbed.
/// - Still inside the timeout window ⇒ `Healthy`.
/// - Past the timeout with no progress and not yet retried ⇒
///   [`WatchdogDecision::Restart`].
/// - Past the timeout with no progress and already retried ⇒
///   [`WatchdogDecision::GiveUp`] — the retry is bounded to exactly one.
#[must_use]
pub fn watchdog_decision(
    elapsed: Duration,
    timeout: Duration,
    made_progress: bool,
    already_retried: bool,
) -> WatchdogDecision {
    if made_progress || elapsed < timeout {
        WatchdogDecision::Healthy
    } else if already_retried {
        WatchdogDecision::GiveUp
    } else {
        WatchdogDecision::Restart
    }
}

/// Decide, from a sweep log's contents, whether the child has produced any
/// output *past* the daemon spawn header + `spawn-claude.sh` wrapper lines —
/// i.e. whether Claude Code itself has started doing work (Issue #3887).
///
/// A hung child's log region (after the most recent dispatch header) contains
/// only the header itself and `spawn-claude:`-prefixed wrapper lines (the
/// account/model/effort selection). Any other non-blank line means the child
/// got past local startup / MCP-init and is making progress. Anchoring to the
/// LAST dispatch header ensures a previous run's output in a reused per-issue
/// log is never counted as this dispatch's progress.
#[must_use]
pub fn log_has_progress(contents: &str) -> bool {
    let region = match contents.rfind(DISPATCH_HEADER_MARKER) {
        Some(i) => &contents[i..],
        None => contents,
    };
    region.lines().any(|line| {
        let t = line.trim();
        !t.is_empty() && !t.contains("loom-daemon dispatch:") && !t.contains("spawn-claude:")
    })
}

// ============================================================================
// Token-name capture (Issue #3802)
// ============================================================================

/// Extract the OAuth account name `spawn-claude.sh` logged, from the per-sweep
/// log `contents`, scanning only the region at/after this dispatch's header
/// (`header_anchor`, e.g. `sweep_id=<id>`). Returns `None` when the marker /
/// closing quote isn't present yet or the captured name is empty. Anchoring to
/// the current header avoids picking up a stale selection line left by a
/// previous dispatch in the same reused per-issue log.
fn parse_token_name_after(contents: &str, header_anchor: &str) -> Option<String> {
    // Use the LAST header occurrence: reruns append a fresh header, and the
    // current child logs its selection after the most recent one.
    let region_start = contents.rfind(header_anchor)?;
    let region = &contents[region_start..];
    let marker_at = region.find(TOKEN_NAME_LOG_MARKER)? + TOKEN_NAME_LOG_MARKER.len();
    let after = &region[marker_at..];
    let close = after.find('\'')?;
    let name = &after[..close];
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Poll `log_path` for the account-selection marker until it appears, the
/// child exits, or `TOKEN_NAME_CAPTURE_TIMEOUT` elapses.
///
/// Returns the captured account name, or `UNKNOWN_TOKEN_NAME` when no
/// selection was logged (the `LOOM_SPAWN_NO_EXPORT` bypass, a timeout, or a
/// child that exited before logging). Never blocks longer than the timeout and
/// never fails dispatch.
///
/// The `try_wait` early-exit keeps no-selection cases fast (a fixture that
/// exits without logging is detected immediately rather than waiting out the
/// full window). `try_wait` caches the exit status, so the reaper's later
/// `try_wait` on the same handle still observes the exit.
fn poll_token_name(child: &mut Child, log_path: &Path, header_anchor: &str) -> String {
    let deadline = std::time::Instant::now() + TOKEN_NAME_CAPTURE_TIMEOUT;
    loop {
        if let Ok(contents) = std::fs::read_to_string(log_path) {
            if let Some(name) = parse_token_name_after(&contents, header_anchor) {
                return name;
            }
        }
        // If the child has already exited without logging a selection, the
        // line will never appear — do a final read (covers a log flushed right
        // before exit) and stop, rather than waiting out the timeout.
        if matches!(child.try_wait(), Ok(Some(_))) {
            if let Ok(contents) = std::fs::read_to_string(log_path) {
                if let Some(name) = parse_token_name_after(&contents, header_anchor) {
                    return name;
                }
            }
            return UNKNOWN_TOKEN_NAME.to_string();
        }
        if std::time::Instant::now() >= deadline {
            return UNKNOWN_TOKEN_NAME.to_string();
        }
        std::thread::sleep(TOKEN_NAME_CAPTURE_POLL_INTERVAL);
    }
}

// ============================================================================
// Registry
// ============================================================================

/// Configuration for a `SweepRegistry`.
///
/// All paths are resolved relative to `workspace_root`. Tests should supply
/// a `tempdir` here.
#[derive(Debug, Clone)]
pub struct SweepRegistryConfig {
    /// Absolute path to the workspace root (parent of `.loom/`).
    pub workspace_root: PathBuf,
    /// Optional override for the spawn binary. Defaults to
    /// `<workspace_root>/defaults/scripts/spawn-claude.sh` or, if absent,
    /// `<workspace_root>/.loom/scripts/spawn-claude.sh`.
    pub spawn_bin: Option<PathBuf>,
    /// Override the `gh` binary (for tests). Defaults to `gh` from `PATH`.
    pub gh_bin: Option<PathBuf>,
    /// When `true`, skip the actual label flip via `gh`. Used by unit tests
    /// that don't have GitHub credentials.
    pub skip_label_flip: bool,
}

impl SweepRegistryConfig {
    /// Construct a config rooted at `workspace_root` with default lookups.
    #[must_use]
    pub fn new(workspace_root: PathBuf) -> Self {
        Self {
            workspace_root,
            spawn_bin: None,
            gh_bin: None,
            skip_label_flip: false,
        }
    }

    /// Resolve the spawn binary, preferring (in order):
    /// 1. `spawn_bin` explicit override.
    /// 2. `LOOM_SWEEP_SPAWN_BIN` env var.
    /// 3. `<workspace>/.loom/scripts/spawn-claude.sh`.
    /// 4. `<workspace>/defaults/scripts/spawn-claude.sh`.
    pub fn resolve_spawn_bin(&self) -> Result<PathBuf> {
        if let Some(ref p) = self.spawn_bin {
            return Ok(p.clone());
        }
        if let Ok(path) = std::env::var(SPAWN_BIN_ENV) {
            return Ok(PathBuf::from(path));
        }
        let installed = self
            .workspace_root
            .join(".loom")
            .join("scripts")
            .join("spawn-claude.sh");
        if installed.exists() {
            return Ok(installed);
        }
        let defaults = self
            .workspace_root
            .join("defaults")
            .join("scripts")
            .join("spawn-claude.sh");
        if defaults.exists() {
            return Ok(defaults);
        }
        Err(anyhow!(
            "spawn-claude.sh not found under {} (looked in .loom/scripts and defaults/scripts; \
             set {SPAWN_BIN_ENV} to override)",
            self.workspace_root.display()
        ))
    }

    /// Directory holding per-issue claim locks.
    #[must_use]
    pub fn locks_dir(&self) -> PathBuf {
        self.workspace_root.join(".loom").join("locks")
    }

    /// Directory holding per-sweep log files.
    #[must_use]
    pub fn logs_dir(&self) -> PathBuf {
        self.workspace_root.join(".loom").join("logs")
    }

    /// Directory holding sweep checkpoint files (#3373).
    #[must_use]
    pub fn checkpoint_dir(&self) -> PathBuf {
        self.workspace_root.join(".loom").join("sweep-checkpoint")
    }
}

/// On-disk owner metadata written inside the lock dir. Schema mirrors
/// `defaults/scripts/spawn-loop.sh:299-305`.
#[derive(Debug, Serialize, Deserialize)]
struct LockOwner {
    issue: u32,
    owner_pid: u32,
    acquired_at: String,
    sweep_id: SweepId,
}

/// In-memory registry of dispatched sweeps.
#[derive(Debug)]
pub struct SweepRegistry {
    config: SweepRegistryConfig,
    entries: BTreeMap<SweepId, SweepInfo>,
    /// Retained `Child` handles for sweeps this daemon instance spawned
    /// (Issue #3801). Keyed by `sweep_id`.
    ///
    /// The handle is kept — rather than dropped at spawn — so the reaper
    /// (and `cancel`) can `try_wait()` / `wait()` the child, which reaps
    /// the OS-level process (no `<defunct>` zombie under the daemon PID)
    /// AND yields the real exit status. `kill(pid, 0)` alone is proven
    /// insufficient: a terminated-but-unreaped child is a zombie whose PID
    /// is still allocated, so `kill(pid, 0)` reports it alive forever and
    /// the registry stays stuck `Running`.
    ///
    /// Reconstructed entries (from a prior daemon, see [`reconstruct`]) have
    /// no handle here — we never spawned them — so their liveness falls
    /// back to the `kill(pid, 0)` probe. Those entries are already admitted
    /// as terminal (`Crashed`) or point at the previous daemon's PID, so the
    /// fallback is correct for them.
    ///
    /// [`reconstruct`]: SweepRegistry::reconstruct
    children: BTreeMap<SweepId, Child>,
    /// Optional event bus for lifecycle events (Issue #3453, Phase B).
    /// When `None`, the registry behaves identically to Phase A — bus
    /// emission is best-effort and never blocks core dispatch/reaper
    /// progress.
    bus: Option<Arc<EventBus>>,
    /// Minimum wall-clock gap enforced between consecutive child spawns to
    /// avoid the simultaneous-startup MCP-init race (Issue #3887). Defaults to
    /// `Duration::ZERO` (no stagger — byte-for-byte the pre-#3887 behavior and
    /// zero added latency in tests); `main.rs` sets the resolved
    /// env > config > default value on the production registry.
    dispatch_stagger: Duration,
    /// Instant of the most recent child spawn, used with `dispatch_stagger` to
    /// compute the stagger wait (Issue #3887). `None` until the first spawn.
    last_spawn_at: Option<Instant>,
    /// Issues the watchdog has already auto-restarted once (Issue #3887). The
    /// re-dispatch is bounded to a single attempt per issue — a second hang
    /// resolves to [`WatchdogDecision::GiveUp`], never another restart.
    watchdog_retried: HashSet<u32>,
    /// Issues the watchdog has already logged a give-up for, so the loud
    /// give-up warning fires once per issue rather than every tick.
    watchdog_gaveup: HashSet<u32>,
}

impl SweepRegistry {
    /// Construct an empty registry without an event bus.
    ///
    /// Equivalent to Phase A's behavior. Use [`set_event_bus`](Self::set_event_bus)
    /// or [`with_event_bus`](Self::with_event_bus) to attach a bus.
    #[must_use]
    pub fn new(config: SweepRegistryConfig) -> Self {
        Self {
            config,
            entries: BTreeMap::new(),
            children: BTreeMap::new(),
            bus: None,
            dispatch_stagger: Duration::ZERO,
            last_spawn_at: None,
            watchdog_retried: HashSet::new(),
            watchdog_gaveup: HashSet::new(),
        }
    }

    /// Construct an empty registry with the given event bus pre-attached.
    #[must_use]
    pub fn with_event_bus(config: SweepRegistryConfig, bus: Arc<EventBus>) -> Self {
        Self {
            config,
            entries: BTreeMap::new(),
            children: BTreeMap::new(),
            bus: Some(bus),
            dispatch_stagger: Duration::ZERO,
            last_spawn_at: None,
            watchdog_retried: HashSet::new(),
            watchdog_gaveup: HashSet::new(),
        }
    }

    /// Attach (or replace) the event bus used for lifecycle emission.
    /// Additive setter — exposed so `main.rs` can construct the bus and
    /// the registry separately, then wire them together at startup.
    pub fn set_event_bus(&mut self, bus: Arc<EventBus>) {
        self.bus = Some(bus);
    }

    /// Set the minimum wall-clock gap enforced between consecutive child spawns
    /// (Issue #3887). `main.rs` calls this once at startup with the resolved
    /// env > config > default value. `Duration::ZERO` disables the stagger.
    pub fn set_dispatch_stagger(&mut self, stagger: Duration) {
        self.dispatch_stagger = stagger;
    }

    /// Read-only accessor for the configured dispatch stagger (Issue #3887).
    #[must_use]
    pub fn dispatch_stagger(&self) -> Duration {
        self.dispatch_stagger
    }

    /// Read-only accessor for the event bus, if any. Exposed so external
    /// callers (IPC handlers) can publish directly via the same bus the
    /// registry uses.
    #[must_use]
    pub fn event_bus(&self) -> Option<&Arc<EventBus>> {
        self.bus.as_ref()
    }

    /// Returns a shared, mutex-guarded registry suitable for tokio tasks.
    #[must_use]
    pub fn shared(config: SweepRegistryConfig) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self::new(config)))
    }

    /// Read-only view of the registry config.
    #[must_use]
    pub fn config(&self) -> &SweepRegistryConfig {
        &self.config
    }

    /// Test/inspection helper: number of tracked sweeps.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Test/inspection helper.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Look up a sweep by ID.
    #[must_use]
    pub fn get(&self, sweep_id: &str) -> Option<&SweepInfo> {
        self.entries.get(sweep_id)
    }

    /// Return all tracked sweeps matching the optional state filter.
    pub fn list(&self, filter: Option<&SweepState>) -> Vec<SweepInfo> {
        self.entries
            .values()
            .filter(|info| match filter {
                None => true,
                Some(target) => {
                    std::mem::discriminant(&info.state) == std::mem::discriminant(target)
                }
            })
            .cloned()
            .collect()
    }

    // ------------------------------------------------------------------------
    // Dispatch
    // ------------------------------------------------------------------------

    /// Dispatch a sweep. See module docs.
    ///
    /// On idempotency hit returns the existing entry with `was_new = false`.
    ///
    /// `model` (issue #3477): when `Some` and non-empty, the spawned child
    /// receives `--model <value>` appended to the `spawn-claude.sh` argv.
    /// When `None`, no `--model` flag is emitted at all — the session/CLI
    /// default is preserved end-to-end.
    ///
    /// `effort` (issue #3716): mirrors `model` exactly. When `Some` and
    /// non-empty, the spawned child receives `--effort <level>` appended to
    /// the argv (immediately after any `--model`). When `None` or empty, no
    /// `--effort` flag is emitted at all — the session default reasoning
    /// effort is preserved end-to-end.
    ///
    /// `depends_on` (issue #3729, stacked-PR v1): when `Some(N)`, the spawned
    /// child receives `--depends-on <N>` appended to the argv (immediately
    /// after any `--model` / `--effort`), instructing `/loom:sweep` to branch
    /// its worktree/PR off `feature/issue-<N>`. When `None`, no
    /// `--depends-on` flag is emitted — byte-for-byte unchanged behavior. A
    /// single optional parent (not a list) makes diamonds unrepresentable.
    pub fn dispatch(
        &mut self,
        kind: &SweepKind,
        idempotency_key: Option<String>,
        model: Option<&str>,
        effort: Option<&str>,
        depends_on: Option<u32>,
    ) -> Result<DispatchOutcome> {
        // 1. Idempotency dedup against Running entries.
        if let Some(ref key) = idempotency_key {
            if let Some(existing) = self.find_running_by_key(key) {
                return Ok(DispatchOutcome {
                    sweep_id: existing.sweep_id.clone(),
                    pid: existing.pid,
                    token_name: existing.token_name.clone(),
                    log_path: existing.log_path.clone(),
                    was_new: false,
                });
            }
        }

        // 2. Phase A only fully implements Issue dispatch.
        let issue_number = match kind {
            SweepKind::Issue(n) => *n,
            SweepKind::PrSet(_) => {
                return Err(anyhow!(
                    "PrSet dispatch is reserved for a future phase of #3449 \
                     (Phase A handles Issue dispatch only)"
                ));
            }
        };

        // 3. Acquire the claim lock atomically.
        let sweep_id = generate_sweep_id(kind);
        self.acquire_lock(issue_number, &sweep_id)?;

        // 4. Flip the forge label loom:issue -> loom:building (best-effort
        //    when the dispatcher has gh credentials; tests opt out via
        //    `skip_label_flip`).
        if !self.config.skip_label_flip {
            if let Err(e) = self.flip_label_to_building(issue_number) {
                log::warn!(
                    "label flip for issue #{issue_number} failed (continuing dispatch): {e}"
                );
            }
        }

        // 5. Compute the log path and spawn the child.
        //
        // Serialize concurrent child startups (Issue #3887): enforce a minimum
        // wall-clock gap since the previous spawn so a burst of back-to-back
        // dispatches does not launch many `claude`/`mcp-loom` startups in the
        // same ~1s window (the 0-HTTPS MCP-init race). `dispatch` holds the
        // registry mutex here, so the brief stagger sleep also serializes the
        // contended startup step across concurrent dispatch callers. A zero
        // stagger (the default outside production / in tests) is a no-op.
        self.apply_dispatch_stagger();
        let log_path = self.compute_log_path(issue_number);
        let (child, token_name) = self
            .spawn_child(issue_number, &log_path, &sweep_id, model, effort, depends_on)
            .context("failed to spawn sweep child")?;
        let pid = child.id();
        // Retain the handle so the reaper can `try_wait()` it (Issue #3801).
        self.children.insert(sweep_id.clone(), child);

        // Record the spawned child's PID in the lock (Issue #3808). The lock's
        // owner.json is written provisionally at `acquire_lock` time with the
        // daemon's own PID (the child does not exist yet), but the value that
        // matters for post-restart reconstruction is the *child's* PID: the
        // daemon PID is gone after any restart, so keeping it would make even a
        // still-live daemon-dispatched child look stale in `reconstruct()`'s
        // lock pass. Rewrite `owner_pid` now that the child exists.
        if let Err(e) = self.record_child_pid_in_lock(issue_number, pid) {
            log::warn!(
                "failed to record child pid {pid} in lock for issue #{issue_number} \
                 (reconstruct may treat it as stale after a daemon restart): {e}"
            );
        }

        // 6. Record the entry. The model is carried on the registry entry
        //    (#3482, Phase 3a observability) so `list_sweeps` /
        //    `get_sweep_status` can report which model a sweep runs. Empty
        //    strings are normalized to None, matching the spawn-side rule
        //    that `--model ""` is never emitted.
        let info = SweepInfo {
            sweep_id: sweep_id.clone(),
            kind: kind.clone(),
            pid,
            token_name: token_name.clone(),
            log_path: log_path.clone(),
            idempotency_key,
            started_at: Utc::now(),
            state: SweepState::Running,
            latest_phase: None,
            pr_number: None,
            model: model.filter(|m| !m.is_empty()).map(String::from),
            effort: effort.filter(|e| !e.is_empty()).map(String::from),
            depends_on,
        };
        self.entries.insert(sweep_id.clone(), info);

        // 7. Emit `sweep.global.dispatch` (best-effort — never block
        //    dispatch progress on the bus). If no subscribers are
        //    listening, the bus returns NoSubscribers; log at debug.
        self.emit_event(Event::SweepGlobalDispatch {
            sweep_id: sweep_id.clone(),
            kind: kind.clone(),
        });

        Ok(DispatchOutcome {
            sweep_id,
            pid,
            token_name,
            log_path,
            was_new: true,
        })
    }

    /// Internal helper: publish an event on the attached bus (if any).
    /// Best-effort — logs a debug line if no subscribers are listening.
    fn emit_event(&self, event: Event) {
        if let Some(ref bus) = self.bus {
            let topic = event.topic();
            match bus.publish(event) {
                Ok(n) => log::debug!("event_bus: published {topic} to {n} subscriber(s)"),
                Err(_) => {
                    log::debug!("event_bus: published {topic} (no subscribers)");
                }
            }
        }
    }

    fn find_running_by_key(&self, key: &str) -> Option<&SweepInfo> {
        self.entries.values().find(|info| {
            matches!(info.state, SweepState::Running | SweepState::Pending)
                && info.idempotency_key.as_deref() == Some(key)
        })
    }

    // ------------------------------------------------------------------------
    // Lock primitive (mirrors spawn-loop.sh:293-309)
    // ------------------------------------------------------------------------

    fn acquire_lock(&self, issue: u32, sweep_id: &str) -> Result<()> {
        let locks_dir = self.config.locks_dir();
        std::fs::create_dir_all(&locks_dir)
            .with_context(|| format!("failed to create locks dir {}", locks_dir.display()))?;
        let lock = locks_dir.join(format!("issue-{issue}"));

        // `mkdir` is POSIX-atomic — see spawn-loop.sh:286-292 for rationale.
        match std::fs::create_dir(&lock) {
            Ok(()) => {
                let owner = LockOwner {
                    issue,
                    // Provisional: the child does not exist yet. `dispatch`
                    // rewrites this with the spawned child's PID via
                    // `record_child_pid_in_lock` once the child is running
                    // (Issue #3808), so `reconstruct()` can recognise a live
                    // daemon sweep after a restart.
                    owner_pid: std::process::id(),
                    acquired_at: Utc::now().to_rfc3339(),
                    sweep_id: sweep_id.to_string(),
                };
                let owner_json =
                    serde_json::to_string_pretty(&owner).context("serialize lock owner")?;
                std::fs::write(lock.join("owner.json"), owner_json)
                    .context("write lock owner.json")?;
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(anyhow!(
                "lock collision: issue #{issue} is already claimed (lock at {})",
                lock.display()
            )),
            Err(e) => {
                Err(anyhow!("failed to acquire lock for issue #{issue} at {}: {e}", lock.display()))
            }
        }
    }

    /// Rewrite the lock's `owner.json` so `owner_pid` records the spawned
    /// sweep child's PID rather than the daemon's own PID (Issue #3808).
    ///
    /// `acquire_lock` runs *before* the child is spawned, so it can only
    /// stamp `std::process::id()` (the daemon) provisionally. After a real
    /// daemon restart that PID is gone by definition, which previously made
    /// `reconstruct()`'s lock pass treat every daemon-dispatched sweep as
    /// stale — dropping the lock and (before #3808) synthesizing a spurious
    /// `Crashed` entry even for a child that was still alive. Storing the
    /// child PID lets the lock pass admit a genuinely-live child as `Running`
    /// across a restart. The rest of the owner record is preserved.
    fn record_child_pid_in_lock(&self, issue: u32, child_pid: u32) -> Result<()> {
        let owner_path = self
            .config
            .locks_dir()
            .join(format!("issue-{issue}"))
            .join("owner.json");
        let existing = std::fs::read_to_string(&owner_path)
            .with_context(|| format!("read lock owner {}", owner_path.display()))?;
        let mut owner: LockOwner =
            serde_json::from_str(&existing).context("parse lock owner.json")?;
        owner.owner_pid = child_pid;
        let owner_json = serde_json::to_string_pretty(&owner).context("serialize lock owner")?;
        std::fs::write(&owner_path, owner_json)
            .with_context(|| format!("write lock owner {}", owner_path.display()))?;
        Ok(())
    }

    /// Release the lock dir for an issue (idempotent).
    pub fn release_lock(&self, issue: u32) -> Result<()> {
        let lock = self.config.locks_dir().join(format!("issue-{issue}"));
        if lock.exists() {
            std::fs::remove_dir_all(&lock)
                .with_context(|| format!("failed to remove lock dir {}", lock.display()))?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------------
    // Forge label flip
    // ------------------------------------------------------------------------

    fn flip_label_to_building(&self, issue: u32) -> Result<()> {
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        let mut cmd = Command::new(&gh);
        cmd.arg("issue")
            .arg("edit")
            .arg(issue.to_string())
            .arg("--remove-label")
            .arg("loom:issue")
            .arg("--add-label")
            .arg("loom:building");
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        cmd.stdout(Stdio::null()).stderr(Stdio::piped());
        let output = cmd
            .output()
            .with_context(|| format!("failed to invoke {} for issue #{issue}", gh.display()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("gh issue edit failed for #{issue}: {}", stderr.trim()));
        }
        Ok(())
    }

    fn restore_label_to_ready(&self, issue: u32) -> Result<()> {
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        let mut cmd = Command::new(&gh);
        cmd.arg("issue")
            .arg("edit")
            .arg(issue.to_string())
            .arg("--remove-label")
            .arg("loom:building")
            .arg("--add-label")
            .arg("loom:issue");
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        cmd.stdout(Stdio::null()).stderr(Stdio::piped());
        let _ = cmd.output()?; // best-effort during reap
        Ok(())
    }

    // ------------------------------------------------------------------------
    // Stacked-PR block-the-subtree (issue #3729, v1 item 4)
    // ------------------------------------------------------------------------

    /// Return the issue numbers of every still-live (`Running`/`Pending`)
    /// sweep whose `depends_on` names `parent`. Terminal children are
    /// excluded — they no longer need blocking. Because `depends_on` is a
    /// single optional parent, this only ever returns the *direct* children
    /// of `parent` (a linear chain hop, never a diamond).
    #[must_use]
    pub fn children_of(&self, parent: u32) -> Vec<u32> {
        self.entries
            .values()
            .filter(|info| {
                matches!(info.state, SweepState::Running | SweepState::Pending)
                    && info.depends_on == Some(parent)
            })
            .filter_map(|info| match &info.kind {
                SweepKind::Issue(n) => Some(*n),
                SweepKind::PrSet(_) => None,
            })
            .collect()
    }

    /// Block the subtree stacked on `parent` (issue #3729, v1 item 4).
    ///
    /// For each direct child of `parent` (see [`Self::children_of`]), emit a
    /// `sweep.issue.{child}.blocker` event on the existing frozen event-bus
    /// topic (#3453 — no new topic). This is the safety net that keeps a
    /// stacked child from auto-progressing (opening/merging its PR) when its
    /// parent ends in `loom:blocked`. Auto-detach (rebasing an orphaned child
    /// onto `main`) is explicitly out of v1 scope — block-the-subtree is the
    /// only cascade behavior.
    ///
    /// Returns the child issue numbers that were signalled. Emission is
    /// best-effort (no subscribers ⇒ debug log only), mirroring the rest of
    /// the reaper's event handling.
    pub fn block_children_of(&self, parent: u32, reason: &str) -> Vec<u32> {
        let children = self.children_of(parent);
        for child in &children {
            self.emit_event(Event::SweepBlocker {
                issue: *child,
                reason: reason.to_string(),
                label_added: "loom:blocked".to_string(),
            });
        }
        children
    }

    /// Best-effort check of whether `issue` currently carries the
    /// `loom:blocked` label on the forge. Used by the reaper to decide
    /// whether a terminated parent ended blocked (in which case its stacked
    /// children must be blocked too) versus completing successfully.
    ///
    /// Returns `false` on any error, when label flips are skipped (test
    /// fixtures), or when `gh` is unavailable — a conservative default that
    /// never blocks a child on an unverifiable parent state.
    fn issue_has_blocked_label(&self, issue: u32) -> bool {
        if self.config.skip_label_flip {
            return false;
        }
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        let mut cmd = Command::new(&gh);
        cmd.arg("issue")
            .arg("view")
            .arg(issue.to_string())
            .arg("--json")
            .arg("labels")
            .arg("--jq")
            .arg(r#"[.labels[].name] | index("loom:blocked") != null"#);
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        cmd.stderr(Stdio::null());
        match cmd.output() {
            Ok(out) if out.status.success() => {
                String::from_utf8_lossy(&out.stdout).trim() == "true"
            }
            _ => false,
        }
    }

    // ------------------------------------------------------------------------
    // Spawn
    // ------------------------------------------------------------------------

    fn compute_log_path(&self, issue: u32) -> PathBuf {
        self.config
            .logs_dir()
            .join(format!("sweep-issue-{issue}.log"))
    }

    /// Enforce the configured dispatch stagger (Issue #3887): if less than
    /// `dispatch_stagger` has elapsed since the previous spawn, sleep the
    /// remainder, then record now as the latest spawn instant. A zero stagger
    /// is a no-op. Called under the registry mutex from `dispatch`, so it also
    /// serializes concurrent dispatch callers past the contended startup step.
    fn apply_dispatch_stagger(&mut self) {
        let wait = stagger_wait(self.last_spawn_at, self.dispatch_stagger, Instant::now());
        if !wait.is_zero() {
            log::debug!(
                "sweep_registry: staggering spawn by {}ms to avoid startup race (#3887)",
                wait.as_millis()
            );
            std::thread::sleep(wait);
        }
        self.last_spawn_at = Some(Instant::now());
    }

    fn spawn_child(
        &self,
        issue: u32,
        log_path: &Path,
        sweep_id: &str,
        model: Option<&str>,
        effort: Option<&str>,
        depends_on: Option<u32>,
    ) -> Result<(Child, String)> {
        let spawn_bin = self.config.resolve_spawn_bin()?;

        // Ensure log dir exists.
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create log dir {}", parent.display()))?;
        }

        // Append a header so reruns are distinguishable. Mirrors
        // spawn-loop.sh:377-380.
        {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_path)
            {
                let _ = writeln!(
                    f,
                    "\n==== loom-daemon dispatch: {} sweep_id={sweep_id} issue={issue} ====",
                    Utc::now().to_rfc3339()
                );
            }
        }

        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .with_context(|| format!("failed to open log {}", log_path.display()))?;
        let log_clone = log_file.try_clone()?;

        let prompt = format!("/loom:sweep {issue}");
        let mut cmd = Command::new(&spawn_bin);
        cmd.arg("-p").arg(&prompt);
        // Model selection (issue #3477, Phase 1): the dispatch-param tier of
        // the precedence chain. Appended as an explicit `--model` arg (which
        // beats any ambient LOOM_MODEL env inside spawn-claude.sh). Empty
        // strings are treated as unset — `--model ""` must never be emitted.
        if let Some(m) = model {
            if !m.is_empty() {
                cmd.arg("--model").arg(m);
            }
        }
        // Reasoning-effort selection (issue #3716): the dispatch-param tier,
        // mirroring `--model` exactly. Appended as an explicit `--effort` arg
        // (which beats any ambient LOOM_EFFORT env inside spawn-claude.sh).
        // Empty strings are treated as unset — `--effort ""` must never be
        // emitted, so the session-default effort is preserved end-to-end.
        if let Some(e) = effort {
            if !e.is_empty() {
                cmd.arg("--effort").arg(e);
            }
        }
        // Stacked-PR dependency (issue #3729, v1): when a parent issue is
        // declared, append `--depends-on <N>` so `/loom:sweep` branches the
        // child worktree/PR off `feature/issue-<N>` instead of the default
        // branch. Absent the param, no flag is emitted (byte-for-byte
        // unchanged). Mirrors the `--model` / `--effort` append-only contract.
        if let Some(parent) = depends_on {
            cmd.arg("--depends-on").arg(parent.to_string());
        }
        // Unattended-permissions flag (issue #3824): a daemon-dispatched child
        // is a detached, non-interactive `claude -p` process — there is no
        // human to answer a permission prompt, so any tool call needing
        // approval (`.loom/` writes, `sweep-run-registry.sh`, the
        // `mcp__loom__list_sweeps` daemon probe) auto-denies and stalls the
        // build. Append `--dangerously-skip-permissions` so the child runs
        // non-interactively with hooks still firing — mirroring the established
        // unattended cron pattern (`.github/workflows/loom-*.yml`, which spawn
        // `claude -p "/<role>" --dangerously-skip-permissions`). Scoped to this
        // daemon-only dispatch path; `spawn-claude.sh` stays a generic
        // pass-through and never adds a permission flag of its own. Appended
        // AFTER `--model`/`--effort`/`--depends-on` so the positional argv
        // contract for those flags is unchanged.
        cmd.arg("--dangerously-skip-permissions");
        cmd.env("LOOM_TERMINAL_ID", format!("daemon-{sweep_id}"))
            // Claim-ownership marker (issue #3823): `dispatch()` flips
            // loom:issue -> loom:building on the forge BEFORE this child is
            // spawned (step 4, for immediate external visibility of the claim).
            // Without a signal, the child's own `/loom:sweep` pre-flight would
            // read that label and skip issue N as "already being built by
            // someone else" — self-skipping the daemon's OWN claim, so no
            // worktree, no build, no PR. Export the issue number this sweep
            // owns so the child's pre-flight recognises an existing
            // loom:building as ITS OWN daemon claim and proceeds to build.
            // Scoped to daemon-dispatched children only: an operator-run
            // `/loom:sweep N` never sets this env var, so the manual-terminal
            // skip rule (honor any loom:building) is unchanged.
            .env("LOOM_SWEEP_CLAIM_OWNED", issue.to_string())
            // Always pin LOOM_WORKSPACE to the registry's configured root so
            // spawn-claude.sh resolves `.loom/tokens/` from the same place
            // the daemon thinks the workspace is — never inheriting an
            // ambient value that might point elsewhere.
            .env(WORKSPACE_ENV, &self.config.workspace_root)
            // Issue #3730: pin the child's cwd to the resolved workspace root
            // so the child's relative `.loom/config.json` read
            // (loom_tools/sweep_experiment.py) and archive-transcripts.sh's
            // cwd-slug resolve deterministically, rather than depending on the
            // daemon's own cwd happening to be the workspace root.
            .current_dir(&self.config.workspace_root)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(log_clone));

        // Issue #3800: put the sweep child in its OWN process group
        // (`setpgid(0, 0)` runs post-fork/pre-exec via `process_group(0)`,
        // stable since Rust 1.64). spawn-claude.sh ends in `exec claude`, so
        // the tracked PID becomes the `claude` process itself AND the leader
        // of a fresh group. `claude` forks real OS subprocesses for tool
        // execution (Bash-tool commands, MCP servers, git clones, …); those
        // descendants inherit this group. Making the child a group leader lets
        // `cancel()` signal the WHOLE group (`kill(-pgid, sig)`) so the entire
        // sweep subtree is torn down — instead of leaving orphans behind when
        // only the top-level PID is signalled.
        #[cfg(unix)]
        cmd.process_group(0);

        // Issue #3730: explicitly forward the experiment-related env vars to
        // the detached child via an EXPLICIT ALLOWLIST — never a blanket
        // env_clear/copy. Without this, `LOOM_MODEL_EXPERIMENT` /
        // `LOOM_MODEL_EXPERIMENT_CANARY` / `LOOM_TRANSCRIPT_ARCHIVE` only reach
        // the child if the daemon *itself* was launched with them; an operator
        // exporting them before dispatching would get a silent no-effect.
        //
        // `var_os` guards each name: an UNSET var is not forwarded, and an
        // empty-string value is not forwarded either (no empty-string
        // forwarding — mirrors the archiver / experiment-parser treatment of
        // empty as "unset"). This keeps the spawn a byte-for-byte no-op when
        // none of the vars are set.
        for name in EXPERIMENT_ENV_ALLOWLIST {
            if let Some(val) = std::env::var_os(name) {
                if !val.is_empty() {
                    cmd.env(name, val);
                }
            }
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn {} -p '{}'", spawn_bin.display(), prompt))?;
        // Issue #3801: we RETAIN the `Child` handle (returned to `dispatch`,
        // which stores it in `self.children`) instead of dropping it. The
        // reaper `try_wait()`s it each tick so an exited child is reaped
        // (no `<defunct>` zombie) and the registry transitions to a terminal
        // state with the real exit status.
        //
        // Issue #3802: capture which OAuth account `spawn-claude.sh` selected
        // for this sweep so `list_sweeps` / `get_sweep_status` can report it
        // (an observability gap for a multi-account pool otherwise). The
        // wrapper's selection is logged (not exposed on stdout), and the
        // child's stderr is already captured into the per-sweep log above, so
        // we poll that log for the `using OAuth account '<name>'` marker. The
        // scan is anchored to THIS dispatch's header line (`sweep_id=<id>`,
        // written above) so a stale line from a previous dispatch appended to
        // the same per-issue log is never mistaken for the current selection.
        // Falls back to `UNKNOWN_TOKEN_NAME` on timeout / no-selection — never
        // blocks or fails dispatch.
        let header_anchor = format!("sweep_id={sweep_id}");
        let token_name = poll_token_name(&mut child, log_path, &header_anchor);
        Ok((child, token_name))
    }

    // ------------------------------------------------------------------------
    // Cancellation + status accessors (Issue #3455, Phase C)
    // ------------------------------------------------------------------------

    /// Return the `SweepInfo` for the given sweep ID, cloned (so callers
    /// can release the registry lock immediately). Phase C exposes this
    /// as the `get_sweep_status` MCP tool.
    #[must_use]
    pub fn get_status(&self, sweep_id: &str) -> Option<SweepInfo> {
        self.entries.get(sweep_id).cloned()
    }

    /// Signal a sweep's process. When this daemon still owns a retained
    /// `Child` handle for `sweep_id` (i.e. we spawned it into its own process
    /// group via `process_group(0)`), the signal is delivered to the WHOLE
    /// process group (`kill(-pgid, sig)`, and the leader's pgid == its pid)
    /// so the entire `claude` subprocess subtree is reached (Issue #3800).
    /// Reconstructed entries with no handle fall back to single-PID delivery.
    fn signal_sweep(&self, sweep_id: &str, pid: u32, sig: i32) -> bool {
        if self.children.contains_key(sweep_id) {
            send_group_signal(pid, sig)
        } else {
            send_signal(pid, sig)
        }
    }

    /// Determine whether a sweep's child has terminated, reaping it when it
    /// has. Prefers the retained `Child` handle: `try_wait()` reaps an exited
    /// child (no zombie) and yields the real exit status. Falls back to the
    /// `kill(pid, 0)` liveness probe for reconstructed entries with no handle.
    ///
    /// Returns `(is_dead, exit_code)`. On a handle-observed exit the handle is
    /// removed from `self.children`; `exit_code` is `None` when the child was
    /// terminated by a signal (no clean code) or when liveness came from the
    /// fallback probe.
    fn poll_liveness(&mut self, sweep_id: &str, pid: u32) -> (bool, Option<i32>) {
        if let Some(child) = self.children.get_mut(sweep_id) {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let code = status.code();
                    self.children.remove(sweep_id);
                    (true, code)
                }
                Ok(None) => (false, None),
                Err(e) => {
                    log::warn!("sweep_registry: try_wait for {sweep_id} (pid {pid}) failed: {e}");
                    let dead = !is_pid_alive(pid);
                    if dead {
                        self.children.remove(sweep_id);
                    }
                    (dead, None)
                }
            }
        } else {
            (!is_pid_alive(pid), None)
        }
    }

    /// Reap the retained `Child` handle for `sweep_id`, blocking briefly until
    /// it exits. Called after `cancel` has SIGKILL'd (or observed the exit of)
    /// the child so the OS-level zombie is reclaimed under the daemon PID.
    /// No-op when no handle is retained (reconstructed / test-injected entry).
    fn reap_handle(&mut self, sweep_id: &str) -> Option<std::process::ExitStatus> {
        self.children.remove(sweep_id).and_then(|mut child| {
            // Bounded: we only reach here once the child has exited or has
            // just been SIGKILL'd, so `wait()` returns promptly.
            child.wait().ok()
        })
    }

    /// Cancel a running sweep.
    ///
    /// Sends SIGTERM to the sweep's process group, waits up to `grace` for the
    /// child to exit, then SIGKILL to the group if still alive. On any path the
    /// registry entry is transitioned to `Exited{code: None, at: now}`
    /// and the per-issue lock is released. Emits the same lifecycle
    /// events the reaper would emit on a clean exit
    /// (`sweep.issue.{N}.exited` + `sweep.global.completed`).
    ///
    /// Returns [`CancelOutcome`] describing what actually happened. Calls
    /// against unknown sweep IDs return `Err`. Calls against already-
    /// terminal sweeps return `Ok` with `was_running = false` — cancel
    /// is idempotent so monitor-tool retries don't surface as errors.
    ///
    /// This is the **synchronous, self-contained** composition of the
    /// [`begin_cancel`](Self::begin_cancel) → [`poll_cancel`](Self::poll_cancel)
    /// → [`finish_cancel`](Self::finish_cancel) split. It holds `&mut self`
    /// (and therefore, when the registry lives behind a `Mutex`, the lock)
    /// for the entire grace window, so callers that must not freeze other
    /// registry access during the poll should orchestrate the three steps
    /// themselves and release the lock across the sleep (see the non-blocking
    /// IPC handler for `CancelSweep`, Issue #3807). Kept for direct callers
    /// and unit tests where lock contention is irrelevant.
    pub fn cancel(&mut self, sweep_id: &str, grace: Duration) -> Result<CancelOutcome> {
        let (pid, kind, started_at) = match self.begin_cancel(sweep_id)? {
            BeginCancel::AlreadyTerminal(outcome) => return Ok(outcome),
            BeginCancel::Signalled {
                pid,
                kind,
                started_at,
            } => (pid, kind, started_at),
        };

        // Poll for exit up to the grace window (100ms cadence, matching the
        // spawn-loop's shutdown-grace polling). Blocking sleep is fine here —
        // this path holds `&mut self` throughout by design.
        let poll_interval = Duration::from_millis(100);
        let deadline = std::time::Instant::now() + grace;
        let mut exited_within_grace = self.poll_cancel(sweep_id, pid);
        while !exited_within_grace && std::time::Instant::now() < deadline {
            std::thread::sleep(poll_interval);
            exited_within_grace = self.poll_cancel(sweep_id, pid);
        }

        Ok(self.finish_cancel(sweep_id, pid, &kind, started_at, exited_within_grace))
    }

    /// First, lock-scoped step of a split cancel (Issue #3807): read the
    /// target's pid/kind/liveness and, when it is still running, deliver
    /// SIGTERM to its process GROUP (Issue #3800). Returns quickly — it does
    /// **no** blocking poll — so the caller can release the registry lock
    /// before entering the (potentially multi-second) grace window.
    ///
    /// SIGTERM (signal 15) is sent to the whole process group via `kill(2)`
    /// directly rather than spawning `kill(1)` so the path is identical on
    /// macOS + Linux and doesn't depend on `PATH`. `signal_sweep` falls back
    /// to single-PID delivery for entries with no retained handle.
    ///
    /// - Unknown sweep IDs return `Err`.
    /// - Already-terminal sweeps return [`BeginCancel::AlreadyTerminal`] with
    ///   an idempotent `was_running = false` outcome (no signal, no state
    ///   change) — cancel-from-monitor retries stay idempotent.
    pub fn begin_cancel(&mut self, sweep_id: &str) -> Result<BeginCancel> {
        let (pid, kind, was_running, started_at) = {
            let info = self
                .entries
                .get(sweep_id)
                .ok_or_else(|| anyhow!("unknown sweep_id: {sweep_id}"))?;
            let alive = matches!(info.state, SweepState::Running | SweepState::Pending);
            (info.pid, info.kind.clone(), alive, info.started_at)
        };

        if !was_running {
            return Ok(BeginCancel::AlreadyTerminal(CancelOutcome {
                sweep_id: sweep_id.to_string(),
                pid,
                sigkill_sent: false,
                was_running: false,
            }));
        }

        let term_sent = self.signal_sweep(sweep_id, pid, 15);
        if !term_sent {
            log::warn!(
                "cancel_sweep: SIGTERM to pid {pid} for sweep {sweep_id} failed \
                 (process may already be dead)"
            );
        }

        Ok(BeginCancel::Signalled {
            pid,
            kind,
            started_at,
        })
    }

    /// One lock-scoped liveness poll for an in-progress cancel (Issue #3807).
    /// Returns `true` once the child has exited, reaping it via the retained
    /// `Child` handle so no `<defunct>` zombie survives (Issue #3801). The
    /// caller invokes this under a brief lock between *unlocked* sleep
    /// intervals, so the grace window never holds the registry mutex.
    pub fn poll_cancel(&mut self, sweep_id: &str, pid: u32) -> bool {
        self.poll_liveness(sweep_id, pid).0
    }

    /// Final, lock-scoped step of a split cancel (Issue #3807): SIGKILL the
    /// process group if the child did not exit within grace, reap the retained
    /// handle (Issue #3801), transition the entry to `Exited{code: None}`,
    /// release the per-issue lock, and emit the same lifecycle events a clean
    /// exit would (`sweep.issue.{N}.exited` + `sweep.global.completed`).
    ///
    /// `exited_within_grace` is the terminal result of the caller's poll loop.
    /// Returns the [`CancelOutcome`] for the (running) sweep.
    pub fn finish_cancel(
        &mut self,
        sweep_id: &str,
        pid: u32,
        kind: &SweepKind,
        started_at: DateTime<Utc>,
        exited_within_grace: bool,
    ) -> CancelOutcome {
        // SIGKILL the group if still alive.
        let sigkill_sent = if exited_within_grace {
            false
        } else {
            let killed = self.signal_sweep(sweep_id, pid, 9);
            if !killed {
                log::warn!("cancel_sweep: SIGKILL to pid {pid} also failed");
            }
            true
        };

        // Reap the retained handle so the killed leader does not linger as a
        // `<defunct>` zombie under the daemon PID (Issue #3801). A no-op when
        // the exit was already reaped in the poll loop above, or when no
        // handle is retained (reconstructed / test-injected entry).
        let _ = self.reap_handle(sweep_id);

        // Read `pr_number` BEFORE mutating terminal state so the
        // orphaned-claim gate below sees the pre-cancel value (the state
        // mutation doesn't touch `pr_number`, but reading first keeps the
        // borrow sequencing clean and the intent explicit).
        let produced_pr = self
            .entries
            .get(sweep_id)
            .and_then(|info| info.pr_number)
            .is_some();

        // Transition state, release lock, emit events.
        let now = Utc::now();
        let duration_sec = (now - started_at).num_seconds();
        if let Some(info) = self.entries.get_mut(sweep_id) {
            info.state = SweepState::Exited {
                code: None,
                at: now,
            };
        }
        if let SweepKind::Issue(issue) = kind {
            let _ = self.release_lock(*issue);
            // Orphaned-claim recovery on cancel (issue #3827): a cancelled
            // daemon-owned Issue sweep that never opened a PR still holds its
            // pre-dispatch loom:building claim (set at `dispatch()` step 4).
            // Unlike `reap_once()`'s clean-exit branch (#3823b), `finish_cancel`
            // historically never restored the label, so cancelling stranded the
            // issue in loom:building. Restore loom:building -> loom:issue so the
            // issue is automatically recoverable — but only when this sweep
            // produced no PR, so we never yank the label out from under an
            // in-flight PR's issue. Gated on `!skip_label_flip`, mirroring the
            // reaper path. `SweepKind::PrSet` cancels never reach here.
            if !self.config.skip_label_flip && !produced_pr {
                let _ = self.restore_label_to_ready(*issue);
            }
            self.emit_event(Event::SweepExited {
                issue: *issue,
                exit_code: None,
                duration_sec,
            });
        }
        self.emit_event(Event::SweepGlobalCompleted {
            sweep_id: sweep_id.to_string(),
            outcome: SweepOutcome::Exited,
        });

        CancelOutcome {
            sweep_id: sweep_id.to_string(),
            pid,
            sigkill_sent,
            was_running: true,
        }
    }

    /// Read the last `lines` lines from a sweep's log file.
    ///
    /// Resolves the log path from the registry entry (so callers don't
    /// have to know the workspace-relative naming convention). Returns
    /// the absolute log path alongside the tail so the MCP layer can
    /// surface it.
    pub fn tail_log(&self, sweep_id: &str, lines: usize) -> Result<(PathBuf, Vec<String>)> {
        let info = self
            .entries
            .get(sweep_id)
            .ok_or_else(|| anyhow!("unknown sweep_id: {sweep_id}"))?;
        let log_path = info.log_path.clone();
        let tail = tail_lines(&log_path, lines)
            .with_context(|| format!("failed to tail {}", log_path.display()))?;
        Ok((log_path, tail))
    }

    // ------------------------------------------------------------------------
    // Reaper
    // ------------------------------------------------------------------------

    /// Run one reaper tick. Updates entry state for dead PIDs, releases
    /// locks, restores labels on crashed sweeps (if a checkpoint exists),
    /// and GCs entries older than the retention window.
    ///
    /// Returns the number of entries whose state changed.
    ///
    /// Emits the following events when an attached event bus is present
    /// (Issue #3453, Phase B):
    ///
    /// - `sweep.issue.{N}.exited` on a clean-exit transition.
    /// - `sweep.issue.{N}.crashed` on a checkpoint-present transition
    ///   (which also re-arms the `loom:issue` label).
    /// - `sweep.global.completed` on every terminal transition, regardless
    ///   of which per-issue event also fired.
    #[allow(clippy::too_many_lines)]
    pub fn reap_once(&mut self) -> usize {
        let mut changes = 0usize;

        // Snapshot keys + pids first so we can borrow mutably below.
        // Capture started_at so we can compute durations for Exited events.
        let candidates: Vec<(SweepId, u32, SweepState, SweepKind, chrono::DateTime<Utc>)> = self
            .entries
            .iter()
            .map(|(id, info)| {
                (id.clone(), info.pid, info.state.clone(), info.kind.clone(), info.started_at)
            })
            .collect();

        // Buffer events to emit after we've finished mutating the
        // registry — so we never call into the bus while holding the
        // registry mutex's lifetime budget unnecessarily.
        let mut events_to_emit: Vec<Event> = Vec::new();

        for (sweep_id, pid, state, kind, started_at) in candidates {
            if !matches!(state, SweepState::Running | SweepState::Pending) {
                continue;
            }
            // Liveness via the retained `Child` handle when we own it: this
            // `try_wait()`s the child, reaping any zombie (Issue #3801) and
            // yielding the real exit code. Reconstructed entries with no
            // handle fall back to the `kill(pid, 0)` probe.
            let (is_dead, exit_code) = self.poll_liveness(&sweep_id, pid);
            if is_dead {
                {
                    changes += 1;
                    let issue = match &kind {
                        SweepKind::Issue(n) => Some(*n),
                        SweepKind::PrSet(_) => None,
                    };
                    let now = Utc::now();
                    let duration_sec = (now - started_at).num_seconds();
                    // Release lock and decide between Exited vs Crashed.
                    if let Some(issue) = issue {
                        let _ = self.release_lock(issue);
                        let checkpoint = self
                            .config
                            .checkpoint_dir()
                            .join(format!("issue-{issue}.json"));
                        if checkpoint.exists() {
                            if !self.config.skip_label_flip {
                                let _ = self.restore_label_to_ready(issue);
                            }
                            let checkpoint_phase = read_checkpoint_phase(&checkpoint);
                            if let Some(info) = self.entries.get_mut(&sweep_id) {
                                info.state = SweepState::Crashed { at: now };
                                if info.latest_phase.is_none() {
                                    info.latest_phase.clone_from(&checkpoint_phase);
                                }
                            }
                            events_to_emit.push(Event::SweepCrashed {
                                issue,
                                checkpoint_phase,
                            });
                            events_to_emit.push(Event::SweepGlobalCompleted {
                                sweep_id: sweep_id.clone(),
                                outcome: SweepOutcome::Crashed,
                            });
                        } else {
                            // Orphaned-claim recovery (issue #3823b): a
                            // daemon-owned sweep that exits cleanly WITHOUT a
                            // checkpoint never reached the Builder phase — the
                            // canonical case is a self-skip / no-work exit. Its
                            // pre-dispatch loom:building claim (set at
                            // `dispatch()` step 4) would otherwise stay orphaned
                            // on the forge forever, because the Crashed branch
                            // above is the ONLY place the reaper restored the
                            // label and it fires only when a checkpoint exists.
                            // Restore loom:building -> loom:issue so the issue
                            // is automatically recoverable (no manual
                            // `restore_label_to_ready` reclaim) — but only when
                            // this sweep produced no PR, so we never yank the
                            // label out from under an in-flight PR's issue
                            // should `pr_number` ever be recorded on the entry.
                            let produced_pr = self
                                .entries
                                .get(&sweep_id)
                                .and_then(|info| info.pr_number)
                                .is_some();
                            if !self.config.skip_label_flip && !produced_pr {
                                let _ = self.restore_label_to_ready(issue);
                            }
                            if let Some(info) = self.entries.get_mut(&sweep_id) {
                                info.state = SweepState::Exited {
                                    code: exit_code,
                                    at: now,
                                };
                            }
                            events_to_emit.push(Event::SweepExited {
                                issue,
                                exit_code,
                                duration_sec,
                            });
                            events_to_emit.push(Event::SweepGlobalCompleted {
                                sweep_id: sweep_id.clone(),
                                outcome: SweepOutcome::Exited,
                            });
                        }
                        // Block-the-subtree (issue #3729, v1 item 4): if this
                        // parent ended in `loom:blocked` and stacked children
                        // still depend on it, signal each child's blocker on
                        // the existing frozen topic so it does not
                        // auto-progress. Cheap-guarded: we only consult the
                        // forge label when direct children actually exist.
                        let children = self.children_of(issue);
                        if !children.is_empty() && self.issue_has_blocked_label(issue) {
                            let reason = format!(
                                "parent sweep #{issue} ended in loom:blocked; \
                                 stacked child cannot auto-progress (block-the-subtree, #3729)"
                            );
                            for child in children {
                                events_to_emit.push(Event::SweepBlocker {
                                    issue: child,
                                    reason: reason.clone(),
                                    label_added: "loom:blocked".to_string(),
                                });
                            }
                        }
                    } else {
                        if let Some(info) = self.entries.get_mut(&sweep_id) {
                            info.state = SweepState::Exited {
                                code: exit_code,
                                at: now,
                            };
                        }
                        // PrSet sweeps don't have a single issue id, so we
                        // only emit the global event. Per-issue events are
                        // intentionally not emitted for PrSet (out of scope
                        // for Phase A — see sweep_registry::dispatch).
                        events_to_emit.push(Event::SweepGlobalCompleted {
                            sweep_id: sweep_id.clone(),
                            outcome: SweepOutcome::Exited,
                        });
                    }
                }
            }
        }

        // Drain the buffered events onto the bus. Each emission is
        // best-effort and never propagates an error back into reaper
        // progress.
        for event in events_to_emit {
            self.emit_event(event);
        }

        // GC: drop terminal entries past the retention window.
        let cutoff = Utc::now() - chrono::Duration::seconds(TERMINAL_RETENTION_SECS);
        let to_drop: Vec<SweepId> = self
            .entries
            .iter()
            .filter_map(|(id, info)| {
                let terminated_at = match &info.state {
                    SweepState::Exited { at, .. } | SweepState::Crashed { at } => Some(*at),
                    _ => None,
                };
                terminated_at.filter(|t| *t < cutoff).map(|_| id.clone())
            })
            .collect();
        for id in to_drop {
            self.entries.remove(&id);
            // Defensive: a terminal entry should have had its handle reaped in
            // `poll_liveness` already, but drop any lingering handle so a
            // GC'd sweep never leaks a `Child` (Issue #3801).
            let _ = self.children.remove(&id);
            changes += 1;
        }
        changes
    }

    /// Promptly reconcile sweep liveness on a **read path** (Issue #3893).
    ///
    /// `ListSweeps` / `GetSweepStatus` / the work-finder occupancy seed call
    /// this before reading, so a caller never observes a sweep as `Running`
    /// after its child has already exited. Before #3893 the only path out of
    /// `Running` was the 30s [`reap_once`](Self::reap_once) timer, so a read
    /// taken between a child's exit and the next tick over-reported active work
    /// (the registry accumulated stale `Running` entries across a burst of
    /// merges). Reap-on-read bounds that staleness window to the read itself.
    ///
    /// This performs exactly the same liveness `try_wait` + terminal transition
    /// (and best-effort event/label side effects) the background timer does; on
    /// a steady-state read with no newly-exited children it is just one cheap
    /// `try_wait` per running entry and no side effects. Returns the number of
    /// entries reaped.
    pub fn reap_liveness(&mut self) -> usize {
        self.reap_once()
    }

    // ------------------------------------------------------------------------
    // Startup watchdog (Issue #3887)
    // ------------------------------------------------------------------------

    /// Probe whether a daemon-dispatched sweep has made any startup progress
    /// (Issue #3887). Progress = the sweep got past the pre-API local-startup /
    /// MCP-init phase, evidenced by ANY of:
    ///
    /// - a worktree at `.loom/worktrees/issue-<N>` (Builder-phase artifact),
    /// - a checkpoint at `.loom/sweep-checkpoint/issue-<N>.json` (a phase
    ///   completed), or
    /// - log output past the spawn header + `spawn-claude.sh` wrapper lines
    ///   ([`log_has_progress`]).
    ///
    /// A hung child exhibits none of these: no worktree, no checkpoint, and a
    /// log containing only the dispatch header and the account-selection line.
    fn sweep_made_progress(&self, issue: u32, log_path: &Path) -> bool {
        let worktree = self
            .config
            .workspace_root
            .join(".loom")
            .join("worktrees")
            .join(format!("issue-{issue}"));
        if worktree.exists() {
            return true;
        }
        let checkpoint = self
            .config
            .checkpoint_dir()
            .join(format!("issue-{issue}.json"));
        if checkpoint.exists() {
            return true;
        }
        matches!(std::fs::read_to_string(log_path), Ok(c) if log_has_progress(&c))
    }

    /// Run one watchdog tick (Issue #3887): for each running daemon-dispatched
    /// Issue sweep, apply the [`watchdog_decision`] state machine and, on
    /// [`WatchdogDecision::Restart`], auto-cancel the hung child and
    /// re-dispatch the issue **exactly once** (bounded — a second hang resolves
    /// to [`WatchdogDecision::GiveUp`] and is left for the operator).
    ///
    /// Both the auto-cancel and the retry log loudly. No new event topics are
    /// introduced: the cancel reuses the frozen
    /// `sweep.issue.{N}.exited` / `sweep.global.completed` emission from
    /// [`finish_cancel`], and the re-dispatch reuses `sweep.global.dispatch`
    /// from [`dispatch`]. Returns the number of sweeps restarted this tick.
    ///
    /// Only sweeps this daemon instance actually spawned (a retained `Child`
    /// handle exists) are eligible — a reconstructed entry from a prior daemon
    /// has no handle to cancel and is left to the reaper.
    pub fn watchdog_once(&mut self, timeout: Duration) -> usize {
        let now = Utc::now();
        // Snapshot eligible candidates first so we can mutate below.
        let candidates: Vec<(SweepId, u32, PathBuf, Duration)> = self
            .entries
            .iter()
            .filter(|(id, info)| {
                matches!(info.state, SweepState::Running | SweepState::Pending)
                    && matches!(info.kind, SweepKind::Issue(_))
                    // Only sweeps we spawned (own the Child handle) are cancelable.
                    && self.children.contains_key(*id)
            })
            .filter_map(|(id, info)| {
                let SweepKind::Issue(issue) = info.kind else {
                    return None;
                };
                let elapsed = (now - info.started_at).to_std().unwrap_or(Duration::ZERO);
                Some((id.clone(), issue, info.log_path.clone(), elapsed))
            })
            .collect();

        let mut restarts = 0usize;
        for (sweep_id, issue, log_path, elapsed) in candidates {
            let made_progress = self.sweep_made_progress(issue, &log_path);
            let already_retried = self.watchdog_retried.contains(&issue);
            match watchdog_decision(elapsed, timeout, made_progress, already_retried) {
                WatchdogDecision::Healthy => {}
                WatchdogDecision::GiveUp => {
                    // Bounded: already retried once. Log once per issue.
                    if self.watchdog_gaveup.insert(issue) {
                        log::error!(
                            "watchdog: sweep for issue #{issue} ({sweep_id}) is still stuck \
                             {}s after an auto-restart — giving up (bounded to one retry). \
                             Operator intervention needed (cancel + re-dispatch, or investigate \
                             the MCP-init hang).",
                            elapsed.as_secs()
                        );
                    }
                }
                WatchdogDecision::Restart => {
                    log::warn!(
                        "watchdog: sweep for issue #{issue} ({sweep_id}) made no progress in \
                         {}s (no worktree/checkpoint, log stuck at the spawn header) — \
                         auto-cancelling and re-dispatching once (#3887).",
                        elapsed.as_secs()
                    );
                    // Capture re-dispatch params from the hung entry BEFORE
                    // cancel mutates it.
                    let (model, effort, depends_on, idempotency_key) = self
                        .entries
                        .get(&sweep_id)
                        .map(|i| {
                            (
                                i.model.clone(),
                                i.effort.clone(),
                                i.depends_on,
                                i.idempotency_key.clone(),
                            )
                        })
                        .unwrap_or((None, None, None, None));

                    // Mark retried BEFORE acting so any error path still counts
                    // the single allowed attempt (never loops).
                    self.watchdog_retried.insert(issue);

                    // Cancel the hung child (SIGTERM → grace → SIGKILL). This
                    // releases the per-issue lock and restores loom:building ->
                    // loom:issue (finish_cancel's orphaned-claim recovery), so
                    // the re-dispatch below can re-acquire cleanly.
                    if let Err(e) = self.cancel(&sweep_id, WATCHDOG_CANCEL_GRACE) {
                        log::error!(
                            "watchdog: auto-cancel of hung sweep {sweep_id} (issue #{issue}) \
                             failed: {e}"
                        );
                        continue;
                    }

                    match self.dispatch(
                        &SweepKind::Issue(issue),
                        idempotency_key,
                        model.as_deref(),
                        effort.as_deref(),
                        depends_on,
                    ) {
                        Ok(outcome) => {
                            restarts += 1;
                            log::warn!(
                                "watchdog: re-dispatched issue #{issue} as {} (pid {}) after \
                                 startup hang (#3887).",
                                outcome.sweep_id,
                                outcome.pid
                            );
                        }
                        Err(e) => {
                            log::error!(
                                "watchdog: re-dispatch of issue #{issue} after hang failed: {e} \
                                 (issue left recoverable — its claim was already restored)."
                            );
                        }
                    }
                }
            }
        }
        restarts
    }

    // ------------------------------------------------------------------------
    // Reconstruction
    // ------------------------------------------------------------------------

    /// Reconstruct registry entries on daemon startup by combining:
    ///
    /// 1. Live lock dirs under `.loom/locks/issue-<N>/` (the lock's
    ///    `owner.json` records the dispatching daemon's PID and sweep ID).
    /// 2. Sweep checkpoints under `.loom/sweep-checkpoint/issue-<N>.json`
    ///    (#3373) — these survive crashes and signal that a sweep was in
    ///    flight even if the lock is gone.
    ///
    /// This is best-effort: locks whose `owner_pid` is dead are released
    /// (they're stale); locks whose owner is live are admitted as `Running`.
    ///
    /// # Daemon ownership of checkpoints (Issue #3808)
    ///
    /// `.loom/sweep-checkpoint/` is written by the shared `/loom:sweep` skill
    /// regardless of how the run was launched — an in-session (subagent-path)
    /// sweep writes checkpoints there just like a daemon-dispatched detached
    /// child does. A checkpoint file alone therefore does **not** imply the
    /// daemon owns the sweep. The daemon-ownership signal is the **lock**: only
    /// `dispatch` writes `.loom/locks/issue-<N>/`, and in-session sweeps never
    /// touch it. So the checkpoint pass synthesizes a `Crashed` recovery entry
    /// only for issues that had a daemon-owned lock whose owner PID is now dead
    /// (a genuine daemon-owned sweep whose process is gone). Checkpoints with
    /// no lock — in-session `/loom:sweep` runs the daemon never dispatched —
    /// are skipped, so the daemon no longer ingests phantom entries for sweeps
    /// it does not own. Genuine daemon-crash recovery is preserved because the
    /// lock survives a daemon crash (it is only removed on clean release).
    #[allow(clippy::too_many_lines)]
    pub fn reconstruct(&mut self) -> Result<usize> {
        let locks_dir = self.config.locks_dir();
        let mut admitted = 0usize;
        // Issues that had a daemon-owned lock whose owner PID is now dead.
        // These are the only issues whose checkpoints the checkpoint pass may
        // recover as `Crashed` (Issue #3808) — the lock is the daemon-ownership
        // signal that a bare checkpoint file lacks.
        let mut daemon_owned_dead: HashSet<u32> = HashSet::new();

        if locks_dir.exists() {
            for entry in std::fs::read_dir(&locks_dir)? {
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => {
                        log::warn!("read_dir error in {}: {e}", locks_dir.display());
                        continue;
                    }
                };
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default();
                let Some(issue_str) = name.strip_prefix("issue-") else {
                    continue;
                };
                let Ok(issue): Result<u32, _> = issue_str.parse() else {
                    continue;
                };
                let owner_path = path.join("owner.json");
                let owner: Option<LockOwner> = std::fs::read_to_string(&owner_path)
                    .ok()
                    .and_then(|s| serde_json::from_str(&s).ok());
                let Some(owner) = owner else {
                    // No owner.json — treat as stale, remove.
                    let _ = std::fs::remove_dir_all(&path);
                    continue;
                };
                if !is_pid_alive(owner.owner_pid) {
                    // Stale lock: the daemon-dispatched child's PID (recorded
                    // by `record_child_pid_in_lock`, #3808) is dead. This lock
                    // is the daemon's own crash-surviving evidence that it
                    // dispatched this issue, so record the issue — the
                    // checkpoint pass may recover it as `Crashed` — then drop
                    // the stale lock and continue.
                    daemon_owned_dead.insert(issue);
                    let _ = std::fs::remove_dir_all(&path);
                    continue;
                }
                let log_path = self.compute_log_path(issue);
                let started_at = chrono::DateTime::parse_from_rfc3339(&owner.acquired_at)
                    .map_or_else(|_| Utc::now(), |t| t.with_timezone(&Utc));
                self.entries.insert(
                    owner.sweep_id.clone(),
                    SweepInfo {
                        sweep_id: owner.sweep_id.clone(),
                        kind: SweepKind::Issue(issue),
                        pid: owner.owner_pid,
                        token_name: "unknown".to_string(),
                        log_path,
                        idempotency_key: None,
                        started_at,
                        state: SweepState::Running,
                        latest_phase: None,
                        pr_number: None,
                        // Lock owner.json does not record the model; the
                        // dispatching daemon instance is gone (#3482).
                        model: None,
                        // Effort is likewise unrecoverable from the lock (#3716).
                        effort: None,
                        // depends_on is not recorded in the lock owner (#3729).
                        depends_on: None,
                    },
                );
                admitted += 1;
            }
        }

        // Checkpoints for daemon-owned sweeps whose process is gone -> Crashed
        // entries (so list_sweeps shows them; the next dispatch resumes via the
        // sweep skill). Gated on daemon ownership (Issue #3808): a checkpoint
        // is only recovered when a daemon-owned lock existed for its issue.
        let checkpoint_dir = self.config.checkpoint_dir();
        if checkpoint_dir.exists() {
            for entry in std::fs::read_dir(&checkpoint_dir)? {
                let Ok(entry) = entry else { continue };
                let path = entry.path();
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default();
                let Some(rest) = name.strip_prefix("issue-") else {
                    continue;
                };
                let Some(issue_str) = rest.strip_suffix(".json") else {
                    continue;
                };
                let Ok(issue): Result<u32, _> = issue_str.parse() else {
                    continue;
                };
                // Skip if we already have a Running entry for this issue.
                let already_running = self.entries.values().any(|info| {
                    matches!(info.state, SweepState::Running | SweepState::Pending)
                        && matches!(info.kind, SweepKind::Issue(n) if n == issue)
                });
                if already_running {
                    continue;
                }
                // Issue #3808: only recover a checkpoint when the daemon has
                // independent evidence it dispatched this issue — a daemon-owned
                // lock existed for it (captured in the lock pass above). A bare
                // checkpoint file does NOT imply daemon ownership because the
                // shared /loom:sweep skill writes `.loom/sweep-checkpoint/`
                // regardless of launch mechanism. In-session sweeps never write
                // a lock, so their checkpoints are skipped here — no phantom
                // daemon registry entry.
                if !daemon_owned_dead.contains(&issue) {
                    continue;
                }
                let sweep_id = format!("sweep-issue-{issue}-recovered-{}", Utc::now().timestamp());
                let phase = read_checkpoint_phase(&path);
                self.entries.insert(
                    sweep_id.clone(),
                    SweepInfo {
                        sweep_id,
                        kind: SweepKind::Issue(issue),
                        pid: 0, // unknown — owner is gone
                        token_name: "unknown".to_string(),
                        log_path: self.compute_log_path(issue),
                        idempotency_key: None,
                        started_at: Utc::now(),
                        state: SweepState::Crashed { at: Utc::now() },
                        latest_phase: phase,
                        pr_number: None,
                        model: None,      // not recoverable from a checkpoint-only entry
                        effort: None,     // not recoverable from a checkpoint-only entry
                        depends_on: None, // not recoverable from a checkpoint-only entry
                    },
                );
                admitted += 1;
            }
        }

        Ok(admitted)
    }
}

// ============================================================================
// Public helpers
// ============================================================================

/// Result of a successful dispatch.
#[derive(Debug, Clone)]
pub struct DispatchOutcome {
    pub sweep_id: SweepId,
    pub pid: u32,
    pub token_name: String,
    pub log_path: PathBuf,
    /// `false` when the dispatch was an idempotency hit on an existing
    /// `Running` entry.
    pub was_new: bool,
}

/// Result of the lock-scoped [`begin_cancel`](SweepRegistry::begin_cancel)
/// step of a split cancel (Issue #3807).
///
/// Splitting `cancel` into begin → poll → finish lets the IPC handler run the
/// grace poll/sleep window WITHOUT holding the registry mutex, so concurrent
/// `ListSweeps` / `GetSweepStatus` / `DispatchSweep` for other sweeps are not
/// blocked for the (potentially multi-second) grace duration.
#[derive(Debug, Clone)]
pub enum BeginCancel {
    /// The sweep was already terminal — nothing was signalled. Carries the
    /// idempotent [`CancelOutcome`] (`was_running = false`) to return directly.
    AlreadyTerminal(CancelOutcome),
    /// SIGTERM has been delivered to the sweep's process group. The caller must
    /// now poll for exit (unlocked) via
    /// [`poll_cancel`](SweepRegistry::poll_cancel) and then call
    /// [`finish_cancel`](SweepRegistry::finish_cancel).
    Signalled {
        pid: u32,
        kind: SweepKind,
        started_at: DateTime<Utc>,
    },
}

/// Result of a `cancel` call (Issue #3455, Phase C).
#[derive(Debug, Clone)]
pub struct CancelOutcome {
    pub sweep_id: SweepId,
    pub pid: u32,
    /// `true` when the child did not exit within the grace window and
    /// a SIGKILL was issued.
    pub sigkill_sent: bool,
    /// `true` when the sweep was in `Running`/`Pending` state at the
    /// moment of the call; `false` when it was already terminal.
    pub was_running: bool,
}

/// Generate a stable sweep ID for the given kind. Format follows the
/// spawn-loop log naming convention so operators can correlate.
#[must_use]
pub fn generate_sweep_id(kind: &SweepKind) -> SweepId {
    let ts = Utc::now().timestamp();
    match kind {
        SweepKind::Issue(n) => format!("sweep-issue-{n}-{ts}"),
        SweepKind::PrSet(prs) => {
            let joined = prs
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("-");
            format!("sweep-prs-{joined}-{ts}")
        }
    }
}

/// Resolve the configured reaper interval from the environment, falling
/// back to [`DEFAULT_REAPER_INTERVAL_SECS`].
#[must_use]
pub fn resolve_reaper_interval() -> Duration {
    let secs = std::env::var(REAPER_INTERVAL_ENV)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_REAPER_INTERVAL_SECS);
    Duration::from_secs(secs)
}

/// Spawn the long-running reaper task. Returns the task handle so the
/// daemon can keep it alive for the lifetime of the process.
///
/// The reaper takes the registry lock briefly each tick; it never holds
/// the lock across the sleep.
pub fn spawn_reaper_task(registry: Arc<Mutex<SweepRegistry>>) -> tokio::task::JoinHandle<()> {
    let interval = resolve_reaper_interval();
    log::info!("sweep_registry: starting reaper with interval={}s", interval.as_secs());
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // First tick fires immediately; skip it so we don't churn at boot.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let changed = {
                match registry.lock() {
                    Ok(mut r) => r.reap_once(),
                    Err(poisoned) => {
                        log::error!("sweep_registry: mutex poisoned ({poisoned:?})");
                        return;
                    }
                }
            };
            if changed > 0 {
                log::info!(
                    "sweep_registry: reaper changed {changed} entr{}",
                    if changed == 1 { "y" } else { "ies" }
                );
            }
        }
    })
}

// ============================================================================
// Startup-race config resolution + watchdog task (Issue #3887)
// ============================================================================

/// The subset of `.loom/config.json → autonomous` this module consumes for the
/// startup-race mitigation (Issue #3887). Each field is `Option` so an absent
/// key falls through to the env-var / built-in-default resolution — precedence
/// **env > config > default** for every knob, matching
/// [`crate::work_finder::WorkFinderConfig`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartupRaceConfig {
    /// `autonomous.dispatchStaggerMs` — min gap between spawns, in ms. A value
    /// of `0` is honored (disables the stagger).
    pub dispatch_stagger_ms: Option<u64>,
    /// `autonomous.watchdog.enabled` — whether to run the watchdog task.
    pub watchdog_enabled: Option<bool>,
    /// `autonomous.watchdog.timeoutSecs` — no-progress timeout, in seconds
    /// (zero/invalid dropped to `None`).
    pub watchdog_timeout_secs: Option<u64>,
    /// `autonomous.watchdog.intervalSecs` — probe interval, in seconds
    /// (zero/invalid dropped to `None`).
    pub watchdog_interval_secs: Option<u64>,
}

/// Read `.loom/config.json → autonomous` for the startup-race knobs (Issue
/// #3887), soft-failing every field to `None` on a missing file, malformed
/// JSON, or an absent `autonomous` block. Mirrors
/// [`crate::work_finder::read_work_finder_config`].
#[must_use]
pub fn read_startup_race_config(repo_root: &Path) -> StartupRaceConfig {
    let config_path = repo_root.join(".loom").join("config.json");
    let Ok(config_str) = std::fs::read_to_string(&config_path) else {
        return StartupRaceConfig::default();
    };
    let Ok(config) = serde_json::from_str::<serde_json::Value>(&config_str) else {
        log::warn!("sweep_registry: could not parse config at {}", config_path.display());
        return StartupRaceConfig::default();
    };
    let Some(auto) = config.get("autonomous") else {
        return StartupRaceConfig::default();
    };
    let watchdog = auto.get("watchdog");
    StartupRaceConfig {
        // A stagger of 0 is a meaningful "disable" value, so it is NOT filtered
        // out here (unlike interval/timeout where 0 is nonsensical).
        dispatch_stagger_ms: auto
            .get("dispatchStaggerMs")
            .and_then(serde_json::Value::as_u64),
        watchdog_enabled: watchdog
            .and_then(|w| w.get("enabled"))
            .and_then(serde_json::Value::as_bool),
        watchdog_timeout_secs: watchdog
            .and_then(|w| w.get("timeoutSecs"))
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
        watchdog_interval_secs: watchdog
            .and_then(|w| w.get("intervalSecs"))
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
    }
}

/// Resolve the dispatch stagger with precedence **env > config > default**
/// (Issue #3887). A `0` (from either env or config) disables the stagger.
#[must_use]
pub fn resolve_dispatch_stagger(config: &StartupRaceConfig) -> Duration {
    let ms = std::env::var(DISPATCH_STAGGER_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .or(config.dispatch_stagger_ms)
        .unwrap_or(DEFAULT_DISPATCH_STAGGER_MS);
    Duration::from_millis(ms)
}

/// Resolve whether the watchdog runs, precedence **env > config >
/// default(true)** (Issue #3887). The watchdog defaults **on** — it is a
/// self-healing backstop with a generous timeout and a bounded single retry —
/// but can be disabled entirely via env or config.
#[must_use]
pub fn resolve_watchdog_enabled(config: &StartupRaceConfig) -> bool {
    if let Ok(v) = std::env::var(WATCHDOG_ENABLE_ENV) {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config.watchdog_enabled.unwrap_or(true)
}

/// Resolve the watchdog no-progress timeout, precedence **env > config >
/// default** (Issue #3887).
#[must_use]
pub fn resolve_watchdog_timeout(config: &StartupRaceConfig) -> Duration {
    let secs = std::env::var(WATCHDOG_TIMEOUT_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(config.watchdog_timeout_secs)
        .unwrap_or(DEFAULT_WATCHDOG_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// Resolve the watchdog probe interval, precedence **env > config > default**
/// (Issue #3887).
#[must_use]
pub fn resolve_watchdog_interval(config: &StartupRaceConfig) -> Duration {
    let secs = std::env::var(WATCHDOG_INTERVAL_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(config.watchdog_interval_secs)
        .unwrap_or(DEFAULT_WATCHDOG_INTERVAL_SECS);
    Duration::from_secs(secs)
}

/// Spawn the startup watchdog task (Issue #3887). Every `interval`, it probes
/// each running daemon-dispatched sweep for progress and auto-cancels +
/// re-dispatches (once, bounded) any that have hung past `timeout`. Mirrors
/// [`spawn_reaper_task`]: brief lock per tick, never held across the sleep.
pub fn spawn_watchdog_task(
    registry: Arc<Mutex<SweepRegistry>>,
    timeout: Duration,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    log::info!(
        "sweep_registry: starting startup watchdog (interval={}s, timeout={}s) (#3887)",
        interval.as_secs(),
        timeout.as_secs()
    );
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // First tick fires immediately; skip it so we don't act at boot before
        // any sweep has had a chance to start.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let restarted = {
                match registry.lock() {
                    Ok(mut r) => r.watchdog_once(timeout),
                    Err(poisoned) => {
                        log::error!("sweep_registry: watchdog mutex poisoned ({poisoned:?})");
                        return;
                    }
                }
            };
            if restarted > 0 {
                log::warn!(
                    "sweep_registry: watchdog auto-restarted {restarted} hung sweep{} (#3887)",
                    if restarted == 1 { "" } else { "s" }
                );
            }
        }
    })
}

// ============================================================================
// Internal helpers
// ============================================================================

/// Liveness probe via `kill(pid, 0)`. Returns true when the signal would
/// be deliverable (i.e. the process exists and is owned by us). PID 0 is
/// always treated as dead.
#[cfg(unix)]
fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: kill(pid, 0) is a documented liveness probe; signal 0 is not
    // sent, just checked.
    let pid_t: i32 = match pid.try_into() {
        Ok(p) => p,
        Err(_) => return false,
    };
    libc_kill(pid_t, 0) == 0
}

#[cfg(not(unix))]
fn is_pid_alive(_pid: u32) -> bool {
    // Non-unix platforms are not supported targets for Loom; assume alive
    // so the test suite can run without a hard panic.
    true
}

#[cfg(unix)]
extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

#[cfg(unix)]
#[allow(non_snake_case)]
fn libc_kill(pid: i32, sig: i32) -> i32 {
    // Indirection so we can stub this in unit tests if needed.
    unsafe { kill(pid, sig) }
}

/// Best-effort extraction of the `phase` field from a sweep checkpoint
/// JSON file. Schema is owned by the sweep skill (#3373); we treat the
/// file as opaque and only peek at one field.
fn read_checkpoint_phase(path: &Path) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&s).ok()?;
    v.get("phase")
        .and_then(|p| p.as_str())
        .map(ToString::to_string)
}

/// Send a signal to a PID. Returns `true` on success (signal queued or
/// process already absent and the caller can treat that as "done"). PID
/// 0 is rejected to avoid the POSIX broadcast-to-group semantics.
#[cfg(unix)]
fn send_signal(pid: u32, sig: i32) -> bool {
    if pid == 0 {
        return false;
    }
    let Ok(pid_t): Result<i32, _> = pid.try_into() else {
        return false;
    };
    libc_kill(pid_t, sig) == 0
}

#[cfg(not(unix))]
fn send_signal(_pid: u32, _sig: i32) -> bool {
    // Non-unix platforms are not supported; return false so the cancel
    // path surfaces a "kill failed" log but still transitions state.
    false
}

/// Send a signal to the entire process GROUP led by `pgid` (Issue #3800).
///
/// POSIX `kill(-pgid, sig)` delivers `sig` to every process in the group
/// `pgid`. Because sweep children are spawned as group leaders
/// (`process_group(0)` → `setpgid(0, 0)`), a child's pgid equals its own PID,
/// so passing the tracked child PID here reaches the child AND every
/// descendant it forked (Bash-tool commands, MCP servers, git clones, …) —
/// tearing down the whole subtree instead of orphaning it.
///
/// Returns `true` on success. `pgid == 0` is rejected: `kill(0, sig)` targets
/// the *caller's* group (the daemon itself), which would be catastrophic.
#[cfg(unix)]
fn send_group_signal(pgid: u32, sig: i32) -> bool {
    if pgid == 0 {
        return false;
    }
    let Ok(pgid_t): Result<i32, _> = pgid.try_into() else {
        return false;
    };
    // Negative target = process group. See kill(2).
    libc_kill(-pgid_t, sig) == 0
}

#[cfg(not(unix))]
fn send_group_signal(_pgid: u32, _sig: i32) -> bool {
    false
}

/// Read the last `n` lines of a file. Returns an empty vec when the
/// file is empty; returns an error when the file does not exist (so the
/// caller can distinguish "no log yet" from "log gone").
///
/// Implementation is a simple full-read + split — sweep logs are
/// bounded by the lifetime of a sweep (~tens of minutes typical) and
/// the buffering overhead is dwarfed by the IPC round-trip. If sweep
/// logs grow to GB-scale in a future release, swap this for a reverse
/// reader.
fn tail_lines(path: &Path, n: usize) -> Result<Vec<String>> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    if n == 0 {
        return Ok(Vec::new());
    }
    let mut out: Vec<String> = contents.lines().map(ToString::to_string).collect();
    if out.len() > n {
        out = out.split_off(out.len() - n);
    }
    Ok(out)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    /// Build a temp-workspace registry with a fake spawn binary that
    /// records its argv + env into a log and exits immediately. This lets
    /// us assert on the dispatch behavior without invoking real `claude`.
    ///
    /// We invoke the fake via `bash -c '...'` (returned from
    /// `SweepRegistryConfig.spawn_bin`) rather than relying on a shebang +
    /// exec bit, because parallel-test load on macOS occasionally races the
    /// chmod with the child's posix_spawn exec call and the script silently
    /// fails to launch (no shebang resolution, no exec-bit yet).
    fn fixture_registry(workspace: &Path) -> (SweepRegistry, PathBuf) {
        let record_log = workspace.join("fake-spawn.log");
        // We use /bin/bash as the spawn binary, and the dispatch path appends
        // "-p <prompt>" — we ignore the args via `--`, then run an inline
        // recording script.
        let scripts_dir = workspace.join(".loom").join("scripts");
        std::fs::create_dir_all(&scripts_dir).unwrap();
        let fake_bin = scripts_dir.join("spawn-claude.sh");
        // Use exec on bash directly with arguments: we write a wrapper that
        // bash will invoke. The wrapper is small enough that a bad chmod
        // would be a system-level problem, not a test-flake.
        let script = format!(
            r#"#!/usr/bin/env bash
# Test fixture: record dispatch args + selected env into a log.
{{
  printf 'argv: %s\n' "$*"
  printf 'CLAUDE_CODE_OAUTH_TOKEN=%s\n' "${{CLAUDE_CODE_OAUTH_TOKEN:-unset}}"
  printf 'LOOM_WORKSPACE=%s\n' "${{LOOM_WORKSPACE:-unset}}"
  printf 'PWD=%s\n' "$(pwd -P)"
  printf 'LOOM_MODEL_EXPERIMENT=%s\n' "${{LOOM_MODEL_EXPERIMENT:-unset}}"
  printf 'LOOM_MODEL_EXPERIMENT_CANARY=%s\n' "${{LOOM_MODEL_EXPERIMENT_CANARY:-unset}}"
  printf 'LOOM_TRANSCRIPT_ARCHIVE=%s\n' "${{LOOM_TRANSCRIPT_ARCHIVE:-unset}}"
  printf 'LOOM_SWEEP_CLAIM_OWNED=%s\n' "${{LOOM_SWEEP_CLAIM_OWNED:-unset}}"
  printf 'LOOM_TERMINAL_ID=%s\n' "${{LOOM_TERMINAL_ID:-unset}}"
}} >> "{rec}" 2>&1
exit 0
"#,
            rec = record_log.display()
        );
        std::fs::write(&fake_bin, script).unwrap();
        let mut perms = std::fs::metadata(&fake_bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_bin, perms).unwrap();
        // Sync the perms change to the filesystem so the child sees it.
        // On macOS APFS under heavy load, posix_spawn occasionally exec's
        // before the chmod is visible to the child process.
        if let Ok(f) = std::fs::File::open(&fake_bin) {
            let _ = f.sync_all();
        }

        let mut config = SweepRegistryConfig::new(workspace.to_path_buf());
        config.spawn_bin = Some(fake_bin);
        config.skip_label_flip = true;
        (SweepRegistry::new(config), record_log)
    }

    /// Wait until `path` exists AND contains `needle`. Returns true on
    /// success, false on timeout.
    fn wait_for_contents(path: &Path, needle: &str, timeout_ms: u64) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed().as_millis() < u128::from(timeout_ms) {
            if let Ok(s) = std::fs::read_to_string(path) {
                if s.contains(needle) {
                    return true;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        false
    }

    /// Poll `.loom/scripts/spawn-claude.sh` fixture installation: write a
    /// custom script body + exec bit into `workspace` and return a registry
    /// configured to spawn it. Used by the child-process-lifecycle tests
    /// (#3800/#3801) which need a long-lived / tree-forking child rather than
    /// the record-and-exit fake in `fixture_registry`.
    fn lifecycle_registry(workspace: &Path, script_body: &str) -> SweepRegistry {
        let scripts_dir = workspace.join(".loom").join("scripts");
        std::fs::create_dir_all(&scripts_dir).unwrap();
        let fake_bin = scripts_dir.join("spawn-claude.sh");
        std::fs::write(&fake_bin, script_body).unwrap();
        let mut perms = std::fs::metadata(&fake_bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_bin, perms).unwrap();
        if let Ok(f) = std::fs::File::open(&fake_bin) {
            let _ = f.sync_all();
        }
        let mut config = SweepRegistryConfig::new(workspace.to_path_buf());
        config.spawn_bin = Some(fake_bin);
        config.skip_label_flip = true;
        SweepRegistry::new(config)
    }

    /// Poll until `pid` becomes alive (via `kill(pid, 0)`), up to
    /// `timeout_ms`. Returns true once alive, false on timeout.
    fn wait_until_alive(pid: u32, timeout_ms: u64) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed().as_millis() < u128::from(timeout_ms) {
            if is_pid_alive(pid) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        is_pid_alive(pid)
    }

    /// Poll until `pid` is no longer alive (via `kill(pid, 0)`), up to
    /// `timeout_ms`. Returns the final liveness (false = dead = success).
    fn wait_until_dead(pid: u32, timeout_ms: u64) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed().as_millis() < u128::from(timeout_ms) {
            if !is_pid_alive(pid) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        !is_pid_alive(pid)
    }

    /// Read a PID written to `path` by a fixture child, polling until a
    /// parseable integer appears (or timeout). Returns `None` on timeout.
    fn read_pid_file(path: &Path, timeout_ms: u64) -> Option<u32> {
        let start = std::time::Instant::now();
        while start.elapsed().as_millis() < u128::from(timeout_ms) {
            if let Ok(s) = std::fs::read_to_string(path) {
                if let Ok(p) = s.trim().parse::<u32>() {
                    return Some(p);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        None
    }

    #[test]
    #[serial]
    fn dispatch_happy_path_records_entry() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(42), None, None, None, None)
            .expect("dispatch should succeed");

        assert!(outcome.was_new);
        assert!(outcome.pid > 0);
        assert_eq!(outcome.token_name, "unknown");
        assert_eq!(registry.len(), 1);

        let info = registry.get(&outcome.sweep_id).unwrap();
        assert!(matches!(info.kind, SweepKind::Issue(42)));
        assert!(matches!(info.state, SweepState::Running));

        // Wait for the fake spawn to record its invocation. We wait for
        // the final line (LOOM_TERMINAL_ID) so the assertion isn't racing
        // mid-write.
        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        let sweep_log = dir
            .path()
            .join(".loom")
            .join("logs")
            .join("sweep-issue-42.log");
        assert!(
            wait_for_contents(&record_log, &needle, 10000),
            "fake spawn-claude.sh did not finish writing within 10s\n  record_log: {}\n  record_log exists: {}\n  sweep_log: {}",
            std::fs::read_to_string(&record_log).unwrap_or_default(),
            record_log.exists(),
            std::fs::read_to_string(&sweep_log).unwrap_or_default(),
        );
        let recorded = std::fs::read_to_string(&record_log).unwrap();
        assert!(
            recorded.contains("argv: -p /loom:sweep 42"),
            "expected argv in recorded log; got: {recorded}"
        );
        // Issue #3477 zero-behavior-change criterion: with model=None the
        // spawned command must NOT receive a --model flag at all.
        assert!(
            !recorded.contains("--model"),
            "model=None must not emit --model; got: {recorded}"
        );
        // Issue #3716: with effort=None the spawned command must likewise NOT
        // receive a --effort flag at all (byte-for-byte unchanged default).
        assert!(
            !recorded.contains("--effort"),
            "effort=None must not emit --effort; got: {recorded}"
        );
        // #3482: model=None dispatches record no model on the entry.
        assert_eq!(registry.get(&outcome.sweep_id).unwrap().model, None);
        // #3716: effort=None dispatches record no effort on the entry.
        assert_eq!(registry.get(&outcome.sweep_id).unwrap().effort, None);

        // The lock dir should exist while Running.
        let lock = dir.path().join(".loom").join("locks").join("issue-42");
        assert!(lock.exists(), "expected lock dir at {}", lock.display());

        // Issue #3824: every daemon-dispatched child must carry
        // --dangerously-skip-permissions (unattended, non-interactive).
        assert!(
            recorded.contains("--dangerously-skip-permissions"),
            "expected --dangerously-skip-permissions in argv; got: {recorded}"
        );
    }

    /// Issue #3824: `spawn_child` unconditionally appends
    /// `--dangerously-skip-permissions` to the child argv so a detached,
    /// non-interactive `claude -p` sweep never stalls on a permission prompt.
    /// With no model/effort/depends-on the flag is the sole trailing arg,
    /// appended AFTER any of those (verified by the exact positional form).
    #[test]
    #[serial]
    fn dispatch_appends_dangerously_skip_permissions() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(4242), None, None, None, None)
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        assert!(
            wait_for_contents(&record_log, &needle, 10000),
            "fake spawn-claude.sh did not finish writing within 10s"
        );
        let recorded = std::fs::read_to_string(&record_log).unwrap();
        assert!(
            recorded.contains("argv: -p /loom:sweep 4242 --dangerously-skip-permissions"),
            "expected --dangerously-skip-permissions appended after the prompt; got: {recorded}"
        );
    }

    /// Issue #3823 (Option A): `spawn_child` exports the claim-ownership
    /// marker `LOOM_SWEEP_CLAIM_OWNED=<issue>` into the dispatched child so its
    /// `/loom:sweep` pre-flight recognises the daemon's own pre-dispatch
    /// loom:building flip as its OWN claim (and proceeds to build) rather than
    /// self-skipping. The value is exactly the dispatched issue number.
    #[test]
    #[serial]
    fn dispatch_exports_claim_ownership_marker() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(4243), None, None, None, None)
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        assert!(
            wait_for_contents(&record_log, &needle, 10000),
            "fake spawn-claude.sh did not finish writing within 10s"
        );
        let recorded = std::fs::read_to_string(&record_log).unwrap();
        assert!(
            recorded.contains("LOOM_SWEEP_CLAIM_OWNED=4243"),
            "expected claim-ownership marker for issue 4243; got: {recorded}"
        );
    }

    /// Issue #3477 (Phase 1): a `model` dispatch param threads through to
    /// the spawn command as an explicit `--model <value>` argument.
    #[test]
    #[serial]
    fn dispatch_with_model_appends_model_arg() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(43), None, Some("claude-sonnet-4-6"), None, None)
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        assert!(
            wait_for_contents(&record_log, &needle, 10000),
            "fake spawn-claude.sh did not finish writing within 10s"
        );
        let recorded = std::fs::read_to_string(&record_log).unwrap();
        assert!(
            recorded.contains("argv: -p /loom:sweep 43 --model claude-sonnet-4-6"),
            "expected --model in argv; got: {recorded}"
        );
        // #3482 (Phase 3a): the dispatch model is carried on the registry
        // entry so list_sweeps / get_sweep_status report it.
        assert_eq!(
            registry.get(&outcome.sweep_id).unwrap().model.as_deref(),
            Some("claude-sonnet-4-6"),
            "dispatch model must be recorded on the SweepInfo entry"
        );
    }

    /// Issue #3477: an empty-string model is treated as unset — `--model ""`
    /// must never be emitted (acceptance criterion: no flag at all, not an
    /// empty flag).
    #[test]
    #[serial]
    fn dispatch_with_empty_model_emits_no_model_flag() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(44), None, Some(""), None, None)
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        assert!(
            wait_for_contents(&record_log, &needle, 10000),
            "fake spawn-claude.sh did not finish writing within 10s"
        );
        let recorded = std::fs::read_to_string(&record_log).unwrap();
        assert!(
            !recorded.contains("--model"),
            "empty model must not emit --model; got: {recorded}"
        );
        // #3482: empty-string model normalizes to None on the entry too.
        assert_eq!(
            registry.get(&outcome.sweep_id).unwrap().model,
            None,
            "empty model must be recorded as None on the SweepInfo entry"
        );
    }

    /// Issue #3716: an `effort` dispatch param threads through to the spawn
    /// command as an explicit `--effort <level>` argument, mirroring `--model`.
    #[test]
    #[serial]
    fn dispatch_with_effort_appends_effort_arg() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(45), None, None, Some("xhigh"), None)
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        assert!(
            wait_for_contents(&record_log, &needle, 10000),
            "fake spawn-claude.sh did not finish writing within 10s"
        );
        let recorded = std::fs::read_to_string(&record_log).unwrap();
        assert!(
            recorded.contains("argv: -p /loom:sweep 45 --effort xhigh"),
            "expected --effort in argv; got: {recorded}"
        );
        // The dispatch effort is carried on the registry entry so
        // list_sweeps / get_sweep_status report it (mirrors #3482 for model).
        assert_eq!(
            registry.get(&outcome.sweep_id).unwrap().effort.as_deref(),
            Some("xhigh"),
            "dispatch effort must be recorded on the SweepInfo entry"
        );
    }

    /// Issue #3716: `model` + `effort` both set emit both flags, in the
    /// order `--model <m> --effort <e>` (effort appended right after model).
    #[test]
    #[serial]
    fn dispatch_with_model_and_effort_appends_both_args() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(46), None, Some("claude-sonnet-4-6"), Some("xhigh"), None)
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        assert!(
            wait_for_contents(&record_log, &needle, 10000),
            "fake spawn-claude.sh did not finish writing within 10s"
        );
        let recorded = std::fs::read_to_string(&record_log).unwrap();
        assert!(
            recorded.contains("argv: -p /loom:sweep 46 --model claude-sonnet-4-6 --effort xhigh"),
            "expected --model then --effort in argv; got: {recorded}"
        );
        let entry = registry.get(&outcome.sweep_id).unwrap();
        assert_eq!(entry.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(entry.effort.as_deref(), Some("xhigh"));
    }

    /// Issue #3716: an empty-string effort is treated as unset — `--effort ""`
    /// must never be emitted (no flag at all, not an empty flag).
    #[test]
    #[serial]
    fn dispatch_with_empty_effort_emits_no_effort_flag() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(47), None, None, Some(""), None)
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        assert!(
            wait_for_contents(&record_log, &needle, 10000),
            "fake spawn-claude.sh did not finish writing within 10s"
        );
        let recorded = std::fs::read_to_string(&record_log).unwrap();
        assert!(
            !recorded.contains("--effort"),
            "empty effort must not emit --effort; got: {recorded}"
        );
        // Empty-string effort normalizes to None on the entry too.
        assert_eq!(
            registry.get(&outcome.sweep_id).unwrap().effort,
            None,
            "empty effort must be recorded as None on the SweepInfo entry"
        );
    }

    /// Issue #3729 (stacked-PR v1): a `depends_on` dispatch param threads
    /// through to the spawn command as an explicit `--depends-on <N>`
    /// argument, mirroring `--model` / `--effort`. It is recorded on the
    /// SweepInfo entry so the reaper can block the subtree on parent failure.
    #[test]
    #[serial]
    fn dispatch_with_depends_on_appends_depends_on_arg() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(50), None, None, None, Some(49))
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        assert!(
            wait_for_contents(&record_log, &needle, 10000),
            "fake spawn-claude.sh did not finish writing within 10s"
        );
        let recorded = std::fs::read_to_string(&record_log).unwrap();
        assert!(
            recorded.contains("argv: -p /loom:sweep 50 --depends-on 49"),
            "expected --depends-on in argv; got: {recorded}"
        );
        assert_eq!(
            registry.get(&outcome.sweep_id).unwrap().depends_on,
            Some(49),
            "dispatch depends_on must be recorded on the SweepInfo entry"
        );
    }

    /// Issue #3729: absent `depends_on`, no `--depends-on` flag is emitted —
    /// byte-for-byte unchanged behavior (opt-in, no default-path regression).
    #[test]
    #[serial]
    fn dispatch_without_depends_on_emits_no_flag() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(51), None, None, None, None)
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        assert!(
            wait_for_contents(&record_log, &needle, 10000),
            "fake spawn-claude.sh did not finish writing within 10s"
        );
        let recorded = std::fs::read_to_string(&record_log).unwrap();
        assert!(
            !recorded.contains("--depends-on"),
            "depends_on=None must not emit --depends-on; got: {recorded}"
        );
        assert_eq!(
            registry.get(&outcome.sweep_id).unwrap().depends_on,
            None,
            "depends_on=None must be recorded as None on the SweepInfo entry"
        );
    }

    /// Issue #3729 (v1 item 4, block-the-subtree): `block_children_of` emits a
    /// `sweep.issue.{child}.blocker` event for every live child whose
    /// `depends_on` names the given parent — and nothing for unrelated sweeps.
    #[tokio::test]
    async fn block_children_of_emits_blocker_for_dependents_only() {
        use crate::event_bus::EventBus;

        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        let bus = Arc::new(EventBus::new());
        registry.set_event_bus(bus.clone());
        let mut sub = bus.subscribe::<[&str; 0], &str>([]);

        // Parent #60, a stacked child #61 (depends_on=60), and an unrelated
        // independent sweep #62 (depends_on=None).
        for (sid, issue, dep) in [
            ("sweep-issue-60", 60u32, None),
            ("sweep-issue-61", 61u32, Some(60u32)),
            ("sweep-issue-62", 62u32, None),
        ] {
            registry.entries.insert(
                sid.to_string(),
                SweepInfo {
                    sweep_id: sid.to_string(),
                    kind: SweepKind::Issue(issue),
                    pid: 2_147_483_640,
                    token_name: "unknown".into(),
                    log_path: registry.compute_log_path(issue),
                    idempotency_key: None,
                    started_at: Utc::now(),
                    state: SweepState::Running,
                    latest_phase: None,
                    pr_number: None,
                    model: None,
                    effort: None,
                    depends_on: dep,
                },
            );
        }

        let blocked = registry.block_children_of(60, "parent #60 blocked");
        assert_eq!(blocked, vec![61], "only #61 depends on #60");

        // Exactly one blocker event, for issue 61 on its .blocker topic.
        let ev = sub.recv().await.unwrap();
        match ev {
            Event::SweepBlocker {
                issue, label_added, ..
            } => {
                assert_eq!(issue, 61);
                assert_eq!(label_added, "loom:blocked");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// Issue #3729: `children_of` only returns *live* direct children, and a
    /// terminal child is excluded (it no longer needs blocking).
    #[test]
    #[serial]
    fn children_of_returns_live_direct_children_only() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        fn mk(issue: u32, dep: Option<u32>, state: SweepState) -> SweepInfo {
            SweepInfo {
                sweep_id: format!("s{issue}"),
                kind: SweepKind::Issue(issue),
                pid: 2_147_483_640,
                token_name: "unknown".into(),
                log_path: PathBuf::from(format!(".loom/logs/sweep-issue-{issue}.log")),
                idempotency_key: None,
                started_at: Utc::now(),
                state,
                latest_phase: None,
                pr_number: None,
                model: None,
                effort: None,
                depends_on: dep,
            }
        }
        registry
            .entries
            .insert("s70".into(), mk(70, None, SweepState::Running));
        registry
            .entries
            .insert("s71".into(), mk(71, Some(70), SweepState::Running));
        // Terminal child — excluded.
        registry.entries.insert(
            "s72".into(),
            mk(
                72,
                Some(70),
                SweepState::Exited {
                    code: None,
                    at: Utc::now(),
                },
            ),
        );

        let mut kids = registry.children_of(70);
        kids.sort_unstable();
        assert_eq!(kids, vec![71], "only the live child #71 is returned");
    }

    /// Issue #3730: when the experiment-related env vars are set in the daemon
    /// process, `spawn_child` forwards them (via the explicit allowlist) to the
    /// detached child, and pins the child's cwd to the workspace root.
    #[test]
    #[serial]
    fn dispatch_forwards_experiment_env_and_sets_cwd() {
        let dir = tempdir().unwrap();
        // Canonicalize because the fixture records `pwd -P` (symlink-resolved),
        // while tempdir() on macOS lives under a /var -> /private/var symlink.
        let expected_cwd = std::fs::canonicalize(dir.path()).unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        // Export the experiment vars into the daemon (test) process env just
        // before dispatch — this is exactly the operator scenario #3730 fixes.
        std::env::set_var("LOOM_MODEL_EXPERIMENT", "canary");
        std::env::set_var("LOOM_MODEL_EXPERIMENT_CANARY", "1");
        std::env::set_var("LOOM_TRANSCRIPT_ARCHIVE", "/tmp/loom-archive-3730");

        let outcome = registry
            .dispatch(&SweepKind::Issue(48), None, None, None, None)
            .expect("dispatch should succeed");

        // Clean up the process env immediately so a failure below can't leak
        // into sibling #[serial] tests.
        std::env::remove_var("LOOM_MODEL_EXPERIMENT");
        std::env::remove_var("LOOM_MODEL_EXPERIMENT_CANARY");
        std::env::remove_var("LOOM_TRANSCRIPT_ARCHIVE");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        assert!(
            wait_for_contents(&record_log, &needle, 10000),
            "fake spawn-claude.sh did not finish writing within 10s"
        );
        let recorded = std::fs::read_to_string(&record_log).unwrap();
        assert!(
            recorded.contains("LOOM_MODEL_EXPERIMENT=canary"),
            "expected LOOM_MODEL_EXPERIMENT forwarded to child; got: {recorded}"
        );
        assert!(
            recorded.contains("LOOM_MODEL_EXPERIMENT_CANARY=1"),
            "expected LOOM_MODEL_EXPERIMENT_CANARY forwarded to child; got: {recorded}"
        );
        assert!(
            recorded.contains("LOOM_TRANSCRIPT_ARCHIVE=/tmp/loom-archive-3730"),
            "expected LOOM_TRANSCRIPT_ARCHIVE forwarded to child; got: {recorded}"
        );
        assert!(
            recorded.contains(&format!("PWD={}", expected_cwd.display())),
            "expected child cwd pinned to workspace root {}; got: {recorded}",
            expected_cwd.display()
        );
    }

    /// Issue #3730 no-op criterion: when none of the experiment env vars are
    /// set in the daemon process, `spawn_child` does NOT forward them to the
    /// child (the child observes them as unset). The cwd is still pinned to
    /// the workspace root regardless.
    #[test]
    #[serial]
    fn dispatch_does_not_forward_unset_experiment_env() {
        // Ensure a clean slate — a leaked value from another test would make
        // this a false pass.
        std::env::remove_var("LOOM_MODEL_EXPERIMENT");
        std::env::remove_var("LOOM_MODEL_EXPERIMENT_CANARY");
        std::env::remove_var("LOOM_TRANSCRIPT_ARCHIVE");

        let dir = tempdir().unwrap();
        let expected_cwd = std::fs::canonicalize(dir.path()).unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(49), None, None, None, None)
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        assert!(
            wait_for_contents(&record_log, &needle, 10000),
            "fake spawn-claude.sh did not finish writing within 10s"
        );
        let recorded = std::fs::read_to_string(&record_log).unwrap();
        // The fixture prints `<VAR>=unset` when the child sees the var unset.
        assert!(
            recorded.contains("LOOM_MODEL_EXPERIMENT=unset"),
            "unset LOOM_MODEL_EXPERIMENT must not be forwarded; got: {recorded}"
        );
        assert!(
            recorded.contains("LOOM_MODEL_EXPERIMENT_CANARY=unset"),
            "unset LOOM_MODEL_EXPERIMENT_CANARY must not be forwarded; got: {recorded}"
        );
        assert!(
            recorded.contains("LOOM_TRANSCRIPT_ARCHIVE=unset"),
            "unset LOOM_TRANSCRIPT_ARCHIVE must not be forwarded; got: {recorded}"
        );
        // cwd is pinned unconditionally.
        assert!(
            recorded.contains(&format!("PWD={}", expected_cwd.display())),
            "expected child cwd pinned to workspace root {}; got: {recorded}",
            expected_cwd.display()
        );
    }

    #[test]
    #[serial]
    fn dispatch_lock_collision_rejected() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        let first = registry.dispatch(&SweepKind::Issue(7), None, None, None, None);
        assert!(first.is_ok());

        let second = registry.dispatch(&SweepKind::Issue(7), None, None, None, None);
        assert!(second.is_err(), "second dispatch for issue #7 should fail (lock collision)");
        let err = second.unwrap_err().to_string();
        assert!(err.contains("lock collision"), "expected lock collision error; got: {err}");
    }

    #[test]
    #[serial]
    fn dispatch_idempotency_returns_existing() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        let first = registry
            .dispatch(&SweepKind::Issue(99), Some("key-A".to_string()), None, None, None)
            .unwrap();
        assert!(first.was_new);

        // While still Running, a dispatch with the same key must dedup.
        // Issue #99 is the same kind, but we don't need a different issue —
        // the dedup is purely on the idempotency key.
        let second = registry
            .dispatch(&SweepKind::Issue(99), Some("key-A".to_string()), None, None, None)
            .unwrap();
        assert!(!second.was_new);
        assert_eq!(first.sweep_id, second.sweep_id);
    }

    #[test]
    fn pr_set_dispatch_rejected_in_phase_a() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        let outcome = registry.dispatch(&SweepKind::PrSet(vec![1, 2, 3]), None, None, None, None);
        assert!(outcome.is_err());
        assert!(outcome
            .unwrap_err()
            .to_string()
            .contains("PrSet dispatch is reserved"));
    }

    #[test]
    fn list_sweeps_filters_by_state() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        // Dispatch and then poke an entry into Exited state directly.
        let outcome = registry
            .dispatch(&SweepKind::Issue(11), None, None, None, None)
            .unwrap();
        let entry = registry.entries.get_mut(&outcome.sweep_id).unwrap();
        entry.state = SweepState::Exited {
            code: Some(0),
            at: Utc::now(),
        };

        let running = registry.list(Some(&SweepState::Running));
        assert!(running.is_empty());

        let exited = registry.list(Some(&SweepState::Exited {
            code: None,
            at: Utc::now(),
        }));
        assert_eq!(exited.len(), 1);

        let all = registry.list(None);
        assert_eq!(all.len(), 1);
    }

    /// AC #2: reaper emits `sweep.issue.{N}.crashed` AND re-arms the
    /// `loom:building` -> `loom:issue` label when a dead pid has a
    /// checkpoint on disk. We don't actually invoke `gh` here (that's
    /// covered by integration tests with `skip_label_flip = false`); we
    /// assert the event payload and the registry state transition, which
    /// is the contract Phase B exposes to subscribers.
    #[tokio::test]
    async fn reaper_emits_crashed_event_with_checkpoint_phase() {
        use crate::event_bus::EventBus;

        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        let bus = Arc::new(EventBus::new());
        registry.set_event_bus(bus.clone());
        let mut sub = bus.subscribe::<[&str; 0], &str>([]);

        let cp_dir = registry.config.checkpoint_dir();
        std::fs::create_dir_all(&cp_dir).unwrap();
        std::fs::write(cp_dir.join("issue-55.json"), r#"{"phase":"doctor","issue":55}"#).unwrap();

        let sweep_id = "sweep-issue-55-test".to_string();
        registry.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(55),
                pid: 2_147_483_640,
                token_name: "unknown".into(),
                log_path: registry.compute_log_path(55),
                idempotency_key: None,
                started_at: Utc::now(),
                state: SweepState::Running,
                latest_phase: None,
                pr_number: None,
                model: None,
                effort: None,
                depends_on: None,
            },
        );

        let changed = registry.reap_once();
        assert!(changed >= 1);

        // Should observe: sweep.issue.55.crashed + sweep.global.completed
        let mut saw_crashed = false;
        let mut saw_completed = false;
        for _ in 0..2 {
            let ev = sub.recv().await.unwrap();
            match ev {
                Event::SweepCrashed {
                    issue,
                    checkpoint_phase,
                } => {
                    assert_eq!(issue, 55);
                    assert_eq!(checkpoint_phase.as_deref(), Some("doctor"));
                    saw_crashed = true;
                }
                Event::SweepGlobalCompleted {
                    sweep_id: sid,
                    outcome,
                } => {
                    assert_eq!(sid, sweep_id);
                    assert_eq!(outcome, SweepOutcome::Crashed);
                    saw_completed = true;
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert!(saw_crashed, "expected sweep.issue.55.crashed event");
        assert!(saw_completed, "expected sweep.global.completed event");

        // And the registry state should be Crashed (the label re-arm
        // side-effect is suppressed because skip_label_flip is true in
        // the fixture; the contract is the state transition + event
        // emission, which together signal the re-arm has happened in
        // production).
        let info = registry.get(&sweep_id).unwrap();
        assert!(matches!(info.state, SweepState::Crashed { .. }));
    }

    /// Clean-exit (no checkpoint) emits `sweep.issue.{N}.exited` plus
    /// `sweep.global.completed{outcome=Exited}`.
    #[tokio::test]
    async fn reaper_emits_exited_event_for_clean_exit() {
        use crate::event_bus::EventBus;

        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        let bus = Arc::new(EventBus::new());
        registry.set_event_bus(bus.clone());
        let mut sub = bus.subscribe::<[&str; 0], &str>([]);

        let sweep_id = "sweep-issue-66-test".to_string();
        registry.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(66),
                pid: 2_147_483_640,
                token_name: "unknown".into(),
                log_path: registry.compute_log_path(66),
                idempotency_key: None,
                started_at: Utc::now() - chrono::Duration::seconds(10),
                state: SweepState::Running,
                latest_phase: None,
                pr_number: None,
                model: None,
                effort: None,
                depends_on: None,
            },
        );

        let changed = registry.reap_once();
        assert!(changed >= 1);

        let mut saw_exited = false;
        let mut saw_completed = false;
        for _ in 0..2 {
            let ev = sub.recv().await.unwrap();
            match ev {
                Event::SweepExited {
                    issue,
                    duration_sec,
                    ..
                } => {
                    assert_eq!(issue, 66);
                    assert!(duration_sec >= 0);
                    saw_exited = true;
                }
                Event::SweepGlobalCompleted { outcome, .. } => {
                    assert_eq!(outcome, SweepOutcome::Exited);
                    saw_completed = true;
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert!(saw_exited);
        assert!(saw_completed);
    }

    #[test]
    fn reap_marks_dead_pid_exited_when_no_checkpoint() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        // Stuff an entry with a guaranteed-dead PID (very large pid_t).
        let sweep_id = "sweep-issue-21-test".to_string();
        registry.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(21),
                pid: 2_147_483_640, // ~i32::MAX, almost certainly dead
                token_name: "unknown".into(),
                log_path: registry.compute_log_path(21),
                idempotency_key: None,
                started_at: Utc::now(),
                state: SweepState::Running,
                latest_phase: None,
                pr_number: None,
                model: None,
                effort: None,
                depends_on: None,
            },
        );

        let changed = registry.reap_once();
        assert!(changed >= 1);
        let info = registry.get(&sweep_id).unwrap();
        assert!(matches!(info.state, SweepState::Exited { .. }));
    }

    /// Issue #3823b: orphaned-claim recovery. A daemon-owned sweep that exits
    /// cleanly with NO checkpoint (the self-skip / no-work case) must have its
    /// pre-dispatch loom:building claim restored to loom:issue by the reaper —
    /// otherwise the claim is orphaned and needs manual reclamation (the exact
    /// dogfood symptom). Point `gh_bin` at a fake recorder with the real label
    /// path enabled (`skip_label_flip = false`) and assert the restore fired.
    #[test]
    fn reap_restores_label_for_orphaned_clean_exit_without_pr() {
        let dir = tempdir().unwrap();
        let gh_log = dir.path().join("gh-invocations.log");
        // Fake gh: record the space-joined argv and exit 0.
        let fake_gh = dir.path().join("fake-gh.sh");
        let script = format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{}\"\nexit 0\n",
            gh_log.display()
        );
        std::fs::write(&fake_gh, &script).unwrap();
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();
        if let Ok(f) = std::fs::File::open(&fake_gh) {
            let _ = f.sync_all();
        }

        let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
        config.gh_bin = Some(fake_gh);
        config.skip_label_flip = false; // exercise the real restore path
        let mut registry = SweepRegistry::new(config);

        let sweep_id = "sweep-issue-77-test".to_string();
        registry.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(77),
                pid: 2_147_483_640, // ~i32::MAX, almost certainly dead
                token_name: "unknown".into(),
                log_path: registry.compute_log_path(77),
                idempotency_key: None,
                started_at: Utc::now(),
                state: SweepState::Running,
                latest_phase: None,
                pr_number: None, // no PR produced -> recoverable claim
                model: None,
                effort: None,
                depends_on: None,
            },
        );

        // No checkpoint file exists -> Exited branch -> orphaned-claim recovery.
        let changed = registry.reap_once();
        assert!(changed >= 1);

        let info = registry.get(&sweep_id).unwrap();
        assert!(matches!(info.state, SweepState::Exited { .. }));

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls.contains("issue edit 77 --remove-label loom:building --add-label loom:issue"),
            "expected reaper to restore loom:building -> loom:issue for an orphaned \
             clean exit without a PR; got gh invocations: {gh_calls:?}"
        );
    }

    /// Issue #3827: a cancelled daemon-owned Issue sweep that never opened a
    /// PR must have its pre-dispatch loom:building claim restored to loom:issue
    /// by `finish_cancel` — mirroring the reaper's clean-exit recovery (#3823b).
    /// Otherwise cancelling a daemon-owned sweep strands the issue in
    /// loom:building forever (the live repro: #3780/#3785).
    #[test]
    fn cancel_restores_label_when_no_pr_produced() {
        let dir = tempdir().unwrap();
        let gh_log = dir.path().join("gh-invocations.log");
        let fake_gh = dir.path().join("fake-gh.sh");
        let script = format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{}\"\nexit 0\n",
            gh_log.display()
        );
        std::fs::write(&fake_gh, &script).unwrap();
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();
        if let Ok(f) = std::fs::File::open(&fake_gh) {
            let _ = f.sync_all();
        }

        let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
        config.gh_bin = Some(fake_gh);
        config.skip_label_flip = false; // exercise the real restore path
        let mut registry = SweepRegistry::new(config);

        let kind = SweepKind::Issue(88);
        let started_at = Utc::now();
        let sweep_id = "sweep-issue-88-test".to_string();
        registry.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                sweep_id: sweep_id.clone(),
                kind: kind.clone(),
                pid: 2_147_483_640, // ~i32::MAX, almost certainly dead
                token_name: "unknown".into(),
                log_path: registry.compute_log_path(88),
                idempotency_key: None,
                started_at,
                state: SweepState::Running,
                latest_phase: None,
                pr_number: None, // no PR produced -> recoverable claim
                model: None,
                effort: None,
                depends_on: None,
            },
        );

        // exited_within_grace = true: no SIGKILL, straight to terminal path.
        let outcome = registry.finish_cancel(&sweep_id, 2_147_483_640, &kind, started_at, true);
        assert!(outcome.was_running);

        let info = registry.get(&sweep_id).unwrap();
        assert!(matches!(info.state, SweepState::Exited { .. }));

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls.contains("issue edit 88 --remove-label loom:building --add-label loom:issue"),
            "expected finish_cancel to restore loom:building -> loom:issue for a \
             cancelled sweep without a PR; got gh invocations: {gh_calls:?}"
        );
    }

    /// Issue #3827: a cancelled sweep that DID open a PR (`pr_number` set) must
    /// NOT have its label reset — that would yank loom:building out from under
    /// an in-flight PR's issue and undo real progress.
    #[test]
    fn cancel_does_not_restore_label_when_pr_produced() {
        let dir = tempdir().unwrap();
        let gh_log = dir.path().join("gh-invocations.log");
        let fake_gh = dir.path().join("fake-gh.sh");
        let script = format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{}\"\nexit 0\n",
            gh_log.display()
        );
        std::fs::write(&fake_gh, &script).unwrap();
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();
        if let Ok(f) = std::fs::File::open(&fake_gh) {
            let _ = f.sync_all();
        }

        let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
        config.gh_bin = Some(fake_gh);
        config.skip_label_flip = false; // real restore path enabled but must not fire
        let mut registry = SweepRegistry::new(config);

        let kind = SweepKind::Issue(99);
        let started_at = Utc::now();
        let sweep_id = "sweep-issue-99-test".to_string();
        registry.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                sweep_id: sweep_id.clone(),
                kind: kind.clone(),
                pid: 2_147_483_640,
                token_name: "unknown".into(),
                log_path: registry.compute_log_path(99),
                idempotency_key: None,
                started_at,
                state: SweepState::Running,
                latest_phase: None,
                pr_number: Some(456), // PR opened -> must NOT reset the label
                model: None,
                effort: None,
                depends_on: None,
            },
        );

        let outcome = registry.finish_cancel(&sweep_id, 2_147_483_640, &kind, started_at, true);
        assert!(outcome.was_running);

        let info = registry.get(&sweep_id).unwrap();
        assert!(matches!(info.state, SweepState::Exited { .. }));

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            !gh_calls.contains("--remove-label loom:building"),
            "expected finish_cancel to NOT restore the label when a PR was \
             produced; got gh invocations: {gh_calls:?}"
        );
    }

    /// Issue #3827: `SweepKind::PrSet` cancels must be unaffected — the
    /// `if let SweepKind::Issue` scoping already excludes them, so no
    /// `restore_label_to_ready` call is ever attempted.
    #[test]
    fn cancel_prset_does_not_restore_label() {
        let dir = tempdir().unwrap();
        let gh_log = dir.path().join("gh-invocations.log");
        let fake_gh = dir.path().join("fake-gh.sh");
        let script = format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{}\"\nexit 0\n",
            gh_log.display()
        );
        std::fs::write(&fake_gh, &script).unwrap();
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();
        if let Ok(f) = std::fs::File::open(&fake_gh) {
            let _ = f.sync_all();
        }

        let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
        config.gh_bin = Some(fake_gh);
        config.skip_label_flip = false;
        let mut registry = SweepRegistry::new(config);

        let kind = SweepKind::PrSet(vec![101, 102]);
        let started_at = Utc::now();
        let sweep_id = "sweep-prset-test".to_string();
        registry.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                sweep_id: sweep_id.clone(),
                kind: kind.clone(),
                pid: 2_147_483_640,
                token_name: "unknown".into(),
                log_path: registry.compute_log_path(0),
                idempotency_key: None,
                started_at,
                state: SweepState::Running,
                latest_phase: None,
                pr_number: None,
                model: None,
                effort: None,
                depends_on: None,
            },
        );

        let outcome = registry.finish_cancel(&sweep_id, 2_147_483_640, &kind, started_at, true);
        assert!(outcome.was_running);

        let info = registry.get(&sweep_id).unwrap();
        assert!(matches!(info.state, SweepState::Exited { .. }));

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            !gh_calls.contains("--remove-label loom:building"),
            "expected finish_cancel to NOT touch labels for a PrSet cancel; \
             got gh invocations: {gh_calls:?}"
        );
    }

    #[test]
    fn reap_marks_dead_pid_crashed_when_checkpoint_present() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        // Create a checkpoint file so the reaper picks Crashed over Exited.
        let cp_dir = registry.config.checkpoint_dir();
        std::fs::create_dir_all(&cp_dir).unwrap();
        std::fs::write(cp_dir.join("issue-33.json"), r#"{"phase":"builder","issue":33}"#).unwrap();

        let sweep_id = "sweep-issue-33-test".to_string();
        registry.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(33),
                pid: 2_147_483_640,
                token_name: "unknown".into(),
                log_path: registry.compute_log_path(33),
                idempotency_key: None,
                started_at: Utc::now(),
                state: SweepState::Running,
                latest_phase: None,
                pr_number: None,
                model: None,
                effort: None,
                depends_on: None,
            },
        );

        let changed = registry.reap_once();
        assert!(changed >= 1);
        let info = registry.get(&sweep_id).unwrap();
        assert!(matches!(info.state, SweepState::Crashed { .. }));
    }

    #[test]
    fn reconstruct_admits_live_lock_owners() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        // Write a lock dir with our own PID as the owner (guaranteed alive).
        let locks = registry.config.locks_dir();
        std::fs::create_dir_all(&locks).unwrap();
        let lock = locks.join("issue-77");
        std::fs::create_dir(&lock).unwrap();
        let owner = LockOwner {
            issue: 77,
            owner_pid: std::process::id(),
            acquired_at: Utc::now().to_rfc3339(),
            sweep_id: "sweep-issue-77-reconstruct".to_string(),
        };
        std::fs::write(lock.join("owner.json"), serde_json::to_string_pretty(&owner).unwrap())
            .unwrap();

        let admitted = registry.reconstruct().unwrap();
        assert!(admitted >= 1);
        let info = registry.get("sweep-issue-77-reconstruct").unwrap();
        assert_eq!(info.pid, std::process::id());
        assert!(matches!(info.state, SweepState::Running));
    }

    #[test]
    fn reconstruct_drops_stale_locks() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        let locks = registry.config.locks_dir();
        std::fs::create_dir_all(&locks).unwrap();
        let lock = locks.join("issue-78");
        std::fs::create_dir(&lock).unwrap();
        let owner = LockOwner {
            issue: 78,
            owner_pid: 2_147_483_640, // dead
            acquired_at: Utc::now().to_rfc3339(),
            sweep_id: "sweep-issue-78-stale".to_string(),
        };
        std::fs::write(lock.join("owner.json"), serde_json::to_string_pretty(&owner).unwrap())
            .unwrap();

        let _ = registry.reconstruct().unwrap();
        assert!(!lock.exists(), "stale lock should be removed");
        assert!(registry.get("sweep-issue-78-stale").is_none());
    }

    /// Issue #3808: a checkpoint with no corresponding daemon-owned lock is an
    /// in-session `/loom:sweep` run the daemon never dispatched. `reconstruct`
    /// must NOT synthesize a phantom `Crashed` entry for it. (Replaces the old
    /// `reconstruct_admits_orphan_checkpoints_as_crashed`, which locked in the
    /// pre-#3808 overly-broad behavior.)
    #[test]
    fn reconstruct_skips_in_session_checkpoints_without_lock() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        let cp_dir = registry.config.checkpoint_dir();
        std::fs::create_dir_all(&cp_dir).unwrap();
        // In-session checkpoint: no lock dir was ever written for it.
        std::fs::write(cp_dir.join("issue-91.json"), r#"{"phase":"judge","issue":91}"#).unwrap();

        let admitted = registry.reconstruct().unwrap();
        assert_eq!(admitted, 0, "in-session checkpoint must not be recovered");
        let crashed = registry.list(Some(&SweepState::Crashed { at: Utc::now() }));
        assert!(crashed.is_empty(), "no phantom Crashed entry for issue 91");
        assert!(registry.list(None).is_empty(), "registry must be empty");
    }

    /// Issue #3808: genuine daemon-crash recovery is preserved. A checkpoint
    /// whose issue had a daemon-owned lock with a now-dead owner PID (the
    /// daemon dispatched it, then crashed along with its child) IS recovered as
    /// a `Crashed` entry so the next dispatch resumes it.
    #[test]
    fn reconstruct_recovers_daemon_owned_checkpoint() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        // Daemon-owned lock with a dead owner PID (crashed daemon + child).
        let locks = registry.config.locks_dir();
        std::fs::create_dir_all(&locks).unwrap();
        let lock = locks.join("issue-91");
        std::fs::create_dir(&lock).unwrap();
        let owner = LockOwner {
            issue: 91,
            owner_pid: 2_147_483_640, // dead
            acquired_at: Utc::now().to_rfc3339(),
            sweep_id: "sweep-issue-91-daemon".to_string(),
        };
        std::fs::write(lock.join("owner.json"), serde_json::to_string_pretty(&owner).unwrap())
            .unwrap();

        // Matching checkpoint written by the (now-gone) daemon-dispatched child.
        let cp_dir = registry.config.checkpoint_dir();
        std::fs::create_dir_all(&cp_dir).unwrap();
        std::fs::write(cp_dir.join("issue-91.json"), r#"{"phase":"judge","issue":91}"#).unwrap();

        let admitted = registry.reconstruct().unwrap();
        assert!(admitted >= 1, "daemon-owned checkpoint must be recovered");
        let crashed = registry.list(Some(&SweepState::Crashed { at: Utc::now() }));
        assert_eq!(crashed.len(), 1);
        assert_eq!(crashed[0].latest_phase.as_deref(), Some("judge"));
        // The stale daemon lock is cleaned up as part of recovery.
        assert!(!lock.exists(), "stale daemon lock should be removed");
    }

    #[test]
    fn sweep_id_format() {
        let id = generate_sweep_id(&SweepKind::Issue(42));
        assert!(id.starts_with("sweep-issue-42-"));

        let pr = generate_sweep_id(&SweepKind::PrSet(vec![10, 20]));
        assert!(pr.starts_with("sweep-prs-10-20-"));
    }

    #[test]
    #[serial]
    fn reaper_interval_env_override() {
        // Serialized: this test mutates a process-wide env var.
        std::env::remove_var(REAPER_INTERVAL_ENV);
        let d = resolve_reaper_interval();
        assert_eq!(d.as_secs(), DEFAULT_REAPER_INTERVAL_SECS);

        std::env::set_var(REAPER_INTERVAL_ENV, "7");
        let d = resolve_reaper_interval();
        assert_eq!(d.as_secs(), 7);
        std::env::remove_var(REAPER_INTERVAL_ENV);
    }

    /// AC #3: assert that the spawned child receives a
    /// `CLAUDE_CODE_OAUTH_TOKEN` env var that came from `.loom/tokens/`.
    /// We achieve this with a fixture tokens dir and a fixture spawn-claude
    /// that selects from it. The real `spawn-claude.sh` would invoke the
    /// Python selector; here we substitute a thin shell that picks the
    /// first token file and exports it, so the test exercises the dispatch
    /// path end-to-end without depending on a working Python install.
    #[test]
    #[serial]
    fn dispatch_propagates_oauth_token_from_tokens_dir() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();

        // Build a fixture tokens dir with one token.
        let tokens_dir = workspace.join(".loom").join("tokens");
        std::fs::create_dir_all(&tokens_dir).unwrap();
        let token_value = "sk-ant-oat01-fixture-token-value";
        let token_path = tokens_dir.join("agent-1.token");
        std::fs::write(&token_path, token_value).unwrap();
        let mut perms = std::fs::metadata(&token_path).unwrap().permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&token_path, perms).unwrap();

        // Build a fake spawn-claude that selects the first token file and
        // records the exported CLAUDE_CODE_OAUTH_TOKEN. This is a stand-in
        // for the real wrapper's Python-backed selection — the assertion
        // is that the *registry's* dispatch path produces a child whose
        // OAuth token came from `.loom/tokens/`.
        let scripts_dir = workspace.join(".loom").join("scripts");
        std::fs::create_dir_all(&scripts_dir).unwrap();
        let fake_bin = scripts_dir.join("spawn-claude.sh");
        let record_log = workspace.join("oauth-record.log");
        let script = format!(
            r#"#!/usr/bin/env bash
set -euo pipefail
ws="${{LOOM_WORKSPACE:-{ws}}}"
tokens_dir="$ws/.loom/tokens"
token_file="$(ls "$tokens_dir"/*.token 2>/dev/null | head -n1)"
if [ -z "$token_file" ]; then
  echo "no token files in $tokens_dir" >&2
  exit 78
fi
export CLAUDE_CODE_OAUTH_TOKEN="$(cat "$token_file")"
{{
  echo "TOKEN_SOURCE=$token_file"
  echo "CLAUDE_CODE_OAUTH_TOKEN=$CLAUDE_CODE_OAUTH_TOKEN"
  echo "argv: $*"
}} >> "{rec}"
exit 0
"#,
            ws = workspace.display(),
            rec = record_log.display()
        );
        std::fs::write(&fake_bin, script).unwrap();
        let mut perms = std::fs::metadata(&fake_bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_bin, perms).unwrap();

        let mut config = SweepRegistryConfig::new(workspace.to_path_buf());
        config.spawn_bin = Some(fake_bin);
        config.skip_label_flip = true;
        let mut registry = SweepRegistry::new(config);

        let outcome = registry
            .dispatch(&SweepKind::Issue(123), None, None, None, None)
            .unwrap();
        assert!(outcome.was_new);

        let needle = format!("CLAUDE_CODE_OAUTH_TOKEN={token_value}");
        assert!(
            wait_for_contents(&record_log, &needle, 10000),
            "fake spawn did not record OAuth token within 10s; got: {}",
            std::fs::read_to_string(&record_log).unwrap_or_default()
        );
        let recorded = std::fs::read_to_string(&record_log).unwrap();
        assert!(
            recorded.contains(".loom/tokens/agent-1.token"),
            "expected TOKEN_SOURCE to point at .loom/tokens/; got: {recorded}"
        );
    }

    // ------------------------------------------------------------------------
    // Issue #3802: capture spawn-claude's selected account into token_name.
    // ------------------------------------------------------------------------

    #[test]
    fn parse_token_name_after_extracts_selected_account() {
        let log = "\
==== loom-daemon dispatch: 2026-07-23T00:00:00Z sweep_id=sweep-issue-3780-abc issue=3780 ====
\u{1b}[0;34m[2026-07-23T00:00:01Z]\u{1b}[0m spawn-claude: using OAuth account 'agent3-2amlogic' (mode=random)
";
        assert_eq!(
            parse_token_name_after(log, "sweep_id=sweep-issue-3780-abc").as_deref(),
            Some("agent3-2amlogic"),
        );
    }

    #[test]
    fn parse_token_name_after_ignores_stale_line_before_current_header() {
        // A previous dispatch's selection line, then this dispatch's header
        // with NO selection line yet: must NOT return the stale name.
        let log = "\
==== loom-daemon dispatch: old sweep_id=sweep-issue-3780-OLD issue=3780 ====
spawn-claude: using OAuth account 'stale-account' (mode=random)
==== loom-daemon dispatch: new sweep_id=sweep-issue-3780-NEW issue=3780 ====
";
        assert_eq!(parse_token_name_after(log, "sweep_id=sweep-issue-3780-NEW"), None,);
        // Once the current child logs, the current selection wins.
        let log2 =
            format!("{log}spawn-claude: using OAuth account 'fresh-account' (mode=ranking)\n");
        assert_eq!(
            parse_token_name_after(&log2, "sweep_id=sweep-issue-3780-NEW").as_deref(),
            Some("fresh-account"),
        );
    }

    #[test]
    fn parse_token_name_after_none_when_marker_absent_or_empty() {
        // No marker at all.
        assert_eq!(parse_token_name_after("no selection here", "sweep_id=x"), None);
        // Header present but no marker.
        assert_eq!(
            parse_token_name_after("sweep_id=x issue=1 ====\nsome other line\n", "sweep_id=x"),
            None,
        );
        // Empty account name is treated as "nothing to report".
        assert_eq!(
            parse_token_name_after("sweep_id=x using OAuth account '' (mode=random)", "sweep_id=x"),
            None,
        );
    }

    /// End-to-end: a dispatched sweep whose (fake) `spawn-claude.sh` logs the
    /// `using OAuth account '<name>'` line records that account as the registry
    /// `token_name` — reported by both `DispatchOutcome` and the stored
    /// `SweepInfo` (which `list_sweeps` / `get_sweep_status` read from). This
    /// closes the "always unknown" gap (issue #3802). Mirrors the live-dispatch
    /// finding: issue #3780 selected account `agent3-2amlogic`.
    #[test]
    #[serial]
    fn dispatch_captures_selected_account_into_token_name() {
        let dir = tempdir().unwrap();
        // A fake wrapper that logs the selection to stderr exactly as the real
        // spawn-claude.sh does, then lingers briefly (mimicking `exec claude`,
        // which keeps running long after the selection is logged). The daemon
        // already captures this stderr into the per-sweep log.
        let script = "#!/usr/bin/env bash\n\
set -euo pipefail\n\
echo \"spawn-claude: using OAuth account 'agent3-2amlogic' (mode=random)\" >&2\n\
sleep 0.5\n\
exit 0\n";
        let mut registry = lifecycle_registry(dir.path(), script);

        let outcome = registry
            .dispatch(&SweepKind::Issue(3780), None, None, None, None)
            .unwrap();
        assert!(outcome.was_new);
        assert_eq!(
            outcome.token_name, "agent3-2amlogic",
            "DispatchOutcome should carry the selected account, not 'unknown'"
        );

        let info = registry
            .get_status(&outcome.sweep_id)
            .expect("dispatched sweep should be in the registry");
        assert_eq!(
            info.token_name, "agent3-2amlogic",
            "stored SweepInfo (what list_sweeps/get_sweep_status report) should \
             carry the selected account"
        );
    }

    /// The `LOOM_SPAWN_NO_EXPORT` bypass path selects no account, so nothing is
    /// logged — `token_name` must remain `unknown` (not a regression, the
    /// expected "nothing to report" case). Verified here with a fixture that
    /// exits without logging a selection: the `try_wait` early-exit means this
    /// resolves promptly rather than waiting out the capture timeout.
    #[test]
    #[serial]
    fn dispatch_token_name_unknown_when_no_selection_logged() {
        let dir = tempdir().unwrap();
        let script = "#!/usr/bin/env bash\nset -euo pipefail\nexit 0\n";
        let mut registry = lifecycle_registry(dir.path(), script);

        let outcome = registry
            .dispatch(&SweepKind::Issue(4242), None, None, None, None)
            .unwrap();
        assert_eq!(
            outcome.token_name, UNKNOWN_TOKEN_NAME,
            "no selection logged => token_name stays 'unknown'"
        );
    }

    /// AC #4: snapshot the JSON shape produced by serializing
    /// `Vec<SweepInfo>`. If this shape changes in a future PR, this test
    /// will fail and force a deliberate update — pinning the schema.
    #[test]
    fn sweep_info_schema_snapshot() {
        let info = SweepInfo {
            sweep_id: "sweep-issue-42-1700000000".to_string(),
            kind: SweepKind::Issue(42),
            pid: 12_345,
            token_name: "agent-1.token".to_string(),
            log_path: PathBuf::from(".loom/logs/sweep-issue-42.log"),
            idempotency_key: Some("operator-key".to_string()),
            started_at: chrono::DateTime::parse_from_rfc3339("2026-06-05T10:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            state: SweepState::Running,
            latest_phase: Some("builder".to_string()),
            pr_number: Some(456),
            model: Some("claude-sonnet-4-6".to_string()),
            effort: Some("xhigh".to_string()),
            depends_on: None,
        };
        let json = serde_json::to_value(vec![info]).unwrap();
        let expected = serde_json::json!([{
            "sweep_id": "sweep-issue-42-1700000000",
            "kind": {"type": "Issue", "value": 42},
            "pid": 12_345,
            "token_name": "agent-1.token",
            "log_path": ".loom/logs/sweep-issue-42.log",
            "idempotency_key": "operator-key",
            "started_at": "2026-06-05T10:00:00Z",
            "state": {"state": "Running"},
            "latest_phase": "builder",
            "pr_number": 456,
            "model": "claude-sonnet-4-6",
            "effort": "xhigh",
        }]);
        assert_eq!(
            json, expected,
            "SweepInfo wire schema drifted — update the snapshot intentionally if this is desired"
        );

        // model=None is omitted from the wire (skip_serializing_if), and
        // pre-#3482 JSON without the field deserializes to model=None —
        // the backward-compat half of the schema pin.
        let legacy_json = serde_json::json!({
            "sweep_id": "sweep-issue-43-1700000000",
            "kind": {"type": "Issue", "value": 43},
            "pid": 1,
            "token_name": "unknown",
            "log_path": ".loom/logs/sweep-issue-43.log",
            "started_at": "2026-06-05T10:00:00Z",
            "state": {"state": "Running"},
        });
        let legacy: SweepInfo =
            serde_json::from_value(legacy_json).expect("legacy SweepInfo without model must parse");
        assert_eq!(legacy.model, None);
        // Pre-#3716 JSON also lacks the `effort` field — it must default to
        // None (#[serde(default)]) and be omitted on re-serialization
        // (skip_serializing_if).
        assert_eq!(legacy.effort, None);
        let reserialized = serde_json::to_value(&legacy).unwrap();
        assert!(
            reserialized.get("model").is_none(),
            "model=None must be omitted from serialized SweepInfo"
        );
        assert!(
            reserialized.get("effort").is_none(),
            "effort=None must be omitted from serialized SweepInfo"
        );

        // Also pin the variant shapes for Exited and Crashed.
        let exited = serde_json::to_value(SweepState::Exited {
            code: Some(0),
            at: chrono::DateTime::parse_from_rfc3339("2026-06-05T10:05:00Z")
                .unwrap()
                .with_timezone(&Utc),
        })
        .unwrap();
        assert_eq!(
            exited,
            serde_json::json!({
                "state": "Exited",
                "details": {"code": 0, "at": "2026-06-05T10:05:00Z"}
            })
        );

        let crashed = serde_json::to_value(SweepState::Crashed {
            at: chrono::DateTime::parse_from_rfc3339("2026-06-05T10:05:00Z")
                .unwrap()
                .with_timezone(&Utc),
        })
        .unwrap();
        assert_eq!(
            crashed,
            serde_json::json!({
                "state": "Crashed",
                "details": {"at": "2026-06-05T10:05:00Z"}
            })
        );
    }

    // ========================================================================
    // Phase C tests (Issue #3455)
    // ========================================================================

    #[test]
    fn get_status_returns_clone_or_none() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        assert!(registry.get_status("missing").is_none());

        let sweep_id = "sweep-status-test".to_string();
        registry.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(42),
                pid: 1234,
                token_name: "agent-1.token".into(),
                log_path: registry.compute_log_path(42),
                idempotency_key: None,
                started_at: Utc::now(),
                state: SweepState::Running,
                latest_phase: Some("builder".into()),
                pr_number: None,
                model: None,
                effort: None,
                depends_on: None,
            },
        );

        let info = registry.get_status(&sweep_id).expect("status should exist");
        assert_eq!(info.pid, 1234);
        assert!(matches!(info.kind, SweepKind::Issue(42)));
        assert!(matches!(info.state, SweepState::Running));
    }

    #[test]
    fn tail_log_returns_last_n_lines() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        let log_path = registry.compute_log_path(99);
        std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        let body = (1..=20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&log_path, body).unwrap();

        let sweep_id = "sweep-tail-test".to_string();
        registry.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(99),
                pid: 1,
                token_name: "unknown".into(),
                log_path: log_path.clone(),
                idempotency_key: None,
                started_at: Utc::now(),
                state: SweepState::Running,
                latest_phase: None,
                pr_number: None,
                model: None,
                effort: None,
                depends_on: None,
            },
        );

        let (path, tail) = registry.tail_log(&sweep_id, 5).unwrap();
        assert_eq!(path, log_path);
        assert_eq!(tail.len(), 5);
        assert_eq!(tail[0], "line 16");
        assert_eq!(tail[4], "line 20");

        // Requesting more lines than the file has should yield the whole file.
        let (_path, tail) = registry.tail_log(&sweep_id, 1000).unwrap();
        assert_eq!(tail.len(), 20);

        // Zero is honored (returns empty vec).
        let (_path, tail) = registry.tail_log(&sweep_id, 0).unwrap();
        assert!(tail.is_empty());
    }

    #[test]
    fn tail_log_rejects_unknown_sweep() {
        let dir = tempdir().unwrap();
        let (registry, _record_log) = fixture_registry(dir.path());
        let err = registry.tail_log("nope", 10).unwrap_err();
        assert!(err.to_string().contains("unknown sweep_id"));
    }

    #[test]
    fn cancel_unknown_sweep_returns_error() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        let err = registry
            .cancel("does-not-exist", Duration::from_millis(50))
            .unwrap_err();
        assert!(err.to_string().contains("unknown sweep_id"));
    }

    #[test]
    fn cancel_on_already_terminal_is_idempotent_noop() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        let sweep_id = "sweep-already-exited".to_string();
        registry.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(11),
                pid: 1,
                token_name: "unknown".into(),
                log_path: registry.compute_log_path(11),
                idempotency_key: None,
                started_at: Utc::now(),
                state: SweepState::Exited {
                    code: Some(0),
                    at: Utc::now(),
                },
                latest_phase: None,
                pr_number: None,
                model: None,
                effort: None,
                depends_on: None,
            },
        );

        let outcome = registry
            .cancel(&sweep_id, Duration::from_millis(50))
            .unwrap();
        assert!(!outcome.was_running);
        assert!(!outcome.sigkill_sent);
        // State should remain Exited (not flipped to Exited{None, now}).
        let info = registry.get(&sweep_id).unwrap();
        if let SweepState::Exited { code, .. } = &info.state {
            assert_eq!(*code, Some(0));
        } else {
            panic!("state should remain Exited");
        }
    }

    #[test]
    fn cancel_dead_pid_transitions_to_exited_without_sigkill() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        let sweep_id = "sweep-dead-pid".to_string();
        registry.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(22),
                pid: 2_147_483_640, // ~i32::MAX, almost certainly dead
                token_name: "unknown".into(),
                log_path: registry.compute_log_path(22),
                idempotency_key: None,
                started_at: Utc::now(),
                state: SweepState::Running,
                latest_phase: None,
                pr_number: None,
                model: None,
                effort: None,
                depends_on: None,
            },
        );

        let outcome = registry
            .cancel(&sweep_id, Duration::from_millis(200))
            .unwrap();
        assert!(outcome.was_running);
        // SIGTERM to a dead pid is a no-op success; the poll loop sees
        // pid dead immediately and never escalates to SIGKILL.
        assert!(!outcome.sigkill_sent);
        let info = registry.get(&sweep_id).unwrap();
        assert!(matches!(info.state, SweepState::Exited { .. }));
    }

    /// AC #3: SIGTERM -> grace -> SIGKILL against a fixture child that
    /// ignores SIGTERM. Spawns `bash -c 'trap "" TERM; sleep 5'`, asks
    /// the registry to cancel with a short grace, and asserts that the
    /// registry transitioned + sigkill_sent=true. We then `wait()` on the
    /// `Child` handle to reap the zombie before asserting liveness.
    #[test]
    fn cancel_escalates_to_sigkill_when_child_ignores_sigterm() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        // Spawn a real child that traps SIGTERM and sleeps for 30s.
        // We need a real PID so SIGTERM/SIGKILL paths are exercised end
        // to end. We keep the Child handle so we can `wait()` after the
        // cancel — without that, SIGKILL leaves the child as a zombie
        // and `kill(pid, 0)` still returns success (the PID is still in
        // the process table).
        let mut child = Command::new("bash")
            .arg("-c")
            .arg("trap '' TERM; sleep 30")
            .spawn()
            .expect("spawn fixture child");
        let pid = child.id();

        // Give bash a moment to install the trap before we try to TERM it.
        std::thread::sleep(Duration::from_millis(100));

        let sweep_id = "sweep-trap-term".to_string();
        registry.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(77),
                pid,
                token_name: "unknown".into(),
                log_path: registry.compute_log_path(77),
                idempotency_key: None,
                started_at: Utc::now(),
                state: SweepState::Running,
                latest_phase: None,
                pr_number: None,
                model: None,
                effort: None,
                depends_on: None,
            },
        );

        // Use a short grace — long enough for SIGTERM to be delivered to
        // a healthy bash (~200ms), short enough to keep the test fast.
        let outcome = registry
            .cancel(&sweep_id, Duration::from_millis(500))
            .expect("cancel should succeed");
        assert!(outcome.was_running);
        assert!(
            outcome.sigkill_sent,
            "trap '' TERM child should have survived SIGTERM and escalated to SIGKILL"
        );

        // Reap the zombie so the PID is truly gone from the process table.
        let exit_status = child.wait().expect("wait on cancelled child");
        // Exit status: killed by SIGKILL means no clean exit code on Unix;
        // `success()` should be false. We don't assert specifics — the
        // platform's signal-vs-exit-code reporting varies.
        assert!(!exit_status.success(), "child should not have exited cleanly after SIGKILL");

        let info = registry.get(&sweep_id).unwrap();
        assert!(matches!(info.state, SweepState::Exited { .. }));
    }

    /// Issue #3807 core AC: the SIGTERM → grace-poll → SIGKILL escalation must
    /// NOT hold the registry lock for the full grace window. We drive the split
    /// `begin_cancel` → `poll_cancel` (unlocked sleeps between polls) →
    /// `finish_cancel` orchestration on one thread against a real trap-TERM
    /// child (forced to run the FULL grace before escalating), and assert a
    /// concurrent `get_status` on a DIFFERENT sweep returns PROMPTLY — well
    /// under the grace window — rather than blocking for it. With the old
    /// `cancel(&mut self)` (lock held throughout) the concurrent read would
    /// block for the entire grace.
    #[test]
    fn split_cancel_does_not_hold_lock_across_grace_window() {
        use std::sync::{Arc, Mutex};
        use std::thread;

        let dir = tempdir().unwrap();
        let (registry, _record_log) = fixture_registry(dir.path());
        let registry = Arc::new(Mutex::new(registry));

        // A real child that traps (ignores) SIGTERM and sleeps, so the cancel
        // is forced to poll for the full grace before escalating to SIGKILL.
        let mut child = Command::new("bash")
            .arg("-c")
            .arg("trap '' TERM; sleep 30")
            .spawn()
            .expect("spawn fixture child");
        let target_pid = child.id();
        // Give bash a moment to install the trap before we TERM it.
        thread::sleep(Duration::from_millis(100));

        let target = "sweep-cancel-target".to_string();
        let other = "sweep-concurrent-reader".to_string();
        {
            let mut reg = registry.lock().unwrap();
            let target_log = reg.compute_log_path(880);
            let other_log = reg.compute_log_path(881);
            reg.entries.insert(
                target.clone(),
                SweepInfo {
                    sweep_id: target.clone(),
                    kind: SweepKind::Issue(880),
                    pid: target_pid,
                    token_name: "unknown".into(),
                    log_path: target_log,
                    idempotency_key: None,
                    started_at: Utc::now(),
                    state: SweepState::Running,
                    latest_phase: None,
                    pr_number: None,
                    model: None,
                    effort: None,
                    depends_on: None,
                },
            );
            reg.entries.insert(
                other.clone(),
                SweepInfo {
                    sweep_id: other.clone(),
                    kind: SweepKind::Issue(881),
                    pid: 2_147_483_640, // ~i32::MAX, harmless dead pid
                    token_name: "unknown".into(),
                    log_path: other_log,
                    idempotency_key: None,
                    started_at: Utc::now(),
                    state: SweepState::Running,
                    latest_phase: None,
                    pr_number: None,
                    model: None,
                    effort: None,
                    depends_on: None,
                },
            );
        }

        // 1s grace: long enough that a lock held throughout would clearly
        // block the concurrent read for ~1s, short enough to keep the test fast.
        let grace = Duration::from_millis(1000);

        // Thread A: run the split orchestration (mirrors the IPC handler),
        // releasing the mutex between the 100ms poll sleeps.
        let reg_a = Arc::clone(&registry);
        let target_a = target.clone();
        let canceller = thread::spawn(move || {
            let (pid, kind, started_at) =
                match reg_a.lock().unwrap().begin_cancel(&target_a).unwrap() {
                    BeginCancel::Signalled {
                        pid,
                        kind,
                        started_at,
                    } => (pid, kind, started_at),
                    BeginCancel::AlreadyTerminal(_) => panic!("target should be running"),
                };
            let deadline = std::time::Instant::now() + grace;
            let mut exited = reg_a.lock().unwrap().poll_cancel(&target_a, pid);
            while !exited && std::time::Instant::now() < deadline {
                thread::sleep(Duration::from_millis(100));
                exited = reg_a.lock().unwrap().poll_cancel(&target_a, pid);
            }
            reg_a
                .lock()
                .unwrap()
                .finish_cancel(&target_a, pid, &kind, started_at, exited)
        });

        // Let thread A send SIGTERM and enter the (unlocked) poll loop.
        thread::sleep(Duration::from_millis(150));

        // Concurrent read on the OTHER sweep: must return well under the grace
        // window because the poll loop releases the mutex between polls.
        let start = std::time::Instant::now();
        let info = registry.lock().unwrap().get_status(&other);
        let elapsed = start.elapsed();
        assert!(info.is_some(), "other sweep should still be queryable");
        assert!(
            elapsed < Duration::from_millis(400),
            "concurrent get_status blocked for {elapsed:?} — the registry mutex \
             was held across the grace window (grace was {grace:?})"
        );

        let outcome = canceller.join().expect("cancel thread panicked");
        assert!(outcome.was_running);
        assert!(
            outcome.sigkill_sent,
            "trap-TERM child should have survived SIGTERM and escalated to SIGKILL"
        );

        // Reap the zombie so the PID leaves the process table.
        let exit_status = child.wait().expect("wait on cancelled child");
        assert!(!exit_status.success(), "child should not have exited cleanly after SIGKILL");

        let final_state = registry.lock().unwrap().get(&target).unwrap().state.clone();
        assert!(matches!(final_state, SweepState::Exited { .. }));
    }

    #[test]
    fn cancel_emits_exited_and_completed_events() {
        // Bus emission path: cancel a dead-pid sweep and confirm we
        // see sweep.issue.{N}.exited + sweep.global.completed.
        use crate::event_bus::EventBus;

        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        let bus = Arc::new(EventBus::new());
        registry.set_event_bus(bus.clone());
        let mut sub = bus.subscribe::<[&str; 0], &str>([]);

        let sweep_id = "sweep-cancel-event".to_string();
        registry.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(88),
                pid: 2_147_483_640,
                token_name: "unknown".into(),
                log_path: registry.compute_log_path(88),
                idempotency_key: None,
                started_at: Utc::now(),
                state: SweepState::Running,
                latest_phase: None,
                pr_number: None,
                model: None,
                effort: None,
                depends_on: None,
            },
        );

        registry
            .cancel(&sweep_id, Duration::from_millis(100))
            .unwrap();

        // Drain two events synchronously (cancel emits inline).
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut saw_exited = false;
            let mut saw_completed = false;
            for _ in 0..2 {
                match sub.recv().await.unwrap() {
                    Event::SweepExited { issue, .. } => {
                        assert_eq!(issue, 88);
                        saw_exited = true;
                    }
                    Event::SweepGlobalCompleted { outcome, .. } => {
                        assert_eq!(outcome, SweepOutcome::Exited);
                        saw_completed = true;
                    }
                    other => panic!("unexpected event: {other:?}"),
                }
            }
            assert!(saw_exited);
            assert!(saw_completed);
        });
    }

    /// Issue #3800: `cancel()` must tear down the WHOLE process tree, not just
    /// the tracked leader PID. We dispatch a fixture whose leader forks a
    /// backgrounded grandchild (both in the leader's process group, thanks to
    /// `dispatch()`'s `process_group(0)`), then cancel and assert BOTH the
    /// leader and the grandchild are gone within the grace window. A
    /// single-PID kill would orphan the backgrounded grandchild — this test
    /// fails without the group-kill fix.
    #[test]
    #[serial]
    fn cancel_terminates_whole_process_group_including_grandchild() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let gc_pidfile = workspace.join("grandchild.pid");

        // Leader (= group leader after process_group(0)) forks a background
        // grandchild that sleeps, records its PID, then blocks in a foreground
        // sleep. All three processes share the leader's process group.
        let script = format!(
            "#!/usr/bin/env bash\nsleep 300 &\necho \"$!\" > \"{gc}\"\nsleep 300\n",
            gc = gc_pidfile.display()
        );
        let mut registry = lifecycle_registry(workspace, &script);

        let outcome = registry
            .dispatch(&SweepKind::Issue(4242), None, None, None, None)
            .expect("dispatch should succeed");
        let leader_pid = outcome.pid;
        let sweep_id = outcome.sweep_id.clone();

        let gc_pid = read_pid_file(&gc_pidfile, 5000).expect("grandchild pid should be recorded");
        assert!(is_pid_alive(leader_pid), "leader should be running post-dispatch");
        assert!(is_pid_alive(gc_pid), "grandchild should be running post-dispatch");
        assert_ne!(leader_pid, gc_pid);

        // None of the processes trap SIGTERM, so a group SIGTERM tears the
        // whole tree down inside the grace window (no SIGKILL escalation).
        let cancel = registry
            .cancel(&sweep_id, Duration::from_secs(3))
            .expect("cancel should succeed");
        assert!(cancel.was_running);

        // The ENTIRE tree must be gone. The grandchild assertion is the crux:
        // it proves the signal reached the whole process group (#3800), not
        // just the tracked leader PID.
        assert!(wait_until_dead(leader_pid, 3000), "leader still alive after cancel");
        assert!(
            wait_until_dead(gc_pid, 3000),
            "grandchild survived cancel — group-kill did not reach it (single-PID regression)"
        );

        let info = registry.get(&sweep_id).unwrap();
        assert!(matches!(info.state, SweepState::Exited { .. }));
    }

    /// Issue #3801: a child killed OUT OF BAND (operator `kill -KILL`, not via
    /// `cancel()`) must be reaped by the reaper — no `<defunct>` zombie — and
    /// the registry entry must transition out of `Running`. Without the
    /// retained-`Child`-handle `try_wait()`, the killed leader becomes a
    /// zombie whose `kill(pid, 0)` still reports alive, so `reap_once()` would
    /// leave the entry stuck `Running` forever.
    #[test]
    #[serial]
    fn reaper_reaps_out_of_band_killed_child_and_transitions_state() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();

        let mut registry = lifecycle_registry(workspace, "#!/usr/bin/env bash\nsleep 300\n");

        let outcome = registry
            .dispatch(&SweepKind::Issue(5151), None, None, None, None)
            .expect("dispatch should succeed");
        let pid = outcome.pid;
        let sweep_id = outcome.sweep_id.clone();

        // Let the child start.
        assert!(wait_until_alive(pid, 3000), "child should have started");
        assert!(matches!(registry.get(&sweep_id).unwrap().state, SweepState::Running));

        // Kill out of band: SIGKILL the leader PID directly (mimics an
        // operator `kill -KILL <pid>`), bypassing cancel(). The leader is now
        // a zombie under the daemon (test) PID until we wait() it.
        assert!(send_signal(pid, 9), "SIGKILL to live child should succeed");

        // Drive reaper ticks. The retained handle's try_wait() reaps the
        // zombie and observes the exit, transitioning the entry to terminal.
        let mut transitioned = false;
        for _ in 0..80 {
            registry.reap_once();
            match registry.get(&sweep_id).map(|i| i.state.clone()) {
                Some(SweepState::Running | SweepState::Pending) => {}
                _ => {
                    transitioned = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            transitioned,
            "reaper did not transition the out-of-band-killed sweep out of Running"
        );

        // No zombie: because try_wait() reaped the child, kill(pid, 0) now
        // fails (the PID is no longer in the process table).
        assert!(
            wait_until_dead(pid, 2000),
            "killed child left a <defunct> zombie — reaper did not wait() it"
        );
    }

    /// Issue #3893: a read path (`reap_liveness`, wired into `ListSweeps` /
    /// `GetSweepStatus` / the work-finder occupancy seed) must transition a
    /// sweep whose child has already exited out of `Running` promptly —
    /// bounded to seconds — WITHOUT waiting for the 30s reaper timer. This is
    /// the regression that made `list_sweeps` over-report active work across a
    /// burst of merges.
    #[test]
    #[serial]
    fn read_path_reaps_exited_child_out_of_running_promptly() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();

        // A fake spawn that exits immediately: mirrors a sweep whose lifecycle
        // has completed (PR merged) and whose process has already exited.
        let mut registry = lifecycle_registry(workspace, "#!/usr/bin/env bash\nexit 0\n");

        let outcome = registry
            .dispatch(&SweepKind::Issue(4242), None, None, None, None)
            .expect("dispatch should succeed");
        let sweep_id = outcome.sweep_id.clone();

        // Reap-on-read reconciles liveness via the retained handle's
        // `try_wait()`. Bound the loop to ~2s to prove "prompt" — a healthy
        // implementation transitions on the first reconcile once the child has
        // exited (`try_wait` reaps the zombie and yields the exit status).
        let mut still_running = true;
        for _ in 0..80 {
            registry.reap_liveness();
            let running = registry.list(Some(&SweepState::Running));
            if !running.iter().any(|i| i.sweep_id == sweep_id) {
                still_running = false;
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            !still_running,
            "exited child still reported Running after a read-path reconcile (#3893)"
        );
        assert!(
            matches!(registry.get(&sweep_id).unwrap().state, SweepState::Exited { .. }),
            "exited child should have transitioned to terminal Exited state"
        );
        // And it should no longer count as in-flight for occupancy accounting.
        assert!(
            registry.list(Some(&SweepState::Running)).is_empty(),
            "no sweep should remain Running after the exited child was reaped"
        );
    }

    // ===================================================================
    // Startup-race mitigation: stagger + watchdog (Issue #3887)
    // ===================================================================

    // --- stagger_wait pure function ---

    #[test]
    fn stagger_wait_zero_stagger_never_waits() {
        let now = Instant::now();
        assert_eq!(stagger_wait(None, Duration::ZERO, now), Duration::ZERO);
        assert_eq!(
            stagger_wait(Some(now), Duration::ZERO, now + Duration::from_secs(1)),
            Duration::ZERO
        );
    }

    #[test]
    fn stagger_wait_no_prior_spawn_never_waits() {
        let now = Instant::now();
        assert_eq!(stagger_wait(None, Duration::from_secs(2), now), Duration::ZERO);
    }

    #[test]
    fn stagger_wait_returns_remaining_gap() {
        let base = Instant::now();
        let stagger = Duration::from_millis(2000);
        // 500ms elapsed since the last spawn ⇒ 1500ms still to wait.
        let now = base + Duration::from_millis(500);
        assert_eq!(stagger_wait(Some(base), stagger, now), Duration::from_millis(1500));
    }

    #[test]
    fn stagger_wait_elapsed_past_stagger_is_zero() {
        let base = Instant::now();
        let stagger = Duration::from_millis(2000);
        // 3s elapsed ⇒ the full gap has passed, no wait.
        let now = base + Duration::from_millis(3000);
        assert_eq!(stagger_wait(Some(base), stagger, now), Duration::ZERO);
    }

    // --- watchdog_decision state machine ---

    #[test]
    fn watchdog_decision_progress_is_always_healthy() {
        // Progress observed ⇒ Healthy regardless of elapsed / retried.
        let t = Duration::from_secs(120);
        assert_eq!(
            watchdog_decision(Duration::from_secs(9999), t, true, false),
            WatchdogDecision::Healthy
        );
        assert_eq!(
            watchdog_decision(Duration::from_secs(9999), t, true, true),
            WatchdogDecision::Healthy
        );
    }

    #[test]
    fn watchdog_decision_within_timeout_is_healthy() {
        let t = Duration::from_secs(120);
        assert_eq!(
            watchdog_decision(Duration::from_secs(119), t, false, false),
            WatchdogDecision::Healthy
        );
    }

    #[test]
    fn watchdog_decision_hung_first_time_restarts() {
        let t = Duration::from_secs(120);
        assert_eq!(
            watchdog_decision(Duration::from_secs(121), t, false, false),
            WatchdogDecision::Restart
        );
    }

    #[test]
    fn watchdog_decision_hung_after_retry_gives_up() {
        // Bounded: a second hang past the timeout does not restart again.
        let t = Duration::from_secs(120);
        assert_eq!(
            watchdog_decision(Duration::from_secs(500), t, false, true),
            WatchdogDecision::GiveUp
        );
    }

    // --- log_has_progress probe ---

    #[test]
    fn log_has_progress_false_for_header_and_wrapper_only() {
        // The hung case: only the daemon header + spawn-claude wrapper lines.
        let log = "\n==== loom-daemon dispatch: 2026-07-23T00:00:00Z sweep_id=sweep-issue-1-1 issue=1 ====\n\
                   [2026-07-23T00:00:01Z] spawn-claude: model=default\n\
                   [2026-07-23T00:00:01Z] spawn-claude: using OAuth account 'agent-2' (mode=ranked)\n";
        assert!(!log_has_progress(log));
    }

    #[test]
    fn log_has_progress_true_when_claude_emits_output() {
        let log = "\n==== loom-daemon dispatch: 2026-07-23T00:00:00Z sweep_id=sweep-issue-1-1 issue=1 ====\n\
                   [2026-07-23T00:00:01Z] spawn-claude: using OAuth account 'agent-2' (mode=ranked)\n\
                   Stage 0: resolving backend...\n";
        assert!(log_has_progress(log));
    }

    #[test]
    fn log_has_progress_anchors_to_last_dispatch() {
        // A prior run produced output; the CURRENT dispatch (after the last
        // header) has none ⇒ no progress for this dispatch.
        let log = "==== loom-daemon dispatch: t1 sweep_id=a issue=1 ====\n\
                   Curator done\n\
                   ==== loom-daemon dispatch: t2 sweep_id=b issue=1 ====\n\
                   [ts] spawn-claude: using OAuth account 'x' (mode=random)\n";
        assert!(!log_has_progress(log));
    }

    #[test]
    fn log_has_progress_empty_log_is_false() {
        assert!(!log_has_progress(""));
    }

    // --- sweep_made_progress: filesystem probes ---

    #[test]
    fn sweep_made_progress_worktree_and_checkpoint_and_log() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        let (reg, _rec) = fixture_registry(ws);
        let log = ws.join("sweep.log");

        // Nothing yet ⇒ no progress.
        std::fs::write(&log, "==== loom-daemon dispatch: t sweep_id=s issue=7 ====\n[ts] spawn-claude: using OAuth account 'x' (mode=random)\n").unwrap();
        assert!(!reg.sweep_made_progress(7, &log));

        // A worktree ⇒ progress.
        let wt = ws.join(".loom").join("worktrees").join("issue-7");
        std::fs::create_dir_all(&wt).unwrap();
        assert!(reg.sweep_made_progress(7, &log));
        std::fs::remove_dir_all(&wt).unwrap();
        assert!(!reg.sweep_made_progress(7, &log));

        // A checkpoint ⇒ progress.
        let cp_dir = ws.join(".loom").join("sweep-checkpoint");
        std::fs::create_dir_all(&cp_dir).unwrap();
        std::fs::write(cp_dir.join("issue-7.json"), "{}").unwrap();
        assert!(reg.sweep_made_progress(7, &log));
        std::fs::remove_file(cp_dir.join("issue-7.json")).unwrap();
        assert!(!reg.sweep_made_progress(7, &log));

        // Log output past the header ⇒ progress.
        std::fs::write(
            &log,
            "==== loom-daemon dispatch: t sweep_id=s issue=7 ====\nBuilder: writing code\n",
        )
        .unwrap();
        assert!(reg.sweep_made_progress(7, &log));
    }

    // --- set/get dispatch stagger ---

    #[test]
    fn dispatch_stagger_setter_roundtrips() {
        let tmp = tempdir().unwrap();
        let (mut reg, _rec) = fixture_registry(tmp.path());
        assert_eq!(reg.dispatch_stagger(), Duration::ZERO, "default is zero");
        reg.set_dispatch_stagger(Duration::from_millis(1500));
        assert_eq!(reg.dispatch_stagger(), Duration::from_millis(1500));
    }

    #[test]
    fn dispatch_applies_configured_stagger_between_spawns() {
        // With a small stagger, two back-to-back dispatches are spaced by at
        // least the stagger (the second waits out the gap in `dispatch`).
        let tmp = tempdir().unwrap();
        let (mut reg, rec) = fixture_registry(tmp.path());
        reg.set_dispatch_stagger(Duration::from_millis(400));

        let start = Instant::now();
        reg.dispatch(&SweepKind::Issue(8001), None, None, None, None)
            .unwrap();
        reg.dispatch(&SweepKind::Issue(8002), None, None, None, None)
            .unwrap();
        let elapsed = start.elapsed();

        assert!(
            elapsed >= Duration::from_millis(400),
            "second dispatch should have waited out the stagger; elapsed={elapsed:?}"
        );
        // Both fake children ran.
        assert!(wait_for_contents(&rec, "issue=8002", 5000) || rec.exists());
    }

    // --- watchdog_once: bounded auto-restart end-to-end ---

    /// A hung-child fixture: emits the account-selection line quickly (so
    /// token-name capture returns fast) then sleeps, producing NO progress
    /// (no worktree/checkpoint, log stuck at the spawn header).
    fn hung_child_registry(ws: &Path) -> SweepRegistry {
        let body = "#!/usr/bin/env bash\n\
                    echo \"spawn-claude: using OAuth account 'faketok' (mode=random)\"\n\
                    sleep 30\n";
        lifecycle_registry(ws, body)
    }

    /// Backdate a running entry's `started_at` so the watchdog sees it as past
    /// the no-progress timeout.
    fn backdate(reg: &mut SweepRegistry, sweep_id: &str, secs: i64) {
        if let Some(info) = reg.entries.get_mut(sweep_id) {
            info.started_at = Utc::now() - chrono::Duration::seconds(secs);
        }
    }

    fn running_issue_sweep_id(reg: &SweepRegistry, issue: u32) -> Option<String> {
        reg.entries
            .values()
            .find(|i| {
                matches!(i.state, SweepState::Running | SweepState::Pending)
                    && matches!(i.kind, SweepKind::Issue(n) if n == issue)
            })
            .map(|i| i.sweep_id.clone())
    }

    #[test]
    fn watchdog_restarts_hung_sweep_once_then_gives_up() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        let mut reg = hung_child_registry(ws);

        // 1. Dispatch a hung sweep for issue 4242.
        let out = reg
            .dispatch(&SweepKind::Issue(4242), None, None, None, None)
            .unwrap();
        assert!(wait_until_alive(out.pid, 5000), "hung fixture child should start");
        let first_id = out.sweep_id.clone();

        // 2. Healthy while inside the timeout window.
        assert_eq!(
            reg.watchdog_once(Duration::from_secs(120)),
            0,
            "a fresh sweep is not disturbed"
        );

        // 3. Backdate so it looks hung, then run the watchdog.
        backdate(&mut reg, &first_id, 600);
        let restarts = reg.watchdog_once(Duration::from_secs(60));
        assert_eq!(restarts, 1, "the hung sweep is auto-restarted once");
        assert!(reg.watchdog_retried.contains(&4242), "issue marked retried (bounded)");

        // A fresh Running sweep now exists for the issue (the re-dispatch).
        // Note: `generate_sweep_id` is second-granular, so within this fast
        // test the re-dispatched id may coincide with the original — in
        // production the watchdog fires ≥120s later, so ids differ. Either way,
        // the registry holds exactly one Running entry for the issue again.
        let _ = first_id;
        let second_id =
            running_issue_sweep_id(&reg, 4242).expect("a fresh sweep was re-dispatched");

        // 4. Backdate the NEW sweep too; the watchdog must NOT restart again
        //    (bounded) — it gives up instead.
        backdate(&mut reg, &second_id, 600);
        let restarts2 = reg.watchdog_once(Duration::from_secs(60));
        assert_eq!(restarts2, 0, "bounded: never a second auto-restart");
        assert!(reg.watchdog_gaveup.contains(&4242), "give-up recorded for the issue");
        // The second sweep is still running (left for the operator).
        assert!(running_issue_sweep_id(&reg, 4242).is_some());

        // Cleanup: cancel the lingering hung child.
        if let Some(id) = running_issue_sweep_id(&reg, 4242) {
            let _ = reg.cancel(&id, Duration::from_secs(2));
        }
    }

    #[test]
    fn watchdog_leaves_progressing_sweep_alone() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        let mut reg = hung_child_registry(ws);

        let out = reg
            .dispatch(&SweepKind::Issue(4343), None, None, None, None)
            .unwrap();
        assert!(wait_until_alive(out.pid, 5000));

        // Simulate progress: create a worktree for the issue.
        let wt = ws.join(".loom").join("worktrees").join("issue-4343");
        std::fs::create_dir_all(&wt).unwrap();

        // Even backdated well past the timeout, an issue with a worktree is
        // never restarted.
        backdate(&mut reg, &out.sweep_id, 9999);
        assert_eq!(reg.watchdog_once(Duration::from_secs(10)), 0);
        assert!(!reg.watchdog_retried.contains(&4343));

        // Cleanup.
        let _ = reg.cancel(&out.sweep_id, Duration::from_secs(2));
    }

    // --- config resolution: env > config > default ---

    fn write_cfg(dir: &Path, body: &str) {
        let loom = dir.join(".loom");
        std::fs::create_dir_all(&loom).unwrap();
        std::fs::write(loom.join("config.json"), body).unwrap();
    }

    #[test]
    fn startup_race_config_missing_is_all_none() {
        let tmp = tempdir().unwrap();
        assert_eq!(read_startup_race_config(tmp.path()), StartupRaceConfig::default());
    }

    #[test]
    fn startup_race_config_full_block_parsed() {
        let tmp = tempdir().unwrap();
        write_cfg(
            tmp.path(),
            r#"{"autonomous":{"dispatchStaggerMs":3000,"watchdog":{"enabled":false,"timeoutSecs":90,"intervalSecs":15}}}"#,
        );
        assert_eq!(
            read_startup_race_config(tmp.path()),
            StartupRaceConfig {
                dispatch_stagger_ms: Some(3000),
                watchdog_enabled: Some(false),
                watchdog_timeout_secs: Some(90),
                watchdog_interval_secs: Some(15),
            }
        );
    }

    #[test]
    fn startup_race_config_zero_stagger_is_honored() {
        // A 0 stagger is a real "disable" value and must be preserved (unlike
        // the interval/timeout fields where 0 is dropped to None).
        let tmp = tempdir().unwrap();
        write_cfg(tmp.path(), r#"{"autonomous":{"dispatchStaggerMs":0}}"#);
        assert_eq!(read_startup_race_config(tmp.path()).dispatch_stagger_ms, Some(0));
    }

    #[test]
    #[serial]
    fn resolve_dispatch_stagger_precedence() {
        std::env::remove_var(DISPATCH_STAGGER_ENV);
        // Default when nothing set.
        assert_eq!(
            resolve_dispatch_stagger(&StartupRaceConfig::default()),
            Duration::from_millis(DEFAULT_DISPATCH_STAGGER_MS)
        );
        // Config used when env unset.
        let cfg = StartupRaceConfig {
            dispatch_stagger_ms: Some(500),
            ..Default::default()
        };
        assert_eq!(resolve_dispatch_stagger(&cfg), Duration::from_millis(500));
        // Env overrides config.
        std::env::set_var(DISPATCH_STAGGER_ENV, "750");
        assert_eq!(resolve_dispatch_stagger(&cfg), Duration::from_millis(750));
        // Env 0 disables (overriding a non-zero config).
        std::env::set_var(DISPATCH_STAGGER_ENV, "0");
        assert_eq!(resolve_dispatch_stagger(&cfg), Duration::ZERO);
        std::env::remove_var(DISPATCH_STAGGER_ENV);
    }

    #[test]
    #[serial]
    fn resolve_watchdog_enabled_precedence() {
        std::env::remove_var(WATCHDOG_ENABLE_ENV);
        // Default ON (self-healing backstop).
        assert!(resolve_watchdog_enabled(&StartupRaceConfig::default()));
        // Config can disable.
        let off = StartupRaceConfig {
            watchdog_enabled: Some(false),
            ..Default::default()
        };
        assert!(!resolve_watchdog_enabled(&off));
        // Env overrides config in both directions.
        std::env::set_var(WATCHDOG_ENABLE_ENV, "1");
        assert!(resolve_watchdog_enabled(&off));
        std::env::set_var(WATCHDOG_ENABLE_ENV, "0");
        let on = StartupRaceConfig {
            watchdog_enabled: Some(true),
            ..Default::default()
        };
        assert!(!resolve_watchdog_enabled(&on));
        std::env::remove_var(WATCHDOG_ENABLE_ENV);
    }

    #[test]
    #[serial]
    fn resolve_watchdog_timeout_and_interval_precedence() {
        std::env::remove_var(WATCHDOG_TIMEOUT_ENV);
        std::env::remove_var(WATCHDOG_INTERVAL_ENV);
        assert_eq!(
            resolve_watchdog_timeout(&StartupRaceConfig::default()),
            Duration::from_secs(DEFAULT_WATCHDOG_TIMEOUT_SECS)
        );
        assert_eq!(
            resolve_watchdog_interval(&StartupRaceConfig::default()),
            Duration::from_secs(DEFAULT_WATCHDOG_INTERVAL_SECS)
        );
        let cfg = StartupRaceConfig {
            watchdog_timeout_secs: Some(200),
            watchdog_interval_secs: Some(45),
            ..Default::default()
        };
        assert_eq!(resolve_watchdog_timeout(&cfg), Duration::from_secs(200));
        assert_eq!(resolve_watchdog_interval(&cfg), Duration::from_secs(45));
        std::env::set_var(WATCHDOG_TIMEOUT_ENV, "77");
        std::env::set_var(WATCHDOG_INTERVAL_ENV, "11");
        assert_eq!(resolve_watchdog_timeout(&cfg), Duration::from_secs(77));
        assert_eq!(resolve_watchdog_interval(&cfg), Duration::from_secs(11));
        std::env::remove_var(WATCHDOG_TIMEOUT_ENV);
        std::env::remove_var(WATCHDOG_INTERVAL_ENV);
    }
}
