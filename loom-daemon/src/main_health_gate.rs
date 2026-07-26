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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::workspace_registry::WorkspaceRegistry;

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

/// Throttle interval for the repeated "gate run SKIPPED" log line (#3950
/// AC2). A dirty workspace logs once immediately on the clean→dirty
/// transition, then at most once per this interval while it stays dirty —
/// down from logging on *every* ~30s tick (2,000+ lines/day observed on the
/// canary host from a single stuck-dirty repo).
const SKIP_WARN_THROTTLE: Duration = Duration::from_secs(3600);

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
    /// Whether the most recent gate tick was `Skipped` (indeterminate — the
    /// workspace tree had unexpected dirt and the gate did not run at all).
    /// Tracked separately from `halted` so status can distinguish "not
    /// evaluated (dirty tree)" from "halted (red main)" (#3950 AC3): the two
    /// can even both be `true` at once (main was red before the tree went
    /// dirty; dispatch remains halted from that prior red run while
    /// evaluation is now skipped). `false` for a fresh state and after any
    /// completed (non-skip) tick.
    skipped: AtomicBool,
    /// Throttle bookkeeping for the skip-warn log line (#3950 AC2): the
    /// instant the warning was last emitted for the *current* dirty streak.
    /// Reset to `None` whenever a tick is not skipped, so the next dirty
    /// streak logs immediately again on its first tick.
    last_skip_warn: Mutex<Option<Instant>>,
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
            skipped: AtomicBool::new(false),
            last_skip_warn: Mutex::new(None),
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

    /// Whether the most recent gate tick was `Skipped` (dirty tree) — see the
    /// field doc on [`Self::skipped`].
    #[must_use]
    pub fn is_skipped(&self) -> bool {
        self.skipped.load(Ordering::SeqCst)
    }

    /// Record this tick's skip/non-skip outcome and report whether the
    /// skip-warn log line should fire now (#3950 AC2): `true` exactly once on
    /// a not-skipped -> skipped transition, then at most once per `throttle`
    /// while the tree remains dirty. Always `false` when `is_skip` is
    /// `false`, which also clears the throttle timer so the next dirty streak
    /// warns immediately again. Call this exactly once per tick, alongside
    /// [`apply_gate_outcome`].
    pub fn note_gate_tick(&self, is_skip: bool, throttle: Duration) -> bool {
        let was_skipped = self.skipped.swap(is_skip, Ordering::SeqCst);
        let mut last = self
            .last_skip_warn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !is_skip {
            *last = None;
            return false;
        }
        let now = Instant::now();
        let should_warn = !was_skipped || last.is_none_or(|t| now.duration_since(t) >= throttle);
        if should_warn {
            *last = Some(now);
        }
        should_warn
    }
}

impl Default for MainHealthState {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-workspace halt state for the multi-repo main-health gate (Issue #3930 —
/// phase d of #3835/#3926).
///
/// Phase c (#3929) made dispatch `(repo, issue)`-aware; phase d makes the
/// **reactive main-health gate per-repo** too: each registered repo's `main` is
/// evaluated independently, and a red repo halts only *its own* dispatch, never
/// the others. Before this, a single [`MainHealthState`] driven by one gate
/// check against the daemon's own workspace gated *every* registered repo
/// uniformly.
///
/// This wrapper holds one [`MainHealthState`] per normalized root, mirroring
/// [`crate::workspace_pool::WorkspacePool`]'s `HashMap<PathBuf, _>` keying. It is
/// shared (as an `Arc`) between the multi-workspace gate loop (writer), the
/// multi-workspace work-finder / epic supervisor (readers), and the IPC
/// `DaemonStatus` per-repo breakdown (reader).
///
/// **Empty-registry equivalence**: with a single workspace (the empty-registry
/// fallback), exactly one root is ever keyed, so this reduces to the single
/// `MainHealthState` behavior byte-for-byte. A root that has never been gated
/// (no map entry) reports **not halted** — a repo with no `buildGate` block
/// simply never gates (soft-fail, unchanged contract).
#[derive(Default)]
pub struct WorkspaceHealthStates {
    inner: Mutex<HashMap<PathBuf, Arc<MainHealthState>>>,
}

impl WorkspaceHealthStates {
    /// An empty per-workspace halt map.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Return `root`'s [`MainHealthState`], creating a fresh (not-halted) one on
    /// first access. The returned `Arc` is shared, so the gate loop (writer) and
    /// the work-finder / status readers all observe the same flag.
    pub fn get_or_create(&self, root: &Path) -> Arc<MainHealthState> {
        let mut map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.entry(root.to_path_buf())
            .or_insert_with(|| Arc::new(MainHealthState::new()))
            .clone()
    }

    /// Whether `root`'s dispatch is currently halted. A never-seen root is
    /// treated as green (not halted) — no gate has run against it.
    #[must_use]
    pub fn is_halted(&self, root: &Path) -> bool {
        let map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.get(root).is_some_and(|s| s.is_halted())
    }

    /// Whether `root`'s most recent gate tick was `Skipped` (dirty tree —
    /// "not evaluated", #3950 AC3). A never-seen root reports `false` — no
    /// gate has run against it.
    #[must_use]
    pub fn is_skipped(&self, root: &Path) -> bool {
        let map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.get(root).is_some_and(|s| s.is_skipped())
    }

    /// Directly set `root`'s halt flag (creating its state if absent). Primarily
    /// for tests / explicit control, and for the gate loop to clear a
    /// disabled/absent-`buildGate` repo to green.
    pub fn set_halted(&self, root: &Path, halted: bool) {
        self.get_or_create(root).set_halted(halted);
    }

    /// Snapshot of `(root → halted)` for every root the gate has observed —
    /// consumed by the daemon-status per-repo breakdown (#3930). Roots never
    /// gated are absent (a reader treats an absent root as green).
    #[must_use]
    pub fn snapshot(&self) -> HashMap<PathBuf, bool> {
        let map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.iter()
            .map(|(k, v)| (k.clone(), v.is_halted()))
            .collect()
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
    /// The gate did **not** run because the workspace could not be prepared to
    /// reflect `origin/main` (dirty tree, not on `main`, a failed `git` step, or
    /// a `git fetch` failure). `reason` explains why. A skipped run is
    /// **indeterminate** — it deliberately leaves the halt flag unchanged rather
    /// than greenwashing a stale checkout or spuriously halting on unrelated
    /// local state (Issue #3885).
    Skipped { reason: String },
}

impl GateOutcome {
    /// Convenience constructor for a red outcome.
    #[must_use]
    pub fn red(detail: impl Into<String>) -> Self {
        Self::Red {
            detail: detail.into(),
        }
    }

    /// Convenience constructor for a skipped (indeterminate) outcome.
    #[must_use]
    pub fn skipped(reason: impl Into<String>) -> Self {
        Self::Skipped {
            reason: reason.into(),
        }
    }

    /// True when the run was green.
    #[must_use]
    pub fn is_green(&self) -> bool {
        matches!(self, Self::Green)
    }

    /// True when the run was skipped (indeterminate — did not execute the gate
    /// command).
    #[must_use]
    pub fn is_skipped(&self) -> bool {
        matches!(self, Self::Skipped { .. })
    }

    /// The red-detail / skip-reason string, or empty for a green outcome.
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Self::Green => "",
            Self::Red { detail } => detail,
            Self::Skipped { reason } => reason,
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

/// The concrete [`GateRunner`]: syncs the workspace to `origin/main`, then shells
/// out to `buildGate.command` (via `sh -c`) against that freshly-synced tree,
/// honoring `buildGate.timeoutSeconds`.
///
/// The command runs in `repo_root` — the daemon's workspace, nominally a `main`
/// checkout. Autonomous merges land via the forge API (`merge-pr.sh`), which
/// advances `origin/main` on the **remote** but never the daemon's local `main`
/// checkout. Without a sync step the gate would repeatedly test a stale snapshot:
/// a breaking merge never enters the tree it builds (missed catch), or operator
/// edits / a stray branch turn it red on unrelated state (false halt). So before
/// each run [`prepare_workspace_to_origin_main`] fast-forwards the checkout to
/// `origin/main` — but only when it is on `main` and clean; a dirty tree or a
/// failed `git` step yields a [`GateOutcome::Skipped`] that leaves the halt flag
/// untouched rather than clobbering operator edits or acting on stale state
/// (Issue #3885).
///
/// Sync can be disabled with [`without_sync`](Self::without_sync) (used by unit
/// tests that exercise command classification against a scratch dir).
pub struct CommandGateRunner {
    config: BuildGateConfig,
    repo_root: PathBuf,
    /// Whether to sync `repo_root` to `origin/main` before each run. `true` in
    /// production (via [`new`](Self::new)); tests opt out with
    /// [`without_sync`](Self::without_sync).
    sync: bool,
}

impl CommandGateRunner {
    /// Construct a runner for `config`, executing in `repo_root`. Workspace sync
    /// to `origin/main` is **on** — the production default.
    #[must_use]
    pub fn new(config: BuildGateConfig, repo_root: PathBuf) -> Self {
        Self {
            config,
            repo_root,
            sync: true,
        }
    }

    /// Disable the pre-run `origin/main` sync. Intended for tests that run the
    /// gate command against a non-repo scratch directory; production always syncs.
    #[must_use]
    pub fn without_sync(mut self) -> Self {
        self.sync = false;
        self
    }
}

impl GateRunner for CommandGateRunner {
    fn run_gate(&mut self) -> GateOutcome {
        if self.sync {
            if let PrepOutcome::Skip { reason } = prepare_workspace_to_origin_main(&self.repo_root)
            {
                return GateOutcome::skipped(reason);
            }
        }
        run_command_with_timeout(&self.config.command, &self.repo_root, self.config.timeout)
    }
}

// ============================================================================
// Dirty-tree ignore list (#3778 transient paths + build-artifact lockfiles,
// #3950)
// ============================================================================

/// Loom-owned transient state path prefixes the gate's dirty-tree check
/// ignores when deciding whether the workspace is safe to sync/reset before a
/// run (#3950). Mirrors `.loom/scripts/check-main-clean.sh`'s
/// `LOOM_OWNED_PREFIXES` — kept in sync manually since one lives in bash and
/// the other in Rust, but both exist to solve the same #3778 problem: Loom's
/// own runtime bookkeeping showing up as "dirty" and false-positiving a check
/// that exists to protect *real* operator edits. A prefix ending in `/`
/// matches a directory subtree; the others match an exact path.
const LOOM_OWNED_PREFIXES: &[&str] = &[
    ".loom/sweep-checkpoint/",
    ".loom/sweep-run/",
    ".loom/tokens/",
    ".loom/accounts.env",
    ".loom/exit-codes/",
    ".loom/stats/",
    ".loom/CANARY",
    ".loom/spawn-loop.pid",
    ".loom/spawn-loop-state.json",
    ".loom/stop-spawn-loop",
    ".loom/locks/",
    ".loom/logs/",
    ".loom/worktrees/",
    ".loom-managed",
];

/// Common regenerable lockfile basenames (#3950): a package manager can
/// rewrite one of these with no dependency change (formatting/ordering churn
/// from a `buildGate.command` step like `pnpm install`), leaving tracked-file
/// dirt that would otherwise wedge the dirty-tree check indefinitely — the
/// reported symptom was a lone modified `mcp-loom/package-lock.json`
/// disabling the gate for the whole repo, every tick, forever (a hard reset
/// would have discarded it, but the check refused to run one). Matched by
/// exact basename anywhere in the tree: a small, well-known, documented set,
/// not a repo-specific hardcode.
const BUILD_ARTIFACT_LOCKFILE_BASENAMES: &[&str] = &[
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "Cargo.lock",
    "uv.lock",
];

/// Whether `path` (a repo-relative path from `git status --porcelain`) is
/// ignorable dirt for the gate's dirty-tree check — either a Loom-owned
/// transient path ([`LOOM_OWNED_PREFIXES`]) or a common regenerable lockfile
/// ([`BUILD_ARTIFACT_LOCKFILE_BASENAMES`]). Ignorable dirt is dirt a hard
/// reset to `origin/main` safely discards (it is either untracked Loom
/// runtime state or a tracked file whose content the reset will overwrite
/// anyway) — never a reason to skip the sync step.
fn is_ignorable_dirt(path: &str) -> bool {
    let loom_owned = LOOM_OWNED_PREFIXES.iter().any(|prefix| {
        if prefix.ends_with('/') {
            path.starts_with(prefix)
        } else {
            path == *prefix
        }
    });
    if loom_owned {
        return true;
    }
    let basename = path.rsplit('/').next().unwrap_or(path);
    BUILD_ARTIFACT_LOCKFILE_BASENAMES.contains(&basename)
}

/// Parse `git status --porcelain` v1 `output` and return the lines that are
/// **not** ignorable dirt ([`is_ignorable_dirt`]) — the lines that must still
/// be treated as "the workspace is dirty" for the gate's sync-before-run
/// check. Rename lines (`R  old -> new`) are keyed on the new path. Empty
/// lines are dropped.
fn non_ignorable_dirt(output: &str) -> Vec<&str> {
    output
        .lines()
        .filter(|line| !line.is_empty())
        .filter(|line| {
            let path = line.get(3..).unwrap_or("");
            let path = path.rsplit(" -> ").next().unwrap_or(path);
            let path = path.trim_matches('"');
            !is_ignorable_dirt(path)
        })
        .collect()
}

// ============================================================================
// Workspace preparation — sync to origin/main before a gate run (#3885)
// ============================================================================

/// The remote the gate syncs its checkout from.
const GATE_REMOTE: &str = "origin";

/// The branch the gate builds against.
const GATE_BRANCH: &str = "main";

/// The result of preparing the workspace to reflect `origin/main`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepOutcome {
    /// The workspace is on `main`, clean, and now fast-forwarded to
    /// `origin/main` — the gate command may run against a fresh tree.
    Ready,
    /// The workspace could **not** be safely synced (dirty tree, not on `main`,
    /// or a failed `git` step). `reason` explains why; the caller should skip
    /// the gate run and leave the halt flag unchanged.
    Skip { reason: String },
}

/// Run a `git` subcommand in `repo_root`, returning `Ok((stdout, stderr))` on a
/// zero exit or `Err(reason)` describing the failure (spawn error or non-zero
/// exit with captured stderr). Trims trailing whitespace from captured streams.
fn run_git(repo_root: &Path, args: &[&str]) -> std::result::Result<(String, String), String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("failed to spawn `git {}`: {e}", args.join(" ")))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.status.success() {
        Ok((stdout, stderr))
    } else {
        Err(format!(
            "`git {}` exited with {}{}",
            args.join(" "),
            output.status,
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            }
        ))
    }
}

/// Run `git status --porcelain` in `repo_root` and return its **raw** stdout,
/// trimmed only of *trailing* whitespace. Unlike [`run_git`] (whose blanket
/// `.trim()` is fine for its single-value call sites), the porcelain v1
/// format is column-sensitive: the very first status line can legitimately
/// start with a space (e.g. `" M file"` — unstaged modification of a tracked
/// file), and [`run_git`]'s leading-whitespace trim would eat that space and
/// misalign every downstream column-offset parse ([`non_ignorable_dirt`],
/// #3950). Trailing trim is still safe — trailing whitespace never carries
/// meaning in porcelain output.
fn git_status_porcelain(repo_root: &Path) -> std::result::Result<String, String> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("failed to spawn `git status --porcelain`: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!(
            "`git status --porcelain` exited with {}{}",
            output.status,
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            }
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string())
}

/// Prepare `repo_root` to reflect `origin/main` before a gate run.
///
/// The hybrid safe policy from Issue #3885:
/// 1. **Verify on `main`.** A detached HEAD or a different branch ⇒ `Skip`
///    (never silently reset an operator's checked-out branch).
/// 2. **Verify clean.** Any tracked/untracked local change ⇒ `Skip` (a hard
///    reset would clobber operator edits).
/// 3. **Fetch** `origin main`. A fetch failure (offline, transient) ⇒ `Skip`
///    (better indeterminate than greenwashing a stale tree).
/// 4. **Verify not ahead** of `origin/main`. A clean local `main` that carries
///    commits `origin/main` lacks ⇒ `Skip` — the hard reset would discard those
///    local-only commits (reflog-recoverable, but still). Extreme edge for a
///    daemon workspace that should only ever fast-forward its own `main`, but
///    worth guarding against a data-losing reset (Issue #3912).
/// 5. **Hard-reset** to `origin/main` so the gate builds exactly what the remote
///    `main` now is. Only reached when the tree is on `main`, clean, and not
///    ahead, so the reset only ever fast-forwards the daemon's own `main`
///    checkout.
///
/// A `Skip` leaves the halt flag untouched (see [`apply_gate_outcome`]).
#[must_use]
pub fn prepare_workspace_to_origin_main(repo_root: &Path) -> PrepOutcome {
    // 1. On `main`?
    let branch = match run_git(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"]) {
        Ok((out, _)) => out,
        Err(e) => {
            return PrepOutcome::Skip {
                reason: format!("could not determine current branch ({e})"),
            };
        }
    };
    if branch != GATE_BRANCH {
        return PrepOutcome::Skip {
            reason: format!(
                "workspace is on '{branch}', not '{GATE_BRANCH}' — skipping gate (will not reset an operator branch)"
            ),
        };
    }

    // 2. Clean tree? `git status --porcelain` emits one line per change. Lines
    // that are ignorable dirt (#3950 — Loom-owned transient paths / common
    // regenerable lockfiles, see `non_ignorable_dirt`) are excluded from this
    // decision: they are known-regenerable noise a hard reset safely
    // discards, not operator edits worth protecting.
    match git_status_porcelain(repo_root) {
        Ok(out) => {
            let unexpected = non_ignorable_dirt(&out);
            if !unexpected.is_empty() {
                return PrepOutcome::Skip {
                    reason: format!(
                        "workspace has local changes — skipping gate (will not hard-reset over operator edits): {}",
                        unexpected.join(", ")
                    ),
                };
            }
        }
        Err(e) => {
            return PrepOutcome::Skip {
                reason: format!("could not check workspace cleanliness ({e})"),
            };
        }
    }

    // 3. Fetch origin/main.
    if let Err(e) = run_git(repo_root, &["fetch", GATE_REMOTE, GATE_BRANCH]) {
        return PrepOutcome::Skip {
            reason: format!("`git fetch {GATE_REMOTE} {GATE_BRANCH}` failed ({e}) — skipping gate rather than testing a stale checkout"),
        };
    }

    let remote_ref = format!("{GATE_REMOTE}/{GATE_BRANCH}");

    // 4. Not ahead of the freshly-fetched origin/main? A non-zero count of
    // commits reachable from HEAD but not `origin/main` means a hard reset would
    // discard local-only commits — skip rather than lose them (Issue #3912).
    match run_git(repo_root, &["rev-list", "--count", &format!("{remote_ref}..HEAD")]) {
        Ok((out, _)) => {
            if out != "0" {
                return PrepOutcome::Skip {
                    reason: format!(
                        "workspace '{GATE_BRANCH}' is {out} commit(s) ahead of {remote_ref} — skipping gate (will not hard-reset away local-only commits)"
                    ),
                };
            }
        }
        Err(e) => {
            return PrepOutcome::Skip {
                reason: format!("could not compare workspace to {remote_ref} ({e})"),
            };
        }
    }

    // 5. Hard-reset to the freshly-fetched origin/main.
    if let Err(e) = run_git(repo_root, &["reset", "--hard", &remote_ref]) {
        return PrepOutcome::Skip {
            reason: format!("`git reset --hard {remote_ref}` failed ({e})"),
        };
    }

    PrepOutcome::Ready
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
    /// The gate was skipped (indeterminate) — the halt flag is left exactly as
    /// it was, so a skip neither halts nor resumes dispatch (#3885).
    Skipped,
}

/// Apply a gate `outcome` to the shared `state`, returning the transition.
///
/// Atomic `swap` makes the read-modify-write safe against a concurrent
/// work-finder read (which only ever *loads*). This is the single point that
/// mutates the halt flag. A [`GateOutcome::Skipped`] is a no-op: it never
/// touches the flag, so the previous halt/green decision persists until a run
/// actually completes.
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
        GateOutcome::Skipped { .. } => HealthTransition::Skipped,
    }
}

/// Apply `outcome` to `state`, then render it via `log_fn` — throttling the
/// repeated "gate run SKIPPED" line to at most once per [`SKIP_WARN_THROTTLE`]
/// after its first (not-skipped -> skipped) occurrence (#3950 AC2). Every
/// other transition (green/red, and the clean->dirty transition itself) logs
/// unthrottled via `log_fn` exactly as before. `log_fn` renders the actual
/// `HealthTransition`/`GateOutcome` pair — the single- vs multi-workspace
/// loops pass different renderers (the latter also names the repo root).
fn apply_and_log<F>(state: &MainHealthState, outcome: &GateOutcome, log_fn: F)
where
    F: Fn(HealthTransition, &GateOutcome),
{
    let transition = apply_gate_outcome(state, outcome);
    let should_warn = state.note_gate_tick(outcome.is_skipped(), SKIP_WARN_THROTTLE);
    if matches!(transition, HealthTransition::Skipped) && !should_warn {
        log::debug!(
            "main_health_gate: gate run SKIPPED (throttled) — {} (halt state unchanged)",
            outcome.detail()
        );
        return;
    }
    log_fn(transition, outcome);
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
        HealthTransition::Skipped => log::warn!(
            "main_health_gate: gate run SKIPPED — {} (halt state unchanged)",
            outcome.detail()
        ),
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
        // A gate run can exceed the cadence interval (a `buildGate` build may take
        // minutes). Without this, `interval`'s default `Burst` behavior would fire
        // the missed ticks back-to-back, churning rebuild after rebuild with no
        // gap. `Delay` measures the next interval from when the previous run
        // finished, so a slow build never triggers a rebuild storm (#3885).
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
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
                    health_state.note_gate_tick(false, SKIP_WARN_THROTTLE);
                    return;
                }
            };
            apply_and_log(&health_state, &outcome, log_transition);
        }
    })
}

/// Log a health transition (root-aware variant, #3930) at a severity matching
/// its significance, naming which repo's `main` the gate evaluated.
fn log_transition_for_root(root: &Path, transition: HealthTransition, outcome: &GateOutcome) {
    let r = root.display();
    match transition {
        HealthTransition::EnteredHalt => log::error!(
            "main_health_gate: {r} main is RED — HALTING autonomous dispatch for this repo. {}",
            outcome.detail()
        ),
        HealthTransition::RemainedHalted => log::warn!(
            "main_health_gate: {r} main still RED — dispatch for this repo remains halted. {}",
            outcome.detail()
        ),
        HealthTransition::Recovered => log::info!(
            "main_health_gate: {r} main GREEN again — RESUMING dispatch for this repo on the next work-finder tick"
        ),
        HealthTransition::RemainedHealthy => {
            log::debug!("main_health_gate: {r} main green — dispatch unaffected");
        }
        HealthTransition::Skipped => log::warn!(
            "main_health_gate: {r} gate run SKIPPED — {} (halt state unchanged)",
            outcome.detail()
        ),
    }
}

/// Spawn the **multi-workspace** main-health gate loop (Issue #3930) on the
/// shared daemon runtime and return its task handle.
///
/// This is the multi-repo replacement for [`spawn_main_health_gate_task`]. Every
/// `interval` it re-reads [`WorkspaceRegistry::effective_roots`] against
/// `fallback_root` (an **empty** registry ⇒ the single `fallback_root`,
/// byte-for-byte the pre-#3930 single-workspace behavior) and runs **one gate
/// check per registered root**, applying each outcome to that root's own
/// [`MainHealthState`] in `health_states`. A red repo halts only its own
/// dispatch; sibling repos keep dispatching.
///
/// Per-root enablement is resolved from each repo's own `.loom/config.json`
/// (`autonomous.mainHealthGate.enabled` via [`resolve_enabled`], precedence
/// env > config > default) plus a usable `buildGate` block
/// ([`read_build_gate_config`]). A root that is disabled / has no `buildGate`
/// block is treated as **always-green** — its halt flag is cleared and no gate
/// command runs for it (soft-fail, unchanged contract). No new config schema is
/// introduced: the per-root `buildGate` / `autonomous.mainHealthGate` blocks are
/// exactly the ones phase C already reads.
///
/// Gates run **sequentially** per tick (not concurrently) so several minutes-long
/// per-repo builds firing on the same tick never contend — each
/// [`CommandGateRunner`] already isolates its own `origin/main` sync and uuid
/// temp output file, so there is no shared mutable state to leak across repos.
pub fn spawn_multi_main_health_gate_task(
    health_states: Arc<WorkspaceHealthStates>,
    fallback_root: PathBuf,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    log::info!(
        "main_health_gate: starting multi-workspace loop (interval={}s)",
        interval.as_secs()
    );
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // First tick fires immediately; skip it so we don't churn at boot.
        ticker.tick().await;
        loop {
            ticker.tick().await;

            // Resolve the current workspace set fresh each tick so registry edits
            // (`workspace add|remove`) are hot-applied without a daemon restart.
            let roots = WorkspaceRegistry::load_default()
                .unwrap_or_else(|e| {
                    log::warn!(
                        "main_health_gate: could not load workspace registry ({e}); using fallback"
                    );
                    WorkspaceRegistry::default()
                })
                .effective_roots(&fallback_root);

            for root in roots {
                // Per-root enablement (env > config > default). The env master
                // switch, when set, applies to every root uniformly; when unset,
                // each repo's own config decides.
                if !resolve_enabled(&read_autonomous_gate_config(&root)) {
                    // Disabled ⇒ always green for this repo (clear any stale halt
                    // and any stale skip/dirty state).
                    let state = health_states.get_or_create(&root);
                    state.set_halted(false);
                    state.note_gate_tick(false, SKIP_WARN_THROTTLE);
                    continue;
                }
                let Some(gate_config) = read_build_gate_config(&root) else {
                    log::debug!(
                        "main_health_gate: {} enabled but no usable buildGate config — treating as green",
                        root.display()
                    );
                    let state = health_states.get_or_create(&root);
                    state.set_halted(false);
                    state.note_gate_tick(false, SKIP_WARN_THROTTLE);
                    continue;
                };

                let state = health_states.get_or_create(&root);
                let root_for_task = root.clone();
                // Run the (potentially minutes-long) gate off the runtime.
                let joined = tokio::task::spawn_blocking(move || {
                    let mut runner = CommandGateRunner::new(gate_config, root_for_task);
                    runner.run_gate()
                })
                .await;
                match joined {
                    Ok(outcome) => {
                        let root_for_log = root.clone();
                        apply_and_log(&state, &outcome, move |transition, outcome| {
                            log_transition_for_root(&root_for_log, transition, outcome);
                        });
                    }
                    Err(e) => {
                        // The blocking task panicked; clear this repo's halt so a
                        // panic never wedges it permanently halted, and continue to
                        // the other repos (one bad gate must not stop the loop).
                        log::error!(
                            "main_health_gate: gate run task for {} panicked ({e}); clearing its halt",
                            root.display()
                        );
                        state.set_halted(false);
                        state.note_gate_tick(false, SKIP_WARN_THROTTLE);
                    }
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

    // ===================================================================
    // Per-workspace halt state (#3930)
    // ===================================================================

    #[test]
    fn test_workspace_health_states_unknown_root_not_halted() {
        let states = WorkspaceHealthStates::new();
        assert!(!states.is_halted(Path::new("/repo/never-seen")));
        assert!(states.snapshot().is_empty());
    }

    #[test]
    fn test_workspace_health_states_are_per_root_independent() {
        // Red repo A must not mark repo B halted (the core AC2 property).
        let states = WorkspaceHealthStates::new();
        let a = Path::new("/repo/a");
        let b = Path::new("/repo/b");
        states.set_halted(a, true);
        assert!(states.is_halted(a), "repo A is halted");
        assert!(!states.is_halted(b), "repo B is unaffected by A's halt");

        // Clearing A does not touch B, and setting B does not touch A.
        states.set_halted(b, true);
        states.set_halted(a, false);
        assert!(!states.is_halted(a));
        assert!(states.is_halted(b));
    }

    #[test]
    fn test_workspace_health_states_get_or_create_shares_arc() {
        let states = WorkspaceHealthStates::new();
        let root = Path::new("/repo/a");
        let s1 = states.get_or_create(root);
        s1.set_halted(true);
        // A second get_or_create returns the same shared state (the flag persists).
        let s2 = states.get_or_create(root);
        assert!(s2.is_halted());
        assert!(states.is_halted(root));
    }

    #[test]
    fn test_workspace_health_states_snapshot_lists_seen_roots() {
        let states = WorkspaceHealthStates::new();
        states.set_halted(Path::new("/repo/a"), true);
        states.set_halted(Path::new("/repo/b"), false);
        let snap = states.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap.get(Path::new("/repo/a")), Some(&true));
        assert_eq!(snap.get(Path::new("/repo/b")), Some(&false));
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
        let mut runner = CommandGateRunner::new(cfg, std::env::temp_dir()).without_sync();
        assert_eq!(runner.run_gate(), GateOutcome::Green);
    }

    #[test]
    fn test_command_runner_red_on_nonzero_exit_captures_output() {
        let cfg = BuildGateConfig {
            command: "echo build-failed-marker >&2; exit 1".to_string(),
            timeout: Duration::from_secs(30),
        };
        let mut runner = CommandGateRunner::new(cfg, std::env::temp_dir()).without_sync();
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
        let mut runner = CommandGateRunner::new(cfg, std::env::temp_dir()).without_sync();
        let outcome = runner.run_gate();
        assert!(!outcome.is_green());
        assert!(
            outcome.detail().contains("timed out"),
            "timeout detail expected, got: {}",
            outcome.detail()
        );
    }

    // ===================================================================
    // Workspace preparation — sync to origin/main before a gate run (#3885)
    // ===================================================================

    /// Run `git <args>` in `dir`, asserting success. Test-only helper for
    /// building throwaway repos.
    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    /// Create an `origin` bare repo with an initial `main` commit and a working
    /// clone checked out on `main`. Returns `(origin_dir, clone_dir)` — both
    /// `TempDir` guards so they live for the test's duration.
    fn make_origin_and_clone() -> (tempfile::TempDir, tempfile::TempDir) {
        let origin = tempfile::tempdir().unwrap();
        // A bare origin we can fetch from and push to.
        git(origin.path(), &["init", "--bare", "--initial-branch=main"]);

        // Seed it via a scratch clone so origin has a real `main` commit.
        let seed = tempfile::tempdir().unwrap();
        git(seed.path(), &["init", "--initial-branch=main"]);
        git(seed.path(), &["config", "user.email", "t@t.t"]);
        git(seed.path(), &["config", "user.name", "t"]);
        std::fs::write(seed.path().join("file.txt"), "v1\n").unwrap();
        git(seed.path(), &["add", "."]);
        git(seed.path(), &["commit", "-m", "initial"]);
        git(seed.path(), &["remote", "add", "origin", origin.path().to_str().unwrap()]);
        git(seed.path(), &["push", "origin", "main"]);

        // The workspace under test: a fresh clone on `main`.
        let clone = tempfile::tempdir().unwrap();
        git(
            clone.path(),
            &[
                "clone",
                origin.path().to_str().unwrap(),
                clone.path().to_str().unwrap(),
            ],
        );
        git(clone.path(), &["config", "user.email", "t@t.t"]);
        git(clone.path(), &["config", "user.name", "t"]);
        (origin, clone)
    }

    /// Push a new commit to `origin/main` from a scratch clone, so a workspace
    /// that has not fetched is now behind.
    fn advance_origin_main(origin: &Path) {
        let scratch = tempfile::tempdir().unwrap();
        git(
            scratch.path(),
            &[
                "clone",
                origin.to_str().unwrap(),
                scratch.path().to_str().unwrap(),
            ],
        );
        git(scratch.path(), &["config", "user.email", "t@t.t"]);
        git(scratch.path(), &["config", "user.name", "t"]);
        std::fs::write(scratch.path().join("file.txt"), "v2\n").unwrap();
        git(scratch.path(), &["add", "."]);
        git(scratch.path(), &["commit", "-m", "advance main"]);
        git(scratch.path(), &["push", "origin", "main"]);
    }

    fn head_commit(dir: &Path) -> String {
        String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(dir)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string()
    }

    #[test]
    fn test_prepare_fast_forwards_stale_main_to_origin() {
        let (origin, clone) = make_origin_and_clone();
        let before = head_commit(clone.path());
        // Remote main advances; the local clone is now behind (never fetched).
        advance_origin_main(origin.path());
        assert_eq!(head_commit(clone.path()), before, "clone still stale pre-prep");

        let outcome = prepare_workspace_to_origin_main(clone.path());
        assert_eq!(outcome, PrepOutcome::Ready);
        assert_ne!(
            head_commit(clone.path()),
            before,
            "prepare must fast-forward the workspace to the advanced origin/main"
        );
    }

    #[test]
    fn test_prepare_skips_when_local_main_ahead_of_origin() {
        let (_origin, clone) = make_origin_and_clone();
        // A clean local `main` that carries a commit origin/main lacks.
        std::fs::write(clone.path().join("local.txt"), "local-only\n").unwrap();
        git(clone.path(), &["add", "."]);
        git(clone.path(), &["commit", "-m", "local-only commit"]);
        let ahead = head_commit(clone.path());

        let outcome = prepare_workspace_to_origin_main(clone.path());
        match outcome {
            PrepOutcome::Skip { reason } => {
                assert!(reason.contains("ahead"), "expected ahead-of-origin reason, got: {reason}")
            }
            other => panic!("expected Skip when local main is ahead, got {other:?}"),
        }
        // The local-only commit must NOT have been reset away.
        assert_eq!(
            head_commit(clone.path()),
            ahead,
            "a local main ahead of origin must never be hard-reset away"
        );
    }

    #[test]
    fn test_prepare_skips_dirty_workspace() {
        let (_origin, clone) = make_origin_and_clone();
        // A tracked-file edit makes the tree dirty.
        std::fs::write(clone.path().join("file.txt"), "operator edit\n").unwrap();
        let outcome = prepare_workspace_to_origin_main(clone.path());
        match outcome {
            PrepOutcome::Skip { reason } => assert!(
                reason.contains("local changes"),
                "expected dirty-skip reason, got: {reason}"
            ),
            other => panic!("expected Skip on dirty tree, got {other:?}"),
        }
        // The operator edit must NOT have been reset away.
        assert_eq!(
            std::fs::read_to_string(clone.path().join("file.txt")).unwrap(),
            "operator edit\n",
            "a dirty workspace must never be hard-reset"
        );
    }

    #[test]
    fn test_prepare_skips_untracked_file() {
        let (_origin, clone) = make_origin_and_clone();
        std::fs::write(clone.path().join("scratch.tmp"), "junk\n").unwrap();
        let outcome = prepare_workspace_to_origin_main(clone.path());
        assert!(
            matches!(outcome, PrepOutcome::Skip { .. }),
            "an untracked file must skip (porcelain reports it), got {outcome:?}"
        );
    }

    // ===================================================================
    // Ignore-list: Loom-owned transient paths + build-artifact lockfiles
    // (#3950 AC1) — the dirty-tree check must not block on these.
    // ===================================================================

    #[test]
    fn test_is_ignorable_dirt_loom_owned_prefixes() {
        assert!(is_ignorable_dirt(".loom/logs/sweep-issue-1.log"));
        assert!(is_ignorable_dirt(".loom/worktrees/issue-42/foo.rs"));
        assert!(is_ignorable_dirt(".loom/tokens/agent-1.token"));
        assert!(is_ignorable_dirt(".loom/sweep-checkpoint/issue-1.json"));
        assert!(is_ignorable_dirt(".loom/accounts.env"));
        assert!(is_ignorable_dirt(".loom-managed"));
    }

    #[test]
    fn test_is_ignorable_dirt_lockfile_basenames() {
        assert!(is_ignorable_dirt("mcp-loom/package-lock.json"));
        assert!(is_ignorable_dirt("package-lock.json"));
        assert!(is_ignorable_dirt("some/nested/dir/Cargo.lock"));
        assert!(is_ignorable_dirt("pnpm-lock.yaml"));
    }

    #[test]
    fn test_is_ignorable_dirt_rejects_unknown_paths() {
        // A genuine operator edit outside both lists must never be ignored.
        assert!(!is_ignorable_dirt("src/main.rs"));
        assert!(!is_ignorable_dirt("scratch.tmp"));
        // A path that merely starts with ".loom" but isn't one of the listed
        // transient subtrees (e.g. a hypothetical ".loom/config.json" edit)
        // must NOT be ignored — only the explicitly listed prefixes count.
        assert!(!is_ignorable_dirt(".loom/config.json"));
    }

    #[test]
    fn test_non_ignorable_dirt_filters_porcelain_lines() {
        let status = "?? .loom/logs/foo.log\n M mcp-loom/package-lock.json\n M src/main.rs\n";
        let remaining = non_ignorable_dirt(status);
        assert_eq!(remaining, vec![" M src/main.rs"]);
    }

    #[test]
    fn test_non_ignorable_dirt_empty_when_all_ignorable() {
        let status = "?? .loom/logs/foo.log\n M package-lock.json\n";
        assert!(non_ignorable_dirt(status).is_empty());
    }

    /// AC1(a): a workspace with ONLY Loom-owned transient paths dirty must NOT
    /// block the gate (prep proceeds to sync/reset, not Skip).
    #[test]
    fn test_prepare_ignores_loom_owned_transient_dirt() {
        let (_origin, clone) = make_origin_and_clone();
        // Realistic repo shape: `.loom/config.json` is a tracked, committed
        // file (every installed Loom repo has one) — so `.loom/` itself is
        // never a wholly-untracked directory that `git status --porcelain`
        // could collapse into one opaque `?? .loom/` line. Without this,
        // `.loom/logs/...` below would report as `?? .loom/` (the whole
        // subtree), which no single ignore-list prefix matches.
        std::fs::create_dir_all(clone.path().join(".loom")).unwrap();
        std::fs::write(clone.path().join(".loom/config.json"), "{}\n").unwrap();
        Command::new("git")
            .args(["add", ".loom/config.json"])
            .current_dir(clone.path())
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "add .loom/config.json"])
            .current_dir(clone.path())
            .status()
            .unwrap();
        // Push so this commit is on `origin/main` too — otherwise the clone
        // would be (correctly) skipped as "ahead of origin" by step 4,
        // unrelated to the dirty-tree behavior this test targets.
        Command::new("git")
            .args(["push", "origin", "main"])
            .current_dir(clone.path())
            .status()
            .unwrap();

        std::fs::create_dir_all(clone.path().join(".loom/logs")).unwrap();
        std::fs::write(clone.path().join(".loom/logs/sweep-issue-1.log"), "log\n").unwrap();
        let outcome = prepare_workspace_to_origin_main(clone.path());
        assert_eq!(
            outcome,
            PrepOutcome::Ready,
            "only Loom-owned transient dirt must not block the gate, got {outcome:?}"
        );
    }

    /// AC1(a) variant: a modified build-artifact lockfile alone must not block
    /// the gate either (the reported symptom — a lone modified
    /// `mcp-loom/package-lock.json`).
    #[test]
    fn test_prepare_ignores_lockfile_only_dirt() {
        let (_origin, clone) = make_origin_and_clone();
        std::fs::write(clone.path().join("package-lock.json"), "{}\n").unwrap();
        Command::new("git")
            .args(["add", "package-lock.json"])
            .current_dir(clone.path())
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "add lockfile"])
            .current_dir(clone.path())
            .status()
            .unwrap();
        // Push so the lockfile-add commit is on `origin/main` too — otherwise
        // the clone would be (correctly) skipped as "ahead of origin" by step
        // 4, unrelated to the dirty-tree behavior this test targets.
        Command::new("git")
            .args(["push", "origin", "main"])
            .current_dir(clone.path())
            .status()
            .unwrap();
        // Now mutate it — a benign build-side-effect regen, no real change.
        std::fs::write(clone.path().join("package-lock.json"), "{ \"regen\": true }\n").unwrap();
        let outcome = prepare_workspace_to_origin_main(clone.path());
        assert_eq!(
            outcome,
            PrepOutcome::Ready,
            "a lone modified lockfile must not block the gate, got {outcome:?}"
        );
    }

    /// AC1(b): a genuine unexpected dirty file — alongside otherwise-ignorable
    /// dirt — must still cause a skip.
    #[test]
    fn test_prepare_still_skips_on_unexpected_dirt_alongside_ignorable() {
        let (_origin, clone) = make_origin_and_clone();
        std::fs::create_dir_all(clone.path().join(".loom/logs")).unwrap();
        std::fs::write(clone.path().join(".loom/logs/sweep-issue-1.log"), "log\n").unwrap();
        // A genuine operator edit — not on either ignore list.
        std::fs::write(clone.path().join("file.txt"), "operator edit\n").unwrap();
        let outcome = prepare_workspace_to_origin_main(clone.path());
        match outcome {
            PrepOutcome::Skip { reason } => {
                assert!(
                    reason.contains("file.txt"),
                    "skip reason should name the unexpected file, got: {reason}"
                );
                assert!(
                    !reason.contains(".loom/logs"),
                    "skip reason should not blame the ignorable Loom-owned path, got: {reason}"
                );
            }
            other => panic!("expected Skip on genuine unexpected dirt, got {other:?}"),
        }
    }

    #[test]
    fn test_prepare_skips_when_not_on_main() {
        let (_origin, clone) = make_origin_and_clone();
        git(clone.path(), &["checkout", "-b", "feature/x"]);
        let outcome = prepare_workspace_to_origin_main(clone.path());
        match outcome {
            PrepOutcome::Skip { reason } => assert!(
                reason.contains("feature/x") && reason.contains("not 'main'"),
                "expected not-on-main reason, got: {reason}"
            ),
            other => panic!("expected Skip off main, got {other:?}"),
        }
    }

    #[test]
    fn test_prepare_skips_non_git_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let outcome = prepare_workspace_to_origin_main(tmp.path());
        assert!(
            matches!(outcome, PrepOutcome::Skip { .. }),
            "a non-git dir cannot determine a branch and must skip, got {outcome:?}"
        );
    }

    #[test]
    fn test_prepare_skips_when_fetch_fails_offline() {
        // A repo whose `origin` points nowhere: on main + clean, but fetch fails.
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "--initial-branch=main"]);
        git(repo.path(), &["config", "user.email", "t@t.t"]);
        git(repo.path(), &["config", "user.name", "t"]);
        std::fs::write(repo.path().join("f.txt"), "x\n").unwrap();
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "c"]);
        git(
            repo.path(),
            &[
                "remote",
                "add",
                "origin",
                "/nonexistent/loom-gate-no-such-remote.git",
            ],
        );
        let outcome = prepare_workspace_to_origin_main(repo.path());
        match outcome {
            PrepOutcome::Skip { reason } => {
                assert!(reason.contains("fetch"), "expected fetch-failure reason, got: {reason}")
            }
            other => panic!("expected Skip on fetch failure, got {other:?}"),
        }
    }

    #[test]
    fn test_command_runner_returns_skipped_when_prep_skips() {
        // Sync ON (production default) against a non-repo dir ⇒ prep skips ⇒ the
        // gate command is NOT run and the outcome is Skipped.
        let cfg = BuildGateConfig {
            command: "exit 1".to_string(), // would be red if it ran
            timeout: Duration::from_secs(5),
        };
        let tmp = tempfile::tempdir().unwrap();
        let mut runner = CommandGateRunner::new(cfg, tmp.path().to_path_buf());
        let outcome = runner.run_gate();
        assert!(
            outcome.is_skipped(),
            "prep skip must short-circuit before running the command, got {outcome:?}"
        );
    }

    #[test]
    fn test_command_runner_runs_gate_after_successful_prep() {
        // Sync ON against a real on-main clean clone ⇒ prep Ready ⇒ command runs.
        let (_origin, clone) = make_origin_and_clone();
        let cfg = BuildGateConfig {
            command: "exit 0".to_string(),
            timeout: Duration::from_secs(30),
        };
        let mut runner = CommandGateRunner::new(cfg, clone.path().to_path_buf());
        assert_eq!(runner.run_gate(), GateOutcome::Green);
    }

    // ===================================================================
    // Skipped outcome + transition (#3885)
    // ===================================================================

    #[test]
    fn test_skipped_outcome_leaves_halt_flag_unchanged() {
        // From halted: a skip must NOT clear the halt.
        let state = MainHealthState::new();
        state.set_halted(true);
        assert_eq!(
            apply_gate_outcome(&state, &GateOutcome::skipped("dirty")),
            HealthTransition::Skipped
        );
        assert!(state.is_halted(), "skip must not clear an existing halt");

        // From green: a skip must NOT halt.
        let state = MainHealthState::new();
        assert_eq!(
            apply_gate_outcome(&state, &GateOutcome::skipped("offline")),
            HealthTransition::Skipped
        );
        assert!(!state.is_halted(), "skip must not spuriously halt");
    }

    // ===================================================================
    // Skip-warn throttling (#3950 AC2 / AC3): once per clean->dirty
    // transition, then throttled while the tree stays dirty; `is_skipped()`
    // tracks the "not evaluated" status surfaced to `loom-daemon status`.
    // ===================================================================

    #[test]
    fn test_note_gate_tick_warns_once_on_transition_then_throttles() {
        let state = MainHealthState::new();
        let throttle = Duration::from_secs(3600);

        // First dirty tick: clean -> dirty transition, must warn.
        assert!(state.note_gate_tick(true, throttle));
        assert!(state.is_skipped(), "status must reflect the skip");

        // Still dirty, well within the throttle window: must NOT warn again.
        assert!(!state.note_gate_tick(true, throttle));
        assert!(!state.note_gate_tick(true, throttle));
        assert!(state.is_skipped(), "still not-evaluated while throttled");
    }

    #[test]
    fn test_note_gate_tick_warns_again_after_throttle_elapses() {
        let state = MainHealthState::new();
        // A throttle of ~0 means "always past the window" on the very next tick.
        let tiny_throttle = Duration::from_millis(1);

        assert!(state.note_gate_tick(true, tiny_throttle), "first tick always warns");
        std::thread::sleep(Duration::from_millis(5));
        assert!(
            state.note_gate_tick(true, tiny_throttle),
            "a second dirty tick past the throttle window must warn again"
        );
    }

    #[test]
    fn test_note_gate_tick_clears_on_recovery_and_rewarns_on_next_dirty_streak() {
        let state = MainHealthState::new();
        let throttle = Duration::from_secs(3600);

        assert!(state.note_gate_tick(true, throttle));
        assert!(!state.note_gate_tick(true, throttle), "throttled mid-streak");

        // Tree becomes clean again (a completed Green/Red tick) — clears skip
        // status and the throttle timer.
        assert!(!state.note_gate_tick(false, throttle));
        assert!(!state.is_skipped(), "no longer skipped after a completed tick");

        // A NEW dirty streak must warn immediately again, not stay throttled
        // from the previous streak.
        assert!(
            state.note_gate_tick(true, throttle),
            "a fresh dirty streak must warn on its first tick"
        );
    }

    #[test]
    fn test_note_gate_tick_never_warns_when_not_skipped() {
        let state = MainHealthState::new();
        assert!(!state.note_gate_tick(false, Duration::from_secs(3600)));
        assert!(!state.is_skipped());
    }

    #[test]
    fn test_workspace_health_states_is_skipped_tracks_per_root() {
        let states = WorkspaceHealthStates::new();
        let root = Path::new("/repo/a");
        // Never-seen root: not skipped.
        assert!(!states.is_skipped(root));

        states
            .get_or_create(root)
            .note_gate_tick(true, Duration::from_secs(3600));
        assert!(states.is_skipped(root));
        assert!(
            !states.is_skipped(Path::new("/repo/b")),
            "a sibling root's skip state is independent"
        );
    }

    #[test]
    fn test_skipped_outcome_helpers() {
        let s = GateOutcome::skipped("because reasons");
        assert!(s.is_skipped());
        assert!(!s.is_green());
        assert_eq!(s.detail(), "because reasons");
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
