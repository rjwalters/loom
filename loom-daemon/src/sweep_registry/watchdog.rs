//! Liveness watchdogs: the hung-sweep watchdog, the midbuild-recovery
//! watchdog, and the review-stall watchdog, plus their shared
//! `StartupRaceConfig` timing knobs.

mod dirty_probe;
mod reset_quarantine;

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

/// Marker prefix for the forge comment `SweepRegistry::post_watchdog_gaveup_comment`
/// posts when the startup watchdog exhausts its single bounded auto-restart
/// (Issue #5302, extending #3887). Mirrors `QUARANTINE_COMMENT_MARKER`'s
/// inline-marker convention so a give-up comment is grep/dedup-detectable
/// the same way an auto-quarantine comment is.
pub const WATCHDOG_GAVEUP_COMMENT_MARKER: &str = "Watchdog gave up on this sweep (loom-daemon)";

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

/// Pure stale-untracked-sweep state machine (Issue #7529). Unlike the other
/// three watchdogs' decisions, this one is a plain bool: there is no bounded
/// "restart once, give up on the second" here — a stale-untracked sweep is
/// reaped exactly once (the reap itself transitions the entry out of
/// `Running`, so it can never match this predicate again), never restarted.
///
/// - `elapsed < min_age` ⇒ `false` (too young to judge — every legitimate
///   sweep phase this repo has observed completes well inside `min_age`, but
///   there is no reason to be impatient about it).
/// - `elapsed >= min_age` AND the log has gone silent past `log_silence_timeout`
///   (or has no readable mtime at all — `log_idle: None`, which degrades to
///   "cannot prove it's alive-and-working" rather than "assume healthy") ⇒
///   `true`.
/// - `elapsed >= min_age` but the log was appended to within
///   `log_silence_timeout` ⇒ `false` — a genuinely alive, still-producing
///   sweep (e.g. a long-running Builder that happened to survive a restart)
///   is never disturbed, mirroring every other watchdog's "any observed
///   progress is Healthy" rule.
#[must_use]
pub fn is_stale_untracked_sweep(
    elapsed: Duration,
    min_age: Duration,
    log_idle: Option<Duration>,
    log_silence_timeout: Duration,
) -> bool {
    if elapsed < min_age {
        return false;
    }
    match log_idle {
        Some(idle) => idle >= log_silence_timeout,
        None => true,
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

// ----------------------------------------------------------------------------
// Stale-untracked-sweep backstop (Issue #7529)
// ----------------------------------------------------------------------------
//
// # Root cause this closes
//
// The three watchdogs above ([`watchdog_once`], [`SweepRegistry::midbuild_watchdog_once`],
// [`review_stall_watchdog_once`]) are the daemon's only liveness enforcement,
// but two of the three ([`watchdog_once`], [`review_stall_watchdog_once`]) are
// gated on `self.children.contains_key(sweep_id)` — "does THIS daemon
// instance hold a retained `Child` handle for it". That gate exists for a
// good reason (only a handle we spawned can be `SIGTERM`ed/`wait()`ed
// directly), but it has a permanent blind spot: an entry this daemon instance
// re-admitted from durable state rather than spawned itself NEVER has a
// handle, by construction — there is no way to resurrect a `tokio::process::
// Child` for a pid this process did not fork. That happens via TWO existing,
// already-correct paths:
//
// - [`SweepRegistry::reconstruct`] (`locks.rs`), on daemon startup, re-admits
//   a `.loom/locks/issue-<N>/owner.json` whose `owner_pid` is still alive as
//   `SweepState::Running` — no handle.
// - [`SweepRegistry::adopt_live_journal_sweeps`] (`locks.rs`, Issue #6262),
//   called from both startup adoption and the work-finder's own tick, adopts
//   a surviving `~/.loom/sweeps.json` (`crate::sweep_journal`) entry whose
//   `owner.json` did NOT survive (missing/corrupt/removed) but whose `pid`
//   is still alive — also `SweepState::Running`, also no handle.
//
// Both paths do EXACTLY what they are supposed to: the sweep is correctly
// tracked, correctly visible in `status`'s `in_flight`, and correctly alive.
// The bug is that from the instant either path admits such an entry, it is
// invisible to BOTH the startup-hang and review-stall watchdogs forever — a
// process that is alive but has produced no output for days sits in
// `SweepState::Running` with zero liveness enforcement, which is exactly the
// incident that prompted this issue (a `claude` sweep idle 5+ days, still
// `loom:building`, with no give-up comment because no watchdog ever
// evaluated it even once).
//
// # The backstop
//
// [`SweepRegistry::stale_sweep_findings`] is a pure, read-only scan of
// `self.entries` for exactly the sweeps the two `children`-gated watchdogs
// can never reach (`!self.children.contains_key(id)`) whose age has passed a
// sanity ceiling well above either watchdog's own timeout AND whose log has
// gone silent past the same signal [`review_stall_decision`] already uses —
// so a genuinely-alive, still-producing-output sweep that merely survived a
// restart is never disturbed, exactly like every other watchdog here.
// [`SweepRegistry::stale_sweep_watchdog_once`] acts on those findings
// (cancel + restore `loom:issue`, bounded — the cancel itself transitions the
// entry out of `Running`, so it can never re-match next tick). Because
// `stale_sweep_findings` is computed fresh from `self.entries` on every call
// — never from a tick-populated cache — it is also exactly what
// `build_daemon_status` calls on every `status`/`health` IPC round-trip
// (`ipc.rs`), so a fleet operator sees the finding even on a host where the
// watchdog tick task itself never ran a single iteration (the property the
// issue's own test asks for).
//
// Deliberately excludes the three existing watchdogs' own candidates
// (`self.children.contains_key(id)` sweeps are left untouched here) — the
// bounded single-retry behavior of those three is unaffected, and nothing is
// ever double-reaped.

/// Env var toggling the stale-untracked-sweep backstop (Issue #7529). `0`/
/// `false`/`no`/`off` disables; `1`/`true`/`yes`/`on` forces on. Overrides
/// config.
pub const STALE_SWEEP_ENABLE_ENV: &str = "LOOM_SWEEP_STALE_SWEEP";

/// Env var overriding the stale-sweep age sanity ceiling, in seconds.
pub const STALE_SWEEP_AGE_ENV: &str = "LOOM_SWEEP_STALE_AGE_SECS";

/// Default stale-sweep age sanity ceiling (Issue #7529): four times
/// [`DEFAULT_REVIEW_STALL_TIMEOUT_SECS`] (3 hours). Chosen well above every
/// legitimate sweep-phase duration this repo has observed (a healthy Judge
/// completes in minutes; even the #3910 pathological review-phase hang the
/// review-stall watchdog exists for tops out in the 45-minute window that
/// timeout already covers for a *tracked* sweep) while staying small enough
/// that a multi-day hang like the one that prompted this issue is caught in
/// hours, not days.
pub const DEFAULT_STALE_SWEEP_AGE_SECS: u64 = 4 * DEFAULT_REVIEW_STALL_TIMEOUT_SECS;

/// Grace period the stale-sweep backstop gives a hung child to exit after
/// SIGTERM before escalating to SIGKILL — mirrors [`WATCHDOG_CANCEL_GRACE`].
pub(crate) const STALE_SWEEP_CANCEL_GRACE: Duration = Duration::from_secs(3);

/// Marker prefix for the forge comment [`SweepRegistry::post_stale_sweep_comment`]
/// posts when the stale-sweep backstop reaps an untracked sweep — mirrors
/// [`WATCHDOG_GAVEUP_COMMENT_MARKER`]'s grep/dedup-detectable convention.
pub const STALE_SWEEP_COMMENT_MARKER: &str = "Stale-sweep backstop reaped this sweep (loom-daemon)";

/// One finding from [`SweepRegistry::stale_sweep_findings`] (Issue #7529): an
/// in-flight sweep whose age has crossed the sanity ceiling and whose log has
/// gone silent, that neither the startup-hang nor the review-stall watchdog
/// can act on because this daemon instance holds no retained `Child` handle
/// for it (a restart-survived, re-admitted entry — see the module doc above).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleSweepFinding {
    /// The issue this sweep is working.
    pub issue: u32,
    /// The sweep's own id (for log correlation).
    pub sweep_id: SweepId,
    /// PID of the (confirmed alive) sweep process.
    pub pid: u32,
    /// How long the sweep has been `Running`/`Pending`.
    pub elapsed: Duration,
    /// How long the sweep's log file has gone un-appended, when readable.
    pub log_idle: Option<Duration>,
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
    /// `autonomous.watchdog.staleSweep` (Issue #7529) — whether to run the
    /// stale-untracked-sweep backstop.
    pub stale_sweep_enabled: Option<bool>,
    /// `autonomous.watchdog.staleSweepAgeSecs` (Issue #7529) — the age sanity
    /// ceiling for the stale-untracked-sweep backstop, in seconds
    /// (zero/invalid dropped to `None`).
    pub stale_sweep_age_secs: Option<u64>,
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
        stale_sweep_enabled: watchdog
            .and_then(|w| w.get("staleSweep"))
            .and_then(serde_json::Value::as_bool),
        stale_sweep_age_secs: watchdog
            .and_then(|w| w.get("staleSweepAgeSecs"))
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

/// Resolve whether the stale-untracked-sweep backstop runs, precedence **env >
/// config > default(true)** (Issue #7529). Defaults **on** — bounded (only
/// ever acts on entries the other two watchdogs cannot reach at all) and
/// generous (a multi-hour age ceiling plus the same log-silence signal
/// [`review_stall_decision`] uses) — but can be disabled via
/// `LOOM_SWEEP_STALE_SWEEP=0` or `autonomous.watchdog.staleSweep = false`.
#[must_use]
pub fn resolve_stale_sweep_enabled(config: &StartupRaceConfig) -> bool {
    if let Ok(v) = std::env::var(STALE_SWEEP_ENABLE_ENV) {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config.stale_sweep_enabled.unwrap_or(true)
}

/// Resolve the stale-sweep age sanity ceiling, precedence **env > config >
/// default** (Issue #7529).
#[must_use]
pub fn resolve_stale_sweep_age(config: &StartupRaceConfig) -> Duration {
    let secs = std::env::var(STALE_SWEEP_AGE_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(config.stale_sweep_age_secs)
        .unwrap_or(DEFAULT_STALE_SWEEP_AGE_SECS);
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
    stale_sweep_params: Option<(Duration, Duration)>,
) -> tokio::task::JoinHandle<()> {
    log::info!(
        "sweep_registry: starting startup watchdog (interval={}s, timeout={}s) (#3887); \
         review-stall watchdog {} (#3910); stale-untracked-sweep backstop {} (#7529)",
        interval.as_secs(),
        timeout.as_secs(),
        review_stall_timeout
            .map(|t| format!("enabled (timeout={}s)", t.as_secs()))
            .unwrap_or_else(|| "disabled".to_string()),
        stale_sweep_params
            .map(|(age, silence)| format!(
                "enabled (min_age={}s, log_silence={}s)",
                age.as_secs(),
                silence.as_secs()
            ))
            .unwrap_or_else(|| "disabled".to_string())
    );
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // First tick fires immediately; skip it so we don't act at boot before
        // any sweep has had a chance to start.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            // Same tick runs all four liveness backstops: the startup-hang
            // watchdog (#3887, no progress), the mid-build-death watchdog
            // (#3895, made progress then the child died), the review-phase
            // stall watchdog (#3910, alive but log-silent mid-review), and the
            // stale-untracked-sweep backstop (#7529, alive+silent but
            // unreachable by either of the two `children`-gated watchdogs
            // above because this daemon instance holds no retained handle for
            // it). All hold the registry lock only briefly, never across the
            // sleep.
            let (restarted, recovered, unstalled, reaped_stale) = {
                match registry.lock() {
                    Ok(mut r) => {
                        let restarted = r.watchdog_once(timeout);
                        let recovered = r.midbuild_watchdog_once();
                        let unstalled =
                            review_stall_timeout.map_or(0, |t| r.review_stall_watchdog_once(t));
                        let reaped_stale = stale_sweep_params
                            .map_or(0, |(age, silence)| r.stale_sweep_watchdog_once(age, silence));
                        (restarted, recovered, unstalled, reaped_stale)
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
            if reaped_stale > 0 {
                log::warn!(
                    "sweep_registry: watchdog reaped {reaped_stale} stale untracked sweep{} \
                     (#7529)",
                    if reaped_stale == 1 { "" } else { "s" }
                );
            }
        }
    })
}

/// Outcome of [`SweepRegistry::freshest_lease_owner`] (Issue #7612) — the
/// forge-scoped ownership probe the mid-build-death watchdog consults
/// immediately before its destructive `git reset --hard` + `git clean -fd`,
/// on top of the purely-local #4449/#4556/#4564 vetoes. See
/// [`SweepRegistry::midbuild_lease_veto`] for the decision this feeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LeaseOwnerProbeResult {
    /// A lease-record comment was found; this is the one with the most
    /// recent forge-assigned `updated_at` — the currently-renewed claim.
    Found {
        host: String,
        sweep_id: String,
        updated_at: DateTime<Utc>,
    },
    /// The read succeeded and found no lease-record comment at all on this
    /// issue — no evidence of a newer owner either way (a claim predating
    /// the lease-record feature, or a host that never publishes one).
    NotFound,
    /// The query itself could not be completed (unresolved repo, `gh`
    /// timeout, non-zero exit), or every found comment's `updated_at` failed
    /// to parse. Ambiguous by construction — never treated as equivalent to
    /// [`Self::NotFound`].
    ReadFailed,
}

/// The pure, no-`&self` half of [`SweepRegistry::log_idle`] — the method
/// never actually touched `self`, so this is the same body under a
/// standalone name usable from [`scan_stale_sweep_findings`] below (and from
/// [`crate::sweep_registry::RegistrySnapshot::stale_sweep_findings`], Issue
/// #7526) without a `&SweepRegistry` receiver.
fn log_idle_pure(log_path: &Path) -> Option<Duration> {
    let modified = std::fs::metadata(log_path).ok()?.modified().ok()?;
    modified.elapsed().ok()
}

/// The pure per-entry scan half of [`SweepRegistry::stale_sweep_findings`]
/// (Issue #7526): takes an entries iterator + an "is this daemon's own
/// child" predicate instead of `&SweepRegistry` directly, so it can run
/// without holding the registry's mutex — see
/// [`crate::sweep_registry::RegistrySnapshot::stale_sweep_findings`], which
/// calls this on a cloned snapshot outside the lock, and the original
/// instance method below, which still calls it inline (under the lock, as
/// before) for every other caller.
#[must_use]
pub(crate) fn scan_stale_sweep_findings<'a>(
    entries: impl Iterator<Item = (&'a SweepId, &'a SweepInfo)>,
    is_own_child: &dyn Fn(&SweepId) -> bool,
    min_age: Duration,
    log_silence_timeout: Duration,
) -> Vec<StaleSweepFinding> {
    let now = Utc::now();
    entries
        .filter(|(id, info)| {
            matches!(info.state, SweepState::Running | SweepState::Pending)
                && matches!(info.kind, SweepKind::Issue(_))
                // The complement of the other two watchdogs' eligibility
                // gate: ONLY sweeps this daemon instance did not itself
                // spawn are candidates here, so nothing is ever
                // double-covered (or double-reaped) between this backstop
                // and `watchdog_once`/`review_stall_watchdog_once`.
                && !is_own_child(id)
        })
        .filter_map(|(id, info)| {
            let SweepKind::Issue(issue) = info.kind else {
                return None;
            };
            if !is_pid_alive(info.pid) {
                // Already dead: the ordinary reaper's dead-pid path
                // already handles this on its own next tick — not this
                // backstop's job, and a dead pid was never "held
                // indefinitely" by anything.
                return None;
            }
            let elapsed = (now - info.started_at).to_std().unwrap_or(Duration::ZERO);
            let log_idle = log_idle_pure(&info.log_path);
            if !is_stale_untracked_sweep(elapsed, min_age, log_idle, log_silence_timeout) {
                return None;
            }
            Some(StaleSweepFinding {
                issue,
                sweep_id: id.clone(),
                pid: info.pid,
                elapsed,
                log_idle,
            })
        })
        .collect()
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
                    // Bounded: already retried once. Log + surface once per issue
                    // (Issue #5302: previously the daemon log was the *only*
                    // surface — invisible to an operator not tailing it).
                    if self.watchdog_gaveup.insert(issue) {
                        log::error!(
                            "watchdog: sweep for issue #{issue} ({sweep_id}) is still stuck \
                             {}s after an auto-restart — giving up (bounded to one retry). \
                             Operator intervention needed (cancel + re-dispatch, or investigate \
                             the MCP-init hang).",
                            elapsed.as_secs()
                        );
                        self.post_watchdog_gaveup_comment(issue, elapsed);
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

    /// Best-effort forge comment when the startup watchdog gives up (Issue
    /// #5302): the bounded auto-restart (#3887) already retried this issue's
    /// sweep exactly once and it is still stuck, so nothing further will
    /// happen automatically until an operator acts. Before this, "giving up"
    /// meant only a `log::error!` line — invisible unless someone was
    /// tailing the daemon log at the moment it fired. Mirrors
    /// [`SweepRegistry::apply_quarantine_label`]'s "post a comment so the
    /// pause is visible to a human on the forge, not just in the daemon
    /// log" pattern.
    ///
    /// Called from exactly one call site, already inside the
    /// `self.watchdog_gaveup.insert(issue)` dedup guard [`watchdog_once`]
    /// uses to log the give-up once — so this posts at most one comment per
    /// issue no matter how many subsequent ticks re-observe the same
    /// give-up (the same dedup fixture the existing
    /// `watchdog_restarts_hung_sweep_once_then_gives_up` test already
    /// exercises for the log line).
    ///
    /// Skipped when label flips are disabled (test fixtures /
    /// `skip_label_flip`), matching every other best-effort forge mutation
    /// in this registry. Every step is best-effort: a `gh` failure only logs
    /// (at `warn`, not `debug` — a silently-dropped give-up notice defeats
    /// the whole point of this method), it never affects the load-bearing
    /// `watchdog_gaveup` dedup state, which the caller already committed
    /// before this runs.
    fn post_watchdog_gaveup_comment(&self, issue: u32, elapsed: Duration) {
        if self.config.skip_label_flip {
            return;
        }
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        let body = format!(
            "{marker}: this issue's sweep made no progress for {elapsed_secs}s, exhausted its \
             one bounded auto-restart (#3887), and is still stuck — the daemon has stopped \
             retrying and left it running for operator triage rather than looping forever. \
             Investigate the sweep log, then either cancel the stuck sweep and re-dispatch the \
             issue, or investigate the underlying hang (e.g. an MCP-init race, #3887). This \
             issue remains held at `loom:building` until you act.",
            marker = WATCHDOG_GAVEUP_COMMENT_MARKER,
            elapsed_secs = elapsed.as_secs(),
        );
        let mut comment = Command::new(&gh);
        comment
            .arg("issue")
            .arg("comment")
            .arg(issue.to_string())
            .arg("--body")
            .arg(body);
        comment.current_dir(&self.config.workspace_root);
        // #5401: cross-owner managed repo -> its own owner's installation-token
        // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
        crate::credential_preflight::apply_gh_config_for_root(
            &mut comment,
            &self.config.workspace_root,
        );
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            comment.arg("--repo").arg(repo);
        }
        let timeout = reap_gh_timeout();
        match output_with_timeout(comment, timeout) {
            Ok(Some(output)) if output.status.success() => {}
            Ok(Some(output)) => log::warn!(
                "watchdog: give-up comment for #{issue} exited {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            Ok(None) => log::warn!(
                "watchdog: give-up comment for #{issue} exceeded {}s, killed (#3973)",
                timeout.as_secs()
            ),
            Err(e) => log::warn!("watchdog: give-up comment for #{issue} failed: {e}"),
        }
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
    /// unprobeable worktree as recoverable. See
    /// [`dirty_probe::worktree_is_dirty`] for why the failure arm is logged.
    pub(crate) fn worktree_dirty(&self, issue: u32) -> bool {
        dirty_probe::worktree_is_dirty(&self.worktree_path(issue), issue)
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
        // 5+6. The two REGISTRY-INDEPENDENT signals (#8413): a live
        //    `loom-daemon inflight` claim on this tree, and a filesystem write
        //    inside the activity window. Between them they cover the in-session
        //    builder — no claim-lock, no marker, one-shot subshells, output
        //    going only into `target/` — that signals 1-4 cannot see at all.
        //    See `worktree_activity`'s "Destructive passes" section.
        //
        //    The window is `self.activity_window` when a caller pinned one
        //    explicitly (tests only — see the field's doc comment, #8487),
        //    else resolved from the environment exactly as before.
        evidence.extend(match self.activity_window {
            Some(window) => crate::worktree_activity::live_use_evidence_in(&wt, window),
            None => crate::worktree_activity::live_use_evidence(&wt),
        });
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
        // #8413: log-then-destroy is a forensic trace, not a recovery path.
        // Push the same content onto a `loom-quarantine:` stash first so a
        // wrong verdict is undoable (`git stash apply <sha>`, sha logged).
        reset_quarantine::quarantine_before_reset(&wt, issue);
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
        if crate::worker_spawn::uses_native_sweep(&self.config.workspace_root) {
            return true;
        }
        let ranking = self.config.workspace_root.join(".loom/tokens/.ranking");
        match std::fs::read_to_string(&ranking) {
            Ok(contents) => ranking_has_capacity(&contents),
            Err(_) => true,
        }
    }

    // ------------------------------------------------------------------------
    // Forge-lease ownership fence (Issue #7612)
    // ------------------------------------------------------------------------
    //
    // The #4449/#4556/#4564 vetoes above are purely LOCAL: `worktree_in_use`
    // reads this host's filesystem/process table, and `live_claim_evidence`
    // reads this host's own journal/run-registry. Both are blind to a newer
    // owner that neither wrote a local claim-lock nor left a locally-visible
    // process — exactly the #7612 incident shape: an in-session `/loom:sweep`
    // redispatch (which never touches `.loom/locks/issue-<N>/`, whether it
    // runs on this host or a different one) published a fresh forge lease and
    // started editing the shared worktree BEFORE this daemon's own delayed
    // watchdog tick fired for the earlier, now-dead dispatch. The lease
    // record (`defaults/docs/lease-record.md`) is the one ownership channel
    // both the daemon's `dispatch` and the in-session sweep publish to, so it
    // is the only signal that can close this gap.

    /// Fetch the freshest (highest-`updated_at`) lease-record comment on
    /// `issue`, across every comment ever posted for it (Issue #7612).
    ///
    /// Unlike [`resolve_lease_order`](Self::resolve_lease_order)'s
    /// claim-episode lookback window, no time window is applied here — none
    /// is needed: a completed sweep's lease stops being renewed the moment it
    /// exits, so its `updated_at` freezes and ages out of freshness on its
    /// own. The caller ([`midbuild_lease_veto`](Self::midbuild_lease_veto))
    /// applies [`crate::claim_reconciliation::lease_is_fresh`] to the result
    /// before treating it as a live competing claim.
    pub(crate) fn freshest_lease_owner(&self, issue: u32) -> LeaseOwnerProbeResult {
        let Some(comments) = self.read_lease_comments(issue) else {
            return LeaseOwnerProbeResult::ReadFailed;
        };
        if comments.is_empty() {
            return LeaseOwnerProbeResult::NotFound;
        }
        let freshest = comments
            .into_iter()
            .filter_map(|c| c.updated_at.map(|ts| (ts, c.host, c.sweep_id)))
            .max_by_key(|(ts, _, _)| *ts);
        match freshest {
            Some((updated_at, host, sweep_id)) => LeaseOwnerProbeResult::Found {
                host,
                sweep_id,
                updated_at,
            },
            // Comments existed but none carried a parseable `updated_at` —
            // an unexpected shape from the forge, not a verified absence.
            None => LeaseOwnerProbeResult::ReadFailed,
        }
    }

    /// Whether the mid-build-death watchdog must refuse to touch `issue`'s
    /// worktree because the forge's lease record names a CURRENT, fresh
    /// owner other than `dead_sweep_id` (Issue #7612) — the fence that closes
    /// the gap the purely-local vetoes above cannot see (see the module doc
    /// above this section). Returns `Some(reason)` (log and abort) or `None`
    /// (proceed with the existing #3895/#4449/#4556/#4564 recovery).
    ///
    /// # Fail-CLOSED, deliberately inverting this file's usual convention
    ///
    /// Every other forge probe in `sweep_registry` (collision detection,
    /// `resolve_lease_order`, the open-PR probe) fails OPEN on an
    /// unverifiable read, because the worst case of proceeding wrongly there
    /// is a redundant dispatch or a duplicated builder — annoying, never
    /// destructive. This probe gates an IRREVERSIBLE `git reset --hard` +
    /// `git clean -fd`; the worst case of proceeding wrongly here is
    /// unrecoverable data loss — the exact #7612 incident. So
    /// [`LeaseOwnerProbeResult::ReadFailed`] refuses the cleanup here, not
    /// the reverse.
    ///
    /// [`LeaseOwnerProbeResult::NotFound`] (a verified read that found zero
    /// lease comments) is NOT ambiguous — it is a genuine absence of
    /// evidence, so it proceeds. Most claims never publish a lease at all
    /// (predating the feature, or a repo that has never had a
    /// cross-host/cross-session race), and refusing recovery for all of them
    /// would silently defang #3895 for the common case, violating "preserve
    /// existing behavior when there is genuinely no newer owner."
    ///
    /// A found lease naming `dead_sweep_id` itself is also not a newer
    /// owner — that is just the dying dispatch's own (now presumably stale)
    /// lease record — so that case proceeds too.
    ///
    /// Skipped entirely (returns `None` immediately) when
    /// `self.config.skip_label_flip` is set, the same "no real forge in this
    /// process" convention [`write_lease_comment`](Self::write_lease_comment)
    /// and the `resolve_lease_order` call site already use — so test
    /// fixtures that never publish/read lease comments keep exercising the
    /// pre-#7612 recovery path byte-for-byte unchanged.
    pub(crate) fn midbuild_lease_veto(&self, issue: u32, dead_sweep_id: &str) -> Option<String> {
        if self.config.skip_label_flip {
            return None;
        }
        match self.freshest_lease_owner(issue) {
            LeaseOwnerProbeResult::NotFound => None,
            LeaseOwnerProbeResult::ReadFailed => Some(format!(
                "the freshest lease record for issue #{issue} could not be read (unresolved \
                 repo, `gh` timeout/failure, or an unparseable comment) — an ambiguous \
                 ownership read is never treated as safe to destroy (#7612)"
            )),
            LeaseOwnerProbeResult::Found {
                host,
                sweep_id,
                updated_at,
            } => {
                if sweep_id == dead_sweep_id {
                    return None;
                }
                let ttl_minutes = crate::claim_reconciliation::resolve_lease_ttl_minutes();
                if crate::claim_reconciliation::lease_is_fresh(updated_at, Utc::now(), ttl_minutes)
                {
                    let age_minutes = (Utc::now() - updated_at).num_seconds() as f64 / 60.0;
                    Some(format!(
                        "issue #{issue}'s freshest lease record names sweep `{sweep_id}` on \
                         host `{host}` (renewed {age_minutes:.1}m ago, within the \
                         {ttl_minutes:.1}m TTL) — a DIFFERENT, still-live owner than the dying \
                         dispatch `{dead_sweep_id}` (#7612)"
                    ))
                } else {
                    None
                }
            }
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
    /// **Forge-lease ownership fence (Issue #7612)**: run FIRST in the
    /// `Recover` arm, ahead of even the token-health gate — the #4449/#4556/
    /// #4564 vetoes above are purely local (this host's filesystem, process
    /// table, and journal), so a newer owner that never wrote a local
    /// claim-lock (a same-host in-session `/loom:sweep` redispatch, or a
    /// different-host owner entirely) sails past all three undetected.
    /// [`midbuild_lease_veto`] re-reads the issue's freshest forge lease
    /// record immediately before any mutation and refuses — without
    /// consuming the single recovery retry — when it names a different,
    /// still-fresh sweep, OR when the read itself is unverifiable (this one
    /// check fails CLOSED, the opposite of every other forge probe in this
    /// module, because the operation it gates is irreversible).
    ///
    /// [`midbuild_lease_veto`]: SweepRegistry::midbuild_lease_veto
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
                    // #7612: revalidate CURRENT forge-lease ownership before
                    // anything else in this arm — ahead of even the
                    // token-health gate below. This is the fence the
                    // purely-local #4449/#4556/#4564 vetoes above cannot
                    // provide: they only see a holder THIS host's
                    // filesystem/process table can observe, so a same-host
                    // sweep dispatched outside this daemon (the in-session
                    // `/loom:sweep` path never writes `.loom/locks/`), or a
                    // different-host owner entirely, sails straight past
                    // them. A positive match here — or an unverifiable read
                    // — refuses the cleanup WITHOUT consuming the single
                    // recovery retry, exactly like the `InUse`/live-claim
                    // refusals: the worktree may be legitimately free (or
                    // verifiable again) on a later tick.
                    if let Some(reason) = self.midbuild_lease_veto(issue, &sweep_id) {
                        if self.midbuild_lease_superseded.insert(issue) {
                            log::warn!(
                                "midbuild-watchdog: issue #{issue} ({sweep_id}) matches the \
                                 mid-build-death signature, but {reason}. REFUSING to `git reset \
                                 --hard` its worktree, take its claim lock, or re-dispatch: doing \
                                 so is exactly how the #7612 incident destroyed a newer owner's \
                                 dirty, uncommitted work. The single recovery retry is NOT \
                                 consumed; the watchdog re-assesses on the next tick."
                            );
                        }
                        continue;
                    }
                    // Any prior lease-supersession refusal is now stale;
                    // clear it so a later refusal still surfaces.
                    self.midbuild_lease_superseded.remove(&issue);

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
    /// # Open-linked-PR handoff (Issue #7649)
    ///
    /// A cancelled sweep that already produced a PR is not fresh Issue work,
    /// so its Issue-keyed recovery re-dispatch correctly hits the #4123
    /// open-PR guard ([`OpenPrDispatchError`]). Rather than let that refusal
    /// consume the bounded retry and strand the PR (the kicad-tools
    /// #5366/#5371 incident this issue documents), the recovery converts to
    /// the established `SweepKind::PrSet` Mode C surface (#5342/#6593) for
    /// the exact PR the guard confirmed — Judge/Doctor -> Merge processing
    /// picks it up with no Curator/Builder re-run and no worktree touch. The
    /// conversion only fires on a downcast-matched, VERIFIED
    /// `OpenPrDispatchError` (never a string-matched guess), so an
    /// ambiguous/failed probe or any other refusal reason is unaffected.
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
                            // Issue #7649: the #4123 open-PR guard (dispatch.rs
                            // step 2.6) correctly refuses this Issue-keyed
                            // re-dispatch when issue #{issue} still has a
                            // CONFIRMED open linked PR — that PR is this very
                            // sweep's own already-built output (the review
                            // phase that stalled was reviewing/repairing it),
                            // not fresh Issue work. Before this fix the retry
                            // was simply consumed here and the PR was
                            // stranded (the kicad-tools #5366/#5371 incident
                            // this issue documents). Route it to the
                            // established Mode C surface instead —
                            // `SweepKind::PrSet` (#5342), exactly the
                            // alternative `OpenPrDispatchError`'s own
                            // `Display` names (#6593) — so Judge/Doctor ->
                            // Merge picks the PR up directly: no
                            // Curator/Builder re-run, no worktree touch, and
                            // the normal PR claim-lock dedup + capacity
                            // admission + operator/review-hold handling that
                            // every other PrSet dispatch already gets.
                            //
                            // Only a VERIFIED `OpenPrDispatchError` (produced
                            // solely from a confirmed `OpenPrProbe::Open`,
                            // never from an ambiguous/failed probe — see
                            // dispatch.rs step 2.6's fail-open comment)
                            // triggers this conversion; a downcast match
                            // rather than string-parsing keeps that
                            // guarantee intact. Every other refusal (park
                            // label, backoff, workspace-commands, a spawn
                            // failure, …) falls through to the original
                            // error log, unchanged.
                            if let Some(open_pr) = e.downcast_ref::<OpenPrDispatchError>() {
                                let pr = open_pr.pr;
                                log::warn!(
                                    "review-stall-watchdog: issue #{issue}'s Issue-keyed \
                                     recovery re-dispatch was refused because it already has a \
                                     confirmed open linked PR #{pr} — converting the recovery \
                                     to kind=PrSet([{pr}]) (Mode C, #5342/#6593) so the already- \
                                     built PR reaches Judge/Doctor -> Merge instead of being \
                                     stranded (#7649)."
                                );
                                match self.dispatch(
                                    &SweepKind::PrSet(vec![pr]),
                                    // Deliberately NOT the captured Issue-kind
                                    // `idempotency_key`: `PrSet` is a distinct
                                    // idempotency/claim domain (per-PR locks,
                                    // not the per-issue claim lock), so reusing
                                    // the Issue key here would dedup against
                                    // the wrong namespace.
                                    None,
                                    model.as_deref(),
                                    effort.as_deref(),
                                    // `depends_on` (stacked-PR chaining) is
                                    // Issue-only and silently ignored by PrSet
                                    // dispatch regardless; pass None for
                                    // clarity at the call site.
                                    None,
                                ) {
                                    Ok(outcome) => {
                                        restarts += 1;
                                        log::warn!(
                                            "review-stall-watchdog: recovered issue #{issue}'s \
                                             stalled review onto PR #{pr} as {} (pid {}) via a \
                                             PrSet conversion (#7649).",
                                            outcome.sweep_id,
                                            outcome.pid
                                        );
                                    }
                                    Err(pr_err) => {
                                        // Expected, not a bug: the PR is
                                        // already claimed by a live peer PR
                                        // sweep (per-PR lock collision), or a
                                        // capacity/park/runtime-admission
                                        // guard on the PrSet path refused —
                                        // either way this is a deliberate
                                        // skip, not a lost recovery: the
                                        // issue's claim was already restored
                                        // by `cancel()` above, and the #4123
                                        // guard keeps refusing ordinary
                                        // Issue-keyed re-dispatch of it for as
                                        // long as PR #{pr} stays open, so the
                                        // periodic Judge/Champion roles (or
                                        // whichever sweep already owns the PR
                                        // lock) remain the PR's path forward.
                                        log::error!(
                                            "review-stall-watchdog: PR-set recovery dispatch for \
                                             issue #{issue}'s open PR #{pr} failed: {pr_err} \
                                             (issue left recoverable — its claim was already \
                                             restored; a live peer PR sweep or a hold on the PR \
                                             may already own it)."
                                        );
                                    }
                                }
                            } else {
                                log::error!(
                                    "review-stall-watchdog: re-dispatch of issue #{issue} after a \
                                     stall failed: {e} (issue left recoverable — its claim was \
                                     already restored)."
                                );
                            }
                        }
                    }
                }
            }
        }
        restarts
    }

    // ------------------------------------------------------------------------
    // Stale-untracked-sweep backstop (Issue #7529)
    // ------------------------------------------------------------------------

    /// Scan `self.entries` for in-flight Issue sweeps neither
    /// [`watchdog_once`](Self::watchdog_once) nor
    /// [`review_stall_watchdog_once`](Self::review_stall_watchdog_once) can
    /// ever reach — no retained `Child` handle
    /// (`!self.children.contains_key`), the signature of an entry this daemon
    /// instance re-admitted from durable state ([`reconstruct`](Self::reconstruct)
    /// or [`adopt_live_journal_sweeps`](Self::adopt_live_journal_sweeps))
    /// rather than spawned itself — whose age and log silence both cross the
    /// sanity thresholds (Issue #7529, see the module doc above).
    ///
    /// Pure and read-only: never mutates state, never signals a process,
    /// never touches the forge. Computed fresh from `self.entries` on every
    /// call, so — unlike the tick-driven watchdogs above — a caller (e.g.
    /// `build_daemon_status`, per `status`/`health` IPC round-trip) gets an
    /// up-to-date answer regardless of whether the watchdog tick task has
    /// ever run a single iteration.
    #[must_use]
    pub fn stale_sweep_findings(
        &self,
        min_age: Duration,
        log_silence_timeout: Duration,
    ) -> Vec<StaleSweepFinding> {
        scan_stale_sweep_findings(
            self.entries.iter(),
            &|id| self.children.contains_key(id),
            min_age,
            log_silence_timeout,
        )
    }

    /// Run one stale-untracked-sweep backstop tick (Issue #7529): reap every
    /// [`stale_sweep_findings`](Self::stale_sweep_findings) result — cancel
    /// the process (releasing its claim lock and restoring `loom:building` ->
    /// `loom:issue`, exactly like every other watchdog's cancel path) and post
    /// a forge comment explaining why. Unlike the other three watchdogs there
    /// is no bounded retry/give-up pair: the cancel itself transitions the
    /// entry out of `Running`/`Pending`, so it can never match
    /// `stale_sweep_findings` again — a single reap per issue is inherent to
    /// the state machine, not a separate latch.
    ///
    /// Returns the number of sweeps reaped this tick.
    pub fn stale_sweep_watchdog_once(
        &mut self,
        min_age: Duration,
        log_silence_timeout: Duration,
    ) -> usize {
        let findings = self.stale_sweep_findings(min_age, log_silence_timeout);
        let mut reaped = 0usize;
        for finding in findings {
            let log_idle_desc = finding
                .log_idle
                .map_or_else(|| "unreadable/missing".to_string(), |d| format!("{}s", d.as_secs()));
            log::warn!(
                "stale-sweep-watchdog: issue #{} ({}, pid {}) has been alive {}s with log idle \
                 {} and no retained process handle — this daemon instance never spawned it (a \
                 `reconstruct()`/journal-adopted survivor of a prior restart, #7529) so neither \
                 the startup-hang (#3887) nor the review-stall (#3910) watchdog could ever reach \
                 it. Reap reason: age + log silence both exceed the sanity ceiling with zero \
                 watchdog coverage. Cancelling and restoring `loom:issue`.",
                finding.issue,
                finding.sweep_id,
                finding.pid,
                finding.elapsed.as_secs(),
                log_idle_desc,
            );
            match self.cancel(&finding.sweep_id, STALE_SWEEP_CANCEL_GRACE) {
                Ok(_) => {
                    reaped += 1;
                    self.post_stale_sweep_comment(&finding);
                }
                Err(e) => {
                    log::error!(
                        "stale-sweep-watchdog: cancel of issue #{} ({}) failed: {e}",
                        finding.issue,
                        finding.sweep_id,
                    );
                }
            }
        }
        reaped
    }

    /// Best-effort forge comment when the stale-sweep backstop reaps an
    /// untracked sweep (Issue #7529) — mirrors
    /// [`post_watchdog_gaveup_comment`](Self::post_watchdog_gaveup_comment)'s
    /// "make the daemon-log-only fact visible on the forge" pattern, except
    /// this backstop already acted (restored `loom:issue`) rather than
    /// leaving the issue held.
    fn post_stale_sweep_comment(&self, finding: &StaleSweepFinding) {
        if self.config.skip_label_flip {
            return;
        }
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        let body = format!(
            "{marker}: this issue's sweep (pid {pid}) had been running {elapsed_secs}s with its \
             log silent and no daemon-retained process handle for it — a sweep that survived a \
             prior daemon restart, which strips it of coverage from both the startup-hang \
             (#3887) and review-stall (#3910) watchdogs (#7529). The daemon has cancelled the \
             process and restored `loom:issue` so this issue can be re-dispatched.",
            marker = STALE_SWEEP_COMMENT_MARKER,
            pid = finding.pid,
            elapsed_secs = finding.elapsed.as_secs(),
        );
        let mut comment = Command::new(&gh);
        comment
            .arg("issue")
            .arg("comment")
            .arg(finding.issue.to_string())
            .arg("--body")
            .arg(body);
        comment.current_dir(&self.config.workspace_root);
        crate::credential_preflight::apply_gh_config_for_root(
            &mut comment,
            &self.config.workspace_root,
        );
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            comment.arg("--repo").arg(repo);
        }
        let timeout = reap_gh_timeout();
        match output_with_timeout(comment, timeout) {
            Ok(Some(output)) if output.status.success() => {}
            Ok(Some(output)) => log::warn!(
                "stale-sweep-watchdog: reap comment for #{} exited {:?}: {}",
                finding.issue,
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            Ok(None) => log::warn!(
                "stale-sweep-watchdog: reap comment for #{} exceeded {}s, killed (#3973)",
                finding.issue,
                timeout.as_secs()
            ),
            Err(e) => {
                log::warn!("stale-sweep-watchdog: reap comment for #{} failed: {e}", finding.issue)
            }
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    unused_imports
)]
mod tests;

// #8413's own cases, in a sibling file because `tests.rs` is at its file-size
// ratchet baseline (`.loom/docs/file-size-policy.md`).
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    unused_imports
)]
mod liveness_tests;
