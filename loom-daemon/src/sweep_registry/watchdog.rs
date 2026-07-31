//! Liveness watchdogs: the hung-sweep watchdog, the midbuild-recovery
//! watchdog, and the review-stall watchdog, plus their shared
//! `StartupRaceConfig` timing knobs.

use super::*;

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
///
/// Raised from 120s to 300s (Issue #4088): under concurrency the normal
/// dispatch→worktree latency was measured at 110–150s, so the old 120s default
/// sat *inside* the healthy distribution and cancelled progressing sweeps. 300s
/// clears that window with headroom while staying an order of magnitude below
/// the review-stall timeout (2700s), keeping the three backstops well separated.
pub const DEFAULT_WATCHDOG_TIMEOUT_SECS: u64 = 300;

/// Default watchdog probe interval — matches the reaper cadence.
pub const DEFAULT_WATCHDOG_INTERVAL_SECS: u64 = 30;

/// Env var overriding the startup-proof occupancy grace window, in seconds.
pub const STARTUP_PROOF_GRACE_ENV: &str = "LOOM_SWEEP_STARTUP_PROOF_GRACE_SECS";

/// Default startup-proof occupancy grace window (Issue #4003).
///
/// A slot is checked out (counted as occupied) at `fork/exec` success — before
/// the child has proven it reached the API, created a worktree, or wrote a
/// checkpoint. That is fine for the first `grace` seconds of a fresh dispatch
/// (a healthy child legitimately has not produced anything yet in the first
/// moment after spawn). Past `grace`, a sweep that has shown **zero** startup
/// signal — no worktree, no checkpoint, no log output past the spawn header,
/// see [`log_has_progress`] — is excluded from the work-finder's occupancy
/// count, freeing its slot for a healthy queued sweep well before the (still
/// 300s, unchanged) startup watchdog ([`DEFAULT_WATCHDOG_TIMEOUT_SECS`])
/// cancels and re-dispatches it.
///
/// Deliberately much shorter than the watchdog timeout: [`DEFAULT_WATCHDOG_TIMEOUT_SECS`]
/// is sized to the measured 110–150s dispatch→**worktree** latency under
/// concurrency (#4088) — a late, heavy signal. `log_has_progress` is a much
/// earlier signal: it fires the instant Claude Code itself produces ANY
/// output past the daemon's spawn header and the `spawn-claude.sh` wrapper
/// lines, which happens within seconds for a healthy child even under
/// contention. A child that produces literally nothing for this whole window
/// is not merely "slow" — it never reached the API at all, so freeing the
/// slot early never penalizes a healthy dispatch (see the
/// `regression_healthy_fleet_throughput_unaffected` test).
pub const DEFAULT_STARTUP_PROOF_GRACE_SECS: u64 = 90;

/// Grace period the watchdog gives a hung child to exit after SIGTERM before
/// escalating to SIGKILL, when it auto-cancels for re-dispatch.
pub(crate) const WATCHDOG_CANCEL_GRACE: Duration = Duration::from_secs(3);

/// Env var toggling the review-phase stall watchdog (Issue #3910).
/// `0`/`false`/`no`/`off` disables; `1`/`true`/`yes`/`on` forces on. Overrides
/// config. Distinct from `LOOM_SWEEP_WATCHDOG` (the #3887 startup watchdog) so a
/// repo can run one backstop without the other.
pub const REVIEW_STALL_ENABLE_ENV: &str = "LOOM_SWEEP_REVIEW_STALL";

/// Env var overriding the review-phase stall timeout (log-silence window), in
/// seconds.
pub const REVIEW_STALL_TIMEOUT_ENV: &str = "LOOM_SWEEP_REVIEW_STALL_TIMEOUT_SECS";

/// Default review-phase stall timeout (45 min of zero log output). A sweep that
/// has already made startup progress (worktree/checkpoint exists) but whose log
/// file has not been appended to within this window is treated as wedged in a
/// hung role subagent — the canonical case (#3910) is a Judge/Doctor Task that
/// runs 49–66 min (multi-hour in the worst observations) emitting **zero output
/// until the very end**. The threshold is on *log silence*, not total runtime,
/// so it sits far above a healthy Judge (100–380s) or a chatty Builder (which
/// flushes tool output continuously): a live sweep resets its idle clock on
/// every line it writes and is never disturbed.
pub const DEFAULT_REVIEW_STALL_TIMEOUT_SECS: u64 = 2700;

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

/// Pure review-phase stall state machine (Issue #3910). Reuses
/// [`WatchdogDecision`] (`Healthy`/`Restart`/`GiveUp`) but keys off **log
/// silence** rather than total-runtime-with-no-progress: `log_idle` is how long
/// the sweep's log file has gone un-appended.
///
/// - Log written within the timeout window ⇒ [`WatchdogDecision::Healthy`]
///   (the sweep is alive and emitting output — a slow-but-live Judge/Builder is
///   never disturbed).
/// - Silent past the timeout and not yet restarted ⇒
///   [`WatchdogDecision::Restart`] — cancel + re-dispatch once (the sweep
///   resumes from its checkpoint, so a hung Judge/Doctor is re-run, not rebuilt).
/// - Silent past the timeout but already restarted ⇒
///   [`WatchdogDecision::GiveUp`] — bounded to exactly one retry; left for the
///   operator.
#[must_use]
pub fn review_stall_decision(
    log_idle: Duration,
    timeout: Duration,
    already_retried: bool,
) -> WatchdogDecision {
    if log_idle < timeout {
        WatchdogDecision::Healthy
    } else if already_retried {
        WatchdogDecision::GiveUp
    } else {
        WatchdogDecision::Restart
    }
}

// ----------------------------------------------------------------------------
// Mid-build-death watchdog (Issue #3895)
// ----------------------------------------------------------------------------
//
// The startup watchdog (#3887/#3892) catches a sweep that shows NO progress
// past the spawn header (no worktree / no checkpoint / no log-past-header) and
// re-dispatches it once. But a *different* liveness failure slips past it: a
// sweep that got well into the Builder phase (created its worktree, made file
// edits) and then its child process DIED — the canonical cause being a token
// exhausting mid-run. Because `sweep_made_progress` returns `true` the instant
// a worktree exists, the startup watchdog correctly leaves such a sweep alone —
// so it does NOT rescue a sweep that HAD progress and then crashed, leaving the
// issue silently reverted to `loom:issue` with a dirty, uncommitted worktree
// and no PR. In autonomous mode this wedges the issue indefinitely.
//
// The mid-build-death watchdog is the complementary backstop. It scans the
// entries the reaper has already transitioned to a TERMINAL state (`Exited` /
// `Crashed`) for the "made progress then died" signature — an Issue sweep that
// produced no PR and whose worktree exists and is dirty — cleans the worktree
// and re-dispatches it exactly once (bounded, reusing the same retry-cap
// philosophy as #3892). A pre-flight token-health gate reads `.ranking` and
// defers the re-dispatch when the whole pool is exhausted, so a mid-run
// exhaustion is less likely to recur.
//
// ## The live-use veto (Issue #4449)
//
// That signature is NECESSARY but not SUFFICIENT, which cost real work on
// 2026-07-29: the daemon's tracked sweep for issue #4366 had indeed died, but a
// separate, untracked recovery Doctor session was concurrently editing the same
// worktree. `(terminal ∧ no PR ∧ dirty)` matched, the watchdog ran
// `git reset --hard` + `git clean -fd`, and a tested-but-uncommitted fix was
// destroyed in the window between the test run and `git commit`.
//
// Dirtiness cannot distinguish "debris a dead sweep left behind" from "a live
// session's work in progress" — both look identical to `git status`. So the
// watchdog now requires a second, independent condition: NOTHING LIVE may still
// hold the worktree. `SweepRegistry::worktree_in_use` gathers four signals (a
// `.loom-in-use` marker, a claim-lock whose owner PID is alive, an in-flight
// `index.lock`, and processes whose cwd is inside the worktree); any one of them
// vetoes the reset, yielding `MidbuildDecision::InUse` — logged loudly, with the
// single recovery retry left unconsumed so a genuinely-dead sweep is still
// recovered on a later tick once the holder goes away. And `clean_worktree` now
// logs the porcelain status + diffstat of everything it is about to destroy, so
// a wipe can never again be silent in the daemon log.

/// The mid-build-death watchdog's per-sweep decision (Issue #3895). Pure state
/// machine — [`midbuild_decision`] maps `(worktree_dirty, produced_pr,
/// already_retried, worktree_in_use)` onto exactly one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidbuildDecision {
    /// Not a mid-build death (no dirty worktree, or a PR was produced) — leave
    /// the terminal entry alone.
    Healthy,
    /// A dead sweep with a dirty worktree and no PR that has not been recovered
    /// yet — clean the worktree and re-dispatch once.
    Recover,
    /// A dead sweep matching the signature but already recovered once — give up
    /// (bounded: never loop). Left for the operator.
    GiveUp,
    /// The signature matches BUT the worktree is still held by a live session
    /// the daemon does not track (Issue #4449) — refuse the destructive reset
    /// and leave both the worktree and the single recovery retry untouched.
    InUse,
}

/// Pure mid-build-death state machine (Issue #3895, extended by #4449).
///
/// - No dirty worktree, or the sweep already produced a PR ⇒
///   [`MidbuildDecision::Healthy`] (nothing to recover).
/// - Dirty worktree + no PR + **a live session still using the worktree** ⇒
///   [`MidbuildDecision::InUse`] (#4449). This gate sits *above* the retry
///   bookkeeping on purpose: "someone is editing this right now" is never a
///   dead-sweep-recovery candidate, and a refusal must not burn the single
///   recovery retry (the worktree may be legitimately free on a later tick).
/// - Dirty worktree + no PR + not in use + not yet recovered ⇒
///   [`MidbuildDecision::Recover`].
/// - Dirty worktree + no PR + not in use + already recovered ⇒
///   [`MidbuildDecision::GiveUp`] — the recovery is bounded to exactly one
///   re-dispatch per issue.
///
/// # Why the in-use gate exists (#4449)
///
/// Before #4449 the watchdog inferred "died mid-build" from `(terminal state ∧
/// no PR ∧ dirty worktree)` alone and immediately reset the worktree. On
/// 2026-07-29 that inference was wrong: the daemon's own tracked sweep for
/// issue #4366 *had* died, but a separate, untracked recovery Doctor session was
/// concurrently and legitimately working in the same worktree. The watchdog read
/// that session's uncommitted fix as dead-sweep debris and destroyed it mid
/// `git commit`. Dirtiness alone cannot distinguish the two cases — only a
/// liveness signal can.
#[must_use]
pub fn midbuild_decision(
    worktree_dirty: bool,
    produced_pr: bool,
    already_retried: bool,
    worktree_in_use: bool,
) -> MidbuildDecision {
    if !worktree_dirty || produced_pr {
        MidbuildDecision::Healthy
    } else if worktree_in_use {
        MidbuildDecision::InUse
    } else if already_retried {
        MidbuildDecision::GiveUp
    } else {
        MidbuildDecision::Recover
    }
}

/// Max lines of `git status` / `git diff --stat` echoed into the
/// "about to discard" log line (Issue #4449) — enough to identify the lost work,
/// bounded so a runaway worktree cannot flood the daemon log.
pub(crate) const DISCARD_LOG_MAX_LINES: usize = 40;

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
    /// `autonomous.watchdog.reviewStall` — whether to run the review-phase
    /// stall watchdog (Issue #3910).
    pub review_stall_enabled: Option<bool>,
    /// `autonomous.watchdog.reviewStallTimeoutSecs` — log-silence timeout for
    /// the review-phase stall watchdog, in seconds (zero/invalid dropped to
    /// `None`).
    pub review_stall_timeout_secs: Option<u64>,
    /// `autonomous.watchdog.startupProofGraceSecs` (Issue #4003) — how long a
    /// freshly-dispatched sweep counts toward the work-finder's occupancy
    /// budget with zero observed startup-proof signal, in seconds
    /// (zero/invalid dropped to `None`).
    pub startup_proof_grace_secs: Option<u64>,
}

/// Read `.loom/config.json → autonomous` for the startup-race knobs (Issue
/// #3887), soft-failing every field to `None` on a missing file, malformed
/// JSON, or an absent `autonomous` block. Mirrors
/// [`crate::work_finder::read_work_finder_config`].
#[must_use]
pub fn read_startup_race_config(repo_root: &Path) -> StartupRaceConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(auto) = crate::config_resolver::get_path(&effective, "autonomous") else {
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
        review_stall_enabled: watchdog
            .and_then(|w| w.get("reviewStall"))
            .and_then(serde_json::Value::as_bool),
        review_stall_timeout_secs: watchdog
            .and_then(|w| w.get("reviewStallTimeoutSecs"))
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
        startup_proof_grace_secs: watchdog
            .and_then(|w| w.get("startupProofGraceSecs"))
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

/// Resolve whether the review-phase stall watchdog runs, precedence **env >
/// config > default(true)** (Issue #3910). Defaults **on** — a self-healing
/// backstop with a generous 45-minute log-silence timeout and a bounded single
/// retry — but can be disabled via `LOOM_SWEEP_REVIEW_STALL=0` or
/// `autonomous.watchdog.reviewStall = false`.
#[must_use]
pub fn resolve_review_stall_enabled(config: &StartupRaceConfig) -> bool {
    if let Ok(v) = std::env::var(REVIEW_STALL_ENABLE_ENV) {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config.review_stall_enabled.unwrap_or(true)
}

/// Resolve the review-phase stall (log-silence) timeout, precedence **env >
/// config > default** (Issue #3910).
#[must_use]
pub fn resolve_review_stall_timeout(config: &StartupRaceConfig) -> Duration {
    let secs = std::env::var(REVIEW_STALL_TIMEOUT_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(config.review_stall_timeout_secs)
        .unwrap_or(DEFAULT_REVIEW_STALL_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// Resolve the startup-proof occupancy grace window, precedence **env >
/// config > default** (Issue #4003).
#[must_use]
pub fn resolve_startup_proof_grace(config: &StartupRaceConfig) -> Duration {
    let secs = std::env::var(STARTUP_PROOF_GRACE_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(config.startup_proof_grace_secs)
        .unwrap_or(DEFAULT_STARTUP_PROOF_GRACE_SECS);
    Duration::from_secs(secs)
}

/// Spawn the watchdog task (Issue #3887 + #3895 + #3910). Every `interval`, it
/// runs three liveness backstops in one tick: the **startup-hang watchdog**
/// (#3887) probes each running daemon-dispatched sweep for progress and
/// auto-cancels + re-dispatches (once, bounded) any that have hung past
/// `timeout`; the **mid-build-death watchdog** (#3895) scans terminal entries
/// for a sweep that made Builder progress then died (dirty worktree, no PR) and
/// cleans + re-dispatches it (once, bounded); the **review-phase stall
/// watchdog** (#3910, when `review_stall_timeout` is `Some`) cancels +
/// re-dispatches (once, bounded) any still-running sweep past startup whose log
/// has gone silent past that timeout — the hung-Judge/Doctor case. Mirrors
/// [`spawn_reaper_task`]: brief lock per tick, never held across the sleep.
pub fn spawn_watchdog_task(
    registry: Arc<Mutex<SweepRegistry>>,
    timeout: Duration,
    interval: Duration,
    review_stall_timeout: Option<Duration>,
) -> tokio::task::JoinHandle<()> {
    log::info!(
        "sweep_registry: starting startup watchdog (interval={}s, timeout={}s) (#3887); \
         review-stall watchdog {} (#3910)",
        interval.as_secs(),
        timeout.as_secs(),
        review_stall_timeout
            .map(|t| format!("enabled (timeout={}s)", t.as_secs()))
            .unwrap_or_else(|| "disabled".to_string())
    );
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // First tick fires immediately; skip it so we don't act at boot before
        // any sweep has had a chance to start.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            // Same tick runs all three liveness backstops: the startup-hang
            // watchdog (#3887, no progress), the mid-build-death watchdog
            // (#3895, made progress then the child died), and the review-phase
            // stall watchdog (#3910, alive but log-silent mid-review). All hold
            // the registry lock only briefly, never across the sleep.
            let (restarted, recovered, unstalled) = {
                match registry.lock() {
                    Ok(mut r) => {
                        let restarted = r.watchdog_once(timeout);
                        let recovered = r.midbuild_watchdog_once();
                        let unstalled =
                            review_stall_timeout.map_or(0, |t| r.review_stall_watchdog_once(t));
                        (restarted, recovered, unstalled)
                    }
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
            if recovered > 0 {
                log::warn!(
                    "sweep_registry: watchdog recovered {recovered} mid-build-death sweep{} (#3895)",
                    if recovered == 1 { "" } else { "s" }
                );
            }
            if unstalled > 0 {
                log::warn!(
                    "sweep_registry: watchdog re-dispatched {unstalled} review-stalled sweep{} (#3910)",
                    if unstalled == 1 { "" } else { "s" }
                );
            }
        }
    })
}

impl SweepRegistry {
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
    pub(crate) fn sweep_made_progress(&self, issue: u32, log_path: &Path) -> bool {
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

    // ------------------------------------------------------------------------
    // Occupancy accounting (Issue #4003)
    // ------------------------------------------------------------------------

    /// Whether `sweep_id` has proven startup (Issue #4003): reuses the exact
    /// signal [`sweep_made_progress`](Self::sweep_made_progress) polls (worktree
    /// / checkpoint / log-past-header) and latches through the same
    /// `watchdog_progressed` set the startup watchdog (#3887/#4088) already
    /// maintains — a signal observed by either call site is remembered by both,
    /// and neither ever "un-sees" a sweep that once proved it started (the same
    /// monotonicity rationale as #4088: every underlying signal is torn down at
    /// successful completion, so a *finished* sweep must not read as
    /// *never-started*).
    pub(crate) fn has_proven_start(
        &mut self,
        sweep_id: &SweepId,
        issue: u32,
        log_path: &Path,
    ) -> bool {
        if self.watchdog_progressed.contains(sweep_id) {
            return true;
        }
        if self.sweep_made_progress(issue, log_path) {
            self.watchdog_progressed.insert(sweep_id.clone());
            true
        } else {
            false
        }
    }

    /// Issue numbers of `Running`/`Pending` Issue sweeps that count toward the
    /// work-finder's admission budget (Issue #4003).
    ///
    /// A sweep counts while it is inside its [`startup_proof_grace`]
    /// (Self::startup_proof_grace) window (`elapsed < grace`) — a fresh dispatch
    /// legitimately has produced nothing yet — **or** once it has proven
    /// startup progress via [`has_proven_start`](Self::has_proven_start). A
    /// sweep dispatched longer ago than `grace` that has proven NO signal at
    /// all is excluded: its slot no longer counts against the cap, even though
    /// the separate (still 300s-default) startup watchdog has not yet
    /// cancelled/re-dispatched it.
    ///
    /// This is occupancy-accounting ONLY — it never mutates `SweepState`, never
    /// touches the claim lock, and never cancels or re-dispatches anything. The
    /// registry's own dedup (`RegistryDispatcher::in_flight`, used for the
    /// "already in-flight, skip" check) and the forge label
    /// (`loom:building`) are what actually prevent a double-dispatch of the
    /// SAME issue — so under-counting occupancy here only ever lets a
    /// *different* queued issue take the freed slot, never re-dispatches this
    /// one. PrSet sweeps carry no single issue number and are excluded (out of
    /// scope, mirrors [`watchdog_once`](Self::watchdog_once)).
    pub fn occupied_issues(&mut self) -> HashSet<u32> {
        let now = Utc::now();
        let grace = self.startup_proof_grace;
        let candidates: Vec<(SweepId, u32, PathBuf, Duration)> = self
            .entries
            .iter()
            .filter(|(_, info)| matches!(info.state, SweepState::Running | SweepState::Pending))
            .filter_map(|(id, info)| {
                let SweepKind::Issue(issue) = info.kind else {
                    return None;
                };
                let elapsed = (now - info.started_at).to_std().unwrap_or(Duration::ZERO);
                Some((id.clone(), issue, info.log_path.clone(), elapsed))
            })
            .collect();

        let mut occupied = HashSet::new();
        for (sweep_id, issue, log_path, elapsed) in candidates {
            if elapsed < grace || self.has_proven_start(&sweep_id, issue, &log_path) {
                occupied.insert(issue);
            }
        }
        occupied
    }

    /// For status/observability only (Issue #4003): for each currently live
    /// (`Running`/`Pending`) Issue sweep that has not yet proven startup,
    /// return `(issue, time_since_dispatch)`. Empty once a sweep proves
    /// progress (checked against the same `watchdog_progressed` latch
    /// [`has_proven_start`](Self::has_proven_start) maintains, so this can
    /// never disagree with the occupancy computation above). Read-only — does
    /// not mutate any state, so `loom-daemon status` / `GetDaemonStatus` can
    /// poll it on every request with no side effects.
    #[must_use]
    pub fn unproven_startups(&self) -> Vec<(u32, Duration)> {
        let now = Utc::now();
        self.entries
            .iter()
            .filter(|(_, info)| matches!(info.state, SweepState::Running | SweepState::Pending))
            .filter_map(|(id, info)| {
                let SweepKind::Issue(issue) = info.kind else {
                    return None;
                };
                if self.watchdog_progressed.contains(id)
                    || self.sweep_made_progress(issue, &info.log_path)
                {
                    return None;
                }
                let elapsed = (now - info.started_at).to_std().unwrap_or(Duration::ZERO);
                Some((issue, elapsed))
            })
            .collect()
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
            // Latch progress per SweepId (Issue #4088): `sweep_made_progress`
            // only reports the *current* filesystem state, and every signal it
            // reads (worktree, checkpoint, log) is torn down at successful
            // completion — so a finished sweep would otherwise read as
            // never-started and be re-dispatched. Once observed true, the latch
            // keeps `made_progress` true for this SweepId on every later tick.
            let made_progress = self.watchdog_progressed.contains(&sweep_id)
                || self.sweep_made_progress(issue, &log_path);
            if made_progress {
                self.watchdog_progressed.insert(sweep_id.clone());
            }
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

                    // #4485: release any dispatch-backoff window first. This
                    // recovery is already bounded to ONE attempt per issue by the
                    // latch above (marked before acting), so the rate limiting the
                    // backoff provides is redundant here — and a refusal would
                    // silently burn that single allowed attempt.
                    self.clear_dispatch_backoff(issue);

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
    // Mid-build-death watchdog (Issue #3895)
    // ------------------------------------------------------------------------

    /// Absolute path to a sweep's issue worktree (`.loom/worktrees/issue-<N>`).
    #[must_use]
    pub(crate) fn worktree_path(&self, issue: u32) -> PathBuf {
        self.config
            .workspace_root
            .join(".loom")
            .join("worktrees")
            .join(format!("issue-{issue}"))
    }

    /// Whether issue `N`'s worktree exists AND has uncommitted changes (Issue
    /// #3895) — the "made build progress then died" signal. Runs
    /// `git -C <worktree> status --porcelain`; a non-empty output means dirty.
    ///
    /// Degrades to `false` (not a recovery candidate) when the worktree is
    /// absent, `git` is unavailable, or the command fails — we never treat an
    /// unprobeable worktree as recoverable.
    pub(crate) fn worktree_dirty(&self, issue: u32) -> bool {
        let wt = self.worktree_path(issue);
        if !wt.exists() {
            return false;
        }
        let output = Command::new("git")
            .arg("-C")
            .arg(&wt)
            .arg("status")
            .arg("--porcelain")
            .arg("--untracked-files=all")
            .output();
        match output {
            Ok(o) if o.status.success() => !o.stdout.iter().all(u8::is_ascii_whitespace),
            _ => false,
        }
    }

    /// Gather every signal that issue `N`'s worktree is still being used by a
    /// **live** process the in-memory registry does not represent as an active
    /// sweep (Issue #4449). An empty vec means "no live holder found".
    ///
    /// This is the veto gate on the mid-build watchdog's destructive path. It
    /// deliberately fails *closed* on ambiguity in the one direction that matters:
    /// any positive signal blocks the reset. Signals that cannot be probed on
    /// this host simply contribute nothing (the probe helpers already degrade to
    /// "unknown ⇒ empty"), so a host without `/proc` or `lsof` still gets the
    /// marker / claim-lock / index.lock signals.
    ///
    /// Order is cheapest-first so the common "nothing is using it" case does the
    /// least work: two `stat`s and a `read_to_string` before the process scan.
    pub(crate) fn worktree_in_use(&self, issue: u32) -> Vec<WorktreeUseEvidence> {
        let wt = self.worktree_path(issue);
        let mut evidence = Vec::new();
        if !wt.exists() {
            return evidence;
        }

        // 1. Explicit `.loom-in-use` marker — the one signal a manual session
        //    (or an operator) can plant by hand to fence off a worktree.
        if let Some(marker) = crate::worktree_ops::safety::read_in_use_marker(&wt) {
            evidence.push(WorktreeUseEvidence::InUseMarker {
                task_id: marker.task_id,
                pid: marker.pid,
            });
        }

        // 2. A claim-lock whose recorded owner PID is still alive. A dead owner
        //    is NOT evidence — the reaper and `reconstruct` prune those, and
        //    treating a stale lock as "in use" would wedge legitimate recovery.
        let owner_path = self
            .config
            .locks_dir()
            .join(format!("issue-{issue}"))
            .join("owner.json");
        if let Some(owner) = std::fs::read_to_string(&owner_path)
            .ok()
            .and_then(|s| serde_json::from_str::<LockOwner>(&s).ok())
        {
            if is_pid_alive(owner.owner_pid) {
                evidence.push(WorktreeUseEvidence::LiveClaimLock {
                    pid: owner.owner_pid,
                    sweep_id: owner.sweep_id,
                });
            }
        }

        // 3. A git operation mid-flight (`index.lock`) — the exact `git commit`
        //    window in which #4449 destroyed an uncommitted fix.
        if let Some(lock) = git_index_lock_path(&wt) {
            if lock.exists() {
                evidence.push(WorktreeUseEvidence::GitOperationInFlight(lock));
            }
        }

        // 4. Live processes with a cwd inside the worktree (shells, manual role
        //    sessions, orphaned grandchildren still writing files).
        let pids = crate::worktree_ops::safety::find_processes_using_directory(&wt);
        if !pids.is_empty() {
            evidence.push(WorktreeUseEvidence::LiveProcesses(pids));
        }

        evidence
    }

    /// Record, at `warn`, exactly what a [`clean_worktree`] call is about to
    /// destroy (Issue #4449) — the porcelain status plus a diffstat, truncated to
    /// [`DISCARD_LOG_MAX_LINES`]. A wipe must never be silent in the daemon log:
    /// this line is the only forensic trace that survives the `reset --hard`.
    ///
    /// [`clean_worktree`]: SweepRegistry::clean_worktree
    pub(crate) fn log_worktree_discard(&self, wt: &Path, issue: u32) {
        let run = |args: &[&str]| -> String {
            Command::new("git")
                .arg("-C")
                .arg(wt)
                .args(args)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim_end().to_string())
                .unwrap_or_default()
        };
        let status = truncate_lines(
            &run(&["status", "--porcelain", "--untracked-files=all"]),
            DISCARD_LOG_MAX_LINES,
        );
        let diffstat = truncate_lines(&run(&["diff", "--stat", "HEAD"]), DISCARD_LOG_MAX_LINES);
        if status.is_empty() && diffstat.is_empty() {
            // Nothing probeable to report (missing git, unborn HEAD, …) — still
            // announce the destructive action so the log shows it happened.
            log::warn!(
                "clean-worktree: discarding uncommitted state in {} (issue #{issue}) via \
                 `git reset --hard` + `git clean -fd`; could not enumerate the discarded \
                 changes (#4449).",
                wt.display()
            );
            return;
        }
        log::warn!(
            "clean-worktree: DISCARDING uncommitted state in {} (issue #{issue}) via \
             `git reset --hard` + `git clean -fd` — this is irreversible (#4449).\n\
             status --porcelain:\n{status}\n\
             diff --stat HEAD:\n{diffstat}",
            wt.display()
        );
    }

    /// Discard a mid-build-death worktree's uncommitted changes so the
    /// re-dispatched sweep resumes from a clean checkout (Issue #3895): a
    /// `git reset --hard` followed by `git clean -fd`. Best-effort — any commits
    /// the dead sweep managed to make are preserved (only the dirty working tree
    /// and untracked files are dropped).
    ///
    /// Every call first logs what it is about to destroy via
    /// [`log_worktree_discard`](Self::log_worktree_discard) (Issue #4449) — the
    /// #4449 incident was made unrecoverable partly because the wipe left no
    /// trace at all in the daemon log.
    pub(crate) fn clean_worktree(&self, issue: u32) -> Result<()> {
        let wt = self.worktree_path(issue);
        if !wt.exists() {
            return Ok(());
        }
        self.log_worktree_discard(&wt, issue);
        let reset = Command::new("git")
            .arg("-C")
            .arg(&wt)
            .arg("reset")
            .arg("--hard")
            .output()
            .with_context(|| format!("git reset --hard in {}", wt.display()))?;
        if !reset.status.success() {
            return Err(anyhow!(
                "git reset --hard failed in {}: {}",
                wt.display(),
                String::from_utf8_lossy(&reset.stderr).trim()
            ));
        }
        let clean = Command::new("git")
            .arg("-C")
            .arg(&wt)
            .arg("clean")
            .arg("-fd")
            .output()
            .with_context(|| format!("git clean -fd in {}", wt.display()))?;
        if !clean.status.success() {
            return Err(anyhow!(
                "git clean -fd failed in {}: {}",
                wt.display(),
                String::from_utf8_lossy(&clean.stderr).trim()
            ));
        }
        Ok(())
    }

    /// Pre-flight token-health gate (Issue #3895): whether the token pool still
    /// has capacity to (re-)dispatch, per `.loom/tokens/.ranking`. Reads the
    /// ranking file and delegates to [`ranking_has_capacity`]. A missing /
    /// unreadable ranking degrades to `true` (proceed) — matching current
    /// behavior where the spawn-time selector makes its own choice.
    pub(crate) fn token_pool_has_capacity(&self) -> bool {
        let ranking = self
            .config
            .workspace_root
            .join(".loom")
            .join("tokens")
            .join(".ranking");
        match std::fs::read_to_string(&ranking) {
            Ok(contents) => ranking_has_capacity(&contents),
            Err(_) => true,
        }
    }

    /// Whether an active (Running/Pending) sweep already exists for `issue` —
    /// used to avoid racing a re-dispatch the work-finder may have already
    /// issued after the reaper restored `loom:issue` (Issue #3895).
    pub(crate) fn issue_has_active_sweep(&self, issue: u32) -> bool {
        self.entries.values().any(|i| {
            matches!(i.kind, SweepKind::Issue(n) if n == issue)
                && matches!(i.state, SweepState::Running | SweepState::Pending)
        })
    }

    /// Run one mid-build-death watchdog tick (Issue #3895): for each sweep the
    /// reaper has already transitioned to a TERMINAL state (`Exited`/`Crashed`)
    /// whose child died mid-build — an Issue sweep that produced no PR and left
    /// a dirty worktree — clean the worktree and re-dispatch the issue **exactly
    /// once** (bounded; a second mid-build death resolves to
    /// [`MidbuildDecision::GiveUp`] and is left for the operator).
    ///
    /// A pre-flight token-health gate ([`token_pool_has_capacity`]) defers the
    /// re-dispatch (without consuming the single retry) when the whole pool is
    /// `exhausted`/`blocked`, so a mid-run exhaustion is less likely to recur.
    ///
    /// **Live-use veto (Issue #4449)**: before anything destructive happens, a
    /// dirty worktree is probed for live holders ([`worktree_in_use`]). A dirty
    /// worktree is only *dead-sweep debris* if nothing live still owns it — if a
    /// `.loom-in-use` marker, a live claim-lock owner, an in-flight `index.lock`,
    /// or a process with its cwd inside the worktree says otherwise, the decision
    /// resolves to [`MidbuildDecision::InUse`]: log loudly, touch nothing, and do
    /// **not** consume the single recovery retry.
    ///
    /// [`worktree_in_use`]: SweepRegistry::worktree_in_use
    ///
    /// **Lock-held clean (Issue #4564)**: the destructive arm runs while this
    /// watchdog *owns* the issue's claim lock
    /// ([`claim_lock_for_midbuild`]), not merely after a read-only ownership
    /// probe. A claim it cannot win means a peer sweep is live: the worktree is
    /// left alone and the single recovery retry is not consumed.
    ///
    /// [`claim_lock_for_midbuild`]: SweepRegistry::claim_lock_for_midbuild
    ///
    /// No new event topics are introduced (the taxonomy is frozen): the
    /// re-dispatch reuses `sweep.global.dispatch` from [`dispatch`], and a
    /// bounded give-up / a pool-exhausted defer surface on the existing frozen
    /// `sweep.issue.{N}.crashed` topic. Returns the number of sweeps
    /// re-dispatched this tick.
    ///
    /// [`token_pool_has_capacity`]: SweepRegistry::token_pool_has_capacity
    pub fn midbuild_watchdog_once(&mut self) -> usize {
        // Snapshot terminal Issue candidates that produced no PR.
        let candidates: Vec<(SweepId, u32)> = self
            .entries
            .iter()
            .filter(|(_, info)| {
                info.state.is_terminal()
                    && matches!(info.kind, SweepKind::Issue(_))
                    && info.pr_number.is_none()
            })
            .filter_map(|(id, info)| match info.kind {
                SweepKind::Issue(issue) => Some((id.clone(), issue)),
                SweepKind::PrSet(_) => None,
            })
            .collect();

        let mut recovered = 0usize;
        for (sweep_id, issue) in candidates {
            // Don't race a re-dispatch the work-finder may already have issued
            // after the reaper restored loom:issue.
            if self.issue_has_active_sweep(issue) {
                continue;
            }
            let dirty = self.worktree_dirty(issue);
            // #4449: a dirty worktree is only dead-sweep debris if nothing LIVE
            // is still using it. Probe only when dirty (the probe is the
            // expensive part and a clean worktree is never reset anyway).
            let in_use = if dirty {
                self.worktree_in_use(issue)
            } else {
                Vec::new()
            };
            if in_use.is_empty() {
                // No longer held — clear the log-once latch so a later refusal
                // (or a genuine give-up) still surfaces in the daemon log.
                self.midbuild_inuse.remove(&issue);
            }
            let already_retried = self.midbuild_retried.contains(&issue);
            match midbuild_decision(dirty, false, already_retried, !in_use.is_empty()) {
                MidbuildDecision::InUse => {
                    if self.midbuild_inuse.insert(issue) {
                        log::warn!(
                            "midbuild-watchdog: issue #{issue} ({sweep_id}) matches the \
                             mid-build-death signature (terminal, no PR, dirty worktree) but its \
                             worktree at {} is STILL IN USE by a live session the daemon does not \
                             track — {}. REFUSING to `git reset --hard` it: that is exactly how \
                             #4449 destroyed an active recovery session's uncommitted fix \
                             mid-commit. The worktree is left intact and the single recovery retry \
                             is NOT consumed; the watchdog re-assesses once the holder releases \
                             it. If the holder is stale, clear it (remove .loom-in-use / the \
                             claim-lock / the index.lock, or exit the shell) and the next tick \
                             will recover normally.",
                            self.worktree_path(issue).display(),
                            describe_worktree_use(&in_use),
                        );
                    }
                }
                MidbuildDecision::Healthy => {}
                MidbuildDecision::GiveUp => {
                    if self.midbuild_gaveup.insert(issue) {
                        log::error!(
                            "midbuild-watchdog: issue #{issue} ({sweep_id}) died mid-build again \
                             after an auto-recovery — giving up (bounded to one recovery). \
                             Operator intervention needed (inspect the dirty worktree at \
                             .loom/worktrees/issue-{issue}, then clean + re-dispatch)."
                        );
                        self.emit_event(Event::SweepCrashed {
                            issue,
                            checkpoint_phase: None,
                            classification: None,
                            death_class: None, // mid-build death, not pre-flight (#4386)
                            repo: None,        // stamped by emit_event (#3929)
                        });
                    }
                }
                MidbuildDecision::Recover => {
                    // Pre-flight token-health gate: if the whole pool is
                    // exhausted/blocked, defer WITHOUT consuming the single
                    // retry — re-dispatching now would just exhaust again.
                    if !self.token_pool_has_capacity() {
                        if self.midbuild_gaveup.insert(issue) {
                            log::error!(
                                "midbuild-watchdog: issue #{issue} ({sweep_id}) died mid-build \
                                 (dirty worktree, no PR) but every token account is \
                                 exhausted/blocked — deferring re-dispatch until the pool \
                                 recovers (#3895)."
                            );
                            self.emit_event(Event::SweepCrashed {
                                issue,
                                checkpoint_phase: None,
                                classification: None,
                                death_class: None, // pool-exhausted defer, not pre-flight (#4386)
                                repo: None,        // stamped by emit_event (#3929)
                            });
                        }
                        continue;
                    }
                    // #4556: probe for a confirmed-live sweep claim BEFORE any
                    // of the destructive recovery below. ORDERING IS
                    // LOAD-BEARING — this MUST stay ahead of
                    // `claim_lock_for_midbuild` (#4602/#4564), for two reasons:
                    //
                    // 1. That call TAKES OVER `.loom/locks/issue-<N>/owner.json`
                    //    in place, rewriting `owner_pid` to this daemon's pid and
                    //    `sweep_id` to `midbuild-watchdog-<dead>`. It does so
                    //    precisely when the lock still names the sweep this
                    //    daemon believes is dead — i.e. the false-dead case this
                    //    guard exists to catch. Probing afterwards would read the
                    //    watchdog's own record, whose argv is `loom-daemon`, not
                    //    `/loom:sweep <N>`, silently demoting the probe to its
                    //    weaker journal / process-scan legs.
                    // 2. The refusal path below `continue`s without releasing, so
                    //    a takeover that happened first would leave the live
                    //    sweep's owner record permanently clobbered: its own
                    //    `release_lock_owned` would then read `Superseded` and
                    //    skip its label restore.
                    //
                    // The dispatch-time live-claim guard (step 2.9) is NOT
                    // sufficient on its own here either, because this path does
                    // its destructive work FIRST — it burns the single recovery
                    // retry, `git reset --hard`s the shared worktree, and releases
                    // the lock — and only then calls `dispatch`. A refusal at that
                    // point would come too late: the live sweep's uncommitted work
                    // is already gone.
                    //
                    // Strictly stronger than the `#4463` ownership probe inside
                    // `claim_lock_for_midbuild` below, which only sees a lock
                    // re-acquired by a *newer* sweep in this daemon's own
                    // `.loom/locks/`: this probe also catches a still-live sweep
                    // whose lock a false-dead verdict already released, and one
                    // owned by a second `loom-daemon` instance on this host (3 of
                    // #4275's 7 dispatches). Complements the `#4449`
                    // `worktree_in_use` veto, which asks whether the *worktree* is
                    // held; this asks whether the *sweep* is alive, and a sweep
                    // stalled between phases holds neither an index.lock nor a
                    // `.loom-in-use` marker.
                    //
                    // Like `InUse`, the retry latch is deliberately NOT consumed:
                    // once the live sweep really finishes, a genuinely stuck
                    // worktree is still recoverable on a later tick.
                    if let Some(evidence) = self.live_claim_evidence(issue) {
                        if self.midbuild_liveclaim.insert(issue) {
                            log::warn!(
                                "midbuild-watchdog: issue #{issue} ({sweep_id}) matches the \
                                 mid-build-death signature, but the issue still has {evidence}. \
                                 REFUSING to claim its lock, clean its worktree or re-dispatch: \
                                 the sweep this daemon believes is dead is demonstrably alive, and \
                                 recovering here would `git reset --hard` its in-progress work, \
                                 clobber its lock-owner record and start a second sweep on the \
                                 same worktree (#4556). The single recovery retry is NOT consumed; \
                                 the watchdog re-assesses once the live sweep exits."
                            );
                        }
                        continue;
                    }
                    // No live claim: clear the log-once latch so a later refusal
                    // still surfaces.
                    self.midbuild_liveclaim.remove(&issue);

                    // #4463/#4564: before we clean the worktree or re-dispatch,
                    // take EXCLUSIVE ownership of the issue lock. If a newer
                    // sweep holds it (cross-instance double dispatch), its
                    // worktree and lock are live — cleaning the worktree here
                    // would clobber its uncommitted work, exactly the incident
                    // this guards against, so leave everything intact and skip
                    // the re-dispatch. #4463 only *probed* the lock read-only,
                    // which left a probe→clean TOCTOU: acquiring it instead
                    // (#4564) fences the clean below against a peer that would
                    // otherwise race into that window. A failed claim must NOT
                    // consume the single recovery retry, so this runs before
                    // the `midbuild_retried` latch.
                    let Some(watchdog_lock_id) = self.claim_lock_for_midbuild(issue, &sweep_id)
                    else {
                        continue;
                    };

                    // A transient defer may have logged a give-up earlier; clear
                    // it so a later genuine give-up still logs once.
                    self.midbuild_gaveup.remove(&issue);

                    log::warn!(
                        "midbuild-watchdog: issue #{issue} ({sweep_id}) died mid-build with a \
                         dirty worktree and no PR — cleaning the worktree and re-dispatching once \
                         (#3895)."
                    );

                    // Capture re-dispatch params from the dead entry.
                    let (model, effort, depends_on) = self
                        .entries
                        .get(&sweep_id)
                        .map(|i| (i.model.clone(), i.effort.clone(), i.depends_on))
                        .unwrap_or((None, None, None));

                    // Mark recovered BEFORE acting so any error path still counts
                    // the single allowed attempt (never loops).
                    self.midbuild_retried.insert(issue);

                    // #4485: release any dispatch-backoff window first. This
                    // recovery is already bounded to ONE attempt per issue by the
                    // latch above (marked before acting), so the rate limiting the
                    // backoff provides is redundant here — and a refusal would
                    // silently burn that single allowed attempt.
                    self.clear_dispatch_backoff(issue);

                    // Discard the dirty working tree so the resumed sweep starts
                    // clean (commits, if any, are preserved). Safe to do
                    // destructively: the watchdog holds the issue lock for the
                    // whole of this window (#4564), so no peer sweep can have
                    // claimed this worktree since the check above.
                    if let Err(e) = self.clean_worktree(issue) {
                        log::warn!(
                            "midbuild-watchdog: failed to clean worktree for issue #{issue} \
                             (continuing re-dispatch anyway): {e}"
                        );
                    }
                    // Hand the claim to the re-dispatch: release the lock the
                    // watchdog acquired above so `dispatch` can re-acquire it
                    // atomically under its own fresh sweep id. Ownership-checked
                    // (#4463) against the WATCHDOG's id, so the release can only
                    // ever remove the watchdog's own claim. This also covers the
                    // defensive case the pre-#4564 code handled here: a
                    // reconstructed entry whose lock the reaper never released
                    // was taken over in place by `claim_lock_for_midbuild`, so
                    // it is released here rather than left to wedge dispatch.
                    let _ = self.release_lock_owned(issue, &watchdog_lock_id);

                    match self.dispatch(
                        &SweepKind::Issue(issue),
                        None,
                        model.as_deref(),
                        effort.as_deref(),
                        depends_on,
                    ) {
                        Ok(outcome) => {
                            recovered += 1;
                            log::warn!(
                                "midbuild-watchdog: re-dispatched issue #{issue} as {} (pid {}) \
                                 after a mid-build death (#3895).",
                                outcome.sweep_id,
                                outcome.pid
                            );
                        }
                        Err(e) => {
                            log::error!(
                                "midbuild-watchdog: re-dispatch of issue #{issue} after a \
                                 mid-build death failed: {e} (issue left recoverable — its claim \
                                 was already restored by the reaper)."
                            );
                        }
                    }
                }
            }
        }
        recovered
    }

    // ------------------------------------------------------------------------
    // Review-phase stall watchdog (Issue #3910)
    // ------------------------------------------------------------------------

    /// How long a sweep's log file has gone un-appended (its "log silence").
    ///
    /// The daemon redirects each child's stdout/stderr to `log_path` in append
    /// mode, so every line a live sweep emits bumps the file's mtime. A sweep
    /// wedged in a hung role subagent (Judge/Doctor) produces **zero output**
    /// (#3910), so its log mtime stops advancing — this idle duration is the
    /// stall signal.
    ///
    /// Returns `None` when the file is missing or its mtime is unreadable / in
    /// the future (clock skew) — callers treat `None` as "cannot assess, leave
    /// alone", never as a stall.
    pub(crate) fn log_idle(&self, log_path: &Path) -> Option<Duration> {
        let modified = std::fs::metadata(log_path).ok()?.modified().ok()?;
        // `elapsed()` errors if `modified` is in the future (clock skew) — map
        // that to None so we never mistake skew for a stall.
        modified.elapsed().ok()
    }

    /// Run one review-phase stall watchdog tick (Issue #3910): for each running
    /// daemon-dispatched Issue sweep that has already made startup progress
    /// (past the #3887 startup watchdog's remit) but whose log file has gone
    /// silent past `timeout`, auto-cancel the wedged child and re-dispatch the
    /// issue **exactly once** (bounded — a second stall resolves to
    /// [`WatchdogDecision::GiveUp`] and is surfaced for the operator).
    ///
    /// This is the third liveness backstop, complementary to the startup-hang
    /// (#3887, *no* progress at all) and mid-build-death (#3895, made progress
    /// then the child *died*) watchdogs. It covers the remaining gap: a sweep
    /// that is **still alive** but stuck in a hung Judge/Doctor subagent — the
    /// canonical multi-hour hang from #3910. The re-dispatched sweep resumes
    /// from its checkpoint, so the review phase is re-run, not the whole build.
    ///
    /// Gated to sweeps past startup ([`sweep_made_progress`]) so it never
    /// double-acts with the startup watchdog on the same tick. No new event
    /// topics: the cancel reuses `sweep.issue.{N}.exited` / `sweep.global.
    /// completed` from [`finish_cancel`], the re-dispatch reuses
    /// `sweep.global.dispatch`, and a bounded give-up surfaces on the existing
    /// frozen `sweep.issue.{N}.crashed` topic. Returns the number of sweeps
    /// restarted this tick.
    ///
    /// [`sweep_made_progress`]: SweepRegistry::sweep_made_progress
    pub fn review_stall_watchdog_once(&mut self, timeout: Duration) -> usize {
        // Snapshot eligible candidates first so we can mutate below (mirrors
        // `watchdog_once`).
        let candidates: Vec<(SweepId, u32, PathBuf)> = self
            .entries
            .iter()
            .filter(|(id, info)| {
                matches!(info.state, SweepState::Running | SweepState::Pending)
                    && matches!(info.kind, SweepKind::Issue(_))
                    // Only sweeps we spawned (own the Child handle) are cancelable.
                    && self.children.contains_key(*id)
            })
            .filter_map(|(id, info)| match info.kind {
                SweepKind::Issue(issue) => Some((id.clone(), issue, info.log_path.clone())),
                SweepKind::PrSet(_) => None,
            })
            .collect();

        let mut restarts = 0usize;
        for (sweep_id, issue, log_path) in candidates {
            // Gate to sweeps past startup: a sweep that has made NO progress is
            // the #3887 startup watchdog's job, not ours. This keeps the two
            // backstops disjoint on any given tick.
            if !self.sweep_made_progress(issue, &log_path) {
                continue;
            }
            // No readable mtime ⇒ cannot assess ⇒ leave alone.
            let Some(idle) = self.log_idle(&log_path) else {
                continue;
            };
            let already_retried = self.review_stall_retried.contains(&issue);
            match review_stall_decision(idle, timeout, already_retried) {
                WatchdogDecision::Healthy => {}
                WatchdogDecision::GiveUp => {
                    if self.review_stall_gaveup.insert(issue) {
                        log::error!(
                            "review-stall-watchdog: sweep for issue #{issue} ({sweep_id}) stalled \
                             again (log silent {}s) after an auto-restart — giving up (bounded to \
                             one retry). Operator intervention needed: the review phase \
                             (Judge/Doctor) appears wedged; inspect \
                             .loom/logs/sweep-issue-{issue}.log, then cancel + re-dispatch (#3910).",
                            idle.as_secs()
                        );
                        self.emit_event(Event::SweepCrashed {
                            issue,
                            checkpoint_phase: None,
                            classification: None,
                            death_class: None, // review-stall give-up, not pre-flight (#4386)
                            repo: None,        // stamped by emit_event (#3929)
                        });
                    }
                }
                WatchdogDecision::Restart => {
                    log::warn!(
                        "review-stall-watchdog: sweep for issue #{issue} ({sweep_id}) produced no \
                         log output in {}s despite making startup progress — its review phase \
                         (Judge/Doctor) looks hung; auto-cancelling and re-dispatching once. The \
                         re-dispatch resumes from the sweep checkpoint (#3910).",
                        idle.as_secs()
                    );
                    // Capture re-dispatch params from the wedged entry BEFORE
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
                    self.review_stall_retried.insert(issue);
                    // A prior give-up log (if any) is now stale; clear it so a
                    // later genuine give-up still logs once.
                    self.review_stall_gaveup.remove(&issue);

                    // #4485: release any dispatch-backoff window first. This
                    // recovery is already bounded to ONE attempt per issue by the
                    // latch above (marked before acting), so the rate limiting the
                    // backoff provides is redundant here — and a refusal would
                    // silently burn that single allowed attempt.
                    self.clear_dispatch_backoff(issue);

                    // Cancel the wedged child (SIGTERM → grace → SIGKILL). This
                    // releases the per-issue lock and restores loom:building ->
                    // loom:issue, so the re-dispatch can re-acquire cleanly.
                    if let Err(e) = self.cancel(&sweep_id, WATCHDOG_CANCEL_GRACE) {
                        log::error!(
                            "review-stall-watchdog: auto-cancel of stalled sweep {sweep_id} \
                             (issue #{issue}) failed: {e}"
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
                                "review-stall-watchdog: re-dispatched issue #{issue} as {} (pid \
                                 {}) after a review-phase stall (#3910).",
                                outcome.sweep_id,
                                outcome.pid
                            );
                        }
                        Err(e) => {
                            log::error!(
                                "review-stall-watchdog: re-dispatch of issue #{issue} after a \
                                 stall failed: {e} (issue left recoverable — its claim was already \
                                 restored)."
                            );
                        }
                    }
                }
            }
        }
        restarts
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    unused_imports
)]
mod tests {
    use super::*;
    use crate::sweep_registry::test_support::*;
    use serial_test::serial;
    use std::os::unix::fs::PermissionsExt;
    use std::time::SystemTime;
    use tempfile::tempdir;

    /// The mid-build-death watchdog (#3895) recovery must still fire with a
    /// backoff window armed — the recovery is latched to one attempt per issue,
    /// so a backoff refusal would silently consume it and strand the sweep.
    #[test]
    fn midbuild_recovery_is_not_blocked_by_an_armed_backoff() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        let mut reg = backoff_registry(ws, 60, 900);

        make_dirty_git_worktree(ws, 6055);
        insert_terminal_issue(&mut reg, "sweep-issue-6055-dead", 6055, None);
        // An earlier fast failure armed a live window for this very issue.
        reg.record_dispatch_failure(6055);
        assert!(reg.dispatch_backoff_remaining(6055, Utc::now()).is_some());

        let recovered = reg.midbuild_watchdog_once();
        assert_eq!(recovered, 1, "the bounded one-shot recovery still re-dispatches");
        assert!(reg.issue_has_active_sweep(6055));
        assert_eq!(
            reg.dispatch_failure_count(6055),
            0,
            "the watchdog released the window before dispatching"
        );
    }

    /// The mid-build-death watchdog (#3895) must not recover an issue whose
    /// sweep is still alive — the 03:08:52Z re-dispatch in the #4275 timeline.
    ///
    /// The dispatch-time guard alone would be **too late** here: this path
    /// `git reset --hard`s the shared worktree and burns the single recovery
    /// retry *before* it calls `dispatch`. So the refusal must happen up front,
    /// and this test asserts exactly that — the uncommitted mid-build work
    /// survives and the retry is not consumed.
    ///
    /// Nothing holds the worktree (no `.loom-in-use`, no `index.lock`, no live
    /// lock owner), so the #4449 live-use veto does *not* fire: the only thing
    /// standing between the live sweep and a `git reset --hard` is #4556's
    /// live-claim probe.
    #[test]
    fn midbuild_watchdog_does_not_recover_an_issue_with_a_live_claim() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        let (mut reg, _record_log) = fixture_registry(ws);

        make_dirty_git_worktree(ws, 4562);
        insert_terminal_issue(&mut reg, "sweep-issue-4562-dead", 4562, None);
        // The live sweep's claim survives in the machine-level journal even
        // though this daemon believes sweep-…-dead is terminal — the exact
        // state a false-dead verdict leaves behind once it has released the
        // lock and reverted the label.
        let sweep = FakeSweep::spawn(4562);
        write_journal_entry(&reg, &ws.display().to_string(), 4562, sweep.pid());
        assert!(
            reg.worktree_in_use(4562).is_empty(),
            "precondition: the #4449 live-use veto must NOT be what refuses here"
        );

        assert_eq!(
            reg.midbuild_watchdog_once(),
            0,
            "no recovery while a live sweep claim exists for the issue"
        );
        assert!(
            ws.join(".loom/worktrees/issue-4562/dirty.txt").exists(),
            "the live sweep's uncommitted mid-build work MUST survive (#4556)"
        );
        assert!(
            !reg.midbuild_retried.contains(&4562),
            "a live-claim refusal must NOT consume the single recovery retry"
        );
        assert!(
            reg.midbuild_liveclaim.contains(&4562),
            "the refusal is recorded and logged once"
        );
        assert!(reg.entries.values().all(|i| i.state.is_terminal()), "no new sweep was created");
    }

    /// The inverse: once the live claim is gone, the same mid-build recovery
    /// proceeds normally. Without this the guard could wedge the recovery path
    /// permanently on a stale record.
    #[test]
    fn midbuild_watchdog_recovers_once_the_live_claim_is_gone() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        let (mut reg, _record_log) = fixture_registry(ws);

        make_dirty_git_worktree(ws, 4566);
        insert_terminal_issue(&mut reg, "sweep-issue-4566-dead", 4566, None);
        {
            let sweep = FakeSweep::spawn(4566);
            write_journal_entry(&reg, &ws.display().to_string(), 4566, sweep.pid());
            assert_eq!(reg.midbuild_watchdog_once(), 0, "refused while the claim is live");
            assert!(reg.midbuild_liveclaim.contains(&4566));
        } // the stand-in sweep exits here
        assert!(
            reg.live_claim_evidence(4566).is_none(),
            "the journal record's pid is dead once the stand-in sweep exits"
        );

        assert_eq!(reg.midbuild_watchdog_once(), 1, "recovery resumes once the claim dies");
        assert!(!reg.midbuild_liveclaim.contains(&4566), "the log-once latch is cleared");
        assert!(
            reg.midbuild_retried.contains(&4566),
            "the retry is consumed by the real recovery"
        );
    }

    /// Ordering regression pin for the #4556 × #4602/#4564 interaction: the
    /// live-claim probe MUST run **before** `claim_lock_for_midbuild`.
    ///
    /// #4602 replaced the watchdog's read-only `lock_owned_by_other` probe with
    /// a *mutating* takeover that rewrites `.loom/locks/issue-<N>/owner.json` to
    /// this daemon's pid and a `midbuild-watchdog-…` sweep id. A "keep both
    /// hunks" rebase that leaves the #4556 probe after that call compiles, and
    /// every other #4556 test still passes — but it is wrong twice over:
    ///
    /// 1. the probe's strongest leg (the live lock owner) is destroyed by the
    ///    very call it is meant to gate, since the daemon's argv is
    ///    `loom-daemon`, not `/loom:sweep <N>`; and
    /// 2. the refusal path `continue`s without releasing, so the live sweep's
    ///    owner record stays clobbered — its own `release_lock_owned` then reads
    ///    `Superseded` and skips its label restore.
    ///
    /// The fixture makes the takeover genuinely **eligible** (a leftover lock
    /// naming the dead sweep, with a dead owner pid) so that neither the #4449
    /// live-use veto nor the #4463 peer-owner refusal is what stops the
    /// recovery, and leaves the live claim visible **only** through the journal
    /// leg. Byte-comparing `owner.json` across the refused tick is what fails
    /// under the inverted order.
    #[test]
    fn midbuild_live_claim_probe_runs_before_the_lock_takeover() {
        // Above every plausible `pid_max`, so `is_pid_alive` reports dead.
        const DEAD_OWNER_PID: u32 = 2_147_483_640;
        let dir = tempdir().unwrap();
        let ws = dir.path();
        let (mut reg, _record_log) = fixture_registry(ws);

        make_dirty_git_worktree(ws, 4602);
        insert_terminal_issue(&mut reg, "sweep-issue-4602-dead", 4602, None);
        let lock = write_lock_owner(&reg, 4602, "sweep-issue-4602-dead", DEAD_OWNER_PID);
        let owner_path = lock.join("owner.json");
        let owner_before = std::fs::read_to_string(&owner_path).unwrap();

        let sweep = FakeSweep::spawn(4602);
        write_journal_entry(&reg, &ws.display().to_string(), 4602, sweep.pid());

        assert!(
            reg.worktree_in_use(4602).is_empty(),
            "precondition: the #4449 live-use veto must NOT be what refuses here \
             (the lock's owner pid is dead, so it is not live-use evidence)"
        );
        assert!(
            !reg.lock_owned_by_other(4602, "sweep-issue-4602-dead"),
            "precondition: the #4463 peer-owner probe must NOT be what refuses here — \
             the takeover is eligible, which is exactly what makes an ordering \
             regression observable"
        );

        assert_eq!(
            reg.midbuild_watchdog_once(),
            0,
            "the journal-visible live claim must refuse the recovery"
        );

        assert_eq!(
            std::fs::read_to_string(&owner_path).unwrap(),
            owner_before,
            "ORDERING REGRESSION: `claim_lock_for_midbuild` (#4602) rewrote the live \
             sweep's owner.json, which means the #4556 live-claim probe ran AFTER it. \
             The probe must come first — see the ORDERING IS LOAD-BEARING comment in \
             `midbuild_watchdog_once`'s Recover arm."
        );
        assert!(
            reg.midbuild_liveclaim.contains(&4602),
            "the refusal must be the live-claim one, logged once"
        );
        assert!(
            !reg.midbuild_retried.contains(&4602),
            "a live-claim refusal must NOT consume the single recovery retry"
        );
        assert!(
            ws.join(".loom/worktrees/issue-4602/dirty.txt").exists(),
            "the live sweep's uncommitted mid-build work MUST survive"
        );
    }

    /// The review-stall watchdog (#3910) must not re-dispatch either — the
    /// 03:54:25Z re-dispatch in the #4275 timeline.
    ///
    /// Unlike the mid-build path this one is *safe* to guard at dispatch time:
    /// it cancels its own child (SIGTERM → grace → SIGKILL) first and takes no
    /// destructive action on the worktree, so a refusal after the cancel costs
    /// nothing. The guard therefore lives in the shared `dispatch` entry point,
    /// where it also covers a claim held by a sweep this daemon never spawned.
    #[test]
    fn review_stall_watchdog_does_not_redispatch_an_issue_with_a_live_claim() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        let (mut reg, _record_log) = fixture_registry(ws);
        let sweep = FakeSweep::spawn(4563);
        write_journal_entry(&reg, &ws.display().to_string(), 4563, sweep.pid());

        // The watchdog's re-dispatch is the only step that can create a second
        // sweep; assert it is refused for a live-claimed issue.
        let err = reg
            .dispatch(&SweepKind::Issue(4563), None, None, None, None)
            .unwrap_err();
        assert!(
            err.downcast_ref::<LiveClaimDispatchError>().is_some(),
            "the watchdogs' shared re-dispatch entry point must refuse; got: {err}"
        );
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

    // ===================================================================
    // Mid-build-death watchdog (Issue #3895)
    // ===================================================================

    // --- midbuild_decision pure state machine ---

    #[test]
    fn midbuild_decision_no_dirty_worktree_is_healthy() {
        // No dirty worktree ⇒ nothing to recover, regardless of retry state.
        assert_eq!(midbuild_decision(false, false, false, false), MidbuildDecision::Healthy);
        assert_eq!(midbuild_decision(false, false, true, false), MidbuildDecision::Healthy);
    }

    #[test]
    fn midbuild_decision_produced_pr_is_healthy() {
        // A dead sweep that produced a PR is a completed Builder, not a
        // mid-build death — never recovered even with a dirty worktree.
        assert_eq!(midbuild_decision(true, true, false, false), MidbuildDecision::Healthy);
    }

    #[test]
    fn midbuild_decision_dirty_no_pr_first_time_recovers() {
        assert_eq!(midbuild_decision(true, false, false, false), MidbuildDecision::Recover);
    }

    #[test]
    fn midbuild_decision_dirty_no_pr_after_retry_gives_up() {
        // Bounded: a second mid-build death gives up (never loops).
        assert_eq!(midbuild_decision(true, false, true, false), MidbuildDecision::GiveUp);
    }

    #[test]
    fn midbuild_decision_in_use_worktree_is_never_recovered() {
        // #4449: a live holder vetoes the destructive path, and the veto sits
        // ABOVE the retry bookkeeping — it must win over both Recover and
        // GiveUp so a refusal never consumes the single recovery retry.
        assert_eq!(midbuild_decision(true, false, false, true), MidbuildDecision::InUse);
        assert_eq!(midbuild_decision(true, false, true, true), MidbuildDecision::InUse);
    }

    #[test]
    fn midbuild_decision_in_use_is_irrelevant_when_not_dirty_or_pr_exists() {
        // The in-use flag must not manufacture work: a clean worktree or a
        // produced PR is still Healthy even while a session holds the worktree.
        assert_eq!(midbuild_decision(false, false, false, true), MidbuildDecision::Healthy);
        assert_eq!(midbuild_decision(true, true, false, true), MidbuildDecision::Healthy);
    }

    #[test]
    fn worktree_dirty_and_clean_roundtrip() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        let (reg, _rec) = fixture_registry(ws);

        // No worktree ⇒ not dirty.
        assert!(!reg.worktree_dirty(70));

        // A worktree with an untracked file ⇒ dirty.
        make_dirty_git_worktree(ws, 70);
        assert!(reg.worktree_dirty(70));

        // Cleaning discards the untracked edit; the committed file survives.
        reg.clean_worktree(70).unwrap();
        assert!(!reg.worktree_dirty(70), "clean_worktree cleared the dirty state");
        assert!(ws.join(".loom/worktrees/issue-70/committed.txt").exists());
        assert!(!ws.join(".loom/worktrees/issue-70/dirty.txt").exists());
    }

    // --- midbuild_watchdog_once: detection + bounded recovery ---

    #[test]
    fn midbuild_recovers_dead_sweep_with_dirty_worktree_once() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        let (mut reg, rec) = fixture_registry(ws);

        // A sweep that got into the Builder phase (dirty worktree) then its
        // child died (terminal Exited) without producing a PR.
        make_dirty_git_worktree(ws, 6001);
        insert_terminal_issue(&mut reg, "sweep-issue-6001-dead", 6001, None);

        // Detected + recovered: worktree cleaned, issue re-dispatched once.
        let recovered = reg.midbuild_watchdog_once();
        assert_eq!(recovered, 1, "mid-build death detected and re-dispatched");
        assert!(reg.midbuild_retried.contains(&6001), "issue marked recovered (bounded)");

        // Worktree was cleaned before the re-dispatch.
        assert!(!ws.join(".loom/worktrees/issue-6001/dirty.txt").exists());
        assert!(ws.join(".loom/worktrees/issue-6001/committed.txt").exists());

        // A fresh sweep child actually ran, and an active entry now exists.
        assert!(
            wait_for_contents(&rec, "/loom:sweep 6001", 5000),
            "fake spawn ran for the re-dispatch"
        );
        assert!(reg.issue_has_active_sweep(6001), "a fresh sweep is now active for the issue");
    }

    #[test]
    fn midbuild_recovery_is_bounded_to_one() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        let (mut reg, _rec) = fixture_registry(ws);

        // The issue already used its single recovery; the re-dispatched sweep
        // ALSO died mid-build (dirty worktree, terminal, no PR).
        make_dirty_git_worktree(ws, 6002);
        insert_terminal_issue(&mut reg, "sweep-issue-6002-dead2", 6002, None);
        reg.midbuild_retried.insert(6002);

        let recovered = reg.midbuild_watchdog_once();
        assert_eq!(recovered, 0, "bounded: a second mid-build death is not re-dispatched");
        assert!(reg.midbuild_gaveup.contains(&6002), "give-up recorded for the operator");
        // The worktree is left intact for operator inspection (not cleaned).
        assert!(ws.join(".loom/worktrees/issue-6002/dirty.txt").exists());
    }

    #[test]
    fn midbuild_skips_sweep_that_produced_a_pr() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        let (mut reg, _rec) = fixture_registry(ws);

        // A dead sweep with a dirty worktree BUT a PR recorded is a completed
        // Builder, not a mid-build death — never recovered.
        make_dirty_git_worktree(ws, 6004);
        insert_terminal_issue(&mut reg, "sweep-issue-6004-pr", 6004, Some(4321));

        assert_eq!(reg.midbuild_watchdog_once(), 0, "a sweep that produced a PR is not recovered");
        assert!(!reg.midbuild_retried.contains(&6004));
    }

    #[test]
    fn midbuild_leaves_clean_worktree_alone() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        let (mut reg, _rec) = fixture_registry(ws);

        // A dead sweep whose worktree exists but is CLEAN (committed, no
        // uncommitted edits) is not a "dirty mid-build death" and is left alone.
        let wt = make_dirty_git_worktree(ws, 6005);
        std::fs::remove_file(wt.join("dirty.txt")).unwrap();
        assert!(!reg.worktree_dirty(6005), "precondition: worktree is clean");
        insert_terminal_issue(&mut reg, "sweep-issue-6005-clean", 6005, None);

        assert_eq!(reg.midbuild_watchdog_once(), 0, "a clean worktree is not recovered");
        assert!(!reg.midbuild_retried.contains(&6005));
    }

    #[test]
    fn midbuild_token_gate_defers_when_pool_exhausted_then_proceeds() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        let (mut reg, _rec) = fixture_registry(ws);

        make_dirty_git_worktree(ws, 6003);
        insert_terminal_issue(&mut reg, "sweep-issue-6003-dead", 6003, None);

        // Every account exhausted/blocked ⇒ the pre-flight gate defers WITHOUT
        // consuming the single retry.
        let tokens = ws.join(".loom").join("tokens");
        std::fs::create_dir_all(&tokens).unwrap();
        std::fs::write(tokens.join(".ranking"), "agent-1|exhausted\nagent-2|blocked\n").unwrap();

        assert_eq!(
            reg.midbuild_watchdog_once(),
            0,
            "token gate defers re-dispatch when every account is exhausted/blocked"
        );
        assert!(
            !reg.midbuild_retried.contains(&6003),
            "a deferral must NOT consume the single recovery"
        );
        // The dirty worktree is untouched while deferred.
        assert!(ws.join(".loom/worktrees/issue-6003/dirty.txt").exists());

        // Once a healthy account appears, recovery proceeds on the next tick.
        std::fs::write(tokens.join(".ranking"), "agent-1|exhausted\nagent-2|available\n").unwrap();
        assert_eq!(
            reg.midbuild_watchdog_once(),
            1,
            "recovery proceeds once a healthy account is available"
        );
        assert!(reg.midbuild_retried.contains(&6003));
        assert!(
            !ws.join(".loom/worktrees/issue-6003/dirty.txt").exists(),
            "worktree cleaned on recovery"
        );
    }

    #[test]
    fn midbuild_refuses_to_wipe_worktree_held_by_in_use_marker() {
        // The #4449 incident shape: the daemon's tracked sweep really did die
        // (terminal, no PR) but a SEPARATE live session is still using the
        // worktree. A `.loom-in-use` marker is the explicit form of that signal.
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        let (mut reg, _rec) = fixture_registry(ws);

        let wt = make_dirty_git_worktree(ws, 6101);
        insert_terminal_issue(&mut reg, "sweep-issue-6101-dead", 6101, None);
        std::fs::write(
            wt.join(".loom-in-use"),
            r#"{"shepherd_task_id": "recovery-doctor", "pid": 4321}"#,
        )
        .unwrap();

        assert!(!reg.worktree_in_use(6101).is_empty(), "marker is detected as a live holder");
        assert_midbuild_refused(&mut reg, ws, 6101, "a .loom-in-use marker names a live session");

        // Once the holder releases the worktree, the legitimate dead-sweep
        // recovery still works — the veto defers, it does not disable.
        std::fs::remove_file(wt.join(".loom-in-use")).unwrap();
        assert_eq!(reg.midbuild_watchdog_once(), 1, "recovery resumes once the holder releases");
        assert!(reg.midbuild_retried.contains(&6101));
        assert!(!reg.midbuild_inuse.contains(&6101), "the log-once latch is cleared");
        assert!(!wt.join("dirty.txt").exists(), "worktree cleaned on the real recovery");
    }

    #[test]
    fn midbuild_refuses_to_wipe_worktree_with_git_operation_in_flight() {
        // The precise window #4449 lost work in: a `git commit` was mid-write,
        // so git held index.lock. Never reset a worktree in that state.
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        let (mut reg, _rec) = fixture_registry(ws);

        let wt = make_dirty_git_worktree(ws, 6102);
        insert_terminal_issue(&mut reg, "sweep-issue-6102-dead", 6102, None);
        let index_lock =
            git_index_lock_path(&wt).expect("index.lock path resolves for a real repo");
        std::fs::write(&index_lock, "").unwrap();

        assert_midbuild_refused(&mut reg, ws, 6102, "a git index.lock write is in flight");

        // Committing finishes (lock released) ⇒ recovery is available again.
        std::fs::remove_file(&index_lock).unwrap();
        assert_eq!(reg.midbuild_watchdog_once(), 1, "recovery resumes once index.lock clears");
    }

    #[test]
    fn midbuild_refuses_to_wipe_worktree_with_live_claim_lock_owner() {
        // A claim-lock whose owner PID is ALIVE means some session (daemon or
        // not) still owns this issue — never reset its worktree underneath it.
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        let (mut reg, _rec) = fixture_registry(ws);

        make_dirty_git_worktree(ws, 6103);
        insert_terminal_issue(&mut reg, "sweep-issue-6103-dead", 6103, None);
        let lock = reg.config.locks_dir().join("issue-6103");
        std::fs::create_dir_all(&lock).unwrap();
        // Our own PID is trivially alive — stands in for the live holder.
        std::fs::write(
            lock.join("owner.json"),
            format!(
                r#"{{"issue": 6103, "owner_pid": {}, "acquired_at": "{}", "sweep_id": "manual-session"}}"#,
                std::process::id(),
                Utc::now().to_rfc3339()
            ),
        )
        .unwrap();

        assert_midbuild_refused(
            &mut reg,
            ws,
            6103,
            "a live claim-lock owner still holds the issue",
        );
    }

    #[test]
    fn midbuild_ignores_stale_claim_lock_with_dead_owner() {
        // The inverse guard: the dead sweep's OWN claim-lock, left behind
        // because the reaper never released it, must not wedge the legitimate
        // dead-sweep recovery path forever.
        //
        // The lock's `sweep_id` is deliberately the dead entry's own id. A lock
        // naming a *different* sweep is a separate, pre-existing refusal
        // (`lock_owned_by_other`, #4463: a newer sweep superseded this one) that
        // fails closed on the id comparison alone and is covered by its own
        // tests. This test isolates the #4449 live-use veto: a dead owner PID is
        // not live-use evidence, so recovery proceeds.
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        let (mut reg, _rec) = fixture_registry(ws);

        make_dirty_git_worktree(ws, 6104);
        insert_terminal_issue(&mut reg, "sweep-issue-6104-dead", 6104, None);
        let lock = reg.config.locks_dir().join("issue-6104");
        std::fs::create_dir_all(&lock).unwrap();
        std::fs::write(
            lock.join("owner.json"),
            format!(
                r#"{{"issue": 6104, "owner_pid": 2147483640, "acquired_at": "{}", "sweep_id": "sweep-issue-6104-dead"}}"#,
                Utc::now().to_rfc3339()
            ),
        )
        .unwrap();

        assert!(
            reg.worktree_in_use(6104).is_empty(),
            "a dead owner's lock is not live-use evidence"
        );
        assert_eq!(reg.midbuild_watchdog_once(), 1, "a stale lock does not block recovery");
        assert!(!ws.join(".loom/worktrees/issue-6104/dirty.txt").exists());
    }

    // --- #4564: the clean runs while the watchdog OWNS the issue lock --------

    #[test]
    fn midbuild_refuses_to_clean_worktree_when_a_peer_owns_the_issue_lock() {
        // A cross-instance sweep holds issue #6106's claim lock. Its owner PID
        // is deliberately DEAD so the #4449 live-use veto contributes nothing
        // (`worktree_in_use` ignores dead owners) — the ONLY thing that can stop
        // the destructive arm here is the ownership check in
        // `claim_lock_for_midbuild`. This is the shape the pre-#4564 read-only
        // probe could lose to: a peer holding the claim while the watchdog
        // `git reset --hard`s the worktree it just claimed.
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        let (mut reg, _rec) = fixture_registry(ws);

        make_dirty_git_worktree(ws, 6106);
        insert_terminal_issue(&mut reg, "sweep-issue-6106-dead", 6106, None);
        let lock = write_lock_owner(&reg, 6106, "sweep-issue-6106-peer", 2_147_483_640);

        assert!(
            reg.worktree_in_use(6106).is_empty(),
            "precondition: a dead owner PID is not live-use evidence, so only the \
             lock-ownership check can refuse here"
        );

        assert_eq!(reg.midbuild_watchdog_once(), 0, "a peer-owned claim blocks the recovery");
        assert!(
            ws.join(".loom/worktrees/issue-6106/dirty.txt").exists(),
            "the peer's uncommitted work MUST survive (#4564)"
        );
        assert!(
            !reg.midbuild_retried.contains(&6106),
            "a refusal must NOT consume the single recovery retry"
        );

        // The peer's lock is left exactly as it was — the watchdog neither
        // released it nor took it over.
        let owner: LockOwner =
            serde_json::from_str(&std::fs::read_to_string(lock.join("owner.json")).unwrap())
                .unwrap();
        assert_eq!(owner.sweep_id, "sweep-issue-6106-peer", "the peer's claim is untouched");
    }

    #[test]
    fn midbuild_claim_holds_the_issue_lock_across_the_clean() {
        // The structural fix for the probe→clean TOCTOU (#4564): the watchdog no
        // longer merely *reads* the lock before cleaning, it *holds* it. This
        // exercises `claim_lock_for_midbuild` directly, because once the claim
        // and the clean are one operation there is no longer an in-between
        // moment a test could inject a peer into — the invariant to pin down is
        // "while the watchdog holds the claim, a peer cannot acquire it".
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        let (reg, _rec) = fixture_registry(ws);

        // 1. Free lock ⇒ claimed via the POSIX-atomic `mkdir` path.
        let held = reg
            .claim_lock_for_midbuild(6107, "sweep-issue-6107-dead")
            .expect("a free claim lock is acquired");
        assert_eq!(held, "midbuild-watchdog-sweep-issue-6107-dead");

        // 2. THE POINT: a peer racing in during the clean window now loses.
        //    Before #4564 the probe had already returned "free" and the peer's
        //    `acquire_lock` would have succeeded, handing it a live claim on a
        //    worktree the watchdog was about to reset.
        assert!(
            reg.acquire_lock(6107, "sweep-issue-6107-peer").is_err(),
            "a peer cannot acquire the claim while the watchdog holds it (#4564)"
        );

        // 3. The claim is released under the WATCHDOG's id so `dispatch` can
        //    re-acquire it under its own fresh sweep id.
        assert_eq!(reg.release_lock_owned(6107, &held), LockReleaseOutcome::Released);
        assert!(reg.acquire_lock(6107, "sweep-issue-6107-peer").is_ok(), "released ⇒ acquirable");

        // 4. A claim already held by a DIFFERENT sweep is refused outright.
        assert!(
            reg.claim_lock_for_midbuild(6107, "sweep-issue-6107-dead")
                .is_none(),
            "a peer-owned claim is never taken over"
        );

        // 5. The dead sweep's OWN stale claim is taken over IN PLACE (the dir is
        //    never freed and re-created, which would re-open the very window
        //    being closed) — and remains un-acquirable by a peer throughout.
        write_lock_owner(&reg, 6108, "sweep-issue-6108-dead", 2_147_483_640);
        let held = reg
            .claim_lock_for_midbuild(6108, "sweep-issue-6108-dead")
            .expect("the dead sweep's own stale claim is taken over");
        assert_eq!(held, "midbuild-watchdog-sweep-issue-6108-dead");
        let owner: LockOwner = serde_json::from_str(
            &std::fs::read_to_string(reg.config.locks_dir().join("issue-6108/owner.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(owner.sweep_id, held, "owner.json now records the watchdog as the holder");
        assert_eq!(owner.owner_pid, std::process::id(), "…with a live owner PID");
        assert!(
            reg.acquire_lock(6108, "sweep-issue-6108-peer").is_err(),
            "the takeover never leaves the lock momentarily free"
        );
    }

    #[test]
    fn midbuild_releases_its_own_claim_before_re_dispatching() {
        // The watchdog's claim must be handed off, not leaked: a lock left
        // behind would fail the re-dispatch on a collision AND (owned by the
        // live daemon PID) wedge every later tick behind the #4449 live-claim
        // veto. Start from the dead sweep's own stale lock so the takeover path
        // — not the plain `mkdir` path — is the one exercised.
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        let (mut reg, _rec) = fixture_registry(ws);

        make_dirty_git_worktree(ws, 6109);
        insert_terminal_issue(&mut reg, "sweep-issue-6109-dead", 6109, None);
        write_lock_owner(&reg, 6109, "sweep-issue-6109-dead", 2_147_483_640);

        assert_eq!(reg.midbuild_watchdog_once(), 1, "recovery proceeds and re-dispatches");
        assert!(!ws.join(".loom/worktrees/issue-6109/dirty.txt").exists(), "worktree cleaned");

        // The lock now belongs to the freshly dispatched sweep — proof the
        // watchdog released its own claim before dispatching (`dispatch` would
        // otherwise have failed on the lock collision).
        let owner: LockOwner = serde_json::from_str(
            &std::fs::read_to_string(reg.config.locks_dir().join("issue-6109/owner.json")).unwrap(),
        )
        .unwrap();
        assert!(
            owner.sweep_id.starts_with("sweep-issue-6109-"),
            "the re-dispatched sweep owns the claim, got {}",
            owner.sweep_id
        );
        assert!(
            !owner.sweep_id.starts_with("midbuild-watchdog-"),
            "the watchdog's transient claim must not be left behind"
        );
    }

    #[test]
    fn midbuild_refuses_to_wipe_worktree_with_live_process_cwd_inside() {
        // The signal that would have saved the #4449 session with no cooperation
        // from it at all: a live process whose cwd is inside the worktree.
        // `find_processes_using_directory` degrades to an empty list on hosts
        // where it cannot probe (no /proc, no lsof), so self-skip there rather
        // than assert something the host cannot express.
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        let (mut reg, _rec) = fixture_registry(ws);

        let wt = make_dirty_git_worktree(ws, 6105);
        insert_terminal_issue(&mut reg, "sweep-issue-6105-dead", 6105, None);

        let mut child = Command::new("sleep")
            .arg("30")
            .current_dir(&wt)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn a live process with cwd inside the worktree");

        // The child's `chdir` happens after `fork`, so poll briefly rather than
        // read the probe once and race it.
        let mut detected = Vec::new();
        for _ in 0..40 {
            detected = crate::worktree_ops::safety::find_processes_using_directory(&wt);
            if !detected.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if detected.is_empty() {
            // Probe unavailable on this host — nothing to assert; don't leak.
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        assert!(
            detected.contains(&child.id()),
            "the probe found the spawned holder: {detected:?}"
        );

        assert_midbuild_refused(&mut reg, ws, 6105, "a live process has its cwd in the worktree");

        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn midbuild_ignores_running_sweeps() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        let (mut reg, _rec) = fixture_registry(ws);

        // A still-Running sweep (not terminal) is never a mid-build-death
        // candidate, even with a dirty worktree.
        make_dirty_git_worktree(ws, 6006);
        reg.entries.insert(
            "sweep-issue-6006-live".to_string(),
            SweepInfo {
                sweep_id: "sweep-issue-6006-live".to_string(),
                kind: SweepKind::Issue(6006),
                pid: 2_147_483_640,
                token_name: "unknown".into(),
                runtime: "unknown".into(),
                runtime_source: None,
                log_path: reg.compute_log_path(6006),
                idempotency_key: None,
                started_at: Utc::now(),
                state: SweepState::Running,
                latest_phase: None,
                pr_number: None,
                model: None,
                effort: None,
                depends_on: None,
                repo: None,
            },
        );

        assert_eq!(reg.midbuild_watchdog_once(), 0, "a Running sweep is not a mid-build death");
        assert!(!reg.midbuild_retried.contains(&6006));
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
        assert!(
            wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS),
            "hung fixture child should start"
        );
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
        assert!(wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS));

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

    // --- progress latch (Issue #4088) ---

    /// AC5 regression (the headline bug): a sweep that made progress (worktree
    /// present), then had that worktree AND its checkpoint torn down at
    /// completion while still `Running`, with `elapsed` far past the timeout,
    /// must NOT be cancelled or re-dispatched. On `origin/main` the stateless
    /// probe reads "no progress" after cleanup and re-dispatches against the
    /// now-closed issue; the per-`SweepId` latch prevents that.
    #[test]
    fn watchdog_does_not_redispatch_completed_sweep_after_cleanup() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        let mut reg = hung_child_registry(ws);

        let out = reg
            .dispatch(&SweepKind::Issue(4078), None, None, None, None)
            .unwrap();
        assert!(wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS));

        // Progress appears (Builder created a worktree), and a tick observes +
        // latches it.
        let wt = ws.join(".loom").join("worktrees").join("issue-4078");
        std::fs::create_dir_all(&wt).unwrap();
        assert_eq!(reg.watchdog_once(Duration::from_secs(10)), 0);
        assert!(
            reg.watchdog_progressed.contains(&out.sweep_id),
            "progress is latched for the sweep"
        );

        // Completion tears down every progress signal (merge-pr.sh removes the
        // worktree; /loom:sweep deletes the checkpoint). The stateless probe now
        // reads no-progress — the exact #4078 condition.
        std::fs::remove_dir_all(&wt).unwrap();
        assert!(
            !reg.sweep_made_progress(4078, &out.log_path),
            "stateless probe reads no-progress after cleanup (the bug's precondition)"
        );

        // Even backdated far past the timeout, the latched sweep is left alone.
        backdate(&mut reg, &out.sweep_id, 9999);
        assert_eq!(
            reg.watchdog_once(Duration::from_secs(10)),
            0,
            "a completed-then-cleaned-up sweep is never re-dispatched (AC5)"
        );
        assert!(
            !reg.watchdog_retried.contains(&4078),
            "no retry recorded for the completed sweep"
        );

        let _ = reg.cancel(&out.sweep_id, Duration::from_secs(2));
    }

    /// AC2 on re-dispatch (the Finding 6 trap): the latch is keyed by `SweepId`,
    /// not issue. A latch keyed by issue would make a *re-dispatched* sweep that
    /// genuinely hangs read as "already progressed" and never be rescued —
    /// silently defanging the watchdog for the very issues it already rescued
    /// once. A prior sweep's latch (distinct `SweepId`) must not cover a new
    /// hung sweep for the same issue.
    #[test]
    fn watchdog_latch_is_scoped_by_sweep_id_so_redispatch_is_still_rescued() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        let mut reg = hung_child_registry(ws);

        let out = reg
            .dispatch(&SweepKind::Issue(4060), None, None, None, None)
            .unwrap();
        assert!(wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS));

        // Simulate a PRIOR, now-gone sweep for the SAME issue having progressed:
        // its distinct SweepId is latched. An issue-keyed latch would instead
        // hold `4060` and wrongly cover the current sweep.
        reg.watchdog_progressed
            .insert("sweep-issue-4060-prior".to_string());
        assert!(
            !reg.watchdog_progressed.contains(&out.sweep_id),
            "the current (hung) sweep is not itself latched"
        );

        // The current sweep never progressed; backdate it past the timeout.
        backdate(&mut reg, &out.sweep_id, 600);
        assert_eq!(
            reg.watchdog_once(Duration::from_secs(60)),
            1,
            "a re-dispatched sweep that hangs at startup is still rescued (AC2)"
        );
        assert!(reg.watchdog_retried.contains(&4060));

        if let Some(id) = running_issue_sweep_id(&reg, 4060) {
            let _ = reg.cancel(&id, Duration::from_secs(2));
        }
    }

    /// The latch is monotonic (stays true across ticks once observed) AND scoped
    /// to a single `SweepId` — a sibling sweep that never progressed is
    /// unaffected and still eligible for rescue.
    #[test]
    fn watchdog_latch_is_monotonic_and_per_sweep() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        let mut reg = hung_child_registry(ws);

        let a = reg
            .dispatch(&SweepKind::Issue(5001), None, None, None, None)
            .unwrap();
        assert!(wait_until_alive(a.pid, FIXTURE_CHILD_WAIT_MS));
        let b = reg
            .dispatch(&SweepKind::Issue(5002), None, None, None, None)
            .unwrap();
        assert!(wait_until_alive(b.pid, FIXTURE_CHILD_WAIT_MS));

        // Only A makes progress.
        let wt_a = ws.join(".loom").join("worktrees").join("issue-5001");
        std::fs::create_dir_all(&wt_a).unwrap();
        assert_eq!(reg.watchdog_once(Duration::from_secs(10)), 0);
        assert!(reg.watchdog_progressed.contains(&a.sweep_id), "A latched");
        assert!(
            !reg.watchdog_progressed.contains(&b.sweep_id),
            "B never progressed ⇒ not latched"
        );

        // Monotonic: remove A's worktree; a later tick keeps A latched.
        std::fs::remove_dir_all(&wt_a).unwrap();
        assert_eq!(reg.watchdog_once(Duration::from_secs(10)), 0);
        assert!(
            reg.watchdog_progressed.contains(&a.sweep_id),
            "A stays latched across ticks even with its worktree gone"
        );

        // B, never progressing and backdated, is still restarted — A's latch is
        // scoped to A and does not cover its sibling.
        backdate(&mut reg, &b.sweep_id, 600);
        assert_eq!(
            reg.watchdog_once(Duration::from_secs(60)),
            1,
            "the un-latched sibling is rescued"
        );
        assert!(reg.watchdog_retried.contains(&5002));
        assert!(!reg.watchdog_retried.contains(&5001));

        for issue in [5001u32, 5002u32] {
            if let Some(id) = running_issue_sweep_id(&reg, issue) {
                let _ = reg.cancel(&id, Duration::from_secs(2));
            }
        }
    }

    /// Latch pruning: entries for sweeps GC'd from `entries` are dropped from
    /// the latch, so the per-`SweepId` set cannot grow unbounded across many
    /// dispatches.
    #[test]
    fn watchdog_latch_pruned_on_entry_gc() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        let (mut reg, _rec) = fixture_registry(ws);

        // A terminal entry aged past the retention window, with its SweepId
        // latched — exactly the state left behind by a completed sweep.
        let sid = "sweep-issue-6001-done".to_string();
        let old = Utc::now() - chrono::Duration::seconds(TERMINAL_RETENTION_SECS + 60);
        reg.entries.insert(
            sid.clone(),
            SweepInfo {
                sweep_id: sid.clone(),
                kind: SweepKind::Issue(6001),
                pid: 2_147_483_640,
                token_name: "unknown".into(),
                runtime: "unknown".into(),
                runtime_source: None,
                log_path: ws.join(".loom/logs/sweep-issue-6001.log"),
                idempotency_key: None,
                started_at: old,
                state: SweepState::Exited {
                    code: Some(0),
                    at: old,
                },
                latest_phase: None,
                pr_number: None,
                model: None,
                effort: None,
                depends_on: None,
                repo: None,
            },
        );
        reg.watchdog_progressed.insert(sid.clone());

        // GC drops the terminal entry and must prune its latch entry with it.
        reg.reap_once();
        assert!(!reg.entries.contains_key(&sid), "terminal entry is GC'd");
        assert!(
            !reg.watchdog_progressed.contains(&sid),
            "the latch entry is pruned alongside the GC'd sweep"
        );
    }

    // ===================================================================
    // Occupancy accounting — startup-proof grace (Issue #4003)
    // ===================================================================

    #[test]
    fn startup_proof_grace_setter_roundtrips() {
        let tmp = tempdir().unwrap();
        let (mut reg, _rec) = fixture_registry(tmp.path());
        assert_eq!(
            reg.startup_proof_grace(),
            Duration::from_secs(DEFAULT_STARTUP_PROOF_GRACE_SECS),
            "default matches the shipped constant"
        );
        reg.set_startup_proof_grace(Duration::from_secs(12));
        assert_eq!(reg.startup_proof_grace(), Duration::from_secs(12));
    }

    /// Test-plan item (a): a dispatched sweep that never emits the
    /// startup-proof signal releases its admission slot **before** the 300s
    /// watchdog fires. `hung_child_registry`'s fixture child produces zero
    /// progress signal (no worktree, no checkpoint, log stuck at the spawn
    /// header) for its whole life, exactly the "wedged at startup" case #4003
    /// targets.
    #[test]
    fn occupied_issues_excludes_unproven_sweep_past_grace_window() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        let mut reg = hung_child_registry(ws);
        reg.set_startup_proof_grace(Duration::from_millis(50));

        let out = reg
            .dispatch(&SweepKind::Issue(7001), None, None, None, None)
            .unwrap();
        assert!(wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS));

        // Well past the 50ms grace, with zero progress signal.
        backdate(&mut reg, &out.sweep_id, 5);
        let occupied = reg.occupied_issues();
        assert!(
            !occupied.contains(&7001),
            "an unproven sweep past its grace window must stop consuming an \
             admission slot, freeing capacity long before the (unchanged) 300s \
             startup watchdog would cancel/re-dispatch it"
        );
        // The registry's own liveness bookkeeping is untouched: the entry is
        // still `Running` and still the authoritative in-flight/dedup view —
        // discounting occupancy never re-dispatches the SAME issue.
        assert!(matches!(reg.get(&out.sweep_id).unwrap().state, SweepState::Running));

        let _ = reg.cancel(&out.sweep_id, Duration::from_secs(2));
    }

    /// A freshly-dispatched sweep — even one that will eventually turn out to
    /// be hung — counts toward occupancy while inside its grace window. This
    /// is what keeps a burst dispatch from immediately under-counting its own
    /// occupancy the instant `dispatch()` returns.
    #[test]
    fn occupied_issues_keeps_fresh_dispatch_inside_grace_window() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        let mut reg = hung_child_registry(ws);
        reg.set_startup_proof_grace(Duration::from_secs(DEFAULT_STARTUP_PROOF_GRACE_SECS));

        let out = reg
            .dispatch(&SweepKind::Issue(7002), None, None, None, None)
            .unwrap();
        assert!(wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS));

        // No backdating: elapsed is ~0s, well inside the 90s default grace.
        let occupied = reg.occupied_issues();
        assert!(
            occupied.contains(&7002),
            "a fresh dispatch must count toward occupancy immediately, \
             regardless of whether it has produced any startup-proof signal yet"
        );

        let _ = reg.cancel(&out.sweep_id, Duration::from_secs(2));
    }

    /// Test-plan item (c) (throughput regression guard): a sweep that HAS
    /// proven startup progress must never be discounted, no matter how long
    /// ago it was dispatched or how short the configured grace is. Without
    /// this, a normal sweep whose Builder phase legitimately runs for hours
    /// would eventually be discounted from occupancy — silently inflating the
    /// effective concurrency cap for reasons unrelated to health. Proven
    /// progress must dominate elapsed time, unconditionally.
    #[test]
    fn occupied_issues_never_discounts_proven_sweep_regardless_of_age() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        let mut reg = hung_child_registry(ws);
        // A pathologically tiny grace: if elapsed-vs-grace were the only
        // signal, this sweep would be discounted instantly.
        reg.set_startup_proof_grace(Duration::from_millis(1));

        let out = reg
            .dispatch(&SweepKind::Issue(7003), None, None, None, None)
            .unwrap();
        assert!(wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS));

        // Simulate progress (Builder created a worktree) AND age the entry
        // far past any plausible grace or watchdog window.
        let wt = ws.join(".loom").join("worktrees").join("issue-7003");
        std::fs::create_dir_all(&wt).unwrap();
        backdate(&mut reg, &out.sweep_id, 9999);

        let occupied = reg.occupied_issues();
        assert!(
            occupied.contains(&7003),
            "a sweep that proved startup progress must never be discounted \
             from occupancy, regardless of elapsed time — this is the \
             guarantee that a fleet of normally-starting sweeps dispatches at \
             the same rate as before #4003"
        );

        let _ = reg.cancel(&out.sweep_id, Duration::from_secs(2));
    }

    /// The occupancy check and the startup watchdog (#3887/#4088) share the
    /// SAME per-`SweepId` progress latch (`watchdog_progressed`): once either
    /// call site observes progress, neither ever "un-sees" it — even after
    /// the underlying filesystem signal is torn down (e.g. at completion, or
    /// in this test, a manual removal standing in for that teardown).
    #[test]
    fn occupied_issues_latch_is_shared_with_watchdog() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        let mut reg = hung_child_registry(ws);
        reg.set_startup_proof_grace(Duration::from_millis(1));

        let out = reg
            .dispatch(&SweepKind::Issue(7004), None, None, None, None)
            .unwrap();
        assert!(wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS));

        let wt = ws.join(".loom").join("worktrees").join("issue-7004");
        std::fs::create_dir_all(&wt).unwrap();
        backdate(&mut reg, &out.sweep_id, 9999);

        // Observe progress via occupancy accounting first — this latches it.
        assert!(reg.occupied_issues().contains(&7004));
        assert!(reg.watchdog_progressed.contains(&out.sweep_id));

        // Tear down the filesystem signal (mirrors what happens at
        // completion) and confirm BOTH consumers still treat it as proven.
        std::fs::remove_dir_all(&wt).unwrap();
        assert!(
            reg.occupied_issues().contains(&7004),
            "occupancy must not re-discount a sweep once the latch has fired"
        );
        assert_eq!(
            reg.watchdog_once(Duration::from_secs(10)),
            0,
            "the startup watchdog must not restart a sweep the occupancy \
             check already latched as progressed"
        );

        let _ = reg.cancel(&out.sweep_id, Duration::from_secs(2));
    }

    /// Observability (Issue #4003 AC): the daemon can report how long a sweep
    /// has spent in the spawned-but-not-started state, and the report clears
    /// the instant progress is observed.
    #[test]
    fn unproven_startups_reports_elapsed_and_clears_once_proven() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        let mut reg = hung_child_registry(ws);

        let out = reg
            .dispatch(&SweepKind::Issue(7005), None, None, None, None)
            .unwrap();
        assert!(wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS));
        backdate(&mut reg, &out.sweep_id, 42);

        let unproven = reg.unproven_startups();
        let entry = unproven.iter().find(|(issue, _)| *issue == 7005);
        assert!(
            entry.is_some(),
            "an unproven live sweep must be reported by unproven_startups()"
        );
        let (_, elapsed) = entry.unwrap();
        assert!(
            *elapsed >= Duration::from_secs(42),
            "reported elapsed should reflect the backdated dispatch time, got {elapsed:?}"
        );

        // Progress appears — the report must clear immediately.
        let wt = ws.join(".loom").join("worktrees").join("issue-7005");
        std::fs::create_dir_all(&wt).unwrap();
        assert!(
            !reg.unproven_startups()
                .iter()
                .any(|(issue, _)| *issue == 7005),
            "a sweep that has proven progress must not be reported as unproven"
        );

        let _ = reg.cancel(&out.sweep_id, Duration::from_secs(2));
    }

    // --- resolve_startup_proof_grace precedence ---

    #[test]
    #[serial]
    fn resolve_startup_proof_grace_precedence() {
        std::env::remove_var(STARTUP_PROOF_GRACE_ENV);
        assert_eq!(
            resolve_startup_proof_grace(&StartupRaceConfig::default()),
            Duration::from_secs(DEFAULT_STARTUP_PROOF_GRACE_SECS)
        );
        let cfg = StartupRaceConfig {
            startup_proof_grace_secs: Some(30),
            ..Default::default()
        };
        assert_eq!(resolve_startup_proof_grace(&cfg), Duration::from_secs(30));
        std::env::set_var(STARTUP_PROOF_GRACE_ENV, "5");
        assert_eq!(resolve_startup_proof_grace(&cfg), Duration::from_secs(5));
        std::env::remove_var(STARTUP_PROOF_GRACE_ENV);
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
            r#"{"autonomous":{"dispatchStaggerMs":3000,"watchdog":{"enabled":false,"timeoutSecs":90,"intervalSecs":15,"reviewStall":false,"reviewStallTimeoutSecs":1800,"startupProofGraceSecs":45}}}"#,
        );
        assert_eq!(
            read_startup_race_config(tmp.path()),
            StartupRaceConfig {
                dispatch_stagger_ms: Some(3000),
                watchdog_enabled: Some(false),
                watchdog_timeout_secs: Some(90),
                watchdog_interval_secs: Some(15),
                review_stall_enabled: Some(false),
                review_stall_timeout_secs: Some(1800),
                startup_proof_grace_secs: Some(45),
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
    #[serial(loom_config_env)]
    fn startup_race_config_project_tier_only_is_honored_like_legacy() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempdir().unwrap();
        write_project_cfg(
            tmp.path(),
            r#"{"autonomous":{"dispatchStaggerMs":3000,"watchdog":{"enabled":false,"timeoutSecs":90}}}"#,
        );
        let cfg = read_startup_race_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(cfg.dispatch_stagger_ms, Some(3000));
        assert_eq!(cfg.watchdog_enabled, Some(false));
        assert_eq!(cfg.watchdog_timeout_secs, Some(90));
    }

    #[test]
    #[serial(loom_config_env)]
    fn startup_race_config_project_tier_overrides_legacy_overlap_and_supplies_non_overlap() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempdir().unwrap();
        write_cfg(
            tmp.path(),
            r#"{"autonomous":{"dispatchStaggerMs":3000,"watchdog":{"timeoutSecs":90}}}"#,
        );
        write_project_cfg(tmp.path(), r#"{"autonomous":{"dispatchStaggerMs":750}}"#);
        let cfg = read_startup_race_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        // Overlapping `dispatchStaggerMs` -> project tier wins.
        assert_eq!(cfg.dispatch_stagger_ms, Some(750));
        // Non-overlapping `watchdog.timeoutSecs` still supplied by legacy tier.
        assert_eq!(cfg.watchdog_timeout_secs, Some(90));
    }

    #[test]
    #[serial(loom_config_env)]
    fn startup_race_config_local_tier_overrides_legacy_and_project() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempdir().unwrap();
        write_cfg(tmp.path(), r#"{"autonomous":{"dispatchStaggerMs":3000}}"#);
        write_project_cfg(tmp.path(), r#"{"autonomous":{"dispatchStaggerMs":750}}"#);
        write_local_cfg(tmp.path(), r#"{"autonomous":{"dispatchStaggerMs":10}}"#);
        let cfg = read_startup_race_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(cfg.dispatch_stagger_ms, Some(10));
    }

    /// Regression (#4058): `dispatchStaggerMs: 0` set only at the project
    /// tier must still be read as `Some(0)` ("disable stagger"), not dropped
    /// to `None` like a zero `watchdog.timeoutSecs` would be.
    #[test]
    #[serial(loom_config_env)]
    fn startup_race_config_project_tier_dispatch_stagger_zero_is_meaningful() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempdir().unwrap();
        write_project_cfg(tmp.path(), r#"{"autonomous":{"dispatchStaggerMs":0}}"#);
        let cfg = read_startup_race_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(cfg.dispatch_stagger_ms, Some(0));
    }

    /// Explicit `null` at the project tier clears a legacy-tier value —
    /// documents the `deep_merge` "null clears" semantics (#4058) at this
    /// migrated site.
    #[test]
    #[serial(loom_config_env)]
    fn startup_race_config_explicit_null_in_project_tier_clears_legacy_value() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempdir().unwrap();
        write_cfg(tmp.path(), r#"{"autonomous":{"dispatchStaggerMs":3000}}"#);
        write_project_cfg(tmp.path(), r#"{"autonomous":{"dispatchStaggerMs":null}}"#);
        let cfg = read_startup_race_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(cfg.dispatch_stagger_ms, None);
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
        // AC1 (#4088): the default no-progress window is 300s — clear of the
        // observed 110–150s healthy dispatch→worktree distribution.
        assert_eq!(DEFAULT_WATCHDOG_TIMEOUT_SECS, 300);
        assert_eq!(
            resolve_watchdog_timeout(&StartupRaceConfig::default()),
            Duration::from_secs(300)
        );
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

    // ===================================================================
    // Review-phase stall watchdog (Issue #3910)
    // ===================================================================

    // --- review_stall_decision pure state machine ---

    #[test]
    fn review_stall_decision_within_timeout_is_healthy() {
        let t = Duration::from_secs(2700);
        // Log written recently ⇒ alive ⇒ Healthy, regardless of retry state.
        assert_eq!(
            review_stall_decision(Duration::from_secs(120), t, false),
            WatchdogDecision::Healthy
        );
        assert_eq!(
            review_stall_decision(Duration::from_secs(2699), t, true),
            WatchdogDecision::Healthy
        );
    }

    #[test]
    fn review_stall_decision_silent_first_time_restarts() {
        let t = Duration::from_secs(2700);
        assert_eq!(
            review_stall_decision(Duration::from_secs(2701), t, false),
            WatchdogDecision::Restart
        );
    }

    #[test]
    fn review_stall_decision_silent_after_retry_gives_up() {
        // Bounded: a second stall past the timeout does not restart again.
        let t = Duration::from_secs(2700);
        assert_eq!(
            review_stall_decision(Duration::from_secs(9999), t, true),
            WatchdogDecision::GiveUp
        );
    }

    // --- log_idle filesystem probe ---

    #[test]
    fn log_idle_none_for_missing_some_for_present() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        let (reg, _rec) = fixture_registry(ws);

        // Missing file ⇒ None (cannot assess).
        let missing = ws.join("nope.log");
        assert!(reg.log_idle(&missing).is_none());

        // A freshly written file ⇒ Some, and its idle is tiny.
        let present = ws.join("sweep.log");
        std::fs::write(&present, "hello\n").unwrap();
        let idle = reg
            .log_idle(&present)
            .expect("present file has a readable mtime");
        assert!(idle < Duration::from_secs(60), "a just-written log is not idle: {idle:?}");
    }

    // --- resolve_review_stall_* precedence ---

    #[test]
    #[serial]
    fn resolve_review_stall_enabled_precedence() {
        std::env::remove_var(REVIEW_STALL_ENABLE_ENV);
        // Default ON (self-healing backstop).
        assert!(resolve_review_stall_enabled(&StartupRaceConfig::default()));
        // Config can disable.
        let off = StartupRaceConfig {
            review_stall_enabled: Some(false),
            ..Default::default()
        };
        assert!(!resolve_review_stall_enabled(&off));
        // Env overrides config in both directions.
        std::env::set_var(REVIEW_STALL_ENABLE_ENV, "1");
        assert!(resolve_review_stall_enabled(&off));
        std::env::set_var(REVIEW_STALL_ENABLE_ENV, "0");
        let on = StartupRaceConfig {
            review_stall_enabled: Some(true),
            ..Default::default()
        };
        assert!(!resolve_review_stall_enabled(&on));
        std::env::remove_var(REVIEW_STALL_ENABLE_ENV);
    }

    #[test]
    #[serial]
    fn resolve_review_stall_timeout_precedence() {
        std::env::remove_var(REVIEW_STALL_TIMEOUT_ENV);
        assert_eq!(
            resolve_review_stall_timeout(&StartupRaceConfig::default()),
            Duration::from_secs(DEFAULT_REVIEW_STALL_TIMEOUT_SECS)
        );
        let cfg = StartupRaceConfig {
            review_stall_timeout_secs: Some(1800),
            ..Default::default()
        };
        assert_eq!(resolve_review_stall_timeout(&cfg), Duration::from_secs(1800));
        std::env::set_var(REVIEW_STALL_TIMEOUT_ENV, "600");
        assert_eq!(resolve_review_stall_timeout(&cfg), Duration::from_secs(600));
        // A zero/invalid env value is dropped, falling back to config.
        std::env::set_var(REVIEW_STALL_TIMEOUT_ENV, "0");
        assert_eq!(resolve_review_stall_timeout(&cfg), Duration::from_secs(1800));
        std::env::remove_var(REVIEW_STALL_TIMEOUT_ENV);
    }

    // --- review_stall_watchdog_once: bounded auto-restart end-to-end ---

    #[test]
    fn review_stall_watchdog_ignores_prestartup_sweep() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        let mut reg = hung_child_registry(ws);

        let out = reg
            .dispatch(&SweepKind::Issue(5150), None, None, None, None)
            .unwrap();
        assert!(wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS));

        // No worktree/checkpoint yet ⇒ NOT past startup ⇒ the review-stall
        // watchdog leaves it entirely to the #3887 startup watchdog, even with a
        // zero timeout that would otherwise force a stall.
        assert_eq!(reg.review_stall_watchdog_once(Duration::ZERO), 0);
        assert!(!reg.review_stall_retried.contains(&5150));

        let _ = reg.cancel(&out.sweep_id, Duration::from_secs(2));
    }

    #[test]
    fn review_stall_watchdog_restarts_stalled_sweep_once_then_gives_up() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        let mut reg = hung_child_registry(ws);

        // 1. Dispatch a sweep for issue 5252 and mark it past startup by
        //    creating its worktree (the review-stall watchdog only acts on
        //    sweeps that already made progress).
        let out = reg
            .dispatch(&SweepKind::Issue(5252), None, None, None, None)
            .unwrap();
        assert!(wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS), "fixture child should start");
        let wt = ws.join(".loom").join("worktrees").join("issue-5252");
        std::fs::create_dir_all(&wt).unwrap();

        // 2. With a generous timeout the freshly-written log is NOT idle ⇒ the
        //    sweep is healthy and untouched.
        assert_eq!(
            reg.review_stall_watchdog_once(Duration::from_secs(3600)),
            0,
            "a sweep still emitting log output is not disturbed"
        );

        // 3. A zero timeout forces the stall verdict (any log idle >= 0) ⇒ the
        //    wedged sweep is auto-cancelled and re-dispatched exactly once.
        let restarts = reg.review_stall_watchdog_once(Duration::ZERO);
        assert_eq!(restarts, 1, "the stalled sweep is auto-restarted once");
        assert!(reg.review_stall_retried.contains(&5252), "issue marked retried (bounded)");
        let second_id =
            running_issue_sweep_id(&reg, 5252).expect("a fresh sweep was re-dispatched");

        // 4. The re-dispatched sweep still has a worktree (past startup) and a
        //    fresh log; a zero timeout stalls it again, but the watchdog is
        //    bounded — it gives up instead of restarting a second time.
        let restarts2 = reg.review_stall_watchdog_once(Duration::ZERO);
        assert_eq!(restarts2, 0, "bounded: never a second auto-restart");
        assert!(reg.review_stall_gaveup.contains(&5252), "give-up recorded for the issue");
        assert!(
            running_issue_sweep_id(&reg, 5252).is_some(),
            "the sweep is left running for the operator"
        );

        // Cleanup: cancel the lingering child.
        let _ = reg.cancel(&second_id, Duration::from_secs(2));
    }
}
