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
//! `buildGate.command` (schema shipped in #3749) against `main`. On a
//! **verified-red** run — the command *ran to completion* and reported failure
//! — it sets a shared halt flag; the [`crate::work_finder`] loop consults that
//! flag and dispatches **zero** new sweeps while halted (existing in-flight
//! sweeps are never killed — halting only stops making a red `main` worse). The
//! next **green** run clears the flag and dispatch resumes on the following
//! work-finder tick.
//!
//! # "Could not run" is not evidence about `main` (#3974)
//!
//! A gate run that never completed — a timeout, `sh` reporting exit 127 because
//! a build tool is not on the daemon's `PATH`, a spawn failure, a broken
//! process tree that kills `git fetch` — tells you **nothing** about `main`'s
//! health. Treating those as red converts every environmental hiccup into a
//! total dispatch outage, and for the repo that contains the gate's own source
//! into a bootstrap deadlock: the daemon cannot dispatch the fix for the thing
//! that is broken. So every outcome is classified as exactly one of:
//!
//! | Outcome | Meaning | Effect on dispatch |
//! |---------|---------|--------------------|
//! | [`GateOutcome::Green`] | ran to completion, checks passed | clears any halt |
//! | [`GateOutcome::Red`] (VERIFIED_RED) | ran to completion, checks failed | **halts** |
//! | [`GateOutcome::Unevaluated`] (UNEVALUATED) | did not run to completion | **preserves the previous verdict**, logs loudly with the failure class |
//!
//! The discriminator is deliberately narrow so a genuinely failing build still
//! halts: only timeouts, exit 126/127, signal deaths, spawn/IO errors, and the
//! pre-run workspace-preparation failures are UNEVALUATED. Any other non-zero
//! exit is a command that ran and reported failure — trusted as VERIFIED_RED.
//!
//! # Forge-CI corroboration (#3974 AC4)
//!
//! One more case looks exactly like VERIFIED_RED from the exit code but is not
//! evidence about the commit: a local run that fails **because of this host**.
//! Observed on the incident host — six `integration_basic` tests assert
//! `tmux_session_exists(...)` and fail because the host's tmux server is dead,
//! while `.github/workflows/ci.yml` runs the identical `cargo test --workspace`
//! on the same commit and passes. The local gate measures *this host*; forge CI
//! measures *the commit*. So a completed-and-failed run is cross-checked against
//! the forge's CI conclusion for the exact `origin/main` SHA the gate evaluated;
//! a **green** CI on that SHA downgrades the outcome to UNEVALUATED
//! ([`UnevaluatedClass::ContradictedByForgeCi`]) instead of halting. CI red or
//! unavailable keeps the halt — only positive contrary evidence relaxes it. See
//! [`CommandGateRunner`].
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

/// Throttle interval for the repeated "gate run UNEVALUATED" log line (#3950
/// AC2). An unevaluable workspace logs once immediately on the
/// evaluated→unevaluated transition, then at most once per this interval while
/// it stays unevaluable — down from logging on *every* ~30s tick (2,000+
/// lines/day observed on the canary host from a single stuck-dirty repo). A
/// **change of failure class** (#3974) also re-warns immediately, so a repo
/// that flips from `dirty-tree` to `timeout` to `command-not-executable` is
/// never silently throttled behind the first class it hit.
const SKIP_WARN_THROTTLE: Duration = Duration::from_secs(3600);

/// Cap on the exponential backoff applied after consecutive UNEVALUATED
/// (indeterminate) gate runs (#3984 AC3) — a gate that cannot finish
/// (timeout, broken `PATH`, dead process tree) must not busy-loop retrying
/// immediately and contending with in-flight sweeps for cores, but also must
/// not go silent forever once the environment recovers.
pub const MAX_GATE_INDETERMINATE_BACKOFF: Duration = Duration::from_secs(3600);

/// Max characters of an UNEVALUATED reason retained for the `loom-daemon
/// status` surface (#3974 AC2). The full reason (which can embed a multi-KB
/// output tail) still goes to the daemon log; the status line only needs
/// enough to name what actually happened.
const MAX_STATUS_REASON_CHARS: usize = 240;

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
    /// Whether autonomous dispatch is currently halted due to a **verified-red**
    /// `main` — a gate command that ran to completion and reported failure.
    halted: AtomicBool,
    /// Whether the most recent gate tick was [`GateOutcome::Unevaluated`] (the
    /// gate could not produce a verdict — dirty tree, timeout, missing tool,
    /// failed `git` step, …). Tracked separately from `halted` so status can
    /// distinguish "not evaluated" from "halted (red main)" (#3950 AC3): the
    /// two can even both be `true` at once (main was verified red before the
    /// environment broke; dispatch remains halted from that prior red run
    /// while evaluation is now impossible). `false` for a fresh state and
    /// after any completed (Green/Red) tick.
    unevaluated: AtomicBool,
    /// Throttle + diagnosis bookkeeping for the UNEVALUATED log line and the
    /// `loom-daemon status` surface — see [`UnevaluatedTrack`].
    track: Mutex<UnevaluatedTrack>,
    /// SHA-memoization + indeterminate-run backoff bookkeeping (#3984) — see
    /// [`GateMemo`].
    gate_memo: Mutex<GateMemo>,
}

/// Bookkeeping for #3984: the SHA of the last **determinate** (Green/Red)
/// gate evaluation — or of the last commit reviewed and found to touch no
/// `realChangeGlobs` path — plus exponential backoff after a run that
/// produced no verdict at all (UNEVALUATED: timeout, missing tool, broken
/// process tree, …).
#[derive(Debug, Default)]
struct GateMemo {
    /// The `origin/main` commit the gate has most recently either (a) run
    /// against and reached a determinate Green/Red verdict for, or (b)
    /// reviewed via `realChangeGlobs` and found to contain no path worth a
    /// re-run. `None` before the first successful evaluation.
    last_evaluated_sha: Option<String>,
    /// The instant before which the gate must not attempt another run,
    /// following a run that produced no verdict. `None` when not backing off.
    backoff_until: Option<Instant>,
    /// Consecutive UNEVALUATED runs since the last determinate one — drives
    /// the exponential backoff growth. Reset to 0 whenever the SHA memo
    /// advances.
    consecutive_indeterminate: u32,
}

/// Bookkeeping for the current UNEVALUATED streak: when its warning was last
/// emitted (throttle, #3950 AC2) and *what* the last failure actually was
/// (#3974 AC2 — so status names the real class instead of always claiming a
/// dirty tree).
#[derive(Debug, Default)]
struct UnevaluatedTrack {
    /// The instant the warning was last emitted for the *current* streak.
    /// Reset to `None` whenever a tick is evaluated, so the next streak logs
    /// immediately again on its first tick.
    last_warn: Option<Instant>,
    /// The most recent UNEVALUATED class + reason, or `None` after any
    /// completed (Green/Red) tick.
    detail: Option<(UnevaluatedClass, String)>,
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
            unevaluated: AtomicBool::new(false),
            track: Mutex::new(UnevaluatedTrack::default()),
            gate_memo: Mutex::new(GateMemo::default()),
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

    /// Whether the most recent gate tick was [`GateOutcome::Unevaluated`] — see
    /// the field doc on [`Self::unevaluated`].
    #[must_use]
    pub fn is_unevaluated(&self) -> bool {
        self.unevaluated.load(Ordering::SeqCst)
    }

    /// The class of the most recent UNEVALUATED tick, or `None` after a
    /// completed (Green/Red) tick.
    #[must_use]
    pub fn unevaluated_class(&self) -> Option<UnevaluatedClass> {
        self.track
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .detail
            .as_ref()
            .map(|(class, _)| *class)
    }

    /// A short `"<class>: <reason>"` summary of the most recent UNEVALUATED
    /// tick for the `loom-daemon status` surface (#3974 AC2), truncated to
    /// [`MAX_STATUS_REASON_CHARS`]. `None` after a completed (Green/Red) tick.
    #[must_use]
    pub fn unevaluated_summary(&self) -> Option<String> {
        let track = self
            .track
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (class, reason) = track.detail.as_ref()?;
        Some(format!("{class}: {}", truncate_chars(reason, MAX_STATUS_REASON_CHARS)))
    }

    /// Record this tick's evaluated/unevaluated outcome and report whether the
    /// UNEVALUATED log line should fire now (#3950 AC2, #3974): `true` exactly
    /// once on an evaluated -> unevaluated transition, again whenever the
    /// failure **class changes** mid-streak (#3974 — a repo that flips from
    /// `dirty-tree` to `timeout` must not stay silent behind the first class),
    /// then at most once per `throttle` while the same class persists. Always
    /// `false` when `unevaluated` is `None`, which also clears the throttle
    /// timer and the stored detail so the next streak warns immediately again.
    /// Call this exactly once per tick, alongside [`apply_gate_outcome`].
    pub fn note_gate_tick(
        &self,
        unevaluated: Option<(UnevaluatedClass, &str)>,
        throttle: Duration,
    ) -> bool {
        let was_unevaluated = self
            .unevaluated
            .swap(unevaluated.is_some(), Ordering::SeqCst);
        let mut track = self
            .track
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some((class, reason)) = unevaluated else {
            track.last_warn = None;
            track.detail = None;
            return false;
        };
        let class_changed = track.detail.as_ref().is_none_or(|(c, _)| *c != class);
        let now = Instant::now();
        let should_warn = !was_unevaluated
            || class_changed
            || track
                .last_warn
                .is_none_or(|t| now.duration_since(t) >= throttle);
        track.detail = Some((class, reason.to_string()));
        if should_warn {
            track.last_warn = Some(now);
        }
        should_warn
    }

    // ===================================================================
    // SHA memoization + indeterminate-run backoff (#3984)
    // ===================================================================

    /// The `origin/main` SHA of the last determinate evaluation (or
    /// glob-reviewed no-op), or `None` before the first one.
    #[must_use]
    pub fn gate_last_evaluated_sha(&self) -> Option<String> {
        self.gate_memo
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last_evaluated_sha
            .clone()
    }

    /// Record that the gate has settled the question for `sha` — either by
    /// running the command and reaching a Green/Red verdict, or by reviewing
    /// the diff since the previous evaluated SHA and finding no
    /// `realChangeGlobs` match. Clears any indeterminate-run backoff, since
    /// this is by definition not an indeterminate outcome.
    pub fn record_gate_evaluated_sha(&self, sha: &str) {
        let mut memo = self
            .gate_memo
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        memo.last_evaluated_sha = Some(sha.to_string());
        memo.backoff_until = None;
        memo.consecutive_indeterminate = 0;
    }

    /// Whether the gate is currently backing off after one or more
    /// consecutive UNEVALUATED runs and must not attempt another run yet.
    #[must_use]
    pub fn gate_backoff_active(&self, now: Instant) -> bool {
        self.gate_memo
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .backoff_until
            .is_some_and(|until| now < until)
    }

    /// Record one more consecutive UNEVALUATED run and extend the backoff
    /// window exponentially: `min_backoff * 2^(consecutive - 1)`, capped at
    /// `max_backoff`. `min_backoff` is typically the gate's own
    /// `buildGate.timeoutSeconds` — after a run that used the *entire*
    /// timeout without producing a verdict, waiting less than that timeout
    /// before retrying guarantees another overlapping/contending run before
    /// the first ever gets to prove anything (the #3984 doom loop).
    pub fn record_gate_indeterminate_backoff(&self, min_backoff: Duration, max_backoff: Duration) {
        let mut memo = self
            .gate_memo
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        memo.consecutive_indeterminate = memo.consecutive_indeterminate.saturating_add(1);
        // Cap the shift so this can never overflow — 2^20 is already far
        // beyond `max_backoff` for any sane config.
        let shift = memo.consecutive_indeterminate.saturating_sub(1).min(20);
        let multiplier = 1u32.checked_shl(shift).unwrap_or(u32::MAX);
        let backoff = min_backoff.saturating_mul(multiplier).min(max_backoff);
        memo.backoff_until = Some(Instant::now() + backoff);
    }
}

/// Truncate `s` to at most `max` **characters** (never splitting a UTF-8
/// boundary), appending an ellipsis marker when anything was dropped.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…")
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

    /// Whether `root`'s most recent gate tick was [`GateOutcome::Unevaluated`]
    /// ("not evaluated", #3950 AC3). A never-seen root reports `false` — no
    /// gate has run against it.
    #[must_use]
    pub fn is_unevaluated(&self, root: &Path) -> bool {
        let map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.get(root).is_some_and(|s| s.is_unevaluated())
    }

    /// A short `"<class>: <reason>"` summary of `root`'s most recent
    /// UNEVALUATED tick (#3974 AC2), or `None` when its last tick completed
    /// (Green/Red) or it has never been gated. Consumed by the daemon-status
    /// surface so it names the *actual* failure instead of always reporting a
    /// dirty tree.
    #[must_use]
    pub fn unevaluated_summary(&self, root: &Path) -> Option<String> {
        let map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.get(root).and_then(|s| s.unevaluated_summary())
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildGateConfig {
    /// The command to run against `main` (executed via `sh -c`).
    pub command: String,
    /// Timeout for a single gate run.
    pub timeout: Duration,
    /// `buildGate.realChangeGlobs` (#3984): when non-empty, a `main` move
    /// that touches none of these glob patterns does not warrant re-running
    /// the (expensive) gate command — the previous verdict stands. Patterns
    /// with no `/` match by basename anywhere in the tree (`*.rs` matches
    /// `loom-daemon/src/main.rs`); patterns containing `/` match the full
    /// repo-relative path. Empty (the default, and the value for any config
    /// that omits the key) means "any `main` move is a real change" — the
    /// pre-#3984 behavior once the SHA has actually changed.
    pub real_change_globs: Vec<String>,
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

    // #3984: `realChangeGlobs` — malformed/non-string entries are dropped
    // rather than failing the whole config (soft-fail contract).
    let real_change_globs = gate
        .get("realChangeGlobs")
        .and_then(serde_json::Value::as_array)
        .map(|globs| {
            globs
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    Some(BuildGateConfig {
        command: command.to_string(),
        timeout: Duration::from_secs(timeout_secs),
        real_change_globs,
    })
}

// ============================================================================
// Gate outcome + runner
// ============================================================================

/// Why a gate run produced **no verdict** about `main` (#3974).
///
/// Every variant means the same thing for the dispatch decision — *the gate did
/// not run to completion, so it learned nothing about `main`* — and therefore
/// leaves the previous verdict untouched. The class exists so the log line and
/// `loom-daemon status` can name the **actual** failure instead of reporting a
/// generic (and, pre-#3974, frequently wrong) "workspace tree is dirty".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnevaluatedClass {
    /// The workspace had non-ignorable local changes, so it was never synced to
    /// `origin/main` and the gate did not run (protects operator edits, #3885).
    DirtyTree,
    /// The workspace was on a branch other than `main` (or a detached HEAD).
    NotOnMain,
    /// The workspace's local `main` carried commits `origin/main` lacks, so the
    /// pre-run hard reset was refused (#3912).
    LocalAhead,
    /// A `git` step of the pre-run workspace preparation failed — `rev-parse`,
    /// `status`, `fetch`, `rev-list`, or `reset`. Includes the environmental
    /// class that motivated #3974: a broken process tree where `git fetch`
    /// exits 128 with "No user exists for uid …".
    GitFailure,
    /// The gate command exceeded `buildGate.timeoutSeconds` and was killed.
    Timeout,
    /// The gate command could not be executed: `sh` reported 127 (command not
    /// found — e.g. `cargo` missing from the daemon's `PATH`) or 126 (found but
    /// not executable).
    NotExecutable,
    /// The gate command was terminated by a signal rather than exiting on its
    /// own (e.g. an OOM kill on a contended host).
    KilledBySignal,
    /// The gate command could not be spawned at all, or an I/O error occurred
    /// while capturing its output / polling for completion.
    SpawnFailure,
    /// The gate command ran to completion and **failed**, but the forge's CI is
    /// green on the very commit it evaluated (#3974 AC4). The two disagree
    /// because they measure different things — CI measures the commit, the local
    /// run measures this host — so the local failure is host-environmental and
    /// is not evidence about `main`.
    ContradictedByForgeCi,
}

impl UnevaluatedClass {
    /// A short, stable, log/status-friendly name for this class.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::DirtyTree => "dirty-tree",
            Self::NotOnMain => "not-on-main",
            Self::LocalAhead => "local-ahead",
            Self::GitFailure => "git-failure",
            Self::Timeout => "timeout",
            Self::NotExecutable => "command-not-executable",
            Self::KilledBySignal => "killed-by-signal",
            Self::SpawnFailure => "spawn-failure",
            Self::ContradictedByForgeCi => "contradicted-by-forge-ci",
        }
    }
}

impl std::fmt::Display for UnevaluatedClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// The result of one gate run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    /// `buildGate.command` exited 0 — `main` is healthy.
    Green,
    /// **VERIFIED_RED**: `buildGate.command` ran to completion and reported
    /// failure (a non-zero exit that is not one of the "could not run" codes —
    /// see [`UnevaluatedClass`]). `detail` is a human-readable reason + a tail
    /// of captured output. This — and only this — halts autonomous dispatch.
    Red { detail: String },
    /// **UNEVALUATED**: the gate produced no verdict about `main`, either
    /// because the workspace could not be prepared to reflect `origin/main`
    /// (dirty tree, not on `main`, a failed `git` step — Issue #3885) or
    /// because the gate command itself never ran to completion (timeout, exit
    /// 126/127, signal death, spawn error — Issue #3974). `class` names the
    /// failure and `reason` explains it.
    ///
    /// An unevaluated run is **indeterminate**: it deliberately leaves the halt
    /// flag exactly as it was rather than greenwashing a stale checkout or
    /// spuriously halting on the gate's own infrastructure failing.
    Unevaluated {
        /// Which "could not run" case this was.
        class: UnevaluatedClass,
        /// Human-readable explanation (paths, exit status, output tail).
        reason: String,
    },
}

impl GateOutcome {
    /// Convenience constructor for a verified-red outcome.
    #[must_use]
    pub fn red(detail: impl Into<String>) -> Self {
        Self::Red {
            detail: detail.into(),
        }
    }

    /// Convenience constructor for an unevaluated (indeterminate) outcome.
    #[must_use]
    pub fn unevaluated(class: UnevaluatedClass, reason: impl Into<String>) -> Self {
        Self::Unevaluated {
            class,
            reason: reason.into(),
        }
    }

    /// True when the run was green.
    #[must_use]
    pub fn is_green(&self) -> bool {
        matches!(self, Self::Green)
    }

    /// True when the run produced no verdict (did not run to completion).
    #[must_use]
    pub fn is_unevaluated(&self) -> bool {
        matches!(self, Self::Unevaluated { .. })
    }

    /// True only for a **verified**-red run — the one outcome that halts
    /// dispatch.
    #[must_use]
    pub fn is_verified_red(&self) -> bool {
        matches!(self, Self::Red { .. })
    }

    /// The failure class of an unevaluated run, or `None` for Green/Red.
    #[must_use]
    pub fn unevaluated_class(&self) -> Option<UnevaluatedClass> {
        match self {
            Self::Unevaluated { class, .. } => Some(*class),
            _ => None,
        }
    }

    /// The red-detail / unevaluated-reason string, or empty for a green
    /// outcome.
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Self::Green => "",
            Self::Red { detail } => detail,
            Self::Unevaluated { reason, .. } => reason,
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
    /// spawn failure or timeout is a [`GateOutcome::Unevaluated`] (the gate
    /// could not run, which says nothing about `main`), **not** a
    /// [`GateOutcome::Red`] (#3974).
    fn run_gate(&mut self) -> GateOutcome;
}

// ============================================================================
// Forge-CI corroboration of a local red (#3974 AC4)
// ============================================================================

/// Environment variable disabling the forge-CI corroboration of a local red
/// (#3974 AC4). Corroboration is **on** by default — it can only ever *relax* a
/// halt, and only on positive contrary evidence — so this exists as an operator
/// kill switch (`0`/`false`/`no`/`off`) for repos with no forge CI or no `gh`.
pub const GATE_CI_CORROBORATION_ENV: &str = "LOOM_GATE_CI_CORROBORATION";

/// How long to wait for the forge-CI probe before giving up (and keeping the
/// local verdict). Deliberately short: this runs inside the gate tick, and an
/// unavailable answer is the safe answer.
const CI_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// How many recent `main` runs to scan for the evaluated SHA.
///
/// At 3-4 workflows per commit this is roughly 8-10 commits of `main` history.
/// On a fast merge cadence the evaluated SHA can age out of the window before a
/// long gate command finishes — which yields [`CiVerdict::Unknown`] and keeps the
/// halt, so the tradeoff fails safe.
const CI_PROBE_RUN_LIMIT: usize = 30;

/// Whether forge-CI corroboration is enabled (default **on**, per
/// [`GATE_CI_CORROBORATION_ENV`]).
#[must_use]
pub fn ci_corroboration_enabled() -> bool {
    std::env::var(GATE_CI_CORROBORATION_ENV).map_or(true, |v| {
        !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off")
    })
}

/// The forge's CI conclusion for one commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiVerdict {
    /// Every completed CI run for the commit succeeded (or was skipped/neutral).
    Success,
    /// At least one completed CI run for the commit failed / timed out.
    Failure,
    /// No answer: no completed run for that commit yet, or the probe itself
    /// failed (`gh` missing, unauthenticated, offline, timed out).
    Unknown,
}

/// Source of the forge's CI conclusion for a commit — abstracted behind a trait
/// so [`CommandGateRunner`]'s corroboration logic is testable without network,
/// `gh`, or credentials (mirroring [`GateRunner`] itself).
pub trait ForgeCiStatus {
    /// The forge's CI conclusion for `sha` in the repo checked out at
    /// `repo_root`. Never errors — an unanswerable probe is
    /// [`CiVerdict::Unknown`].
    fn conclusion_for(&self, repo_root: &Path, sha: &str) -> CiVerdict;
}

/// The concrete [`ForgeCiStatus`]: `gh run list --branch main --json …`,
/// executed in the repo root so `gh` resolves the repository from its git
/// remote. Conclusions are matched on `headSha`, so a run for a *different*
/// commit can never corroborate (or contradict) this one.
pub struct GhForgeCi;

impl ForgeCiStatus for GhForgeCi {
    fn conclusion_for(&self, repo_root: &Path, sha: &str) -> CiVerdict {
        let limit = CI_PROBE_RUN_LIMIT.to_string();
        let args = [
            "run",
            "list",
            "--branch",
            GATE_BRANCH,
            "--limit",
            limit.as_str(),
            "--json",
            "headSha,status,conclusion,workflowName",
        ];
        let stdout = match run_capture_with_timeout("gh", &args, repo_root, CI_PROBE_TIMEOUT) {
            Ok(s) => s,
            Err(e) => {
                log::debug!("main_health_gate: forge CI probe unavailable ({e})");
                return CiVerdict::Unknown;
            }
        };
        parse_gh_run_list(&stdout, sha)
    }
}

/// Parse `gh run list --json headSha,status,conclusion,workflowName` output and
/// reduce the runs for `sha` to a single [`CiVerdict`].
///
/// The reduction is deliberately **asymmetric**, because only positive contrary
/// evidence may ever relax a halt (#3974 AC4). `Success` is the hardest verdict
/// to reach; anything short of an unambiguous all-clear degrades to `Unknown`,
/// which keeps the local red standing:
///
/// | Runs for `sha` | Verdict |
/// |---|---|
/// | any `failure` / `timed_out` / `startup_failure` | `Failure` |
/// | any run not yet `completed` (`queued`, `in_progress`, …) | `Unknown` — CI has not judged the commit yet |
/// | any `cancelled` / `action_required` / `stale` / unrecognized conclusion | `Unknown` — the workflow reached no verdict about the code |
/// | at least one `success`, every other run `skipped` / `neutral` | `Success` |
/// | none of the above (no runs for `sha`, unparseable output) | `Unknown` |
///
/// **Absence of failure is not success.** A commit is green only when some
/// workflow actually concluded `success` *and* no sibling workflow for the same
/// commit is still outstanding or indeterminate. This closes two fail-open paths
/// that a "saw any completed run" reducer has:
///
/// 1. `cancel-in-progress: true` concurrency groups leave superseded runs at
///    `completed/cancelled` **forever**, which would otherwise read as green in
///    perpetuity for that commit.
/// 2. A fast bookkeeping workflow (line counters, labelers) finishing minutes
///    before the real build would otherwise vouch for the commit on its own.
///
/// Requiring every sibling run to have reached a definitive verdict handles both
/// without hard-coding which workflow "counts" — see the PR discussion on #3974.
fn parse_gh_run_list(stdout: &str, sha: &str) -> CiVerdict {
    let Ok(runs) = serde_json::from_str::<Vec<serde_json::Value>>(stdout) else {
        log::debug!("main_health_gate: could not parse `gh run list` output");
        return CiVerdict::Unknown;
    };
    let mut saw_success = false;
    // The first run that reached no verdict about the code, for diagnostics.
    let mut indeterminate: Option<String> = None;
    for run in runs {
        let field = |k: &str| run.get(k).and_then(serde_json::Value::as_str);
        if field("headSha") != Some(sha) {
            continue;
        }
        let workflow = field("workflowName").unwrap_or("<unnamed workflow>");
        let status = field("status").unwrap_or("<no status>");
        if status != "completed" {
            // CI has not judged this commit yet — not evidence in either
            // direction, and specifically not evidence *for* the commit.
            indeterminate.get_or_insert_with(|| format!("{workflow} is {status}"));
            continue;
        }
        match field("conclusion") {
            Some("failure" | "timed_out" | "startup_failure") => {
                log::debug!(
                    "main_health_gate: forge CI red on {sha} — {workflow} concluded \
                     {}",
                    field("conclusion").unwrap_or("failure")
                );
                return CiVerdict::Failure;
            }
            Some("success") => saw_success = true,
            // Definitive "did not apply": a path/branch filter skipped the run,
            // or the workflow deliberately declined to judge. Neither vouches
            // for the commit nor leaves a verdict outstanding.
            Some("skipped" | "neutral") => {}
            // `cancelled` (superseded by a concurrency group), `action_required`
            // (waiting on a human), `stale`, or anything GitHub adds later: the
            // workflow was interrupted before it could judge the code.
            other => {
                let conclusion = other.unwrap_or("<none>");
                indeterminate.get_or_insert_with(|| format!("{workflow} concluded {conclusion}"));
            }
        }
    }
    if let Some(reason) = indeterminate {
        log::debug!(
            "main_health_gate: forge CI verdict for {sha} is indeterminate ({reason}) — \
             treating as unknown, the local result stands"
        );
        return CiVerdict::Unknown;
    }
    if saw_success {
        CiVerdict::Success
    } else {
        // No run for `sha` at all, or every run was skipped/neutral. Either way
        // nothing positively vouches for the commit.
        CiVerdict::Unknown
    }
}

/// Run `program args…` in `cwd`, capturing stdout, killing it after `timeout`.
///
/// stdout goes to a temp file rather than a pipe for the same reason the gate
/// command's does (no pipe-buffer deadlock while polling); stderr is discarded.
fn run_capture_with_timeout(
    program: &str,
    args: &[&str],
    cwd: &Path,
    timeout: Duration,
) -> std::result::Result<String, String> {
    let log_path =
        std::env::temp_dir().join(format!("loom-gate-ci-probe-{}.json", uuid::Uuid::new_v4()));
    let out_file = std::fs::File::create(&log_path)
        .map_err(|e| format!("could not create probe output file: {e}"))?;
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            let _ = std::fs::remove_file(&log_path);
            format!("could not spawn `{program}`: {e}")
        })?;

    let start = Instant::now();
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                break std::fs::read_to_string(&log_path)
                    .map_err(|e| format!("could not read probe output: {e}"));
            }
            Ok(Some(status)) => break Err(format!("`{program}` exited with {status}")),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(format!("`{program}` timed out after {}s", timeout.as_secs()));
                }
                std::thread::sleep(GATE_POLL_INTERVAL);
            }
            Err(e) => break Err(format!("could not poll `{program}`: {e}")),
        }
    };
    let _ = std::fs::remove_file(&log_path);
    result
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
/// failed `git` step yields a [`GateOutcome::Unevaluated`] that leaves the halt
/// flag untouched rather than clobbering operator edits or acting on stale state
/// (Issue #3885).
///
/// Sync can be disabled with [`without_sync`](Self::without_sync) (used by unit
/// tests that exercise command classification against a scratch dir).
///
/// # Forge-CI corroboration of a local red (#3974 AC4)
///
/// A completed-and-failed local run is only evidence about `main` if the local
/// run measures the *commit* rather than *this host*. Observed on the incident
/// host: six `integration_basic` tests assert `tmux_session_exists(...)` and
/// fail because the host's tmux server is dead, while `.github/workflows/ci.yml`
/// runs the identical `cargo test --workspace` and passes. Exit-code inspection
/// alone cannot tell that apart from a real regression.
///
/// So when the local command completes and fails, the runner asks the forge for
/// its CI conclusion on the **same** `origin/main` SHA the gate just evaluated:
///
/// - CI **green** on that SHA ⇒ the divergence is host-environmental. The
///   outcome is downgraded to [`UnevaluatedClass::ContradictedByForgeCi`], logged
///   loudly, and dispatch is **not** halted.
/// - CI **red** on that SHA ⇒ corroborated; still VERIFIED_RED, still halts.
/// - CI **unknown** (no completed run yet, `gh` unavailable/unauthenticated, a
///   probe timeout) ⇒ fail safe: still VERIFIED_RED, still halts. The local
///   result is only ever overridden by *positive* contrary evidence.
pub struct CommandGateRunner {
    config: BuildGateConfig,
    repo_root: PathBuf,
    /// Whether to sync `repo_root` to `origin/main` before each run. `true` in
    /// production (via [`new`](Self::new)); tests opt out with
    /// [`without_sync`](Self::without_sync).
    sync: bool,
    /// Forge-CI corroboration source for a local red (#3974 AC4). Defaults to
    /// [`GhForgeCi`]; tests substitute a scripted fake.
    ci: Box<dyn ForgeCiStatus + Send>,
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
            ci: Box::new(GhForgeCi),
        }
    }

    /// Disable the pre-run `origin/main` sync. Intended for tests that run the
    /// gate command against a non-repo scratch directory; production always syncs.
    #[must_use]
    pub fn without_sync(mut self) -> Self {
        self.sync = false;
        self
    }

    /// Substitute the forge-CI corroboration source (#3974 AC4). Intended for
    /// tests; production uses [`GhForgeCi`].
    #[must_use]
    pub fn with_ci_status(mut self, ci: Box<dyn ForgeCiStatus + Send>) -> Self {
        self.ci = ci;
        self
    }

    /// Cross-check a completed-and-failed local run against the forge's CI
    /// conclusion for the same evaluated commit — see the type-level docs.
    fn corroborate_red(&self, evaluated_sha: Option<&str>, detail: String) -> GateOutcome {
        if !ci_corroboration_enabled() {
            return GateOutcome::red(detail);
        }
        // No SHA ⇒ we cannot ask about "the same commit" (sync disabled, or
        // `git rev-parse` failed). Fail safe: keep the local verdict.
        let Some(sha) = evaluated_sha else {
            return GateOutcome::red(detail);
        };
        match self.ci.conclusion_for(&self.repo_root, sha) {
            CiVerdict::Success => GateOutcome::unevaluated(
                UnevaluatedClass::ContradictedByForgeCi,
                format!(
                    "local gate FAILED but forge CI is GREEN on the very commit it evaluated ({sha}) — \
                     the local run is measuring THIS HOST, not the commit, so it is not evidence \
                     about main (dispatch not halted). Investigate the host: the local failure was: {detail}"
                ),
            ),
            CiVerdict::Failure => GateOutcome::red(format!(
                "{detail}; corroborated — forge CI is also red on the evaluated commit {sha}"
            )),
            CiVerdict::Unknown => GateOutcome::red(format!(
                "{detail}; forge CI conclusion for the evaluated commit {sha} is unavailable, \
                 so the local result stands"
            )),
        }
    }
}

impl GateRunner for CommandGateRunner {
    fn run_gate(&mut self) -> GateOutcome {
        let mut evaluated_sha = None;
        if self.sync {
            match prepare_workspace_to_origin_main(&self.repo_root) {
                PrepOutcome::Skip { class, reason } => {
                    return GateOutcome::unevaluated(class, reason);
                }
                // Post-sync HEAD *is* `origin/main`, so this is exactly the
                // commit the gate command is about to build (#3974 AC4).
                PrepOutcome::Ready => evaluated_sha = resolve_head_sha(&self.repo_root),
            }
        }
        let outcome =
            run_command_with_timeout(&self.config.command, &self.repo_root, self.config.timeout);
        match outcome {
            GateOutcome::Red { detail } => self.corroborate_red(evaluated_sha.as_deref(), detail),
            other => other,
        }
    }
}

/// Resolve `repo_root`'s current HEAD commit SHA, or `None` when `git` fails.
fn resolve_head_sha(repo_root: &Path) -> Option<String> {
    match run_git(repo_root, &["rev-parse", "HEAD"]) {
        Ok((sha, _)) if !sha.is_empty() => Some(sha),
        Ok(_) => None,
        Err(e) => {
            log::debug!("main_health_gate: could not resolve HEAD of {}: {e}", repo_root.display());
            None
        }
    }
}

// ============================================================================
// SHA memoization + `realChangeGlobs` + indeterminate-run backoff (#3984)
//
// #3984 observed a self-sustaining doom loop: `realChangeGlobs` was declared
// in shipped config but never read anywhere, so the gate re-ran its full
// (potentially minutes-long) command every cadence tick regardless of
// whether `origin/main` had moved at all. Under host contention the run
// timed out, the timeout was UNEVALUATED (correctly, per #3974) and left the
// previous halt verdict standing — but the very next tick fired again almost
// immediately (the cadence interval is far shorter than the build timeout),
// so the gate never got a quiet window to actually finish.
//
// [`decide_gate_run`] is the pure decision function: given the last
// determinately-evaluated SHA, the current `origin/main` SHA, and the
// configured globs, does the (expensive) command need to run again at all?
// [`run_gate_tick`] wires that decision, [`MainHealthState`]'s SHA/backoff
// memo, and [`CommandGateRunner`] together into the one entry point
// [`spawn_multi_main_health_gate_task`] calls per root per cadence tick.
// ============================================================================

/// Whether the (expensive) gate command needs to run again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateRunDecision {
    /// `main` has not moved since the last determinate evaluation, or it
    /// moved but the diff touches no `realChangeGlobs` path — the previous
    /// verdict stands and the command must NOT run again.
    Skip,
    /// The command must run: no prior determinate evaluation exists, `main`
    /// moved and no globs are configured (any movement counts), the diff
    /// touches a matching path, or the diff could not be computed (fail
    /// safe — an uncomputable diff must never be mistaken for "no change").
    Run,
}

/// Decide whether the gate command needs to run again, given the SHA
/// `origin/main` currently points at and the config's `realChangeGlobs`
/// (#3984). Pure aside from the `git diff` it shells out to when a
/// glob-filtered re-check is actually needed — the common "main hasn't moved
/// at all" case (`last_evaluated_sha == current_sha`) never touches `git`
/// beyond what the caller already resolved.
#[must_use]
pub fn decide_gate_run(
    last_evaluated_sha: Option<&str>,
    current_sha: &str,
    globs: &[String],
    repo_root: &Path,
) -> GateRunDecision {
    let Some(last) = last_evaluated_sha else {
        return GateRunDecision::Run; // no baseline yet — must evaluate
    };
    if last == current_sha {
        return GateRunDecision::Skip; // main has not moved at all
    }
    if globs.is_empty() {
        return GateRunDecision::Run; // no filter configured — any movement counts
    }
    match diff_touches_globs(repo_root, last, current_sha, globs) {
        Some(true) | None => GateRunDecision::Run,
        Some(false) => GateRunDecision::Skip,
    }
}

/// Cheaply resolve the commit `origin/main` currently points at via
/// `git ls-remote` — no local fetch, no working-tree mutation, safe to call
/// regardless of the workspace's branch or cleanliness. Used to short-circuit
/// the gate command before paying for [`prepare_workspace_to_origin_main`]'s
/// fetch+reset at all. `None` on any failure (offline, no such remote, `git`
/// missing) — callers must fail safe and treat that as "must run".
fn resolve_remote_main_sha(repo_root: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["ls-remote", GATE_REMOTE, GATE_BRANCH])
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let sha = stdout.split_whitespace().next()?;
    if sha.is_empty() {
        None
    } else {
        Some(sha.to_string())
    }
}

/// Whether the diff between `from_sha` and `to_sha` in `repo_root` touches at
/// least one path matching `globs` ([`glob_matches`]). `None` when the diff
/// itself could not be computed (missing object, `git` failure) — callers
/// must fail safe and run the gate rather than risk hiding a real change
/// behind an uncomputable diff.
fn diff_touches_globs(
    repo_root: &Path,
    from_sha: &str,
    to_sha: &str,
    globs: &[String],
) -> Option<bool> {
    // Make sure the local repo actually has both commits to diff — a cheap,
    // idempotent fetch. The caller already knows `main` moved, so this is not
    // extra work beyond what a real run would have paid anyway.
    let _ = Command::new("git")
        .args(["fetch", GATE_REMOTE, GATE_BRANCH])
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let output = Command::new("git")
        .args(["diff", "--name-only", &format!("{from_sha}..{to_sha}")])
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let changed: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    Some(
        changed
            .iter()
            .any(|path| globs.iter().any(|g| glob_matches(g, path))),
    )
}

/// Whether `path` (a repo-relative path, `/`-separated) matches glob `pattern`
/// (`*` = any run of characters, `?` = exactly one character; no other glob
/// syntax is supported — deliberately minimal, matching the shipped
/// `realChangeGlobs` examples `*.rs` / `*.toml` / `Cargo.lock` / `*.py` /
/// `*.sh`). A pattern containing no `/` matches by **basename** anywhere in
/// the tree (so `*.rs` matches `loom-daemon/src/main.rs`); a pattern
/// containing `/` matches the full path.
fn glob_matches(pattern: &str, path: &str) -> bool {
    let candidate = if pattern.contains('/') {
        path
    } else {
        path.rsplit('/').next().unwrap_or(path)
    };
    glob_match_chars(pattern, candidate)
}

/// Classic greedy `*`/`?` wildcard matcher (anchored — the whole `text` must
/// match the whole `pattern`).
fn glob_match_chars(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star_idx: Option<usize> = None;
    let mut match_idx = 0usize;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_idx = Some(pi);
            match_idx = ti;
            pi += 1;
        } else if let Some(si) = star_idx {
            pi = si + 1;
            match_idx += 1;
            ti = match_idx;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// One gate "tick" for a single root (#3984): decides whether the (expensive)
/// command needs to run at all given `state`'s SHA memo + `config`'s
/// `realChangeGlobs`, and — separately — whether the gate is still backing
/// off after a run that produced no verdict. Runs the command (via a fresh
/// [`CommandGateRunner`]) only when needed, and updates `state`'s memo
/// accordingly.
///
/// Returns `None` when the tick was skipped entirely (backoff, or "no real
/// change") — the halt flag is left exactly as it was and there is nothing to
/// log as a transition. Returns `Some(outcome)` when the command actually
/// ran (or [`prepare_workspace_to_origin_main`] itself short-circuited it),
/// for the caller to [`apply_and_log`] as before.
///
/// Synchronous (git + subprocess I/O) by design, mirroring
/// [`CommandGateRunner::run_gate`] — [`spawn_multi_main_health_gate_task`]
/// runs it inside `spawn_blocking`, and it is directly unit-testable without
/// a tokio runtime.
#[must_use]
pub fn run_gate_tick(
    state: &MainHealthState,
    config: &BuildGateConfig,
    repo_root: &Path,
) -> Option<GateOutcome> {
    if state.gate_backoff_active(Instant::now()) {
        log::debug!(
            "main_health_gate: {} gate is backing off after an indeterminate run — skipping this tick",
            repo_root.display()
        );
        return None;
    }

    let current_sha = resolve_remote_main_sha(repo_root);
    if let Some(sha) = current_sha.as_deref() {
        let last = state.gate_last_evaluated_sha();
        if decide_gate_run(last.as_deref(), sha, &config.real_change_globs, repo_root)
            == GateRunDecision::Skip
        {
            log::debug!(
                "main_health_gate: {} skipping gate command — no real change since {} ({sha})",
                repo_root.display(),
                last.as_deref().unwrap_or("<none>")
            );
            state.record_gate_evaluated_sha(sha);
            return None;
        }
    }

    let mut runner = CommandGateRunner::new(config.clone(), repo_root.to_path_buf());
    let outcome = runner.run_gate();
    match &outcome {
        GateOutcome::Green | GateOutcome::Red { .. } => {
            // Prefer the cheaply-resolved SHA; fall back to the workspace's
            // post-sync HEAD (the runner itself resolves this internally for
            // forge-CI corroboration, but does not expose it — re-deriving it
            // here is one more cheap `rev-parse`).
            let sha = current_sha.or_else(|| resolve_head_sha(repo_root));
            if let Some(sha) = sha {
                state.record_gate_evaluated_sha(&sha);
            }
        }
        GateOutcome::Unevaluated { .. } => {
            state.record_gate_indeterminate_backoff(config.timeout, MAX_GATE_INDETERMINATE_BACKOFF);
        }
    }
    Some(outcome)
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
    /// or a failed `git` step). `class` names which case this was (#3974) and
    /// `reason` explains why; the caller should skip the gate run and leave the
    /// halt flag unchanged.
    Skip {
        /// Which "could not run" case blocked preparation.
        class: UnevaluatedClass,
        /// Human-readable explanation, naming the offending paths / `git` error.
        reason: String,
    },
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
                class: UnevaluatedClass::GitFailure,
                reason: format!(
                    "could not determine current branch of {} ({e})",
                    repo_root.display()
                ),
            };
        }
    };
    if branch != GATE_BRANCH {
        return PrepOutcome::Skip {
            class: UnevaluatedClass::NotOnMain,
            reason: format!(
                "workspace {} is on '{branch}', not '{GATE_BRANCH}' — skipping gate (will not reset an operator branch)",
                repo_root.display()
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
                // Name the exact `git status --porcelain` lines, and the root
                // they were read from, so the claim is checkable against
                // `git -C <root> status --porcelain` by hand (#3974 AC2).
                return PrepOutcome::Skip {
                    class: UnevaluatedClass::DirtyTree,
                    reason: format!(
                        "`git -C {} status --porcelain` reports {} non-ignorable change(s) — skipping gate (will not hard-reset over operator edits): [{}]",
                        repo_root.display(),
                        unexpected.len(),
                        unexpected.join(" | ")
                    ),
                };
            }
        }
        Err(e) => {
            return PrepOutcome::Skip {
                class: UnevaluatedClass::GitFailure,
                reason: format!("could not check cleanliness of {} ({e})", repo_root.display()),
            };
        }
    }

    // 3. Fetch origin/main.
    if let Err(e) = run_git(repo_root, &["fetch", GATE_REMOTE, GATE_BRANCH]) {
        return PrepOutcome::Skip {
            class: UnevaluatedClass::GitFailure,
            reason: format!(
                "`git -C {} fetch {GATE_REMOTE} {GATE_BRANCH}` failed ({e}) — skipping gate rather than testing a stale checkout",
                repo_root.display()
            ),
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
                    class: UnevaluatedClass::LocalAhead,
                    reason: format!(
                        "workspace {} '{GATE_BRANCH}' is {out} commit(s) ahead of {remote_ref} — skipping gate (will not hard-reset away local-only commits)",
                        repo_root.display()
                    ),
                };
            }
        }
        Err(e) => {
            return PrepOutcome::Skip {
                class: UnevaluatedClass::GitFailure,
                reason: format!("could not compare {} to {remote_ref} ({e})", repo_root.display()),
            };
        }
    }

    // 5. Hard-reset to the freshly-fetched origin/main.
    if let Err(e) = run_git(repo_root, &["reset", "--hard", &remote_ref]) {
        return PrepOutcome::Skip {
            class: UnevaluatedClass::GitFailure,
            reason: format!(
                "`git -C {} reset --hard {remote_ref}` failed ({e})",
                repo_root.display()
            ),
        };
    }

    PrepOutcome::Ready
}

/// Shell exit status for "command not found" (`sh` convention).
const EXIT_COMMAND_NOT_FOUND: i32 = 127;

/// Shell exit status for "found but not executable" (`sh` convention).
const EXIT_COMMAND_NOT_EXECUTABLE: i32 = 126;

/// `sh`-reported exit statuses for a child killed by `SIGKILL` / `SIGTERM`
/// (`128 + signo`). A build tool essentially never *chooses* these as a real
/// exit status, whereas an OOM kill or an operator/​supervisor `kill` on a
/// contended host produces them routinely — and neither is a statement about
/// `main` (#3974).
const EXIT_SIGKILL: i32 = 137;
const EXIT_SIGTERM: i32 = 143;

/// Classify a **completed** non-zero gate-command exit as VERIFIED_RED (the
/// command ran and reported failure — trust it) or UNEVALUATED (the command
/// could not run — learn nothing), per #3974.
///
/// The UNEVALUATED set is deliberately narrow so a genuinely failing build
/// still halts dispatch: only the `sh` "could not execute" statuses
/// (127/126), signal deaths (a `None` exit code, or `sh`'s `128 + signo`
/// rendering of `SIGKILL`/`SIGTERM`) qualify. Everything else — including
/// `cargo`'s 101 for a failing test — is a command that ran to completion and
/// reported failure.
fn classify_failed_exit(
    command: &str,
    status: &std::process::ExitStatus,
    tail: &str,
) -> GateOutcome {
    let tail = format_tail(tail);
    match status.code() {
        Some(EXIT_COMMAND_NOT_FOUND) => GateOutcome::unevaluated(
            UnevaluatedClass::NotExecutable,
            format!(
                "gate command '{command}' exited 127 (command not found — a tool the gate needs is not on the daemon's PATH); main was NOT evaluated{tail}"
            ),
        ),
        Some(EXIT_COMMAND_NOT_EXECUTABLE) => GateOutcome::unevaluated(
            UnevaluatedClass::NotExecutable,
            format!(
                "gate command '{command}' exited 126 (found but not executable); main was NOT evaluated{tail}"
            ),
        ),
        Some(code @ (EXIT_SIGKILL | EXIT_SIGTERM)) => GateOutcome::unevaluated(
            UnevaluatedClass::KilledBySignal,
            format!(
                "gate command '{command}' was killed by a signal (exit {code}); main was NOT evaluated{tail}"
            ),
        ),
        None => GateOutcome::unevaluated(
            UnevaluatedClass::KilledBySignal,
            format!(
                "gate command '{command}' was terminated by a signal ({status}); main was NOT evaluated{tail}"
            ),
        ),
        Some(_) => GateOutcome::red(format!(
            "gate command '{command}' ran to completion and exited with {status}{tail}"
        )),
    }
}

/// Run `command` (via `sh -c`) in `cwd`, killing it if it exceeds `timeout`.
///
/// Child stdout+stderr are redirected to a single temp file (not a pipe) so a
/// chatty build can never dead-lock us on a full pipe buffer while we poll for
/// completion. The tail of that file is folded into the outcome detail string.
///
/// Only a command that **ran to completion** and exited non-zero yields
/// [`GateOutcome::Red`]. Every failure of the harness itself — the output file,
/// the spawn, the poll, the timeout — yields [`GateOutcome::Unevaluated`]
/// (#3974): those say nothing about `main`, and halting on them turns an
/// environmental hiccup into a total dispatch outage.
fn run_command_with_timeout(command: &str, cwd: &Path, timeout: Duration) -> GateOutcome {
    use std::fs::File;

    let log_path =
        std::env::temp_dir().join(format!("loom-main-health-gate-{}.log", uuid::Uuid::new_v4()));
    let out_file = match File::create(&log_path) {
        Ok(f) => f,
        Err(e) => {
            return GateOutcome::unevaluated(
                UnevaluatedClass::SpawnFailure,
                format!(
                    "failed to create gate output file {}: {e}; main was NOT evaluated",
                    log_path.display()
                ),
            );
        }
    };
    let err_file = match out_file.try_clone() {
        Ok(f) => f,
        Err(e) => {
            let _ = std::fs::remove_file(&log_path);
            return GateOutcome::unevaluated(
                UnevaluatedClass::SpawnFailure,
                format!("failed to clone gate output file handle: {e}; main was NOT evaluated"),
            );
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
            return GateOutcome::unevaluated(
                UnevaluatedClass::SpawnFailure,
                format!("failed to spawn gate command '{command}': {e}; main was NOT evaluated"),
            );
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
                break classify_failed_exit(command, &status, &tail);
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    let tail = read_output_tail(&log_path);
                    break GateOutcome::unevaluated(
                        UnevaluatedClass::Timeout,
                        format!(
                            "gate command '{command}' timed out after {}s and was killed; main was NOT evaluated{}",
                            timeout.as_secs(),
                            format_tail(&tail)
                        ),
                    );
                }
                std::thread::sleep(GATE_POLL_INTERVAL);
            }
            Err(e) => {
                break GateOutcome::unevaluated(
                    UnevaluatedClass::SpawnFailure,
                    format!("failed to poll gate command '{command}': {e}; main was NOT evaluated"),
                );
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
    /// The gate produced no verdict (UNEVALUATED) — the halt flag is left
    /// exactly as it was, so it neither halts nor resumes dispatch (#3885,
    /// #3974).
    Unevaluated,
}

/// Apply a gate `outcome` to the shared `state`, returning the transition.
///
/// Atomic `swap` makes the read-modify-write safe against a concurrent
/// work-finder read (which only ever *loads*). This is the single point that
/// mutates the halt flag.
///
/// **Only a [`GateOutcome::Red`] halts** — i.e. only a gate command that ran to
/// completion and reported failure (#3974). A [`GateOutcome::Unevaluated`] is a
/// no-op: it never touches the flag, so the previous verdict persists until a
/// run actually completes. This is what keeps an environmental failure of the
/// gate itself (timeout, missing tool, broken process tree) from being recorded
/// as evidence that `main` is broken.
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
        GateOutcome::Unevaluated { .. } => HealthTransition::Unevaluated,
    }
}

/// How the current verdict reads in a log line, for an UNEVALUATED tick that
/// left it untouched.
fn verdict_phrase(halted: bool) -> &'static str {
    if halted {
        "dispatch REMAINS HALTED from the previous verified-red run"
    } else {
        "dispatch remains ALLOWED (no previous verified-red run)"
    }
}

/// Apply `outcome` to `state`, then render it via `log_fn` — throttling the
/// repeated "gate run UNEVALUATED" line to at most once per
/// [`SKIP_WARN_THROTTLE`] after its first (evaluated -> unevaluated) occurrence
/// and after any change of failure class (#3950 AC2, #3974). Every other
/// transition (green/red) logs unthrottled via `log_fn` exactly as before.
/// `log_fn` renders the actual `HealthTransition`/`GateOutcome` pair plus the
/// (unchanged) halt verdict — the single- vs multi-workspace loops pass
/// different renderers (the latter also names the repo root).
fn apply_and_log<F>(state: &MainHealthState, outcome: &GateOutcome, log_fn: F)
where
    F: Fn(HealthTransition, &GateOutcome, bool),
{
    let transition = apply_gate_outcome(state, outcome);
    let unevaluated = match outcome {
        GateOutcome::Unevaluated { class, reason } => Some((*class, reason.as_str())),
        _ => None,
    };
    let should_warn = state.note_gate_tick(unevaluated, SKIP_WARN_THROTTLE);
    let halted = state.is_halted();
    if matches!(transition, HealthTransition::Unevaluated) && !should_warn {
        log::debug!(
            "main_health_gate: gate run UNEVALUATED (throttled) [{}] — {} ({})",
            outcome
                .unevaluated_class()
                .map_or("unknown", UnevaluatedClass::label),
            outcome.detail(),
            verdict_phrase(halted)
        );
        return;
    }
    log_fn(transition, outcome, halted);
}

/// Log a health transition at a severity matching its significance.
fn log_transition(transition: HealthTransition, outcome: &GateOutcome, halted: bool) {
    match transition {
        HealthTransition::EnteredHalt => log::error!(
            "main_health_gate: main is VERIFIED RED — HALTING autonomous dispatch. {}",
            outcome.detail()
        ),
        HealthTransition::RemainedHalted => log::warn!(
            "main_health_gate: main still VERIFIED RED — dispatch remains halted. {}",
            outcome.detail()
        ),
        HealthTransition::Recovered => log::info!(
            "main_health_gate: main GREEN again — RESUMING autonomous dispatch on the next work-finder tick"
        ),
        HealthTransition::RemainedHealthy => {
            log::debug!("main_health_gate: main green — dispatch unaffected");
        }
        // Loud, and explicit that this is NOT a statement about main (#3974).
        HealthTransition::Unevaluated => log::warn!(
            "main_health_gate: gate run UNEVALUATED [{}] — {} — this is a failure of the GATE, not evidence about main; {}",
            outcome
                .unevaluated_class()
                .map_or("unknown", UnevaluatedClass::label),
            outcome.detail(),
            verdict_phrase(halted)
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
                    health_state.note_gate_tick(None, SKIP_WARN_THROTTLE);
                    return;
                }
            };
            apply_and_log(&health_state, &outcome, log_transition);
        }
    })
}

/// Log a health transition (root-aware variant, #3930) at a severity matching
/// its significance, naming which repo's `main` the gate evaluated.
fn log_transition_for_root(
    root: &Path,
    transition: HealthTransition,
    outcome: &GateOutcome,
    halted: bool,
) {
    let r = root.display();
    match transition {
        HealthTransition::EnteredHalt => log::error!(
            "main_health_gate: {r} main is VERIFIED RED — HALTING autonomous dispatch for this repo. {}",
            outcome.detail()
        ),
        HealthTransition::RemainedHalted => log::warn!(
            "main_health_gate: {r} main still VERIFIED RED — dispatch for this repo remains halted. {}",
            outcome.detail()
        ),
        HealthTransition::Recovered => log::info!(
            "main_health_gate: {r} main GREEN again — RESUMING dispatch for this repo on the next work-finder tick"
        ),
        HealthTransition::RemainedHealthy => {
            log::debug!("main_health_gate: {r} main green — dispatch unaffected");
        }
        HealthTransition::Unevaluated => log::warn!(
            "main_health_gate: {r} gate run UNEVALUATED [{}] — {} — this is a failure of the GATE, not evidence about main; {}",
            outcome
                .unevaluated_class()
                .map_or("unknown", UnevaluatedClass::label),
            outcome.detail(),
            verdict_phrase(halted)
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
                    state.note_gate_tick(None, SKIP_WARN_THROTTLE);
                    continue;
                }
                let Some(gate_config) = read_build_gate_config(&root) else {
                    log::debug!(
                        "main_health_gate: {} enabled but no usable buildGate config — treating as green",
                        root.display()
                    );
                    let state = health_states.get_or_create(&root);
                    state.set_halted(false);
                    state.note_gate_tick(None, SKIP_WARN_THROTTLE);
                    continue;
                };

                let state = health_states.get_or_create(&root);
                let state_for_task = state.clone();
                let root_for_task = root.clone();
                // Run the (potentially minutes-long) gate off the runtime.
                // `run_gate_tick` (#3984) short-circuits before the expensive
                // command when `origin/main` has not moved (or moved but
                // touched no `realChangeGlobs` path) since the last
                // determinate evaluation, and backs off after a run that
                // produced no verdict rather than retrying immediately.
                let joined = tokio::task::spawn_blocking(move || {
                    run_gate_tick(&state_for_task, &gate_config, &root_for_task)
                })
                .await;
                match joined {
                    Ok(Some(outcome)) => {
                        let root_for_log = root.clone();
                        apply_and_log(&state, &outcome, move |transition, outcome, halted| {
                            log_transition_for_root(&root_for_log, transition, outcome, halted);
                        });
                    }
                    Ok(None) => {
                        // Skipped this tick (#3984: backoff, or no real
                        // change since the last determinate evaluation) —
                        // the halt flag is left exactly as it was, so there
                        // is nothing to log as a transition.
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
                        state.note_gate_tick(None, SKIP_WARN_THROTTLE);
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

    /// Every [`UnevaluatedClass`], so the "must not halt" and label-uniqueness
    /// invariants are checked exhaustively as classes are added.
    const ALL_UNEVALUATED_CLASSES: [UnevaluatedClass; 9] = [
        UnevaluatedClass::DirtyTree,
        UnevaluatedClass::NotOnMain,
        UnevaluatedClass::LocalAhead,
        UnevaluatedClass::GitFailure,
        UnevaluatedClass::Timeout,
        UnevaluatedClass::NotExecutable,
        UnevaluatedClass::KilledBySignal,
        UnevaluatedClass::SpawnFailure,
        UnevaluatedClass::ContradictedByForgeCi,
    ];

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
            ..Default::default()
        };
        let mut runner = CommandGateRunner::new(cfg, std::env::temp_dir()).without_sync();
        assert_eq!(runner.run_gate(), GateOutcome::Green);
    }

    #[test]
    fn test_command_runner_red_on_nonzero_exit_captures_output() {
        let cfg = BuildGateConfig {
            command: "echo build-failed-marker >&2; exit 1".to_string(),
            timeout: Duration::from_secs(30),
            ..Default::default()
        };
        let mut runner = CommandGateRunner::new(cfg, std::env::temp_dir()).without_sync();
        let outcome = runner.run_gate();
        assert!(!outcome.is_green());
        assert!(
            outcome.is_verified_red(),
            "a command that ran and exited 1 is VERIFIED_RED, got {outcome:?}"
        );
        assert!(
            outcome.detail().contains("build-failed-marker"),
            "red detail should include captured output, got: {}",
            outcome.detail()
        );
    }

    // ===================================================================
    // VERIFIED_RED vs UNEVALUATED classification (#3974)
    //
    // The incident: with `origin/main` green on GitHub CI the whole time, a
    // 600s timeout, a `cargo`-not-on-PATH exit 127, and a broken-process-tree
    // `git fetch` failure were each recorded as "main still RED" and halted
    // tier-0 dispatch. None of those is a statement about main.
    // ===================================================================

    #[test]
    fn test_command_runner_timeout_is_unevaluated_not_red() {
        let cfg = BuildGateConfig {
            command: "sleep 10".to_string(),
            timeout: Duration::from_secs(1),
            ..Default::default()
        };
        let mut runner = CommandGateRunner::new(cfg, std::env::temp_dir()).without_sync();
        let outcome = runner.run_gate();
        assert!(!outcome.is_green());
        assert!(
            !outcome.is_verified_red(),
            "a timeout must NOT be verified-red — the gate never finished, so it \
             learned nothing about main; got {outcome:?}"
        );
        assert_eq!(outcome.unevaluated_class(), Some(UnevaluatedClass::Timeout));
        assert!(
            outcome.detail().contains("timed out"),
            "timeout detail expected, got: {}",
            outcome.detail()
        );
    }

    #[test]
    fn test_command_runner_exit_127_is_unevaluated_not_red() {
        // Exit 127 = `sh` could not find the command (the incident's
        // "cargo not on PATH after a launchd migration").
        let cfg = BuildGateConfig {
            command: "loom-no-such-command-3974 --version".to_string(),
            timeout: Duration::from_secs(30),
            ..Default::default()
        };
        let mut runner = CommandGateRunner::new(cfg, std::env::temp_dir()).without_sync();
        let outcome = runner.run_gate();
        assert!(
            !outcome.is_verified_red(),
            "a command that could not be executed must NOT halt dispatch, got {outcome:?}"
        );
        assert_eq!(outcome.unevaluated_class(), Some(UnevaluatedClass::NotExecutable));
    }

    #[test]
    fn test_command_runner_exit_126_is_unevaluated_not_red() {
        // Exit 126 = found but not executable.
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("not-executable.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        let cfg = BuildGateConfig {
            command: script.display().to_string(),
            timeout: Duration::from_secs(30),
            ..Default::default()
        };
        let mut runner = CommandGateRunner::new(cfg, std::env::temp_dir()).without_sync();
        let outcome = runner.run_gate();
        assert!(!outcome.is_verified_red(), "got {outcome:?}");
        assert_eq!(outcome.unevaluated_class(), Some(UnevaluatedClass::NotExecutable));
    }

    #[test]
    fn test_command_runner_signal_death_is_unevaluated_not_red() {
        // An OOM/`kill -9` of the build (exit 137 as `sh` reports it) is an
        // environmental failure, not a failing build.
        let cfg = BuildGateConfig {
            command: "kill -9 $$".to_string(),
            timeout: Duration::from_secs(30),
            ..Default::default()
        };
        let mut runner = CommandGateRunner::new(cfg, std::env::temp_dir()).without_sync();
        let outcome = runner.run_gate();
        assert!(!outcome.is_verified_red(), "got {outcome:?}");
        assert_eq!(outcome.unevaluated_class(), Some(UnevaluatedClass::KilledBySignal));
    }

    #[test]
    fn test_command_runner_cargo_style_failure_is_still_verified_red() {
        // Guard against overcorrection: `cargo test` exits 101 on a genuinely
        // failing test. That command ran to completion and reported failure, so
        // it must still halt dispatch.
        let cfg = BuildGateConfig {
            command: "echo 'test result: FAILED'; exit 101".to_string(),
            timeout: Duration::from_secs(30),
            ..Default::default()
        };
        let mut runner = CommandGateRunner::new(cfg, std::env::temp_dir()).without_sync();
        let outcome = runner.run_gate();
        assert!(
            outcome.is_verified_red(),
            "a completed non-zero exit must remain verified-red, got {outcome:?}"
        );

        // …and it must actually halt.
        let state = MainHealthState::new();
        assert_eq!(apply_gate_outcome(&state, &outcome), HealthTransition::EnteredHalt);
        assert!(state.is_halted());
    }

    #[test]
    fn test_unevaluated_outcomes_never_halt_dispatch() {
        // AC1: each environmental failure class must leave a green verdict
        // green (no spurious halt) — the bootstrap-deadlock fix.
        for class in ALL_UNEVALUATED_CLASSES {
            let state = MainHealthState::new();
            let outcome = GateOutcome::unevaluated(class, "environmental failure");
            assert_eq!(
                apply_gate_outcome(&state, &outcome),
                HealthTransition::Unevaluated,
                "{class} must be unevaluated"
            );
            assert!(
                !state.is_halted(),
                "{class} must not halt dispatch — the gate did not run, so it is not \
                 evidence about main"
            );
        }
    }

    #[test]
    fn test_unevaluated_class_labels_are_distinct() {
        let mut labels: Vec<&str> = ALL_UNEVALUATED_CLASSES.iter().map(|c| c.label()).collect();
        labels.sort_unstable();
        let count = labels.len();
        labels.dedup();
        assert_eq!(labels.len(), count, "every class needs a distinct label");
        // Display renders the label (used verbatim in logs and status).
        assert_eq!(UnevaluatedClass::Timeout.to_string(), "timeout");
    }

    // ===================================================================
    // Forge-CI corroboration of a local red (#3974 AC4)
    //
    // The local gate measures THIS HOST; forge CI measures the COMMIT. On the
    // incident host six `integration_basic` tests assert `tmux_session_exists`
    // and fail because the tmux server is dead, while CI runs the identical
    // `cargo test --workspace` and passes.
    // ===================================================================

    /// A scripted [`ForgeCiStatus`] returning a fixed verdict, recording the
    /// SHA it was asked about.
    struct FakeCi {
        verdict: CiVerdict,
        asked: Arc<Mutex<Vec<String>>>,
    }
    impl ForgeCiStatus for FakeCi {
        fn conclusion_for(&self, _repo_root: &Path, sha: &str) -> CiVerdict {
            self.asked
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(sha.to_string());
            self.verdict
        }
    }

    fn run_gate_with_ci(command: &str, verdict: CiVerdict) -> (GateOutcome, Vec<String>) {
        let (_origin, clone) = make_origin_and_clone();
        let asked = Arc::new(Mutex::new(Vec::new()));
        let cfg = BuildGateConfig {
            command: command.to_string(),
            timeout: Duration::from_secs(30),
            ..Default::default()
        };
        let mut runner = CommandGateRunner::new(cfg, clone.path().to_path_buf()).with_ci_status(
            Box::new(FakeCi {
                verdict,
                asked: Arc::clone(&asked),
            }),
        );
        let outcome = runner.run_gate();
        let asked = asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        (outcome, asked)
    }

    #[test]
    #[serial]
    fn test_local_red_contradicted_by_green_forge_ci_does_not_halt() {
        std::env::remove_var(GATE_CI_CORROBORATION_ENV);
        let (outcome, asked) =
            run_gate_with_ci("echo 'tmux_session_exists failed'; exit 101", CiVerdict::Success);
        assert!(
            !outcome.is_verified_red(),
            "a local failure that CI contradicts on the same commit must not halt, got {outcome:?}"
        );
        assert_eq!(outcome.unevaluated_class(), Some(UnevaluatedClass::ContradictedByForgeCi));
        assert_eq!(asked.len(), 1, "CI is consulted exactly once, for the evaluated SHA");
        assert_eq!(asked[0].len(), 40, "asked about a full commit SHA: {:?}", asked[0]);
        assert!(
            outcome.detail().contains(&asked[0]),
            "the divergence must name the commit, got: {}",
            outcome.detail()
        );

        // And it must leave the halt flag untouched.
        let state = MainHealthState::new();
        assert_eq!(apply_gate_outcome(&state, &outcome), HealthTransition::Unevaluated);
        assert!(!state.is_halted());
    }

    #[test]
    #[serial]
    fn test_local_red_corroborated_by_red_forge_ci_still_halts() {
        std::env::remove_var(GATE_CI_CORROBORATION_ENV);
        let (outcome, _) = run_gate_with_ci("exit 1", CiVerdict::Failure);
        assert!(
            outcome.is_verified_red(),
            "CI agreeing keeps the red — a genuinely broken main must still halt"
        );
        assert!(outcome.detail().contains("corroborated"), "got: {}", outcome.detail());
    }

    #[test]
    #[serial]
    fn test_local_red_with_unknown_forge_ci_still_halts() {
        // Fail safe: only *positive* contrary evidence relaxes a halt.
        std::env::remove_var(GATE_CI_CORROBORATION_ENV);
        let (outcome, _) = run_gate_with_ci("exit 1", CiVerdict::Unknown);
        assert!(outcome.is_verified_red(), "got {outcome:?}");
        assert!(outcome.detail().contains("unavailable"), "got: {}", outcome.detail());
    }

    #[test]
    #[serial]
    fn test_green_local_run_never_consults_forge_ci() {
        std::env::remove_var(GATE_CI_CORROBORATION_ENV);
        let (outcome, asked) = run_gate_with_ci("exit 0", CiVerdict::Failure);
        assert_eq!(outcome, GateOutcome::Green, "a green local run is authoritative");
        assert!(asked.is_empty(), "CI must not be probed on a green run");
    }

    #[test]
    #[serial]
    fn test_ci_corroboration_kill_switch_keeps_local_red() {
        std::env::set_var(GATE_CI_CORROBORATION_ENV, "0");
        let (outcome, asked) = run_gate_with_ci("exit 1", CiVerdict::Success);
        std::env::remove_var(GATE_CI_CORROBORATION_ENV);
        assert!(
            outcome.is_verified_red(),
            "with corroboration disabled the local red stands, got {outcome:?}"
        );
        assert!(asked.is_empty(), "disabled corroboration must not probe the forge");
    }

    #[test]
    #[serial]
    fn test_ci_corroboration_enabled_by_default_and_env_parsing() {
        std::env::remove_var(GATE_CI_CORROBORATION_ENV);
        assert!(ci_corroboration_enabled(), "unset ⇒ on");
        for v in ["0", "false", "no", "off", "OFF", " No "] {
            std::env::set_var(GATE_CI_CORROBORATION_ENV, v);
            assert!(!ci_corroboration_enabled(), "{v:?} should disable");
        }
        for v in ["1", "true", "yes", "on", "anything-else"] {
            std::env::set_var(GATE_CI_CORROBORATION_ENV, v);
            assert!(ci_corroboration_enabled(), "{v:?} should keep it enabled");
        }
        std::env::remove_var(GATE_CI_CORROBORATION_ENV);
    }

    #[test]
    fn test_parse_gh_run_list_verdicts() {
        let sha = "a".repeat(40);
        let other = "b".repeat(40);

        // All completed runs for the SHA succeeded ⇒ green.
        let json = format!(
            r#"[{{"headSha":"{sha}","status":"completed","conclusion":"success","workflowName":"CI"}},
                {{"headSha":"{sha}","status":"completed","conclusion":"skipped","workflowName":"LOC"}}]"#
        );
        assert_eq!(parse_gh_run_list(&json, &sha), CiVerdict::Success);

        // Any completed failure for the SHA ⇒ red.
        let json = format!(
            r#"[{{"headSha":"{sha}","status":"completed","conclusion":"success","workflowName":"CI"}},
                {{"headSha":"{sha}","status":"completed","conclusion":"failure","workflowName":"Sec"}}]"#
        );
        assert_eq!(parse_gh_run_list(&json, &sha), CiVerdict::Failure);

        // Only in-progress runs for the SHA ⇒ unknown (never a silent green).
        let json = format!(
            r#"[{{"headSha":"{sha}","status":"in_progress","conclusion":null,"workflowName":"CI"}}]"#
        );
        assert_eq!(parse_gh_run_list(&json, &sha), CiVerdict::Unknown);

        // A green run for a DIFFERENT commit must never vouch for this one.
        let json = format!(
            r#"[{{"headSha":"{other}","status":"completed","conclusion":"success","workflowName":"CI"}}]"#
        );
        assert_eq!(parse_gh_run_list(&json, &sha), CiVerdict::Unknown);

        // Empty / unparseable output ⇒ unknown.
        assert_eq!(parse_gh_run_list("[]", &sha), CiVerdict::Unknown);
        assert_eq!(parse_gh_run_list("not json", &sha), CiVerdict::Unknown);
    }

    /// Only *positive* contrary evidence may relax a halt (#3974 AC4). These are
    /// the shapes that a "saw any completed run ⇒ green" reducer read as green
    /// even though no workflow ever concluded the commit was good.
    #[test]
    fn test_parse_gh_run_list_non_evidence_is_never_success() {
        let sha = "c".repeat(40);

        // 1. `cancel-in-progress: true` supersedes the previous commit's CI run,
        //    which then sits at completed/cancelled FOREVER. Not a statement
        //    about the code — and crucially not a permanent green.
        let json = format!(
            r#"[{{"headSha":"{sha}","status":"completed","conclusion":"cancelled","workflowName":"CI"}}]"#
        );
        assert_eq!(parse_gh_run_list(&json, &sha), CiVerdict::Unknown);

        // Cancelled CI alongside a completed-success bookkeeping workflow: still
        // unknown. The success does not paper over the missing CI verdict.
        let json = format!(
            r#"[{{"headSha":"{sha}","status":"completed","conclusion":"cancelled","workflowName":"CI"}},
                {{"headSha":"{sha}","status":"completed","conclusion":"success","workflowName":"Lines of Code"}}]"#
        );
        assert_eq!(parse_gh_run_list(&json, &sha), CiVerdict::Unknown);

        // 2. The ~100s window after every push where the fast bookkeeping
        //    workflow has finished but CI is still running.
        let json = format!(
            r#"[{{"headSha":"{sha}","status":"in_progress","conclusion":null,"workflowName":"CI"}},
                {{"headSha":"{sha}","status":"completed","conclusion":"success","workflowName":"Lines of Code"}}]"#
        );
        assert_eq!(parse_gh_run_list(&json, &sha), CiVerdict::Unknown);

        // Queued counts the same as in-progress: not yet a verdict.
        let json = format!(
            r#"[{{"headSha":"{sha}","status":"queued","conclusion":null,"workflowName":"CI"}},
                {{"headSha":"{sha}","status":"completed","conclusion":"success","workflowName":"Lines of Code"}}]"#
        );
        assert_eq!(parse_gh_run_list(&json, &sha), CiVerdict::Unknown);

        // `action_required` (awaiting a human) and `stale` are likewise not
        // statements about the code.
        for conclusion in ["action_required", "stale"] {
            let json = format!(
                r#"[{{"headSha":"{sha}","status":"completed","conclusion":"{conclusion}","workflowName":"CI"}},
                    {{"headSha":"{sha}","status":"completed","conclusion":"success","workflowName":"Lines of Code"}}]"#
            );
            assert_eq!(
                parse_gh_run_list(&json, &sha),
                CiVerdict::Unknown,
                "conclusion {conclusion:?} must not vouch for the commit"
            );
        }

        // An unrecognized future conclusion degrades to unknown, not to green.
        let json = format!(
            r#"[{{"headSha":"{sha}","status":"completed","conclusion":"some_new_thing","workflowName":"CI"}},
                {{"headSha":"{sha}","status":"completed","conclusion":"success","workflowName":"CI"}}]"#
        );
        assert_eq!(parse_gh_run_list(&json, &sha), CiVerdict::Unknown);

        // Absence of failure is not success: every run skipped ⇒ nothing
        // positively vouches for the commit.
        let json = format!(
            r#"[{{"headSha":"{sha}","status":"completed","conclusion":"skipped","workflowName":"CI"}}]"#
        );
        assert_eq!(parse_gh_run_list(&json, &sha), CiVerdict::Unknown);

        // A real failure still wins over any indeterminate sibling — a halt may
        // always be *established*, it just may not be relaxed on non-evidence.
        let json = format!(
            r#"[{{"headSha":"{sha}","status":"in_progress","conclusion":null,"workflowName":"Lint"}},
                {{"headSha":"{sha}","status":"completed","conclusion":"cancelled","workflowName":"Lines of Code"}},
                {{"headSha":"{sha}","status":"completed","conclusion":"failure","workflowName":"CI"}}]"#
        );
        assert_eq!(parse_gh_run_list(&json, &sha), CiVerdict::Failure);

        // And the genuine all-clear still reads green: every workflow for the
        // commit reached a verdict, at least one of them `success`.
        let json = format!(
            r#"[{{"headSha":"{sha}","status":"completed","conclusion":"success","workflowName":"CI"}},
                {{"headSha":"{sha}","status":"completed","conclusion":"success","workflowName":"Lines of Code"}},
                {{"headSha":"{sha}","status":"completed","conclusion":"skipped","workflowName":"Release"}}]"#
        );
        assert_eq!(parse_gh_run_list(&json, &sha), CiVerdict::Success);
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
            PrepOutcome::Skip { class, reason } => {
                assert_eq!(class, UnevaluatedClass::LocalAhead);
                assert!(reason.contains("ahead"), "expected ahead-of-origin reason, got: {reason}");
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
            PrepOutcome::Skip { class, reason } => {
                assert_eq!(class, UnevaluatedClass::DirtyTree);
                // #3974 AC2: the reason must name the root it inspected and the
                // exact porcelain line(s), so the claim is checkable by hand.
                assert!(
                    reason.contains("status --porcelain"),
                    "dirty reason should cite the command it ran, got: {reason}"
                );
                assert!(
                    reason.contains(&clone.path().display().to_string()),
                    "dirty reason should name the root it inspected, got: {reason}"
                );
                assert!(
                    reason.contains("file.txt"),
                    "dirty reason should name the offending path, got: {reason}"
                );
            }
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
            PrepOutcome::Skip { class, reason } => {
                assert_eq!(class, UnevaluatedClass::DirtyTree);
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
            PrepOutcome::Skip { class, reason } => {
                assert_eq!(class, UnevaluatedClass::NotOnMain);
                assert!(
                    reason.contains("feature/x") && reason.contains("not 'main'"),
                    "expected not-on-main reason, got: {reason}"
                );
            }
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
            PrepOutcome::Skip { class, reason } => {
                assert_eq!(
                    class,
                    UnevaluatedClass::GitFailure,
                    "a failed `git fetch` is a gate-infrastructure failure, not a red main (#3974)"
                );
                assert!(reason.contains("fetch"), "expected fetch-failure reason, got: {reason}");
            }
            other => panic!("expected Skip on fetch failure, got {other:?}"),
        }
    }

    #[test]
    fn test_command_runner_returns_unevaluated_when_prep_skips() {
        // Sync ON (production default) against a non-repo dir ⇒ prep skips ⇒ the
        // gate command is NOT run and the outcome is Unevaluated.
        let cfg = BuildGateConfig {
            command: "exit 1".to_string(), // would be red if it ran
            timeout: Duration::from_secs(5),
            ..Default::default()
        };
        let tmp = tempfile::tempdir().unwrap();
        let mut runner = CommandGateRunner::new(cfg, tmp.path().to_path_buf());
        let outcome = runner.run_gate();
        assert!(
            outcome.is_unevaluated(),
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
            ..Default::default()
        };
        let mut runner = CommandGateRunner::new(cfg, clone.path().to_path_buf());
        assert_eq!(runner.run_gate(), GateOutcome::Green);
    }

    // ===================================================================
    // SHA memoization + `realChangeGlobs` + indeterminate-run backoff
    // (#3984) — the doom loop was: the gate re-ran the full (potentially
    // minutes-long) command every cadence tick regardless of whether
    // `origin/main` had actually moved.
    // ===================================================================

    /// Push a commit that writes `filename` with `contents` to `origin/main`
    /// from a scratch clone (mirrors [`advance_origin_main`] but lets tests
    /// control the changed path, for `realChangeGlobs` matching).
    fn push_file_change(origin: &Path, filename: &str, contents: &str) {
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
        let path = scratch.path().join(filename);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, contents).unwrap();
        git(scratch.path(), &["add", "."]);
        git(scratch.path(), &["commit", "-m", &format!("touch {filename}")]);
        git(scratch.path(), &["push", "origin", "main"]);
    }

    #[test]
    fn test_glob_matches_basename_and_full_path() {
        assert!(glob_matches("*.rs", "loom-daemon/src/main.rs"));
        assert!(glob_matches("*.rs", "main.rs"));
        assert!(!glob_matches("*.rs", "main.py"));
        assert!(glob_matches("Cargo.lock", "Cargo.lock"));
        assert!(glob_matches("Cargo.lock", "loom-daemon/Cargo.lock"));
        assert!(!glob_matches("Cargo.lock", "Cargo.toml"));
        // A pattern containing '/' matches the full path, not just basename.
        assert!(glob_matches("src/*.rs", "src/main.rs"));
        assert!(!glob_matches("src/*.rs", "other/main.rs"));
    }

    #[test]
    fn test_decide_gate_run_no_baseline_must_run() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            decide_gate_run(None, "deadbeef", &[], tmp.path()),
            GateRunDecision::Run,
            "no prior determinate evaluation ⇒ must run"
        );
    }

    #[test]
    fn test_decide_gate_run_unchanged_sha_skips_even_with_globs() {
        let tmp = tempfile::tempdir().unwrap();
        let globs = vec!["*.rs".to_string()];
        assert_eq!(
            decide_gate_run(Some("abc123"), "abc123", &globs, tmp.path()),
            GateRunDecision::Skip,
            "identical SHA means no diff at all — must skip regardless of globs"
        );
    }

    #[test]
    fn test_decide_gate_run_changed_sha_no_globs_must_run() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            decide_gate_run(Some("abc123"), "def456", &[], tmp.path()),
            GateRunDecision::Run,
            "no realChangeGlobs configured ⇒ any movement counts as real"
        );
    }

    #[test]
    fn test_decide_gate_run_changed_sha_glob_diff_matches() {
        let (origin, clone) = make_origin_and_clone();
        let before = head_commit(clone.path());
        push_file_change(origin.path(), "src/lib.rs", "fn x() {}\n");
        // Fetch so the clone's local git has the new commit object available
        // for the diff — mirrors what `resolve_remote_main_sha` + the
        // subsequent fetch inside `diff_touches_globs` do in production.
        git(clone.path(), &["fetch", "origin", "main"]);
        let after = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "origin/main"])
                .current_dir(clone.path())
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        let globs = vec!["*.rs".to_string()];
        assert_eq!(
            decide_gate_run(Some(&before), &after, &globs, clone.path()),
            GateRunDecision::Run,
            "the diff touches a *.rs path — must run"
        );
    }

    #[test]
    fn test_decide_gate_run_changed_sha_glob_diff_does_not_match() {
        let (origin, clone) = make_origin_and_clone();
        let before = head_commit(clone.path());
        push_file_change(origin.path(), "README.md", "docs only\n");
        git(clone.path(), &["fetch", "origin", "main"]);
        let after = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "origin/main"])
                .current_dir(clone.path())
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        let globs = vec!["*.rs".to_string(), "*.toml".to_string()];
        assert_eq!(
            decide_gate_run(Some(&before), &after, &globs, clone.path()),
            GateRunDecision::Skip,
            "the diff touches only README.md — no configured glob matches, must skip"
        );
    }

    #[test]
    fn test_main_health_state_gate_backoff_grows_and_clears() {
        let state = MainHealthState::new();
        let now = Instant::now();
        assert!(!state.gate_backoff_active(now), "fresh state is never backing off");

        let min = Duration::from_secs(60);
        let max = Duration::from_secs(3600);
        state.record_gate_indeterminate_backoff(min, max);
        assert!(
            state.gate_backoff_active(Instant::now()),
            "one indeterminate run must start a backoff window"
        );

        // A determinate evaluation clears the backoff outright.
        state.record_gate_evaluated_sha("abc123");
        assert!(
            !state.gate_backoff_active(Instant::now()),
            "a determinate evaluation must clear any standing backoff"
        );
        assert_eq!(state.gate_last_evaluated_sha(), Some("abc123".to_string()));
    }

    #[test]
    fn test_run_gate_tick_skips_second_run_for_unchanged_sha() {
        // The core #3984 regression: with `origin/main` unchanged between two
        // ticks, the second tick must NOT spawn the gate command again.
        let (_origin, clone) = make_origin_and_clone();
        let marker = tempfile::tempdir().unwrap();
        let marker_file = marker.path().join("invocations.txt");
        let cfg = BuildGateConfig {
            command: format!("echo run >> {}", marker_file.display()),
            timeout: Duration::from_secs(30),
            ..Default::default()
        };
        let state = MainHealthState::new();

        let first = run_gate_tick(&state, &cfg, clone.path());
        assert_eq!(first, Some(GateOutcome::Green), "first tick must run and be green");
        let invocations_after_first = std::fs::read_to_string(&marker_file)
            .unwrap_or_default()
            .lines()
            .count();
        assert_eq!(invocations_after_first, 1, "the command must have run exactly once");

        let second = run_gate_tick(&state, &cfg, clone.path());
        assert_eq!(
            second, None,
            "unchanged origin/main must skip the second tick entirely (no outcome to apply)"
        );
        let invocations_after_second = std::fs::read_to_string(&marker_file)
            .unwrap_or_default()
            .lines()
            .count();
        assert_eq!(
            invocations_after_second, 1,
            "no second gate command must be spawned for an unchanged SHA"
        );
    }

    #[test]
    fn test_run_gate_tick_runs_again_after_main_advances() {
        let (origin, clone) = make_origin_and_clone();
        let marker = tempfile::tempdir().unwrap();
        let marker_file = marker.path().join("invocations.txt");
        let cfg = BuildGateConfig {
            command: format!("echo run >> {}", marker_file.display()),
            timeout: Duration::from_secs(30),
            ..Default::default()
        };
        let state = MainHealthState::new();

        assert_eq!(run_gate_tick(&state, &cfg, clone.path()), Some(GateOutcome::Green));
        assert_eq!(
            std::fs::read_to_string(&marker_file)
                .unwrap_or_default()
                .lines()
                .count(),
            1
        );

        // main moves — the next tick must run again.
        advance_origin_main(origin.path());
        assert_eq!(run_gate_tick(&state, &cfg, clone.path()), Some(GateOutcome::Green));
        assert_eq!(
            std::fs::read_to_string(&marker_file)
                .unwrap_or_default()
                .lines()
                .count(),
            2,
            "a real change to origin/main must trigger another run"
        );
    }

    #[test]
    fn test_run_gate_tick_skips_while_backing_off_after_timeout() {
        let (_origin, clone) = make_origin_and_clone();
        let marker = tempfile::tempdir().unwrap();
        let marker_file = marker.path().join("invocations.txt");
        let cfg = BuildGateConfig {
            command: format!("echo run >> {} && sleep 5", marker_file.display()),
            timeout: Duration::from_millis(200),
            ..Default::default()
        };
        let state = MainHealthState::new();

        let first = run_gate_tick(&state, &cfg, clone.path());
        assert!(
            matches!(first, Some(GateOutcome::Unevaluated { .. })),
            "a timeout must be UNEVALUATED, got {first:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&marker_file)
                .unwrap_or_default()
                .lines()
                .count(),
            1
        );

        // Immediately retrying (well within the backoff window derived from
        // the 200ms timeout) must be skipped — no second spawn.
        let second = run_gate_tick(&state, &cfg, clone.path());
        assert_eq!(
            second, None,
            "an indeterminate run must trigger backoff, not an immediate retry"
        );
        assert_eq!(
            std::fs::read_to_string(&marker_file)
                .unwrap_or_default()
                .lines()
                .count(),
            1,
            "no second gate command must be spawned while backing off"
        );
    }

    // ===================================================================
    // Unevaluated outcome + transition (#3885, reclassified in #3974)
    // ===================================================================

    /// A dirty-tree helper matching the pre-#3974 `skipped(reason)` shape.
    fn dirty(reason: &str) -> GateOutcome {
        GateOutcome::unevaluated(UnevaluatedClass::DirtyTree, reason)
    }

    #[test]
    fn test_unevaluated_outcome_leaves_halt_flag_unchanged() {
        // From halted: an unevaluated tick must NOT clear the halt.
        let state = MainHealthState::new();
        state.set_halted(true);
        assert_eq!(apply_gate_outcome(&state, &dirty("dirty")), HealthTransition::Unevaluated);
        assert!(state.is_halted(), "unevaluated must not clear an existing halt");

        // From green: an unevaluated tick must NOT halt.
        let state = MainHealthState::new();
        assert_eq!(
            apply_gate_outcome(
                &state,
                &GateOutcome::unevaluated(UnevaluatedClass::GitFailure, "offline")
            ),
            HealthTransition::Unevaluated
        );
        assert!(!state.is_halted(), "unevaluated must not spuriously halt");
    }

    // ===================================================================
    // Unevaluated-warn throttling (#3950 AC2 / AC3, extended in #3974):
    // once per evaluated->unevaluated transition, once more on any change of
    // failure class, then throttled; `is_unevaluated()` + the stored class
    // back the "not evaluated" status surfaced to `loom-daemon status`.
    // ===================================================================

    #[test]
    fn test_note_gate_tick_warns_once_on_transition_then_throttles() {
        let state = MainHealthState::new();
        let throttle = Duration::from_secs(3600);
        let d = Some((UnevaluatedClass::DirtyTree, "dirty"));

        // First dirty tick: clean -> dirty transition, must warn.
        assert!(state.note_gate_tick(d, throttle));
        assert!(state.is_unevaluated(), "status must reflect the skip");

        // Still dirty, well within the throttle window: must NOT warn again.
        assert!(!state.note_gate_tick(d, throttle));
        assert!(!state.note_gate_tick(d, throttle));
        assert!(state.is_unevaluated(), "still not-evaluated while throttled");
    }

    #[test]
    fn test_note_gate_tick_warns_again_after_throttle_elapses() {
        let state = MainHealthState::new();
        // A throttle of ~0 means "always past the window" on the very next tick.
        let tiny_throttle = Duration::from_millis(1);
        let d = Some((UnevaluatedClass::DirtyTree, "dirty"));

        assert!(state.note_gate_tick(d, tiny_throttle), "first tick always warns");
        std::thread::sleep(Duration::from_millis(5));
        assert!(
            state.note_gate_tick(d, tiny_throttle),
            "a second dirty tick past the throttle window must warn again"
        );
    }

    #[test]
    fn test_note_gate_tick_rewarns_immediately_on_class_change() {
        // #3974: the incident rotated through timeout / exit-101 / exit-127 /
        // git-fetch failure. A new failure class must never be swallowed by the
        // previous class's throttle window.
        let state = MainHealthState::new();
        let throttle = Duration::from_secs(3600);

        assert!(state.note_gate_tick(Some((UnevaluatedClass::DirtyTree, "dirty")), throttle));
        assert!(!state.note_gate_tick(Some((UnevaluatedClass::DirtyTree, "dirty")), throttle));
        assert!(
            state.note_gate_tick(Some((UnevaluatedClass::Timeout, "timed out")), throttle),
            "a different failure class must warn immediately, not stay throttled"
        );
        assert_eq!(state.unevaluated_class(), Some(UnevaluatedClass::Timeout));
    }

    #[test]
    fn test_note_gate_tick_clears_on_recovery_and_rewarns_on_next_dirty_streak() {
        let state = MainHealthState::new();
        let throttle = Duration::from_secs(3600);
        let d = Some((UnevaluatedClass::DirtyTree, "dirty"));

        assert!(state.note_gate_tick(d, throttle));
        assert!(!state.note_gate_tick(d, throttle), "throttled mid-streak");

        // Tree becomes clean again (a completed Green/Red tick) — clears skip
        // status, the stored detail, and the throttle timer.
        assert!(!state.note_gate_tick(None, throttle));
        assert!(!state.is_unevaluated(), "no longer skipped after a completed tick");
        assert_eq!(state.unevaluated_class(), None);
        assert_eq!(state.unevaluated_summary(), None);

        // A NEW dirty streak must warn immediately again, not stay throttled
        // from the previous streak.
        assert!(
            state.note_gate_tick(d, throttle),
            "a fresh dirty streak must warn on its first tick"
        );
    }

    #[test]
    fn test_note_gate_tick_never_warns_when_evaluated() {
        let state = MainHealthState::new();
        assert!(!state.note_gate_tick(None, Duration::from_secs(3600)));
        assert!(!state.is_unevaluated());
    }

    #[test]
    fn test_unevaluated_summary_names_class_and_reason() {
        // #3974 AC2: status must be able to name the *actual* failure rather
        // than always claiming the workspace tree is dirty.
        let state = MainHealthState::new();
        state.note_gate_tick(
            Some((UnevaluatedClass::GitFailure, "`git fetch origin main` failed")),
            Duration::from_secs(3600),
        );
        let summary = state.unevaluated_summary().unwrap();
        assert!(summary.starts_with("git-failure: "), "got: {summary}");
        assert!(summary.contains("git fetch origin main"), "got: {summary}");
    }

    #[test]
    fn test_unevaluated_summary_truncates_long_reasons() {
        let state = MainHealthState::new();
        let long = "x".repeat(MAX_STATUS_REASON_CHARS * 3);
        state.note_gate_tick(Some((UnevaluatedClass::Timeout, &long)), Duration::from_secs(3600));
        let summary = state.unevaluated_summary().unwrap();
        assert!(
            summary.chars().count() <= MAX_STATUS_REASON_CHARS + 32,
            "status reason must stay short, got {} chars",
            summary.chars().count()
        );
        assert!(summary.ends_with('…'), "truncation marker expected, got: {summary}");
    }

    #[test]
    fn test_workspace_health_states_is_unevaluated_tracks_per_root() {
        let states = WorkspaceHealthStates::new();
        let root = Path::new("/repo/a");
        // Never-seen root: not skipped.
        assert!(!states.is_unevaluated(root));
        assert_eq!(states.unevaluated_summary(root), None);

        states
            .get_or_create(root)
            .note_gate_tick(Some((UnevaluatedClass::Timeout, "slow")), Duration::from_secs(3600));
        assert!(states.is_unevaluated(root));
        assert_eq!(states.unevaluated_summary(root), Some("timeout: slow".to_string()));
        assert!(
            !states.is_unevaluated(Path::new("/repo/b")),
            "a sibling root's skip state is independent"
        );
    }

    #[test]
    fn test_unevaluated_outcome_helpers() {
        let s = GateOutcome::unevaluated(UnevaluatedClass::DirtyTree, "because reasons");
        assert!(s.is_unevaluated());
        assert!(!s.is_green());
        assert!(!s.is_verified_red());
        assert_eq!(s.unevaluated_class(), Some(UnevaluatedClass::DirtyTree));
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
