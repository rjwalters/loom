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

use chrono::{DateTime, Utc};

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

/// Environment variable overriding whether the work-finder suppresses dispatch
/// into a root whose build-gate run is in flight (#4084). Precedence env >
/// config > default; truthy (`1`/`true`/`yes`/`on`) enables, anything else
/// disables. Overrides `autonomous.mainHealthGate.suppressDispatchDuringGate`.
pub const MAIN_HEALTH_GATE_SUPPRESS_DISPATCH_ENV: &str = "LOOM_MAIN_HEALTH_GATE_SUPPRESS_DISPATCH";

/// Default for `autonomous.mainHealthGate.suppressDispatchDuringGate` (#4084):
/// on. Suppressing new dispatch into a root while its own gate build runs is
/// the mechanism that keeps a gate run from losing its CPU race against
/// concurrently-dispatched sweep builds. Set the knob/env to `false` to opt out
/// (e.g. to restore the pre-#4084 always-dispatch behavior).
pub const DEFAULT_SUPPRESS_DISPATCH_DURING_GATE: bool = true;

/// Default gate cadence. Tighter than the work-finder's 60s default — a red
/// `main` should be caught (and dispatch halted) promptly — while still keeping
/// build volume low.
pub const DEFAULT_MAIN_HEALTH_GATE_INTERVAL_SECS: u64 = 30;

/// Environment variable naming the tier the gate command should run as (#4259).
/// The daemon sets this to `fast` when invoking the fast-tier command (see
/// [`resolve_gate_fast_command`]); `build-gate.sh` reads it. Precedence for the
/// *decision* to use the fast tier is daemon-side (load); this env var only
/// carries that decision into the command.
pub const BUILD_GATE_TIER_ENV: &str = "LOOM_BUILD_GATE_TIER";

/// Environment variable overriding the load-average-per-CPU saturation
/// threshold for build-gate deferral (#4259). Precedence env > config
/// (`buildGate.loadThreshold`) > default
/// ([`crate::cpu_headroom::DEFAULT_GATE_LOAD_THRESHOLD`]).
pub const BUILD_GATE_LOAD_THRESHOLD_ENV: &str = "LOOM_BUILD_GATE_LOAD_THRESHOLD";

/// Environment variable overriding the bounded max-defer window in seconds
/// (#4259) — after this many seconds of consecutive load-deferred ticks the
/// gate runs the FAST tier regardless of load, so a permanently-saturated host
/// still reaches a verdict. Precedence env > config (`buildGate.maxDeferSeconds`)
/// > default ([`DEFAULT_GATE_MAX_DEFER_SECS`]).
pub const BUILD_GATE_MAX_DEFER_ENV: &str = "LOOM_BUILD_GATE_MAX_DEFER_SECS";

/// Default bounded max-defer window (#4259): 30 minutes. Deferral must be
/// bounded — an unbounded defer on a host that is *always* at cap (the #4259
/// reproduction host runs 6–7 sweeps around the clock) would re-disable the
/// gate exactly like the 1200s timeout does, only more cheaply. After this
/// window the fast tier runs regardless of load, guaranteeing a verdict at
/// least every `DEFAULT_GATE_MAX_DEFER_SECS`.
pub const DEFAULT_GATE_MAX_DEFER_SECS: u64 = 1800;

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
    /// Wall-clock time of the most recent **completed** (Green/Red) gate
    /// verdict, or `None` before the first one this process (#4012). This is
    /// the disambiguator between "pending" (gate enabled, no verdict yet —
    /// the ambiguous pre-#4012 `(false, false)` reading) and "clear"
    /// (verified green), plus the recency evidence a `clear` reading
    /// otherwise lacks. Stamped only in [`apply_gate_outcome`]'s Green/Red
    /// arms — deliberately **not** touched by the #3984 SHA-memo skip path
    /// ([`run_gate_tick`]'s early return), since a skip proves nothing new
    /// about `main` and stamping it there would let a stale "last verified"
    /// silently refresh itself forever.
    last_verdict_at: Mutex<Option<DateTime<Utc>>>,
    /// Whether a build-gate *run* against this root is currently in flight
    /// (#4084) — set for the lifetime of the `spawn_blocking(run_gate_tick)`
    /// call and cleared the instant it returns or panics (via
    /// [`GateInFlightGuard`]). Distinct from `halted`, which reflects a
    /// *completed* red verdict: this flag reflects that the gate's own
    /// (possibly minutes-long) build is executing right now. The work-finder
    /// reads it as an additional dispatch suppressor so it does not dispatch
    /// new sweeps into a root whose gate build is already competing for the
    /// same cores — the CPU contention that made a `nice -n 5` gate still time
    /// out under 2 concurrent sweeps (#4073 was necessary, not sufficient).
    /// `false` for a fresh state and whenever no gate run is executing.
    ///
    /// This is an **in-process** flag, scoped to one daemon and one root. The
    /// cross-process, machine-wide equivalent — which serializes this gate's
    /// build against `cargo`/build-gate stages in *other* processes (every
    /// concurrent sweep worktree, another workspace's gate) — is the build slot
    /// ([`crate::build_slot`], #4512), taken around the gate command itself.
    /// They compose: this flag holds *dispatch* off a root with a live gate; the
    /// slot holds *concurrent heavy builds* off each other machine-wide.
    gate_in_flight: AtomicBool,
    /// The current load-deferral streak (#4259), or `None` when the gate is not
    /// deferring. A deferred tick is a *scheduling* decision, not an
    /// evaluation: it never touches `halted`, the SHA memo, or the
    /// indeterminate backoff. The streak's start (`since_instant`) bounds the
    /// deferral against `maxDeferSeconds` so a permanently-saturated host still
    /// reaches a verdict; its wall-clock `since` + `load_ratio` feed the
    /// `loom-daemon status` `deferred (load …)` line, kept distinct from the
    /// UNEVALUATED `not evaluated (timeout …)` line.
    defer: Mutex<Option<DeferState>>,
    /// The tier of the most recent completed (Green/Red) verdict (#4259), for
    /// the status/log tier label so a fast-tier Green is never read as a
    /// full-suite Green. `None` before the first completed run.
    last_tier: Mutex<Option<GateTier>>,
}

/// Bookkeeping for the current load-deferral streak (#4259): when it began (a
/// monotonic [`Instant`] for the max-defer bound and a wall-clock
/// [`DateTime`](chrono::DateTime) for the status surface), the load-per-core
/// ratio that triggered it, and the configured bound (so the status summary is
/// self-contained without re-reading config).
#[derive(Debug, Clone)]
struct DeferState {
    since_instant: Instant,
    since: DateTime<Utc>,
    load_ratio: f64,
    max_defer: Duration,
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
            last_verdict_at: Mutex::new(None),
            gate_in_flight: AtomicBool::new(false),
            defer: Mutex::new(None),
            last_tier: Mutex::new(None),
        }
    }

    // ===================================================================
    // Load-aware deferral + tier bookkeeping (#4259)
    // ===================================================================

    /// Whether the gate is currently deferring for host load (#4259).
    #[must_use]
    pub fn is_deferred(&self) -> bool {
        self.defer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    /// Elapsed time since the current deferral streak began, or `None` when not
    /// deferring — the bound checked by [`decide_gate_tier`].
    #[must_use]
    pub fn defer_streak_elapsed(&self) -> Option<Duration> {
        self.defer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|d| d.since_instant.elapsed())
    }

    /// Wall-clock time the current deferral streak began, or `None`.
    #[must_use]
    pub fn deferred_since(&self) -> Option<DateTime<Utc>> {
        self.defer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|d| d.since)
    }

    /// A short human summary of the current deferral for `loom-daemon status`
    /// (#4259), deliberately distinct from the UNEVALUATED `not evaluated (…)`
    /// summary. `None` when not deferring.
    #[must_use]
    pub fn deferred_summary(&self) -> Option<String> {
        let guard = self
            .defer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let d = guard.as_ref()?;
        let mins = d.since_instant.elapsed().as_secs() / 60;
        let bound_mins = d.max_defer.as_secs() / 60;
        Some(format!(
            "load {:.2}/core for {mins}m — fast tier runs at the {bound_mins}m bound",
            d.load_ratio
        ))
    }

    /// Record a load-deferral for this tick, starting a new streak if one is not
    /// already in progress. Returns `true` iff this *started* a new streak (so
    /// the caller logs the transition exactly once, #4083 visibility). Does NOT
    /// touch `halted`, the SHA memo, or the backoff — a deferral is a scheduling
    /// decision, not an evaluation (#4259).
    pub fn record_gate_deferred(&self, load_ratio: f64, max_defer: Duration) -> bool {
        let mut guard = self
            .defer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.is_some() {
            return false;
        }
        *guard = Some(DeferState {
            since_instant: Instant::now(),
            since: Utc::now(),
            load_ratio,
            max_defer,
        });
        true
    }

    /// End any deferral streak (the gate is about to run, or load dropped below
    /// the threshold).
    pub fn clear_gate_deferred(&self) {
        *self
            .defer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// Record the tier of a just-completed (Green/Red) verdict (#4259).
    pub fn record_gate_tier(&self, tier: GateTier) {
        *self
            .last_tier
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tier);
    }

    /// The tier of the most recent completed verdict, or `None` before the first.
    #[must_use]
    pub fn gate_last_tier(&self) -> Option<GateTier> {
        *self
            .last_tier
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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

    /// Whether a build-gate run against this root is currently in flight
    /// (#4084) — see the field doc on [`Self::gate_in_flight`]. Read by the
    /// work-finder as a dispatch suppressor alongside [`Self::is_halted`].
    #[must_use]
    pub fn is_gate_in_flight(&self) -> bool {
        self.gate_in_flight.load(Ordering::SeqCst)
    }

    /// Mark whether a gate run is in flight. Prefer [`GateInFlightGuard`] over
    /// calling this directly, so the flag is cleared on every exit path —
    /// including a panic unwind — and can never latch dispatch off (#4084).
    pub fn set_gate_in_flight(&self, in_flight: bool) {
        self.gate_in_flight.store(in_flight, Ordering::SeqCst);
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

    /// Wall-clock time of the most recent completed (Green/Red) gate verdict,
    /// or `None` if none has landed yet this process (#4012). See the field
    /// doc on [`Self::last_verdict_at`].
    #[must_use]
    pub fn last_verdict_at(&self) -> Option<DateTime<Utc>> {
        *self
            .last_verdict_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Record `at` as the time of a just-completed (Green/Red) gate verdict.
    /// Called from [`apply_gate_outcome`] only — never from the SHA-memo skip
    /// path, which does not represent a completed run.
    fn record_verdict_at(&self, at: DateTime<Utc>) {
        *self
            .last_verdict_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(at);
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

    /// Whether a build-gate run against `root` is currently in flight (#4084).
    /// A never-seen root reports `false` — no gate has run against it. Read by
    /// the work-finder as a dispatch suppressor alongside [`Self::is_halted`].
    #[must_use]
    pub fn is_gate_in_flight(&self, root: &Path) -> bool {
        let map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.get(root).is_some_and(|s| s.is_gate_in_flight())
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

    /// Whether `root`'s most recent gate tick deferred for host load (#4259). A
    /// never-seen root reports `false`.
    #[must_use]
    pub fn is_deferred(&self, root: &Path) -> bool {
        let map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.get(root).is_some_and(|s| s.is_deferred())
    }

    /// A short `load …` summary of `root`'s current load-deferral (#4259), or
    /// `None` when it is not deferring (or has never been gated). Rendered by
    /// the daemon-status surface distinctly from the UNEVALUATED reason.
    #[must_use]
    pub fn deferred_summary(&self, root: &Path) -> Option<String> {
        let map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.get(root).and_then(|s| s.deferred_summary())
    }

    /// The tier of `root`'s most recent completed verdict (#4259), or `None`.
    #[must_use]
    pub fn gate_last_tier(&self, root: &Path) -> Option<GateTier> {
        let map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.get(root).and_then(|s| s.gate_last_tier())
    }

    /// Wall-clock time of `root`'s most recent completed gate verdict
    /// (#4012), or `None` when it has never produced one this process (or the
    /// root has never been seen — deliberately the same "pending" reading as
    /// a registered-but-not-yet-evaluated root, per the field doc on
    /// [`MainHealthState::last_verdict_at`]).
    #[must_use]
    pub fn last_verdict_at(&self, root: &Path) -> Option<DateTime<Utc>> {
        let map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.get(root).and_then(|s| s.last_verdict_at())
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
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let gate = crate::config_resolver::get_path(&effective, "buildGate")?;

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
// Tiered gate + load-aware deferral (#4259)
// ============================================================================

/// Which stage set a gate run executed (#4259). The daemon selects the tier by
/// host load in [`run_gate_tick`]; the label rides on the verdict so a fast-tier
/// Green is never mistaken for a full-suite Green.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateTier {
    /// The full `buildGate.command` (every stage). The default, and the only
    /// tier the single-workspace loop and CI ever use.
    Full,
    /// The cheap compile+smoke subset (`LOOM_BUILD_GATE_TIER=fast`), run when
    /// the host is saturated past the max-defer bound so a permanently-loaded
    /// host still gets a real — if narrower — verdict.
    Fast,
}

impl GateTier {
    /// A short, stable label for logs / status (`"full"` / `"fast"`).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Fast => "fast",
        }
    }

    /// A parenthetical verdict suffix — empty for the full tier so the default
    /// rendering is byte-for-byte unchanged, `" (fast tier)"` for the fast tier.
    #[must_use]
    pub fn verdict_suffix(self) -> &'static str {
        match self {
            Self::Full => "",
            Self::Fast => " (fast tier)",
        }
    }
}

/// The scheduling / tier decision for one gate tick under current host load
/// (#4259). Pure output of [`decide_gate_tier`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateTierDecision {
    /// Run the full command this tick.
    Full,
    /// Defer — the host is saturated and the max-defer window has not elapsed.
    /// No command runs; a deferral is NOT an evaluation (no SHA memo, no
    /// indeterminate backoff).
    Defer,
    /// Run the fast tier — the host is saturated but the max-defer window HAS
    /// elapsed, so a narrower verdict is produced rather than deferring forever.
    Fast,
}

/// Decide the tier for one gate tick (#4259) — pure and fully unit-testable.
///
/// - `saturated == None` (no load data) ⇒ [`GateTierDecision::Full`]: fail safe,
///   run the gate; never defer on absent evidence.
/// - `saturated == Some(false)` (load below threshold) ⇒ [`GateTierDecision::Full`].
/// - `saturated == Some(true)` ⇒ [`GateTierDecision::Defer`], UNLESS the gate has
///   already been deferring for `>= max_defer` (`defer_streak_elapsed`), in which
///   case [`GateTierDecision::Fast`] runs so a permanently-loaded host still
///   reaches a verdict within the bound.
#[must_use]
pub fn decide_gate_tier(
    saturated: Option<bool>,
    defer_streak_elapsed: Option<Duration>,
    max_defer: Duration,
) -> GateTierDecision {
    match saturated {
        None | Some(false) => GateTierDecision::Full,
        Some(true) => match defer_streak_elapsed {
            Some(elapsed) if elapsed >= max_defer => GateTierDecision::Fast,
            _ => GateTierDecision::Defer,
        },
    }
}

/// Env override for [`BUILD_GATE_LOAD_THRESHOLD_ENV`] — `None` unless set to a
/// parseable value `> 0`.
fn env_gate_load_threshold() -> Option<f64> {
    std::env::var(BUILD_GATE_LOAD_THRESHOLD_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|f| *f > 0.0)
}

/// `buildGate.loadThreshold` from config, filtered to `> 0`.
fn config_gate_load_threshold(repo_root: &Path) -> Option<f64> {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    crate::config_resolver::get_path(&effective, "buildGate")?
        .get("loadThreshold")
        .and_then(serde_json::Value::as_f64)
        .filter(|f| *f > 0.0)
}

/// Resolve the saturation threshold with precedence **env > config > default**
/// (#4259), mirroring the cpu_headroom knob resolution shape.
#[must_use]
pub fn resolve_gate_load_threshold(repo_root: &Path) -> f64 {
    env_gate_load_threshold()
        .or_else(|| config_gate_load_threshold(repo_root))
        .unwrap_or(crate::cpu_headroom::DEFAULT_GATE_LOAD_THRESHOLD)
}

/// Env override for [`BUILD_GATE_MAX_DEFER_ENV`] — `None` unless set to a
/// parseable value `> 0`.
fn env_gate_max_defer_secs() -> Option<u64> {
    std::env::var(BUILD_GATE_MAX_DEFER_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
}

/// `buildGate.maxDeferSeconds` from config, filtered to `> 0`.
fn config_gate_max_defer_secs(repo_root: &Path) -> Option<u64> {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    crate::config_resolver::get_path(&effective, "buildGate")?
        .get("maxDeferSeconds")
        .and_then(serde_json::Value::as_u64)
        .filter(|&s| s > 0)
}

/// Resolve the bounded max-defer window with precedence **env > config >
/// default** (#4259).
#[must_use]
pub fn resolve_gate_max_defer(repo_root: &Path) -> Duration {
    Duration::from_secs(
        env_gate_max_defer_secs()
            .or_else(|| config_gate_max_defer_secs(repo_root))
            .unwrap_or(DEFAULT_GATE_MAX_DEFER_SECS),
    )
}

/// The fast-tier command (#4259): `buildGate.fastCommand` when configured, else
/// the base command with `LOOM_BUILD_GATE_TIER=fast` prefixed (which the shipped
/// `build-gate.sh` honors). Running via `sh -c` means the env-prefix form is a
/// valid shell command, so no separate config key is required for the shipped
/// wrapper.
#[must_use]
pub fn resolve_gate_fast_command(repo_root: &Path, base_command: &str) -> String {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    crate::config_resolver::get_path(&effective, "buildGate")
        .and_then(|g| g.get("fastCommand").and_then(serde_json::Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(|| format!("{BUILD_GATE_TIER_ENV}=fast {base_command}"), str::to_string)
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
    /// The daemon's own forge credential is inside a bounded stale window
    /// (#5630): its background refresh tick is failing, so every `gh`/`git`
    /// call this host makes — in every managed repo, simultaneously — is
    /// answering from an aging or dead token.
    ///
    /// This is the class that distinguishes **"CI status unknown because our
    /// credentials are stale"** from **"main is red"**. A credential outage
    /// produces failures in *all* repos at once, which the pre-#5630 gate read
    /// as N-of-N genuinely-broken mains and halted dispatch host-wide. Treating
    /// it as UNEVALUATED holds each repo's previous verdict — a repo that was
    /// green stays green, a repo that was already halted stays halted — until
    /// the credential recovers or the grace window
    /// ([`crate::credential_preflight::resolve_forge_credential_stale_grace`])
    /// expires and normal fail-safe evaluation resumes.
    ForgeCredentialStale,
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
            Self::ForgeCredentialStale => "forge-credential-stale",
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
    /// `buildGate.command` exited 0 — `main` is healthy. `elapsed` is the
    /// wall-clock time the gate command ran (Issue #4083) — surfaced on the
    /// green INFO line so operators can distinguish "green with headroom" from
    /// "green at the edge of the timeout budget" without having to wait for a
    /// timeout to see the duration.
    Green { elapsed: Duration },
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
        matches!(self, Self::Green { .. })
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
            Self::Green { .. } => "",
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

/// Environment variable naming the forge workflow that must have concluded
/// `success` for a commit before forge-CI corroboration will vouch for it
/// (#3987). **Unset by default** — absent, corroboration keeps the #3986
/// unanimity rule byte-for-byte. When set (env overrides
/// `autonomous.mainHealthGate.ciWorkflow`), the named workflow must have a
/// `completed`/`success` run for the evaluated SHA or the verdict degrades to
/// [`CiVerdict::Unknown`]. This closes the residual gap where a repo whose real
/// verification workflow is `paths`-filtered out of a commit — so it produces
/// **no run at all** — could have a fast bookkeeping workflow (line counter,
/// labeler) vouch for the commit on its own. See the module docs.
pub const GATE_CI_WORKFLOW_ENV: &str = "LOOM_GATE_CI_WORKFLOW";

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
///
/// An optional `ci_workflow` name (#3987, resolved env > config > `None` in
/// [`CommandGateRunner::new`]) is threaded into [`parse_gh_run_list`]: when set,
/// that workflow must have concluded `success` for the SHA or the verdict
/// degrades to [`CiVerdict::Unknown`]. Absent, behavior is unchanged.
pub struct GhForgeCi {
    /// The workflow name that must have concluded `success`, or `None` for the
    /// unnamed (unanimity-only) behavior. See [`GATE_CI_WORKFLOW_ENV`].
    ci_workflow: Option<String>,
}

impl GhForgeCi {
    /// Construct a `gh`-backed forge-CI probe. Pass `None` for the unnamed
    /// (unanimity-only) behavior, or a workflow name to additionally require
    /// that workflow to have concluded `success` (#3987).
    #[must_use]
    pub fn new(ci_workflow: Option<String>) -> Self {
        Self { ci_workflow }
    }
}

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
        parse_gh_run_list(&stdout, sha, self.ci_workflow.as_deref())
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
///
/// # Optional named verification workflow (#3987)
///
/// The unanimity rule above reasons only over *runs that exist*. A workflow that
/// a `paths` / `paths-ignore` filter excluded from a commit produces **no run at
/// all** — not a `skipped` run — so it is invisible to the reducer. In a repo
/// where only a fast bookkeeping workflow (line counter, labeler) runs and
/// succeeds for such a commit, every run is `completed`/`success` and the reducer
/// returns `Success` for a commit the real build never judged.
///
/// When `ci_workflow` is `Some(name)` that gap is closed by layering **one extra
/// requirement on top of** — never a relaxation of — the unanimity rule: the
/// named workflow must itself have a `completed`/`success` run for `sha`.
///
/// | `ci_workflow` = `Some(name)`, runs for `sha` | Verdict |
/// |---|---|
/// | named workflow `completed`/`success`, unanimity otherwise satisfied | `Success` |
/// | named workflow has **no run at all** | `Unknown` *(the gap closure)* |
/// | named workflow `skipped` / `neutral` | `Unknown` (a required workflow that declined did not verify the commit) |
/// | any run `failure` / `timed_out` / `startup_failure` | `Failure` (unchanged, still checked first) |
///
/// When `ci_workflow` is `None`, behavior is byte-for-byte the unanimity rule.
/// A configured name that appears for **no** SHA anywhere in the probe window is
/// almost certainly a typo (it would silently pin the gate to permanent
/// `Unknown`), so it is surfaced with a `log::warn!` naming both the configured
/// value and the workflow names actually observed — while still failing safe to
/// `Unknown`.
fn parse_gh_run_list(stdout: &str, sha: &str, ci_workflow: Option<&str>) -> CiVerdict {
    let Ok(runs) = serde_json::from_str::<Vec<serde_json::Value>>(stdout) else {
        log::debug!("main_health_gate: could not parse `gh run list` output");
        return CiVerdict::Unknown;
    };
    let mut saw_success = false;
    // The first run that reached no verdict about the code, for diagnostics.
    let mut indeterminate: Option<String> = None;
    // #3987: whether the configured verification workflow itself reached
    // `completed`/`success` for `sha` (only meaningful when `ci_workflow` is set).
    let mut named_workflow_success = false;
    // Every workflow name observed anywhere in the probe window (any SHA), for
    // the misconfiguration warning below.
    let mut observed_workflows: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    for run in &runs {
        let field = |k: &str| run.get(k).and_then(serde_json::Value::as_str);
        let workflow = field("workflowName").unwrap_or("<unnamed workflow>");
        observed_workflows.insert(workflow.to_string());
        if field("headSha") != Some(sha) {
            continue;
        }
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
            Some("success") => {
                saw_success = true;
                if ci_workflow == Some(workflow) {
                    named_workflow_success = true;
                }
            }
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
    // #3987 misconfiguration guardrail: a configured name matching no workflow
    // anywhere in the window would otherwise silently pin the gate to permanent
    // `Unknown` (recreating #3974 for that repo). Make the cause visible; still
    // return the (fail-safe) computed verdict.
    if let Some(name) = ci_workflow {
        if !observed_workflows.iter().any(|w| w == name) {
            log::warn!(
                "main_health_gate: configured ci_workflow {name:?} (LOOM_GATE_CI_WORKFLOW / \
                 autonomous.mainHealthGate.ciWorkflow) matched no workflow in the last {} runs on \
                 {GATE_BRANCH} — observed: [{}]. Forge-CI corroboration will never vouch for a \
                 commit; verify the name against `gh run list`.",
                CI_PROBE_RUN_LIMIT,
                observed_workflows
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
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
        // #3987: with a verification workflow configured, that specific workflow
        // must itself have concluded `success` — otherwise the commit was vouched
        // for only by bookkeeping workflows (or the named one skipped/declined),
        // which is not evidence the real build judged the commit.
        match ci_workflow {
            Some(_) if !named_workflow_success => CiVerdict::Unknown,
            _ => CiVerdict::Success,
        }
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
///
/// `pub(crate)` (rather than private) so [`crate::credential_preflight`] can
/// reuse the same bounded-subprocess helper for its `gh auth status` probe
/// (#4005) instead of reimplementing timeout/kill handling.
pub(crate) fn run_capture_with_timeout(
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
/// - CI **unknown** (no completed run yet, a probe timeout) ⇒ fail safe: still
///   VERIFIED_RED, still halts. The local result is only ever overridden by
///   *positive* contrary evidence.
/// - CI **unknown AND this daemon's own forge credential is inside its stale
///   window** (#5630) ⇒ the probe is unknown *because we cannot authenticate*,
///   not because the forge has nothing to say. That is not evidence either way,
///   so the outcome degrades to
///   [`UnevaluatedClass::ForgeCredentialStale`] and the repo's **previous**
///   verdict stands. See [`CredentialFreshness`].
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
    /// Whether this daemon's forge credential is currently stale (#5630).
    /// Defaults to [`GlobalCredentialFreshness`] (the real process-global
    /// tracker); tests substitute a fixed answer.
    credential: Box<dyn CredentialFreshness + Send>,
}

impl CommandGateRunner {
    /// Construct a runner for `config`, executing in `repo_root`. Workspace sync
    /// to `origin/main` is **on** — the production default.
    #[must_use]
    pub fn new(config: BuildGateConfig, repo_root: PathBuf) -> Self {
        // #3987: resolve the optional verification-workflow name (env > config >
        // None) from the same repo root the gate runs against, so all existing
        // call sites and the `run_gate_tick` signature stay untouched.
        let ci_workflow = resolve_ci_workflow(&repo_root);
        Self {
            config,
            repo_root,
            sync: true,
            ci: Box::new(GhForgeCi::new(ci_workflow)),
            credential: Box::new(GlobalCredentialFreshness),
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

    /// Substitute the forge-credential freshness source (#5630). Intended for
    /// tests; production uses [`GlobalCredentialFreshness`].
    #[must_use]
    pub fn with_credential_freshness(
        mut self,
        credential: Box<dyn CredentialFreshness + Send>,
    ) -> Self {
        self.credential = credential;
        self
    }

    /// Cross-check a completed-and-failed local run against the forge's CI
    /// conclusion for the same evaluated commit — see the type-level docs.
    fn corroborate_red(&self, evaluated_sha: Option<&str>, detail: String) -> GateOutcome {
        // #5630: a stale forge credential makes the local command fail for
        // reasons that have nothing to do with `main` (any step that touches
        // `gh`/`git fetch` 401s), simultaneously in every managed repo. Check
        // this BEFORE the corroboration probe, because that probe runs `gh`
        // too and would only fail the same way.
        if self.credential.is_stale() {
            return GateOutcome::unevaluated(
                UnevaluatedClass::ForgeCredentialStale,
                credential_stale_reason(&detail),
            );
        }
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
            // #5630: re-check freshness after the probe — a refresh tick can
            // fail *while* a minutes-long gate command is running, in which
            // case this `Unknown` is "we cannot authenticate", not "the forge
            // has nothing to say about this commit".
            CiVerdict::Unknown if self.credential.is_stale() => GateOutcome::unevaluated(
                UnevaluatedClass::ForgeCredentialStale,
                credential_stale_reason(&detail),
            ),
            CiVerdict::Unknown => GateOutcome::red(format!(
                "{detail}; forge CI conclusion for the evaluated commit {sha} is unavailable, \
                 so the local result stands"
            )),
        }
    }
}

/// Whether this daemon's own forge credential is currently inside its bounded
/// stale window (#5630) — abstracted behind a trait, exactly like
/// [`ForgeCiStatus`], so [`CommandGateRunner`]'s credential-aware branch is
/// testable without mutating process-global state.
pub trait CredentialFreshness {
    /// `true` when the credential-refresh tick is failing recently enough that
    /// forge answers must not be treated as evidence about `main`.
    fn is_stale(&self) -> bool;
}

/// The production [`CredentialFreshness`]: the process-global streak tracker
/// that [`crate::credential_preflight`]'s refresh tick maintains.
pub struct GlobalCredentialFreshness;

impl CredentialFreshness for GlobalCredentialFreshness {
    fn is_stale(&self) -> bool {
        crate::credential_preflight::forge_credential_stale()
    }
}

/// A [`CredentialFreshness`] backed by a closure, so [`run_gate_tick_with_fns`]
/// can hand the **same** freshness source it uses for its own tick-level hold
/// down to the [`CommandGateRunner`] it builds (#6663).
///
/// Before this existed the tick took an injected `credential_stale` closure but
/// the runner it constructed still reached for [`GlobalCredentialFreshness`] —
/// two different answers to one question inside a single tick. Production never
/// noticed (both resolve to the same global), but it meant a test that pinned
/// the tick's answer still had its *runner* read process-global state written
/// by whatever else ran first in the test binary.
pub(crate) struct FnCredentialFreshness<F>(pub F);

impl<F: Fn() -> bool> CredentialFreshness for FnCredentialFreshness<F> {
    fn is_stale(&self) -> bool {
        (self.0)()
    }
}

/// The UNEVALUATED reason string for a tick held by a stale forge credential
/// (#5630), embedding the tracker's own summary plus whatever the local run
/// reported (which is itself likely an auth symptom).
fn credential_stale_reason(local_detail: &str) -> String {
    let summary = crate::credential_preflight::forge_credential_stale_summary()
        .unwrap_or_else(|| "the forge-credential refresh tick is failing".to_string());
    format!(
        "this daemon's forge credential is STALE ({summary}) — every gh/git call it makes is \
         failing for authentication reasons, in every managed repo at once, so this is NOT \
         evidence about main. Holding the previous verdict for this repo. The local run \
         reported: {local_detail}"
    )
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
        // Machine-wide build slot (#4512): the gate command is this host's
        // heaviest recurring CPU stage (a full `cargo test` workspace build), so
        // it takes a slot before starting rather than racing every concurrent
        // sweep's own build. Acquired OUTSIDE the timeout window on purpose — a
        // queue wait must not eat into `config.timeout` and turn contention into
        // a false UNEVALUATED (the #3978/#4020 failure mode). Never blocks
        // indefinitely and never fails: the lease degrades open on a bounded-wait
        // expiry or an unusable lock dir, so the gate always runs.
        let slot = crate::build_slot::acquire("main-health-gate");
        let outcome = run_command_with_timeout(
            &self.config.command,
            &self.repo_root,
            self.config.timeout,
            slot.covers_children(),
        );
        // Explicit drop for the release log line's ordering (the RAII drop would
        // fire at end of scope anyway).
        drop(slot);
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
///
/// Always uses the real [`crate::cpu_headroom::read_loadavg_1m`] load probe;
/// see [`run_gate_tick_with_load_fn`] for the injectable variant tests use to
/// pin a deterministic (unsaturated, or saturated) host load (#4441).
#[must_use]
pub fn run_gate_tick(
    state: &MainHealthState,
    config: &BuildGateConfig,
    repo_root: &Path,
) -> Option<GateOutcome> {
    run_gate_tick_with_load_fn(state, config, repo_root, crate::cpu_headroom::read_loadavg_1m)
}

/// Whether a gate tick must be **held** because this daemon's forge credential
/// is inside its bounded stale window (#5630), and the log/status detail to
/// record when it is.
///
/// Held is not a verdict. It leaves `halted` exactly as it was — a repo that
/// was green stays green, one that was already verified-red stays halted — and
/// deliberately does **not** arm the indeterminate backoff, so evaluation
/// resumes on the very next tick once the credential recovers. This is the
/// anti-oscillation property (AC3): while the credential is flapping, no gate
/// verdict changes at all, so `dispatch_halted` cannot flip with refresh-tick
/// timing.
///
/// Returns `None` when the credential is healthy (or its grace window has
/// expired and normal fail-safe evaluation must resume).
fn credential_hold_reason(is_stale: bool) -> Option<String> {
    if !is_stale {
        return None;
    }
    let summary = crate::credential_preflight::forge_credential_stale_summary()
        .unwrap_or_else(|| "the forge-credential refresh tick is failing".to_string());
    Some(format!(
        "this daemon's forge credential is STALE ({summary}) — the gate's own git fetch and every \
         gh call would fail for authentication reasons in EVERY managed repo simultaneously, which \
         is a credential signature, not N broken mains. Skipping this tick and holding the \
         previous verdict"
    ))
}

/// [`run_gate_tick`]'s implementation, parameterized over the 1-minute load
/// average lookup. Production code always goes through [`run_gate_tick`]
/// (which passes the real `/proc`-backed [`crate::cpu_headroom::read_loadavg_1m`]);
/// tests use this directly with a fixed closure so the tier/defer decision is
/// deterministic — the real host's live load average, shared across every
/// unit test running in this one process, is not a value any single test can
/// control, and pinning it via `LOOM_BUILD_GATE_LOAD_THRESHOLD` instead would
/// widen the suite-wide env-mutation race tracked in #4385.
///
/// `pub(crate)` (no production caller outside [`run_gate_tick`], mirroring
/// #4406's `classify_with_process_age_fn`).
#[must_use]
pub(crate) fn run_gate_tick_with_load_fn(
    state: &MainHealthState,
    config: &BuildGateConfig,
    repo_root: &Path,
    read_load: impl Fn() -> Option<f64>,
) -> Option<GateOutcome> {
    run_gate_tick_with_fns(state, config, repo_root, read_load, || {
        crate::credential_preflight::forge_credential_stale()
    })
}

/// [`run_gate_tick_with_load_fn`]'s implementation, additionally parameterized
/// over the forge-credential staleness lookup (#5630) so the credential-hold
/// branch is testable without mutating the process-global streak tracker
/// (which every other test in this process shares — the #4385 hazard).
#[must_use]
pub(crate) fn run_gate_tick_with_fns(
    state: &MainHealthState,
    config: &BuildGateConfig,
    repo_root: &Path,
    read_load: impl Fn() -> Option<f64>,
    credential_stale: impl Fn() -> bool + Send + 'static,
) -> Option<GateOutcome> {
    // #5630: check BEFORE anything that touches the forge. `resolve_remote_main_sha`
    // and the gate command's own `git fetch` both authenticate; with a stale
    // credential they fail in every managed repo at once, and the pre-#5630 gate
    // read that fan-out as N-of-N red mains and halted dispatch host-wide.
    if let Some(reason) = credential_hold_reason(credential_stale()) {
        if state.note_gate_tick(
            Some((UnevaluatedClass::ForgeCredentialStale, reason.as_str())),
            SKIP_WARN_THROTTLE,
        ) {
            log::warn!("main_health_gate: {} gate tick HELD — {reason}", repo_root.display());
        }
        // `None` ⇒ the caller applies nothing: the halt flag, the SHA memo, and
        // the backoff are all left exactly as they were.
        return None;
    }

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

    // Tier / load-deferral decision (#4259). Reuse the cpu_headroom load probe
    // (single read) so the gate and any load-aware dispatch agree on
    // "saturated"; fail safe — missing load data runs the full tier, never
    // defers. A deferred tick does NOT run the command, does NOT record the SHA
    // (main still needs evaluating), and does NOT arm the indeterminate backoff
    // (deferral is a scheduling decision, not an indeterminate outcome).
    let threshold = resolve_gate_load_threshold(repo_root);
    let max_defer = resolve_gate_max_defer(repo_root);
    let ncpu = crate::cpu_headroom::logical_cpu_count();
    let loadavg = read_load();
    let saturated = crate::cpu_headroom::is_host_saturated(loadavg, ncpu, threshold);
    let tier = match decide_gate_tier(saturated, state.defer_streak_elapsed(), max_defer) {
        GateTierDecision::Defer => {
            let ratio = crate::cpu_headroom::load_per_core_from(loadavg, ncpu).unwrap_or(0.0);
            if state.record_gate_deferred(ratio, max_defer) {
                log::info!(
                    "main_health_gate: {} DEFERRING gate run — host saturated (load {ratio:.2}/core ≥ {threshold:.2}); \
                     will run the FAST tier if still saturated at the {}m bound (NOT a verdict about main; dispatch unaffected)",
                    repo_root.display(),
                    max_defer.as_secs() / 60
                );
            } else {
                log::debug!(
                    "main_health_gate: {} still deferring gate run (host saturated, load {ratio:.2}/core)",
                    repo_root.display()
                );
            }
            // Clearing the UNEVALUATED flag makes status read "deferred (load …)"
            // rather than a stale "not evaluated (timeout …)" from a prior tick.
            state.note_gate_tick(None, SKIP_WARN_THROTTLE);
            return None;
        }
        GateTierDecision::Full => GateTier::Full,
        GateTierDecision::Fast => {
            log::info!(
                "main_health_gate: {} running FAST tier — host has been saturated past the {}m max-defer bound; \
                 a fast-tier verdict is narrower than a full-suite verdict (#4259)",
                repo_root.display(),
                max_defer.as_secs() / 60
            );
            GateTier::Fast
        }
    };
    // Not deferring this tick — end any prior streak so status stops reading
    // "deferred".
    state.clear_gate_deferred();

    // Select the command for the chosen tier (fast tier prefixes
    // `LOOM_BUILD_GATE_TIER=fast`, or uses `buildGate.fastCommand`).
    let mut run_config = config.clone();
    if tier == GateTier::Fast {
        run_config.command = resolve_gate_fast_command(repo_root, &config.command);
    }
    // #6663: the runner's own credential check (the `corroborate_red`
    // short-circuit) must consult the SAME freshness source this tick's hold
    // already consulted, not the process global independently. Identical in
    // production (`run_gate_tick_with_load_fn` passes the global); in tests it
    // is what makes an injected answer actually cover the whole tick.
    let mut runner = CommandGateRunner::new(run_config, repo_root.to_path_buf())
        .with_credential_freshness(Box::new(FnCredentialFreshness(credential_stale)));
    let outcome = runner.run_gate();
    match &outcome {
        GateOutcome::Green { .. } | GateOutcome::Red { .. } => {
            // Record which tier produced this verdict so the status/log tier
            // label distinguishes a fast-tier Green from a full-suite Green.
            state.record_gate_tier(tier);
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

/// The daemon's own re-stamped install-manifest (#4239): rewritten by every
/// `resync-installed.sh` run and has no `defaults/` source counterpart (it is
/// generated, not copied), so it cannot be classified by the byte-match rule
/// below — it is ignorable by exact path instead. A hard reset merely reverts
/// its stamps, which the next resync rewrites anyway.
const INSTALL_METADATA_PATH: &str = ".loom/install-metadata.json";

/// Installed↔source surface mapping (#4332) for the loom repo's own
/// dogfooded install: each installed prefix's tracked, edited-upstream
/// counterpart lives under `defaults/`. Mirrors the exact surface table
/// `resync-installed.sh` walks (`defaults/scripts/resync-installed.sh`,
/// search `widened pure-copy surfaces`) — **not** a uniform prefix rewrite:
/// `.loom/bin/` and `.claude/commands/loom/` map into
/// `defaults/.loom/bin/` and `defaults/.claude/commands/loom/`
/// respectively, while the others map straight into `defaults/<name>/`.
/// Only meaningful in a repo that carries a local `defaults/` tree (the loom
/// repo itself); a consumer install has no `defaults/` dir so the byte-match
/// lookup below always misses and classification is unchanged (#4332 is
/// loom-repo-scoped by construction).
///
/// This class is intentionally **not** mirrored into
/// `defaults/scripts/check-main-clean.sh`'s `LOOM_OWNED_PREFIXES` — see the
/// divergence note in that script's header (#4332). That script protects a
/// different property (catching a *builder* writing into the main worktree
/// by mistake, not classifying resync output for the dispatch gate), so a
/// byte-match there could mask real contamination instead of only ignoring
/// safe, known-regenerable dirt.
const INSTALLED_SURFACE_PREFIXES: &[(&str, &str)] = &[
    (".loom/hooks/", "defaults/hooks/"),
    (".loom/scripts/", "defaults/scripts/"),
    (".loom/roles/", "defaults/roles/"),
    (".loom/docs/", "defaults/docs/"),
    (".loom/bin/", "defaults/.loom/bin/"),
    (".claude/commands/loom/", "defaults/.claude/commands/loom/"),
];

/// If `path` falls under one of [`INSTALLED_SURFACE_PREFIXES`], return its
/// `defaults/`-relative source counterpart path. `defaults/` source paths
/// themselves are deliberately **not** in the mapping's domain — this only
/// ever maps *installed* copies to their source, never the reverse, so a
/// dirty `defaults/docs/x.md` (an operator editing the source directly)
/// never matches here and stays non-ignorable.
fn installed_surface_defaults_counterpart(path: &str) -> Option<String> {
    INSTALLED_SURFACE_PREFIXES
        .iter()
        .find_map(|(installed, defaults)| {
            path.strip_prefix(installed)
                .map(|rest| format!("{defaults}{rest}"))
        })
}

/// Whether `path` (a repo-relative path from `git status --porcelain`, rooted
/// at `repo_root`) is ignorable dirt for the gate's dirty-tree check:
///
/// - a Loom-owned transient path ([`LOOM_OWNED_PREFIXES`]),
/// - a common regenerable lockfile ([`BUILD_ARTIFACT_LOCKFILE_BASENAMES`]),
/// - the re-stamped install manifest ([`INSTALL_METADATA_PATH`]), or
/// - an installed-surface path (#4332,
///   [`installed_surface_defaults_counterpart`]) whose content byte-matches
///   its `defaults/` source counterpart — i.e. provably `resync-installed.sh`
///   output, not an operator hand-edit (a hand-edit cannot byte-match the
///   source it was never copied from).
///
/// Ignorable dirt is dirt a hard reset to `origin/main` safely discards (it
/// is either untracked Loom runtime state or a tracked file whose content the
/// reset will overwrite anyway, or — for the byte-match class — content
/// identical to what's already committed under `defaults/`) — never a reason
/// to skip the sync step.
fn is_ignorable_dirt(path: &str, repo_root: &Path) -> bool {
    is_ignorable_dirt_with_readers(path, &mut |p| std::fs::read(repo_root.join(p)).ok(), &mut |p| {
        std::fs::read(repo_root.join(p)).ok()
    })
}

/// Generic form of [`is_ignorable_dirt`], parameterized over how a path's
/// bytes are read, for the byte-match (installed-surface) sub-case only. The
/// disk-backed wrapper above reads both the dirty path and its `defaults/`
/// counterpart straight off `repo_root` — the natural source when classifying
/// `git status --porcelain` output against a live working tree.
///
/// #5693 (stash auto-retirement) reuses this same classification logic
/// against a *stash's* snapshot of a path instead of what's currently on
/// disk, so `read_dirty` and `read_source` are independent closures: the
/// stash classifier reads the dirty side from the stash's git blob (`git show
/// <stash>:<path>`, falling back to the untracked-files parent for `-u`
/// stashes) and the source side from the current `HEAD` blob — never from the
/// working tree, which may be on a different branch entirely from the one the
/// stash was created on.
pub(crate) fn is_ignorable_dirt_with_readers(
    path: &str,
    read_dirty: &mut dyn FnMut(&str) -> Option<Vec<u8>>,
    read_source: &mut dyn FnMut(&str) -> Option<Vec<u8>>,
) -> bool {
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
    if BUILD_ARTIFACT_LOCKFILE_BASENAMES.contains(&basename) {
        return true;
    }
    if path == INSTALL_METADATA_PATH {
        return true;
    }
    if let Some(counterpart) = installed_surface_defaults_counterpart(path) {
        if let (Some(dirty_bytes), Some(source_bytes)) =
            (read_dirty(path), read_source(&counterpart))
        {
            return dirty_bytes == source_bytes;
        }
    }
    false
}

/// Parse `git status --porcelain` v1 `output` and return the lines that are
/// **not** ignorable dirt ([`is_ignorable_dirt`]) — the lines that must still
/// be treated as "the workspace is dirty" for the gate's sync-before-run
/// check. Rename lines (`R  old -> new`) are keyed on the new path. Empty
/// lines are dropped. `repo_root` backs the installed-surface byte-match
/// check (#4332), which reads both the dirty path and its `defaults/`
/// counterpart off disk.
fn non_ignorable_dirt<'a>(output: &'a str, repo_root: &Path) -> Vec<&'a str> {
    output
        .lines()
        .filter(|line| !line.is_empty())
        .filter(|line| {
            let path = line.get(3..).unwrap_or("");
            let path = path.rsplit(" -> ").next().unwrap_or(path);
            let path = path.trim_matches('"');
            !is_ignorable_dirt(path, repo_root)
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
            let unexpected = non_ignorable_dirt(&out, repo_root);
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
///
/// `build_slot_held` propagates [`crate::build_slot::BUILD_SLOT_HELD_ENV`] into
/// the child (#4512). The shipped gate command is `build-gate.sh`, which takes
/// the machine-wide build slot itself via `lib/build-slot.sh`; the sentinel makes
/// that nested acquire a re-entrant no-op instead of a self-inflicted wait
/// against the slot this process is already holding.
fn run_command_with_timeout(
    command: &str,
    cwd: &Path,
    timeout: Duration,
    build_slot_held: bool,
) -> GateOutcome {
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
        .env(crate::build_slot::BUILD_SLOT_HELD_ENV, if build_slot_held { "1" } else { "0" })
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
                    break GateOutcome::Green {
                        elapsed: start.elapsed(),
                    };
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
        GateOutcome::Green { .. } => {
            state.record_verdict_at(Utc::now());
            let was_halted = state.halted.swap(false, Ordering::SeqCst);
            if was_halted {
                HealthTransition::Recovered
            } else {
                HealthTransition::RemainedHealthy
            }
        }
        GateOutcome::Red { .. } => {
            state.record_verdict_at(Utc::now());
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
/// Render a gate run's wall-clock duration for the green INFO line (#4083).
///
/// A whole-second `{}s` is the operator-facing unit the gate budget is
/// expressed in, but a sub-second run would round to a misleading `0s` that
/// reads as "the gate did not actually run". So anything under one second is
/// rendered in milliseconds instead — a fast gate shows e.g. `320ms`, never
/// `0s`.
#[must_use]
fn format_elapsed(elapsed: Duration) -> String {
    if elapsed < Duration::from_secs(1) {
        format!("{}ms", elapsed.as_millis())
    } else {
        format!("{}s", elapsed.as_secs())
    }
}

/// Extract the elapsed run time from a green outcome, defaulting to zero for
/// any other outcome (a `RemainedHealthy` transition only ever arrives paired
/// with a [`GateOutcome::Green`], so the default is unreachable in practice —
/// it exists purely so the renderers never panic on a mismatched pair).
#[must_use]
fn green_elapsed(outcome: &GateOutcome) -> Duration {
    match outcome {
        GateOutcome::Green { elapsed } => *elapsed,
        _ => Duration::ZERO,
    }
}

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
            // Positive evidence, at the default level, that the gate ran and
            // produced a green verdict this tick (#4083). Includes the elapsed
            // run time so a green-with-headroom run is distinguishable from a
            // green-at-the-edge-of-the-timeout-budget run.
            log::info!(
                "main_health_gate: main GREEN in {} — dispatch unaffected",
                format_elapsed(green_elapsed(outcome))
            );
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
/// consumes: the loop's on/off flag and the optional named forge verification
/// workflow (#3987). Further tuning knobs (cadence, timeout) can still be added
/// here without touching the call site.
///
/// The gate's *behavior* (which command runs against `main`, its timeout) still
/// comes from the separate `buildGate` block via [`read_build_gate_config`] —
/// `autonomous.mainHealthGate` is purely the on/off (plus forge-CI
/// corroboration tuning) surface, so Phase C's already-tested `buildGate`
/// semantics are untouched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutonomousGateConfig {
    /// `autonomous.mainHealthGate.enabled` — whether to run the gate loop.
    /// `None` when the key is absent (falls through to env / default).
    pub enabled: Option<bool>,
    /// `autonomous.mainHealthGate.ciWorkflow` (#3987) — the forge workflow that
    /// must have concluded `success` for a commit before forge-CI corroboration
    /// will vouch for it. `None` when the key is absent, empty, or whitespace
    /// (falls through to env / the unnamed unanimity-only behavior).
    pub ci_workflow: Option<String>,
    /// `autonomous.mainHealthGate.suppressDispatchDuringGate` (#4084) — whether
    /// the work-finder holds new dispatch off a root while its build-gate run is
    /// in flight. `None` when the key is absent (falls through to env /
    /// [`DEFAULT_SUPPRESS_DISPATCH_DURING_GATE`]).
    pub suppress_dispatch_during_gate: Option<bool>,
}

/// Read `.loom/config.json → autonomous.mainHealthGate`, soft-failing to an
/// all-`None` config on any of: missing file, malformed JSON, or a missing
/// `autonomous` / `mainHealthGate` block. Mirrors [`read_build_gate_config`]'s
/// soft-fail contract — a repo with no `autonomous` block gets zero behavior
/// change (env-only enablement, exactly like Phase C shipped).
#[must_use]
pub fn read_autonomous_gate_config(repo_root: &Path) -> AutonomousGateConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(gate) = crate::config_resolver::get_path(&effective, "autonomous.mainHealthGate")
    else {
        return AutonomousGateConfig::default();
    };

    AutonomousGateConfig {
        enabled: gate.get("enabled").and_then(serde_json::Value::as_bool),
        // #3987: empty / whitespace-only ⇒ `None` (treated as unset).
        ci_workflow: gate
            .get("ciWorkflow")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        suppress_dispatch_during_gate: gate
            .get("suppressDispatchDuringGate")
            .and_then(serde_json::Value::as_bool),
    }
}

/// Resolve the optional forge verification-workflow name for `repo_root` with
/// precedence **env > config > default(None)** (#3987), mirroring
/// [`resolve_enabled`]. [`GATE_CI_WORKFLOW_ENV`] wins when set to a non-empty
/// (trimmed) value; an empty/whitespace-only env value falls through to
/// `autonomous.mainHealthGate.ciWorkflow`; absent both ⇒ `None`, which keeps the
/// #3986 unanimity rule byte-for-byte.
#[must_use]
pub fn resolve_ci_workflow(repo_root: &Path) -> Option<String> {
    if let Ok(v) = std::env::var(GATE_CI_WORKFLOW_ENV) {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    read_autonomous_gate_config(repo_root).ci_workflow
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

/// Resolve whether the work-finder suppresses dispatch into a root while its
/// build-gate run is in flight, with precedence **env > config >
/// default([`DEFAULT_SUPPRESS_DISPATCH_DURING_GATE`])** (#4084). When
/// [`MAIN_HEALTH_GATE_SUPPRESS_DISPATCH_ENV`] is *set* (to any value) it decides
/// (truthy suppresses, anything else disables); when unset the config flag
/// decides; absent both ⇒ the default (on). Mirrors [`resolve_enabled`]'s
/// env-master-switch shape.
#[must_use]
pub fn resolve_suppress_dispatch_during_gate(config: &AutonomousGateConfig) -> bool {
    if let Ok(v) = std::env::var(MAIN_HEALTH_GATE_SUPPRESS_DISPATCH_ENV) {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config
        .suppress_dispatch_during_gate
        .unwrap_or(DEFAULT_SUPPRESS_DISPATCH_DURING_GATE)
}

/// Whether the gate is **effectively** enabled for `repo_root` (#4012): the
/// [`resolve_enabled`] on/off switch **and** a usable `buildGate` block. A
/// root can be nominally `autonomous.mainHealthGate.enabled: true` yet never
/// actually gate anything because `.loom/config.json` has no `buildGate`
/// block (or an empty command) — [`spawn_multi_main_health_gate_task`]
/// already treats that as always-green (soft-fail, unchanged contract); this
/// helper folds the same two checks into one so the `loom-daemon status`
/// "disabled" reading (#4012 AC2) agrees with what the gate loop actually
/// does, rather than reporting "enabled" for a root that will never run.
#[must_use]
pub fn effective_enabled(repo_root: &Path) -> bool {
    resolve_enabled(&read_autonomous_gate_config(repo_root))
        && read_build_gate_config(repo_root).is_some()
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

/// RAII guard that marks a [`MainHealthState`]'s gate run as in flight for its
/// lifetime (#4084): sets `gate_in_flight` true on construction and clears it on
/// `Drop` — including a panic unwind inside the `spawn_blocking` gate thread.
/// Moving the guard *into* the blocking closure means the flag is cleared on
/// every exit path (normal return, early return, or panic), so a panicking gate
/// run can never latch the flag on and permanently starve dispatch. This is
/// deliberately preferred over clearing the flag in each `match` arm, where a
/// future early-return could silently reintroduce the latch.
struct GateInFlightGuard {
    state: Arc<MainHealthState>,
}

impl GateInFlightGuard {
    fn new(state: Arc<MainHealthState>) -> Self {
        state.set_gate_in_flight(true);
        Self { state }
    }
}

impl Drop for GateInFlightGuard {
    fn drop(&mut self) {
        self.state.set_gate_in_flight(false);
    }
}

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
            // Move the runner in and back out so it survives across ticks. The
            // in-flight guard is set here (before the blocking build starts) and
            // moved into the closure so it clears on return *or* panic (#4084) —
            // the work-finder holds new dispatch off this root while it is set.
            let flight_guard = GateInFlightGuard::new(health_state.clone());
            let joined = tokio::task::spawn_blocking(move || {
                let _flight_guard = flight_guard;
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
            // Positive evidence, at the default level, that this repo's gate ran
            // and produced a green verdict this tick (#4083). Names the repo and
            // includes the elapsed run time (see `log_transition` for rationale).
            log::info!(
                "main_health_gate: {r} main GREEN in {} — dispatch unaffected",
                format_elapsed(green_elapsed(outcome))
            );
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
                // Mark this root's gate run in flight before the blocking build
                // starts and move the guard into the closure so it clears on
                // return *or* panic (#4084). While set, the work-finder holds
                // new dispatch off this root so its gate build is not racing
                // fresh sweep builds for the same cores (the contention that
                // still timed the gate out under #4073's `nice -n 5`).
                let flight_guard = GateInFlightGuard::new(state.clone());
                // Run the (potentially minutes-long) gate off the runtime.
                // `run_gate_tick` (#3984) short-circuits before the expensive
                // command when `origin/main` has not moved (or moved but
                // touched no `realChangeGlobs` path) since the last
                // determinate evaluation, and backs off after a run that
                // produced no verdict rather than retrying immediately.
                let joined = tokio::task::spawn_blocking(move || {
                    let _flight_guard = flight_guard;
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
                        // Skipped this tick (#3984: backoff, or no real change
                        // since the last determinate evaluation; or #4259: a
                        // load-deferral) — the halt flag is left exactly as it
                        // was, so there is nothing to apply here. `run_gate_tick`
                        // already logged the reason and updated the deferral /
                        // status state the status surface reads.
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
mod tests;
