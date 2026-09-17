//! Reaping a dispatched sweep's process/checkpoint state: `reap_once`,
//! cancellation, and resuming a crashed sweep from its checkpoint.

use super::*;

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

/// Retention window after a sweep terminates before it is garbage-collected
/// from the in-memory map. One hour matches the operator intuition that
/// "recently exited sweeps should still show up in `list_sweeps`".
pub const TERMINAL_RETENTION_SECS: i64 = 3600;

/// Issue #4256 (Judge residual-risk backstop): the maximum number of
/// consecutive reaper-driven resume dispatches for a single issue before the
/// reaper stops resuming and leaves the PR for the periodic Judge role /
/// operator.
///
/// The #4123 open-PR guard used to backstop infinite re-dispatch once a PR
/// existed, but the resume path (`dispatch_resume_after_crash`) deliberately
/// bypasses it. A sweep that reliably dies in the **~2s..stall window** — too
/// slow for the sub-`insta_crash_secs` quarantine tally (#3939), too fast to
/// ever rewrite the checkpoint or reach Judge — would otherwise reset every
/// backstop each tick and resume forever. This small constant caps the
/// consecutive *checkpoint-less* resume attempts per issue: any resume run
/// that actually advances the checkpoint (real progress) resets the tally (see
/// [`SweepRegistry::reap_once`]'s `checkpoint_written_by_run` branch), so only
/// a genuine crash→resume→crash loop accrues toward the cap. On exhaustion the
/// reaper stops resuming, emits a failure-visible `SweepResumeDispatched`
/// (`dispatched: false`) event, and adds NO labels — the PR is picked up by the
/// periodic Judge role (repo-config backstop (c)) or an operator.
pub(crate) const MAX_RESUME_ATTEMPTS: u32 = 3;

/// Per-call ceiling for a best-effort `gh` subprocess invoked from the reaper
/// (Issue #3973).
///
/// The reaper's forge-label reconciliation (`restore_label_to_ready`,
/// `issue_has_blocked_label`, the quarantine label flips) runs on the
/// `ListSweeps` / `GetSweepStatus` **read path** via [`SweepRegistry::reap_liveness`].
/// During the 2026-07-26 incident a wedged `gh`/XPC blocked that read under the
/// registry mutex indefinitely, so an operator `list_sweeps` hung ~15 minutes.
/// Every reaper `gh` call is bounded to this window: on timeout the child is
/// killed and the call is treated as the same best-effort failure any other
/// `gh` error already is, so the in-memory liveness transition always completes.
/// Overridable via [`REAP_GH_TIMEOUT_ENV`] for operability.
pub(crate) const REAP_GH_TIMEOUT: Duration = Duration::from_secs(5);

/// Env var overriding [`REAP_GH_TIMEOUT`] (whole seconds; zero/invalid ignored).
pub const REAP_GH_TIMEOUT_ENV: &str = "LOOM_REAP_GH_TIMEOUT_SECS";

/// Poll cadence for [`output_with_timeout`] while waiting on a reaper `gh` call.
pub(crate) const REAP_GH_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Whether a daemon-dispatched child should route through `claude-wrapper.sh`'s
/// retry/backoff/classification layer (Issue #4255).
///
/// Daemon dispatch and the role runner are the unattended paths that most need
/// transient-error recovery, so the wrapper is the **default** — `spawn_child`
/// and the role runner append `--use-wrapper` to the `spawn-claude.sh` argv.
/// An operator can force the legacy single-shot path (bare `claude`, no retry)
/// for debugging by exporting `LOOM_USE_WRAPPER` to a falsey value
/// (`0`/`false`/`no`/`off`, case-insensitive). Any other value — or the var
/// being unset — keeps the wrapper on.
pub(crate) fn wrapper_dispatch_enabled() -> bool {
    match std::env::var("LOOM_USE_WRAPPER") {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"),
        Err(_) => true,
    }
}

/// Resolve the per-call reaper `gh` timeout (Issue #3973): the
/// [`REAP_GH_TIMEOUT_ENV`] override (whole seconds, must be > 0) or the
/// [`REAP_GH_TIMEOUT`] default.
pub(crate) fn reap_gh_timeout() -> Duration {
    std::env::var(REAP_GH_TIMEOUT_ENV)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .map(Duration::from_secs)
        .unwrap_or(REAP_GH_TIMEOUT)
}

/// Run `cmd` to completion but abandon (kill) it if it exceeds `timeout`
/// (Issue #3973).
///
/// Returns `Ok(Some(output))` when the child completed within the window,
/// `Ok(None)` when it was killed for exceeding `timeout`, and `Err` when the
/// spawn itself failed. Used to bound the best-effort `gh` calls the reaper
/// makes on the `ListSweeps` / `GetSweepStatus` read path so a wedged `gh`/XPC
/// cannot block the registry read indefinitely (the 2026-07-26 incident).
///
/// stdout/stderr are forced to `piped()` so a completed call's output is always
/// captured (callers that parse stdout — e.g. the `loom:blocked` probe — depend
/// on this). The reaper's `gh` invocations emit a tiny payload (a label list or
/// an edit ack), so the `try_wait` poll loop never risks a full-pipe-buffer
/// deadlock; the kill-on-timeout path drains the pipe via `wait()` after the
/// signal.
pub(crate) fn output_with_timeout(
    mut cmd: Command,
    timeout: Duration,
) -> std::io::Result<Option<std::process::Output>> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output().map(Some);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(REAP_GH_POLL_INTERVAL);
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

/// A reaper-driven resume dispatch (#4256) whose spawn (`Command::spawn()`)
/// succeeded and now needs its account-selection poll — the one genuinely
/// multi-second step, bounded by `TOKEN_NAME_CAPTURE_TIMEOUT` — run OUTSIDE
/// the registry mutex (Issue #6691). Mirrors [`PreparedIssueDispatch`] for
/// the #6592/#6688 begin/poll/finish split, adapted to also carry the
/// resume-specific bookkeeping (`attempt_no`, `resume_phase_check`) that
/// [`SweepRegistry::log_and_emit_resume_result`] needs once the poll
/// completes and the outcome can be finished under a freshly re-taken lock.
///
/// Produced only by
/// [`SweepRegistry::reap_once_impl`]`(defer_resume_poll = true)`, consumed
/// only by [`reap_once_releasing_poll_lock`] — both reachable only from the
/// reaper, preserving `dispatch_resume_after_crash`'s "only reachable from
/// `Self::reap_once`" #4123 bypass exclusivity (see its doc comment in
/// `dispatch.rs`).
pub(crate) struct PendingResumeDispatch {
    issue: u32,
    pr: u32,
    attempt_no: u32,
    resume_phase_check: Option<String>,
    prepared: Box<PreparedIssueDispatch>,
}

/// Outcome of [`SweepRegistry::reap_once_impl`]: the entry-count delta
/// [`SweepRegistry::reap_once`] has always returned, plus any resume
/// dispatches (#4256) whose account-selection poll was deferred (Issue
/// #6691) to the caller instead of being run inline under the registry
/// mutex.
pub(crate) struct ReapOnceOutcome {
    pub(crate) changes: usize,
    pub(crate) pending_resumes: Vec<PendingResumeDispatch>,
}

/// Issue #6691: extends the #6592 (`ipc.rs::dispatch_sweep_nonblocking`) /
/// #6688 (`work_finder.rs::RegistryDispatcher::dispatch` via
/// [`crate::sweep_registry::dispatch_issue_releasing_poll_lock`]) begin →
/// poll → finish split to the reaper's own crash-resume dispatch path
/// (`dispatch_resume_after_crash`, reachable only via
/// [`SweepRegistry::reap_once`]). Before this, [`spawn_reaper_task`]'s tick
/// held the registry mutex for the entirety of `reap_once()`, including the
/// up-to-5s `TOKEN_NAME_CAPTURE_TIMEOUT` account-selection poll a resume
/// dispatch's spawned child may need — the same class of hazard #6592/#6688
/// fixed for their own call sites, just far rarer here (a resume only fires
/// on a crashed sweep with real Builder-or-later checkpoint progress AND a
/// still-open linked PR, on the reaper's 30s tick cadence).
///
/// Runs [`SweepRegistry::reap_once_impl`] with `defer_resume_poll = true`:
/// every other reap concern (liveness probes, crash/exit classification,
/// label restoration, quarantine bookkeeping, event emission) still
/// completes in that single locked pass exactly as
/// [`SweepRegistry::reap_once`] itself does — only a resume dispatch that
/// actually spawned a child has its poll deferred to here, UNLOCKED, then
/// finished (`finish_issue_dispatch` plus the same dispatched/failed
/// logging and `SweepResumeDispatched` event `reap_once`'s own inline path
/// emits, via the shared [`SweepRegistry::log_and_emit_resume_result`]) under
/// a freshly re-taken lock.
pub(crate) fn reap_once_releasing_poll_lock(registry: &Arc<Mutex<SweepRegistry>>) -> usize {
    let ReapOnceOutcome {
        changes,
        pending_resumes,
    } = {
        let mut r = registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        r.reap_once_impl(true)
    };

    // Issue #6712: test-only synchronization point, fired the instant the
    // locked guard-chain pass above finishes (the `r` guard is dropped by
    // the closing brace before this line runs) and we're about to start
    // iterating `pending_resumes`, each of which polls its spawned child
    // UNLOCKED below. A test can block on this instead of guessing a fixed
    // "head start" sleep for how long the guard chain's subprocess execs
    // take — which is host-load-dependent and was the actual source of the
    // flakiness #6712 tracks, not `reap_once_releasing_poll_lock`'s
    // lock-release correctness itself. Firing here (rather than inside the
    // loop, after the poll) means the signal still lands at the identical
    // code point if this function ever regresses to holding the lock across
    // the loop (e.g. reusing the outer guard instead of re-acquiring it
    // below) — the regression-detection contract a test built on this hook
    // relies on is unaffected. No-op (compiled out) outside `cfg(test)`.
    #[cfg(test)]
    test_hooks::fire_entering_unlocked_poll();

    for pending in pending_resumes {
        let PendingResumeDispatch {
            issue,
            pr,
            attempt_no,
            resume_phase_check,
            mut prepared,
        } = pending;

        // UNLOCKED: the one genuinely multi-second step.
        let (token_name, runtime, immediate_preflight_death) = poll_and_classify_spawned_child(
            &mut prepared.child,
            &prepared.log_path,
            &prepared.header_anchor,
        );

        let mut r = registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result =
            r.finish_issue_dispatch(*prepared, token_name, runtime, immediate_preflight_death);
        let mut events = Vec::new();
        r.log_and_emit_resume_result(
            issue,
            pr,
            attempt_no,
            resume_phase_check,
            result,
            &mut events,
        );
        for event in events {
            r.emit_event(event);
        }
    }

    changes
}

/// Spawn the long-running reaper task. Returns the task handle so the
/// daemon can keep it alive for the lifetime of the process.
///
/// The reaper takes the registry lock briefly each tick; it never holds
/// the lock across the sleep — nor, since Issue #6691, across a
/// crash-resume dispatch's account-selection poll (see
/// [`reap_once_releasing_poll_lock`]).
pub fn spawn_reaper_task(registry: Arc<Mutex<SweepRegistry>>) -> tokio::task::JoinHandle<()> {
    let interval = resolve_reaper_interval();
    log::info!("sweep_registry: starting reaper with interval={}s", interval.as_secs());
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // First tick fires immediately; skip it so we don't churn at boot.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            // Issue #6691: was `registry.lock()` + `r.reap_once()` — a single
            // lock hold across the whole tick, including any crash-resume
            // dispatch's up-to-5s poll. `reap_once_releasing_poll_lock`
            // releases the mutex for that poll internally, taking/dropping
            // the lock itself as needed.
            let changed = reap_once_releasing_poll_lock(&registry);
            match registry.lock() {
                Ok(r) => {
                    // Peer-claim heartbeat (#4431): re-advertise every
                    // live claim each reaper tick so it never expires
                    // from peers' views mid-run, now that label
                    // reconciliation is a slow healing cadence on
                    // safehouse-enabled hosts. Runs after `reap_once` so
                    // a just-reaped (dead) sweep is never re-advertised.
                    let readvertised = r.readvertise_peer_claims();
                    if readvertised > 0 {
                        // #5921: promoted from `debug!` — at the default
                        // log level this heartbeat was previously
                        // invisible, making every duplicate-dispatch
                        // report undiagnosable ("did the re-advertise
                        // path even run?"). The running count is also
                        // now visible without log-scraping via
                        // `PeerClaimStatus::advertised`
                        // (`loom-daemon status` / `loom-daemon
                        // peer-claims`).
                        log::info!(
                            "sweep_registry: re-advertised {readvertised} live peer \
                             claim(s) (#4431)"
                        );
                    }
                    // Peer-coordination health (Issue #6157): evaluate on
                    // this same cadence, right after re-advertising, so
                    // the DEGRADED grace window is measured in reaper-tick
                    // units. Only log on an actual transition — every
                    // other tick would just repeat the same verdict.
                    // Diagnostic-only as of Epic #6165 Phase 4 (#6317):
                    // this verdict no longer gates stale-claim
                    // reclamation (the lease record, #6286, is the sole
                    // fleet-scoped gate for that) — it purely surfaces
                    // whether the peer-claim/safehouse advertisement
                    // channel itself looks healthy, for
                    // `loom-daemon health`/`status`.
                    if let Some(eval) = r.evaluate_peer_coordination() {
                        if eval.transitioned {
                            if eval.degraded {
                                log::warn!(
                                    "sweep_registry: peer coordination DEGRADED — {} \
                                     (#6157, diagnostic-only since #6317)",
                                    eval.reason
                                );
                            } else {
                                log::info!(
                                    "sweep_registry: peer coordination RECOVERED — {} \
                                     (#6157)",
                                    eval.reason
                                );
                            }
                        }
                    }
                }
                Err(poisoned) => {
                    log::error!("sweep_registry: mutex poisoned ({poisoned:?})");
                    return;
                }
            }
            if changed > 0 {
                log::info!(
                    "sweep_registry: reaper changed {changed} entr{}",
                    if changed == 1 { "y" } else { "ies" }
                );
            }
        }
    })
}

/// Best-effort extraction of the `phase` field from a sweep checkpoint
/// JSON file. Schema is owned by the sweep skill (#3373); we treat the
/// file as opaque and only peek at one field.
pub(crate) fn read_checkpoint_phase(path: &Path) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&s).ok()?;
    v.get("phase")
        .and_then(|p| p.as_str())
        .map(ToString::to_string)
}

/// Best-effort extraction of the `pr_number` field from a sweep checkpoint
/// JSON file (Issue #4704). Same opaque-file discipline as
/// [`read_checkpoint_phase`]: one field, no schema coupling. `null` (the
/// pre-Builder shape `sweep-checkpoint.sh` writes) and any non-numeric or
/// out-of-range value yield `None`.
pub(crate) fn read_checkpoint_pr_number(path: &Path) -> Option<u32> {
    let s = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&s).ok()?;
    v.get("pr_number")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
}

/// Returns `true` when the checkpoint at `path` was last written at or after
/// `started_at` — i.e. by the sweep run that began at `started_at` — rather than
/// being a stale artifact left on disk by an earlier dispatch (#4009).
///
/// Sweep checkpoints persist across dispatches (`.loom/sweep-checkpoint/
/// issue-<N>.json` is only removed by an explicit `sweep-checkpoint.sh delete`,
/// which never runs on a crash — #3373), so the mere *presence* of the file
/// does not prove the run that just died made any progress. A single
/// successful-enough historical run would otherwise leave the file on disk
/// forever, permanently exempting the issue from the insta-crash quarantine
/// (#3939) even as every subsequent dispatch dies pre-work in under 2s — an
/// infinite re-dispatch loop.
///
/// Comparing the file's mtime against this run's `started_at` distinguishes
/// "this run reached real work" (a mid-build death — the #3895 watchdog's
/// remit, which must reset the insta-crash tally) from "a checkpoint from an
/// earlier dispatch happens to exist" (a pre-work insta-crash that must still
/// count toward quarantine).
///
/// A missing file, or an unreadable/absent mtime, yields `false` (treated as
/// "no progress by this run"), so an unreadable checkpoint never shields an
/// issue from quarantine.
pub(crate) fn checkpoint_written_by_run(path: &Path, started_at: DateTime<Utc>) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(mtime) = meta.modified() else {
        return false;
    };
    DateTime::<Utc>::from(mtime) >= started_at
}

/// Send a signal to a PID. Returns `true` on success (signal queued or
/// process already absent and the caller can treat that as "done"). PID
/// 0 is rejected to avoid the POSIX broadcast-to-group semantics.
#[cfg(unix)]
pub(crate) fn send_signal(pid: u32, sig: i32) -> bool {
    if pid == 0 {
        return false;
    }
    let Ok(pid_t): Result<i32, _> = pid.try_into() else {
        return false;
    };
    libc_kill(pid_t, sig) == 0
}

#[cfg(not(unix))]
pub(crate) fn send_signal(_pid: u32, _sig: i32) -> bool {
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
pub(crate) fn send_group_signal(pgid: u32, sig: i32) -> bool {
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
pub(crate) fn send_group_signal(_pgid: u32, _sig: i32) -> bool {
    false
}

/// Whether the process group `pgid` still has at least one member (Issue
/// #4980).
///
/// `kill(-pgid, 0)` is the group-scoped twin of the `kill(pid, 0)` liveness
/// probe: it succeeds while *any* process remains in the group and fails with
/// `ESRCH` once the group is empty. This is what lets the crash-path reaper
/// distinguish "the leader died and took its tree with it" (nothing to do) from
/// "the leader died and left an agent running unclaimed work" (the incident this
/// issue exists to close).
///
/// `EPERM` counts as *present* for the same fail-safe reason
/// [`is_pid_alive`](crate::sweep_registry::is_pid_alive) treats it as alive: the
/// group demonstrably exists, we merely may not signal it.
#[cfg(unix)]
pub(crate) fn group_has_members(pgid: u32) -> bool {
    if pgid == 0 {
        return false;
    }
    let Ok(pgid_t): Result<i32, _> = pgid.try_into() else {
        return false;
    };
    if libc_kill(-pgid_t, 0) == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(EPERM)
}

#[cfg(not(unix))]
pub(crate) fn group_has_members(_pgid: u32) -> bool {
    false
}

/// Grace between the crash-path reaper's group SIGTERM and its SIGKILL
/// escalation (Issue #4980).
///
/// The escalation is deliberately deferred to a later reaper tick rather than
/// slept through inline: [`SweepRegistry::reap_once`] also runs on the
/// `ListSweeps` / `GetSweepStatus` read path (via `reap_liveness`) while holding
/// the registry mutex, and blocking there for a grace window is the exact
/// 2026-07-26 wedge [`REAP_GH_TIMEOUT`] exists to prevent. Five seconds means
/// the next ordinary tick (30s) is always past the deadline.
pub(crate) const ORPHAN_GROUP_REAP_GRACE: Duration = Duration::from_secs(5);

/// One entry's snapshot taken at the top of a [`SweepRegistry::reap_once`] tick:
/// `(sweep_id, pid, pgid, state, kind, started_at)`. Snapshotted (rather than
/// iterated in place) so the loop body can borrow the registry mutably; `pgid`
/// joined the tuple in #4980 so the crash path can reap a dead leader's
/// surviving process group.
pub(crate) type ReapCandidate = (SweepId, u32, Option<u32>, SweepState, SweepKind, DateTime<Utc>);

/// A crash-path group reap awaiting SIGKILL escalation (Issue #4980).
#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingGroupReap {
    /// The process group that was SIGTERM'd.
    pub(crate) pgid: u32,
    /// When SIGKILL becomes due if the group still has members.
    pub(crate) escalate_at: Instant,
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
pub(crate) fn tail_lines(path: &Path, n: usize) -> Result<Vec<String>> {
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

impl SweepRegistry {
    // ------------------------------------------------------------------------
    // Cancellation + status accessors (Issue #3455, Phase C)
    // ------------------------------------------------------------------------

    /// Return the `SweepInfo` for the given sweep ID, cloned (so callers
    /// can release the registry lock immediately) and with the live-phase
    /// overlay applied (see [`Self::overlay_live_phase`], #4328). Phase C
    /// exposes this as the `get_sweep_status` MCP tool.
    #[must_use]
    pub fn get_status(&self, sweep_id: &str) -> Option<SweepInfo> {
        let mut info = self.entries.get(sweep_id).cloned()?;
        self.overlay_live_phase(&mut info);
        Some(info)
    }

    /// Signal a sweep's process **group** (`kill(-pgid, sig)`), so the entire
    /// `claude` subprocess subtree — wrapper, agent, build tools, simulations,
    /// watcher loops — is reached rather than just the tracked leader PID
    /// (Issue #3800).
    ///
    /// # Why this is no longer gated on a retained `Child` handle (Issue #4980)
    ///
    /// It used to be: `if self.children.contains_key(sweep_id)`. That made
    /// group delivery an accident of *which process is asking*. Two ordinary
    /// situations have no handle and silently degraded to a single-PID kill:
    ///
    /// - a `reconstruct()`-ed entry after a daemon restart, and
    /// - **every** invocation from a fresh `loom-daemon cancel` CLI process,
    ///   which never held a spawn-time handle at all.
    ///
    /// Degrading there is precisely the 2026-08-03 incident: SIGKILLing the
    /// tracked wrapper left the `claude` agent alive, which noticed its
    /// subprocesses had died and *relaunched them*. So the group is now resolved
    /// from durable state — [`SweepInfo::pgid`], persisted at spawn time and
    /// restored by `reconstruct()` — and used unconditionally.
    ///
    /// # Fallbacks (log, never panic, never mis-target)
    ///
    /// - No recorded pgid but we DO hold the handle ⇒ the leader is live and was
    ///   spawned by us with `process_group(0)`, so `pgid == pid` holds by
    ///   construction (the pre-#4980 behavior, retained for entries created
    ///   before the field was populated).
    /// - No recorded pgid and no handle (a pre-#4980 `owner.json`, a
    ///   checkpoint-only entry, a non-Unix host) ⇒ single-PID delivery, with a
    ///   log line naming the degradation rather than a silent one.
    /// - A recorded pgid equal to **our own** process group ⇒ refuse the group
    ///   signal outright. `kill(-our_pgid, 9)` would kill the daemon and every
    ///   sweep it owns; a stale record naming our group (PID recycling across a
    ///   restart) must never be able to do that.
    pub(crate) fn signal_sweep(&self, sweep_id: &str, pid: u32, sig: i32) -> bool {
        let recorded = self.entries.get(sweep_id).and_then(|info| info.pgid);
        let retained_handle = self.children.contains_key(sweep_id);
        let Some(pgid) = recorded.or_else(|| retained_handle.then_some(pid)) else {
            log::warn!(
                "signal_sweep: sweep {sweep_id} (pid {pid}) has no recorded process group \
                 (pre-#4980 lock record or unknown-group entry) — falling back to single-PID \
                 signal {sig}; descendants may survive"
            );
            return send_signal(pid, sig);
        };
        if Some(pgid) == current_process_group() {
            log::error!(
                "signal_sweep: refusing to send signal {sig} to process group {pgid} for sweep \
                 {sweep_id} — that is THIS process's own group (stale/incorrect pgid record). \
                 Falling back to single-PID delivery to pid {pid} (#4980)."
            );
            return send_signal(pid, sig);
        }
        send_group_signal(pgid, sig)
    }

    /// Terminate the surviving process group of a sweep whose **leader is
    /// already dead** (Issue #4980) — the crash path.
    ///
    /// A dead wrapper does not imply a dead tree. In the 2026-08-03 incident the
    /// tracked pid was gone while the `claude` agent it had spawned kept running
    /// against an issue whose claim had already been returned to the queue: a
    /// zombie agent, invisible to the registry (`in_flight: 0`), burning CPU and
    /// mutating a repo it no longer held. `signal_sweep` cannot help here — the
    /// OS refuses to report a dead pid's group — which is exactly why the pgid is
    /// persisted while the leader is alive.
    ///
    /// Sends SIGTERM now and registers a deferred SIGKILL escalation
    /// ([`ORPHAN_GROUP_REAP_GRACE`]) picked up by a later
    /// [`reap_once`](Self::reap_once) tick, so no caller ever blocks on a grace
    /// window while holding the registry mutex. A no-op (returning `false`) when
    /// the group is already empty — the overwhelmingly common case, where the
    /// leader's death took its whole tree with it.
    pub(crate) fn reap_orphaned_group(
        &mut self,
        sweep_id: &str,
        issue: Option<u32>,
        pgid: u32,
    ) -> bool {
        if pgid == 0 || Some(pgid) == current_process_group() {
            log::error!(
                "reap_orphaned_group: refusing to signal process group {pgid} for sweep \
                 {sweep_id} — it is zero or THIS process's own group (#4980)"
            );
            return false;
        }
        if !group_has_members(pgid) {
            return false;
        }
        let scope = issue.map_or_else(String::new, |n| format!(" (issue #{n})"));
        log::warn!(
            "reap_orphaned_group: sweep {sweep_id}{scope} has a DEAD leader but its process \
             group {pgid} still has members — an orphaned agent/subtree running unclaimed work. \
             Sending SIGTERM to the group; escalating to SIGKILL in {}s if it survives (#4980).",
            ORPHAN_GROUP_REAP_GRACE.as_secs()
        );
        send_group_signal(pgid, 15);
        self.pending_group_reaps.insert(
            sweep_id.to_string(),
            PendingGroupReap {
                pgid,
                escalate_at: Instant::now() + ORPHAN_GROUP_REAP_GRACE,
            },
        );
        true
    }

    /// SIGKILL any orphaned group that survived its crash-path SIGTERM past
    /// [`ORPHAN_GROUP_REAP_GRACE`] (Issue #4980). Called at the top of every
    /// [`reap_once`](Self::reap_once) tick, mirroring how
    /// `retry_pending_quarantine_releases` drains its own deferred work.
    /// Cheap early-return when nothing is pending.
    pub(crate) fn escalate_pending_group_reaps(&mut self) {
        if self.pending_group_reaps.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut done: Vec<SweepId> = Vec::new();
        for (sweep_id, pending) in &self.pending_group_reaps {
            if !group_has_members(pending.pgid) {
                // The SIGTERM worked (or the group drained on its own).
                done.push(sweep_id.clone());
                continue;
            }
            if now < pending.escalate_at {
                continue;
            }
            log::warn!(
                "reap_orphaned_group: process group {} for sweep {sweep_id} survived SIGTERM — \
                 escalating to SIGKILL (#4980)",
                pending.pgid
            );
            send_group_signal(pending.pgid, 9);
            done.push(sweep_id.clone());
        }
        for sweep_id in done {
            self.pending_group_reaps.remove(&sweep_id);
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
    pub(crate) fn poll_liveness(&mut self, sweep_id: &str, pid: u32) -> (bool, Option<i32>) {
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
    pub(crate) fn reap_handle(&mut self, sweep_id: &str) -> Option<std::process::ExitStatus> {
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
            // Ownership-checked release (#4463): if a newer sweep re-acquired
            // this issue's lock after the sweep being cancelled died, leave its
            // live lock intact and skip the label restore below — the newer
            // sweep owns the claim and runs its own lifecycle. #4556 extends the
            // same skip to `HolderAlive`: the cancelled sweep's OWN pid is still
            // a live `/loom:sweep <N>` process, so the claim is not free either.
            //
            // #5017/#5282: `release_lock_owned` only ever sees THIS host's own
            // local `.loom/locks/issue-<N>` — a different host's live claim on
            // the SAME issue is invisible to it (there is no shared lock
            // directory across hosts), so a purely local check can return
            // "Released" even while a peer host's sweep is actively building.
            // `claim_superseded_on_forge` is the cross-host backstop: it only
            // runs (short-circuits via `||`) when the local check did NOT
            // already answer the question, and compares the forge's current
            // `loom:building` labeled-event timestamp against this sweep's own
            // `started_at` — a labeling event strictly after `started_at` means
            // a different claimant (possibly on another host) now owns it.
            let claim_held_elsewhere = self.release_lock_owned(*issue, sweep_id).retained()
                || self.claim_superseded_on_forge(*issue, started_at);
            // Best-effort tidy-up of the machine-level liveness journal
            // (#3953) — this cancelled sweep no longer exists.
            self.journal_remove_best_effort(*issue);
            // Orphaned-claim recovery on cancel (issue #3827): a cancelled
            // daemon-owned Issue sweep that never opened a PR still holds its
            // pre-dispatch loom:building claim (set at `dispatch()` step 4).
            // Unlike `reap_once()`'s clean-exit branch (#3823b), `finish_cancel`
            // historically never restored the label, so cancelling stranded the
            // issue in loom:building. Restore loom:building -> loom:issue so the
            // issue is automatically recoverable — but only when this sweep
            // produced no PR, so we never yank the label out from under an
            // in-flight PR's issue. Gated on `!skip_label_flip`, mirroring the
            // reaper path.
            // #4463/#5017/#5282: a claim held elsewhere (locally superseded OR
            // forge-superseded) means a different sweep now holds the claim —
            // never restore the label out from under it.
            if !self.config.skip_label_flip && !produced_pr && !claim_held_elsewhere {
                let _ = self.restore_label_to_ready(*issue);
                self.note_label_flip(*issue); // #4485 flap detection
            }
            // Durable terminal-outcome record (Issue #4644) — a cancel is a
            // deliberate terminal transition too, so it gets the same
            // append-only journal line as a reaper-observed death. The
            // telemetry `result` (#4704) is unambiguously `Cancelled` here —
            // an operator/watchdog-initiated cancel, not a self-terminated
            // success or failure.
            self.append_outcome_journal(
                *issue,
                sweep_id,
                "exited",
                None,
                None,
                None, // manual cancel, never an account-exhaustion crash
                duration_sec,
                telemetry::SweepResult::Cancelled,
            );
            self.emit_event(Event::SweepExited {
                issue: *issue,
                exit_code: None,
                duration_sec,
                // #4366: an operator/reaper-initiated cancel is not the
                // no-progress-exit-0 failure signature (there's no exit code
                // at all) — never count a cancel toward quarantine.
                no_progress: false,
                death_class: None, // manual cancel, never a pre-flight death (#4386)
                repo: None,        // stamped by emit_event (#3929)
            });
        }
        // Issue #5342: `PrSet` cancels DO reach here (unlike the `Issue` arm
        // above, they were previously unreachable because dispatch always
        // refused `PrSet`). There is no forge label to restore or per-issue
        // outcome journal/event to write — `PrSet` claims no single issue —
        // but the per-PR claim locks acquired at dispatch time must still be
        // released, or every PR in the set stays permanently un-dispatchable.
        if let SweepKind::PrSet(prs) = kind {
            for pr in prs {
                let _ = self.release_pr_lock_owned(*pr, sweep_id);
            }
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
    ///
    /// Fully self-contained: any reaper-driven crash-resume dispatch (#4256)
    /// that spawns a child is polled and finished inline, before this method
    /// returns, exactly as before Issue #6691. Only [`reap_once_releasing_poll_lock`]
    /// — the production path driven from [`spawn_reaper_task`] — defers that
    /// poll to run the registry mutex unlocked; every other caller (every
    /// unit test in this module and its siblings) gets this original,
    /// synchronous behavior with no Mutex to release in the first place.
    pub fn reap_once(&mut self) -> usize {
        self.reap_once_impl(false).changes
    }

    /// The actual reaper-tick body behind [`Self::reap_once`] (Issue #6691).
    /// `defer_resume_poll = false` reproduces `reap_once`'s original,
    /// fully-synchronous behavior byte-for-byte (any spawned resume dispatch
    /// is polled and finished inline, right here, under `&mut self`).
    /// `defer_resume_poll = true` — used only by
    /// [`reap_once_releasing_poll_lock`] — instead collects any spawned
    /// resume dispatch's [`PreparedIssueDispatch`] into
    /// [`ReapOnceOutcome::pending_resumes`] without polling it, so the caller
    /// can run that poll with the registry mutex released.
    #[allow(clippy::too_many_lines)]
    fn reap_once_impl(&mut self, defer_resume_poll: bool) -> ReapOnceOutcome {
        let mut changes = 0usize;
        let mut pending_resumes: Vec<PendingResumeDispatch> = Vec::new();

        // Insta-crash quarantine TTL (#3939): release any issue whose quarantine
        // has aged past the configured window before this tick's work. Cheap
        // early-return when nothing is quarantined.
        self.expire_quarantine();
        // Retry any previously-failed quarantine label restores (Issue #4110).
        // Cheap early-return when nothing is pending.
        self.retry_pending_quarantine_releases();
        // SIGKILL-escalate any orphaned process group that survived a
        // crash-path SIGTERM (Issue #4980). Cheap early-return when nothing is
        // pending; never blocks (the grace is deadline-based, not slept).
        self.escalate_pending_group_reaps();

        // Snapshot keys + pids first so we can borrow mutably below.
        // Capture started_at so we can compute durations for Exited events.
        // `pgid` rides along so the crash path can reap a dead leader's
        // surviving process group (#4980).
        let candidates: Vec<ReapCandidate> = self
            .entries
            .iter()
            .map(|(id, info)| {
                (
                    id.clone(),
                    info.pid,
                    info.pgid,
                    info.state.clone(),
                    info.kind.clone(),
                    info.started_at,
                )
            })
            .collect();

        // Buffer events to emit after we've finished mutating the
        // registry — so we never call into the bus while holding the
        // registry mutex's lifetime budget unnecessarily.
        let mut events_to_emit: Vec<Event> = Vec::new();

        for (sweep_id, pid, pgid, state, kind, started_at) in candidates {
            if !matches!(state, SweepState::Running | SweepState::Pending) {
                continue;
            }
            // Sample the live checkpoint phase BEFORE the liveness probe
            // (Issue #4704) so the tick that observes a death still captures
            // the last phase the sweep reached — the durable `sweep.outcome`
            // record's per-phase breakdown is built from this history, and the
            // checkpoint itself is overwritten per phase (and deleted on
            // success), so nothing else preserves it.
            self.sample_phase_transition(&sweep_id, &kind, started_at);
            // Liveness via the retained `Child` handle when we own it: this
            // `try_wait()`s the child, reaping any zombie (Issue #3801) and
            // yielding the real exit code. Reconstructed entries with no
            // handle fall back to the `kill(pid, 0)` probe.
            let (is_dead, exit_code) = self.poll_liveness(&sweep_id, pid);
            if is_dead {
                // Issue #4980 crash-path reap: the tracked leader is gone, but
                // its process group may still hold a live `claude` agent and
                // whatever that agent spawned — the zombie-agent shape of the
                // 2026-08-03 incident, which the registry rendered as
                // `in_flight: 0` while the survivors kept mutating the repo.
                // Signal the group before the entry transitions terminal (after
                // which nothing tracks the pgid at all). No-op when the group is
                // already empty, which is the ordinary case.
                if let Some(pgid) = pgid {
                    let issue = match &kind {
                        SweepKind::Issue(n) => Some(*n),
                        SweepKind::PrSet(_) => None,
                    };
                    self.reap_orphaned_group(&sweep_id, issue, pgid);
                }
                // #4493: account health must be updated before any bounded
                // re-dispatch path below asks the selector for another profile.
                self.apply_provider_health_feedback(&sweep_id, exit_code);
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
                        // Ownership-checked release (#4463): a reaper tick (in
                        // this daemon or any other instance sharing the
                        // workspace) must never delete a lock that a *newer*
                        // live sweep re-acquired after this dead one. When the
                        // lock is `Superseded`, skip the label restore AND the
                        // resume/re-dispatch below — the dead sweep is not
                        // crashed-needing-recovery, it is superseded. #4556 folds
                        // in `HolderAlive` (this sweep's own pid is still a live
                        // `/loom:sweep <N>` process, so the reap verdict was
                        // wrong) via the shared `retained()` predicate.
                        //
                        // #5017/#5282: the local lock is host-local (see the
                        // `finish_cancel` comment above this same check) so it
                        // cannot see a peer host's live claim on this issue —
                        // `claim_superseded_on_forge` is the cross-host
                        // backstop, only invoked (via `||` short-circuit) when
                        // the local check did not already answer the question.
                        let superseded = self.release_lock_owned(issue, &sweep_id).retained()
                            || self.claim_superseded_on_forge(issue, started_at);
                        // Best-effort tidy-up of the machine-level liveness
                        // journal (#3953): the reaper just confirmed this
                        // PID is dead, so drop its entry now rather than
                        // waiting for the next prune-on-read. Not
                        // load-bearing — a missed removal is pruned on the
                        // next journal touch — but keeps the file small.
                        self.journal_remove_best_effort(issue);
                        let checkpoint = self
                            .config
                            .checkpoint_dir()
                            .join(format!("issue-{issue}.json"));
                        if checkpoint.exists() {
                            // #4463: never restore the label when a newer sweep
                            // owns the lock — it is actively building.
                            if !self.config.skip_label_flip && !superseded {
                                let _ = self.restore_label_to_ready(issue);
                                self.note_label_flip(issue); // #4485 flap detection
                            }
                            let checkpoint_phase = read_checkpoint_phase(&checkpoint);
                            // Issue #4255: attribute WHY the sweep died by
                            // classifying the tail of its log (account
                            // exhaustion / `Execution error` / bare exit code)
                            // and carrying that verdict on the crashed event
                            // alongside the phase. Best-effort: an unreadable
                            // log yields `None`, exactly like a clean exit.
                            let log_path = self.entries.get(&sweep_id).map(|i| i.log_path.clone());
                            let classification = log_path
                                .as_deref()
                                .and_then(|p| tail_lines(p, EXHAUSTION_LOG_TAIL_LINES).ok())
                                .map(|lines| lines.join("\n"))
                                .and_then(|tail| classify_crash(&tail, exit_code));
                            // Issue #4386: whether THIS run's checkpoint write
                            // proves genuine progress (see the comment above
                            // the `if checkpoint_written_by_run` branch below)
                            // — hoisted here because it also determines
                            // whether this death can even be a pre-flight
                            // death: genuine progress definitely reached past
                            // `# CLAUDE_CLI_START`, so there is nothing left
                            // to classify.
                            let checkpoint_progress =
                                checkpoint_written_by_run(&checkpoint, started_at);
                            let insta_crash = duration_sec
                                < self.quarantine_config.insta_crash_secs
                                && exit_code != Some(0);
                            // Reaper-side pre-flight-death classification +
                            // workspace tripwire streak update (#4386),
                            // consulted alongside the #4255 crash
                            // classification above. Precedence: exhaustion
                            // wins (handled inside `record_preflight_streak`),
                            // so a death already attributed to the account is
                            // never also charged toward — or reset — the
                            // pre-flight streak.
                            let death_class = if checkpoint_progress {
                                self.reset_preflight_streak();
                                None
                            } else {
                                self.record_preflight_streak(&sweep_id, insta_crash)
                            };
                            // Captured before `death_class` moves into the
                            // `SweepCrashed` event below — the carve-out check
                            // further down needs to know whether THIS death was
                            // pre-flight-classified without re-borrowing the
                            // (by-then-moved) `Option<String>`.
                            let is_preflight_death = death_class.is_some();
                            // #7708: likewise captured before `classification`
                            // is moved. A `no-usable-account` death is a
                            // POOL-level fault — see this branch's use below.
                            let pool_dead =
                                classification.as_deref() == Some(NO_USABLE_ACCOUNT_CLASS);
                            // Captured before `checkpoint_phase` moves into the
                            // `SweepCrashed` event below — needed for the
                            // reaper-driven resume check further down (#4256).
                            let resume_phase_check = checkpoint_phase.clone();
                            if let Some(info) = self.entries.get_mut(&sweep_id) {
                                info.state = SweepState::Crashed { at: now };
                                if info.latest_phase.is_none() {
                                    info.latest_phase.clone_from(&checkpoint_phase);
                                }
                            }
                            // Durable terminal-outcome record (Issue #4644),
                            // BEFORE the bus emission below moves `death_class`
                            // — independent best-effort side effects of the
                            // same terminal transition (never coupled to the
                            // bus publish's own success). The telemetry
                            // `result` (#4704) is `Failure` — a checkpoint the
                            // sweep skill never got to delete means the
                            // lifecycle did not complete — UNLESS the merge
                            // phase was observed to complete, in which case the
                            // work did land and the death came after it.
                            let telemetry_result = if self.sampled_reached_merge(&sweep_id) {
                                telemetry::SweepResult::Success
                            } else {
                                telemetry::SweepResult::Failure
                            };
                            self.append_outcome_journal(
                                issue,
                                &sweep_id,
                                "crashed",
                                exit_code,
                                death_class.clone(),
                                // Issue #5697: persist the same account-exhaustion
                                // classification (e.g.
                                // `account-exhausted:model-credits-exhausted`)
                                // the `SweepCrashed` bus event carries below —
                                // previously computed here and then dropped the
                                // instant the in-memory-only event had no
                                // subscriber.
                                classification.clone(),
                                duration_sec,
                                telemetry_result,
                            );
                            events_to_emit.push(Event::SweepCrashed {
                                issue,
                                checkpoint_phase,
                                classification,
                                death_class,
                                repo: None, // stamped by emit_event (#3929)
                            });
                            events_to_emit.push(Event::SweepGlobalCompleted {
                                sweep_id: sweep_id.clone(),
                                outcome: SweepOutcome::Crashed,
                            });
                            // Insta-crash quarantine (#3939 + #4009): a checkpoint
                            // FILE existing on disk does not prove THIS run made
                            // progress — checkpoints persist across dispatches
                            // (#3373), so a single successful-enough historical run
                            // would otherwise exempt the issue from quarantine
                            // forever while every later dispatch dies pre-work in
                            // <2s (an infinite re-dispatch loop, #4009). Only a
                            // checkpoint (re)written by THIS run — mtime at/after
                            // our `started_at` — counts as progress. Such a genuine
                            // mid-build death is the mid-build-death watchdog's
                            // remit (#3895) and resets the consecutive tally. A
                            // stale checkpoint from an earlier dispatch does not:
                            // fall through to the same pre-work insta-crash test the
                            // checkpoint-less branch below uses, so a sub-window
                            // non-clean death still counts toward quarantine.
                            // #4485: the dispatch-backoff verdict is computed
                            // here — BEFORE the #4122 / #4386 carve-outs below —
                            // because those carve-outs exist to spare the
                            // *issue's* quarantine tally, not to license an
                            // unbounded retry cadence. Scoped to the same
                            // fast-death window the tally uses: a run that made
                            // real progress clears the window; a fast
                            // checkpoint-less death (the flap shape) arms it; a
                            // SLOW checkpoint-less death is left untouched — that
                            // is the mid-build-death (#3895) / review-stall
                            // (#3910) watchdogs' remit, each already bounded to a
                            // single retry, and arming a window there would risk
                            // burning that one retry on a refusal.
                            if checkpoint_progress {
                                self.clear_dispatch_backoff(issue);
                                // #6670: real progress is conclusive proof the
                                // candidate is no longer a no-op — clear any
                                // armed cooldown so a since-cleared window
                                // (e.g. an operator override that dispatched
                                // through it) does not linger stale.
                                self.clear_noop_cooldown(issue);
                                // #7528: same reasoning for the hard-exclusion
                                // decline record — a run that advanced the
                                // checkpoint plainly was NOT declined on a
                                // label rule, so the rule no longer applies
                                // (a maintainer cleared the label) and the
                                // consecutive tally must not carry forward
                                // into a spurious threshold WARN.
                                self.clear_decline_cooldown(issue);
                            } else if pool_dead {
                                // #7708: the pool held no usable account, so
                                // this dispatch never got far enough to say
                                // ANYTHING about the issue. Charging the
                                // #4485 per-issue ladder would be attributing
                                // a pool-wide fault to whichever issue
                                // happened to be dispatched into it — and a
                                // per-issue ladder structurally cannot damp a
                                // pool-wide fault anyway: with N ready issues
                                // each capped at 900s, the aggregate rate is
                                // still ~N doomed spawns per 15 minutes,
                                // which is exactly the 228-in-4.3h shape that
                                // filed this issue. Neither arm nor clear it;
                                // arm the HOST-level pool hold instead, which
                                // holds every issue at once.
                                crate::work_finder::pool_preflight::note_pool_dead(
                                    &self.config.workspace_root,
                                );
                            } else if insta_crash {
                                self.record_dispatch_failure(issue);
                            }
                            if checkpoint_progress {
                                self.record_terminal_outcome(issue, false);
                                // #4256: a run that advanced the checkpoint made
                                // real progress (reached Judge/Doctor and wrote a
                                // fresh phase), so it is a HEALTHY resume — clear
                                // the resume-attempt runway. Only *consecutive*
                                // checkpoint-less resume crashes (the ~2s..stall
                                // pathology) accrue toward `MAX_RESUME_ATTEMPTS`;
                                // a productively-progressing resume chain is never
                                // capped. Mirrors `record_terminal_outcome`'s
                                // reset of `insta_crash_counts` on progress.
                                self.resume_attempt_counts.remove(&issue);
                            } else if !is_preflight_death {
                                // #4122: re-attribute account-exhaustion deaths
                                // to the spawn account instead of the issue.
                                // #4386: a pre-flight-classified death is skipped
                                // entirely here — it must not charge the issue's
                                // quarantine tally either, same carve-out
                                // reasoning as exhaustion. The exhaustion case
                                // itself is NOT skipped (`PreflightOutcome::Unknown`
                                // always yields a `None`/non-preflight death_class,
                                // so exhaustion still reaches — and is handled
                                // inside — `record_insta_crash_outcome`).
                                self.record_insta_crash_outcome(&sweep_id, issue, insta_crash);
                            }
                            // Reaper-driven resume (Issue #4256): a crash whose
                            // checkpoint shows real Builder-or-later progress
                            // AND whose issue still has an open linked PR is
                            // not fresh work — it is exactly the case the
                            // #4123 open-PR guard exists to protect (an
                            // ordinary re-dispatch would double-build). But
                            // here the open PR *is* this crashed sweep's own
                            // PR, and the checkpoint-resume machinery (#3373)
                            // exists precisely to skip back to the correct
                            // phase (typically Judge) instead of redoing the
                            // Builder. Without this, the guard and the resume
                            // machinery contradict each other: the guard
                            // correctly refuses every ordinary re-dispatch,
                            // and nothing else ever re-dispatches the issue —
                            // stranding the PR at `loom:review-requested`
                            // forever. Gated on `skip_label_flip` like the
                            // guard itself (test fixtures without `gh`
                            // credentials never attempt a real forge probe
                            // here) and only checked for phases at/after
                            // Builder completion, so a pre-PR crash never
                            // pays for the extra forge round trip. A deliberate
                            // park (`loom:blocked` / `loom:operator-only`,
                            // possibly applied by `restore_label_to_ready`'s
                            // #4206 pre-check moments ago) still stops the
                            // resume — but that check now lives centrally in
                            // `dispatch_inner` step 2.7 (#4444) rather than
                            // here, so it covers the watchdogs and IPC/CLI too
                            // and there is only ONE label probe per resume
                            // dispatch. A parked issue therefore reaches the
                            // dispatch call below and is refused there, which
                            // is deliberately *more* visible than the old
                            // silent call-site skip: the refusal surfaces as a
                            // `warn!` naming the park label plus the existing
                            // `SweepResumeDispatched { dispatched: false }`
                            // event.
                            if !self.config.skip_label_flip
                                && !superseded
                                && resume_phase_check
                                    .as_deref()
                                    .is_some_and(|p| RESUMABLE_CHECKPOINT_PHASES.contains(&p))
                            {
                                // Fail-open (#4452): only a VERIFIED `Open(pr)`
                                // is eligible for the bounded resume path; both
                                // `NoneOpen` and `ProbeFailed` fall through to
                                // ordinary handling (unchanged pre-#4452
                                // behavior — a probe failure never triggers a
                                // resume dispatch).
                                if let OpenPrProbe::Open(pr) = self.probe_open_linked_pr(issue) {
                                    // Deterministic-no-op guard (Issue #5614). A
                                    // surviving checkpoint means the sweep skill
                                    // never reached its delete-on-success step —
                                    // but that is NOT the same as "the sweep
                                    // crashed". A sweep that ends its turn with
                                    // `exit_code == Some(0)` finished
                                    // deliberately; when it ALSO left the
                                    // checkpoint exactly as it found it
                                    // (`!checkpoint_progress`), the run reached a
                                    // considered terminal decision and changed
                                    // nothing — the canonical shape being an
                                    // engine-stop state on the linked PR, e.g.
                                    // Champion's `loom:operator` merge-risk hold,
                                    // where every sweep correctly reports "held
                                    // for a human" and exits 0.
                                    //
                                    // Resuming that shape re-runs an identical
                                    // decision over identical inputs and is
                                    // therefore guaranteed to produce the same
                                    // no-op — while costing a full agent spawn, a
                                    // rotated token, and TWO forge label writes
                                    // per cycle (`restore_label_to_ready` above,
                                    // then the resume dispatch's re-claim). That
                                    // is the observed #5565 flap: 7 dispatches in
                                    // 7 minutes, all exit 0, ~10 `loom:issue` /
                                    // `loom:building` transitions, only bounded
                                    // (twice — one run's same-phase checkpoint
                                    // rewrite counted as "progress" and cleared
                                    // the runway) by `MAX_RESUME_ATTEMPTS`.
                                    //
                                    // Narrow by construction, so #4256's remit is
                                    // untouched: the crash shapes it exists for
                                    // (insta-crash, exhaustion, signal death,
                                    // stall-then-kill) never carry `Some(0)`, and
                                    // a run that made real checkpoint progress is
                                    // exempt via `checkpoint_progress` regardless
                                    // of exit code — including the #4366
                                    // parked-mid-turn case, whose whole signature
                                    // is a clean exit that DID advance the
                                    // lifecycle. A no-handle reap reports
                                    // `exit_code == None`, which is not `Some(0)`,
                                    // so reconstructed entries keep pre-#5614
                                    // behavior (fail-open toward resuming).
                                    //
                                    // Deliberately does NOT consume a resume
                                    // attempt: a human-gated pause is not a failed
                                    // attempt, so clearing the hold leaves the
                                    // issue's full resume runway intact. Nor does
                                    // it strand the work — `restore_label_to_ready`
                                    // has already returned the issue to
                                    // `loom:issue`, where the #4123 open-PR guard
                                    // correctly refuses ordinary re-dispatch and
                                    // the periodic Judge/Champion roles own the
                                    // open PR. That is exactly the resting state
                                    // `MAX_RESUME_ATTEMPTS` exhaustion already
                                    // produces, reached without burning the
                                    // attempts first.
                                    let clean_no_progress_exit =
                                        exit_code == Some(0) && !checkpoint_progress;
                                    // Bounded resume attempts (#4256, Judge
                                    // residual-risk backstop): the resume path
                                    // bypasses the #4123 open-PR guard, so a sweep
                                    // stuck in the ~2s..stall crash window (never
                                    // rewrites the checkpoint, never trips the
                                    // sub-`insta_crash_secs` quarantine tally)
                                    // would otherwise resume forever. Once an
                                    // issue has accumulated `MAX_RESUME_ATTEMPTS`
                                    // consecutive checkpoint-less resume crashes,
                                    // stop resuming: emit the failure-visible
                                    // event (`dispatched: false`) once and leave
                                    // the PR for the periodic Judge role /
                                    // operator. No labels are added beyond the
                                    // ones already present.
                                    let attempts = self
                                        .resume_attempt_counts
                                        .get(&issue)
                                        .copied()
                                        .unwrap_or(0);
                                    if clean_no_progress_exit {
                                        log::warn!(
                                            "issue #{issue}: sweep exited cleanly (code 0) without \
                                             advancing its checkpoint (phase \
                                             {resume_phase_check:?}, open PR #{pr}) — a deliberate \
                                             no-op, not a crash; NOT resuming (resuming would \
                                             re-run the same decision and flap \
                                             loom:issue/loom:building, #5614). The issue is back \
                                             at loom:issue with the #4123 open-PR guard in force; \
                                             the open PR is the periodic Judge/Champion roles' \
                                             remit."
                                        );
                                        events_to_emit.push(Event::SweepResumeDispatched {
                                            issue,
                                            pr,
                                            checkpoint_phase: resume_phase_check.clone(),
                                            dispatched: false,
                                            repo: None, // stamped by emit_event (#3929)
                                        });
                                    } else if attempts >= MAX_RESUME_ATTEMPTS {
                                        log::warn!(
                                            "issue #{issue}: reaper-driven resume attempts \
                                             exhausted ({attempts}/{MAX_RESUME_ATTEMPTS} \
                                             consecutive checkpoint-less resume crashes, open \
                                             PR #{pr}, checkpoint phase {resume_phase_check:?}) \
                                             — NOT resuming again; leaving the PR for the \
                                             periodic Judge role / operator (#4256)"
                                        );
                                        events_to_emit.push(Event::SweepResumeDispatched {
                                            issue,
                                            pr,
                                            checkpoint_phase: resume_phase_check.clone(),
                                            dispatched: false,
                                            repo: None, // stamped by emit_event (#3929)
                                        });
                                    } else {
                                        // Count the attempt regardless of whether
                                        // the dispatch call itself succeeds, so a
                                        // persistently-failing resume dispatch is
                                        // bounded too.
                                        let attempt_no = *self
                                            .resume_attempt_counts
                                            .entry(issue)
                                            .and_modify(|c| *c += 1)
                                            .or_insert(1);
                                        if defer_resume_poll {
                                            // Issue #6691: called only from
                                            // `reap_once_releasing_poll_lock`
                                            // (via `reap_once_impl(true)`).
                                            // `dispatch_resume_after_crash`
                                            // composes begin -> poll -> finish
                                            // end to end under `&mut self`, so
                                            // it cannot let its caller release
                                            // the registry mutex for the poll.
                                            // Calling `begin_issue_dispatch`
                                            // directly instead — the same
                                            // first step
                                            // `dispatch_resume_after_crash`
                                            // itself calls, with the identical
                                            // `resume_bypass_pr` — surfaces the
                                            // intermediate `BeginIssueDispatch`
                                            // so a `Spawned` child's poll can
                                            // be deferred to the caller instead
                                            // of run here inline.
                                            match self.begin_issue_dispatch(
                                                &SweepKind::Issue(issue),
                                                None,
                                                None,
                                                None,
                                                None,
                                                Some(pr),
                                            ) {
                                                Ok(BeginIssueDispatch::Done(result)) => {
                                                    self.log_and_emit_resume_result(
                                                        issue,
                                                        pr,
                                                        attempt_no,
                                                        resume_phase_check.clone(),
                                                        result,
                                                        &mut events_to_emit,
                                                    );
                                                }
                                                Ok(BeginIssueDispatch::Spawned(prepared)) => {
                                                    pending_resumes.push(PendingResumeDispatch {
                                                        issue,
                                                        pr,
                                                        attempt_no,
                                                        resume_phase_check: resume_phase_check
                                                            .clone(),
                                                        prepared,
                                                    });
                                                }
                                                Err(e) => {
                                                    self.log_and_emit_resume_result(
                                                        issue,
                                                        pr,
                                                        attempt_no,
                                                        resume_phase_check.clone(),
                                                        Err(e),
                                                        &mut events_to_emit,
                                                    );
                                                }
                                            }
                                        } else {
                                            // Every direct `reap_once` caller
                                            // (no `Arc<Mutex<Self>>` to
                                            // release, so no hazard to avoid)
                                            // keeps the original, fully
                                            // synchronous
                                            // `dispatch_resume_after_crash`
                                            // composition unchanged.
                                            let result =
                                                self.dispatch_resume_after_crash(issue, pr);
                                            self.log_and_emit_resume_result(
                                                issue,
                                                pr,
                                                attempt_no,
                                                resume_phase_check.clone(),
                                                result,
                                                &mut events_to_emit,
                                            );
                                        }
                                    }
                                }
                            }
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
                            // Hard-exclusion decline (Issue #7528). The
                            // #3823b restore below is CORRECT for the case it
                            // was written for and stays byte-for-byte
                            // unchanged — but it treats every checkpoint-less
                            // clean exit identically, and one of those shapes
                            // is permanent rather than transient: a
                            // Curator/Builder that declines immediately
                            // because the issue carries a hard-exclusion
                            // label (`crate::hard_exclusion`, `external`
                            // today). Restoring `loom:issue` for that shape
                            // re-arms the very next tick, which declines for
                            // the identical reason — the unbounded loop in
                            // #7528 (23 dispatches in ~1h on
                            // rjwalters/kicad-tools#5197, ~90s of session
                            // budget each).
                            //
                            // The exit status cannot distinguish the two
                            // shapes, so the discriminator is a FACT ON THE
                            // FORGE — does the issue carry a hard-exclusion
                            // label right now — rather than any signal the
                            // declining agent session has to remember to
                            // send. Probed here, before the restore, only on
                            // a verified clean exit and only when label flips
                            // are enabled: the same two gates every other
                            // forge probe in this branch uses, so a test
                            // fixture without `gh` credentials pays nothing
                            // and the path stays a pure no-op
                            // (`declined_rule == None`, byte-identical to
                            // pre-#7528 behavior).
                            //
                            // Fails closed: an unverifiable read yields
                            // `None`, i.e. the pre-#7528 restore-and-re-offer
                            // behavior, which is the safe direction to fall —
                            // a forge outage must never manufacture a decline
                            // record.
                            let declined_rule = if !self.config.skip_label_flip
                                && exit_code == Some(0)
                                && !produced_pr
                                && !superseded
                            {
                                Some(self.issue_hard_exclusion_label(issue))
                            } else {
                                None
                            };
                            // #4463: skip the restore when a newer sweep owns
                            // the lock (superseded) — it holds the live claim.
                            if !self.config.skip_label_flip && !produced_pr && !superseded {
                                let _ = self.restore_label_to_ready(issue);
                                self.note_label_flip(issue); // #4485 flap detection
                            }
                            // #7528: the claim restore above is deliberately
                            // NOT skipped for a decline — leaving a stranded
                            // `loom:building` would trade this bug for the
                            // exact bug #3823b fixed, and would hide the
                            // issue from `loom-recover-orphans` too. Instead
                            // the forge stays honest (`loom:issue`, which is
                            // what the issue *is*) and the brake lives in the
                            // daemon: an armed decline window the work
                            // finder's `declined-skip` filter honors, plus a
                            // consecutive tally that WARNs at the configured
                            // threshold naming the issue and the rule.
                            //
                            // The `None` arm (gates above were false — a
                            // crash, a produced PR, or a superseded claim,
                            // none of which ran the probe at all) is what
                            // keeps the ordinary #3823b self-skip / no-work
                            // exit unaffected: it clears any stale record
                            // instead of arming one, so such an exit restores
                            // `loom:issue` and is immediately dispatchable
                            // next tick exactly as before.
                            //
                            // `Unknown` (Issue #7553) is deliberately NOT
                            // folded into that same clear: the probe DID run
                            // here (unlike the `None` case) but could not
                            // reach a confirmed verdict, which is the
                            // opposite of the positive evidence
                            // `clear_decline_cooldown` requires. Leave any
                            // existing decline-cooldown record exactly as it
                            // is and let a future tick's probe decide.
                            match declined_rule {
                                Some(HardExclusionProbe::Excluded(rule)) => {
                                    self.record_decline(issue, rule);
                                }
                                Some(HardExclusionProbe::NotExcluded) | None => {
                                    self.clear_decline_cooldown(issue);
                                }
                                Some(HardExclusionProbe::Unknown) => {}
                            }
                            if let Some(info) = self.entries.get_mut(&sweep_id) {
                                info.state = SweepState::Exited {
                                    code: exit_code,
                                    at: now,
                                };
                            }
                            // No-progress backstop (#4366): a headless child
                            // that ends its turn parked on a monitored
                            // background task (e.g. "cache download is
                            // running... I'll pick this back up") exits 0 with
                            // NO checkpoint and NO forward lifecycle progress
                            // whatsoever. That shape is indistinguishable from
                            // the legitimate #3823b self-skip / no-work exit
                            // by exit code alone, so it must be conjunctive:
                            // clean exit AND no open linked PR (excludes the
                            // #4123 open-PR self-skip) AND the issue is still
                            // open (excludes a legitimate curator
                            // close-as-not-planned / already-done self-skip).
                            // Gated on `!skip_label_flip` like every other
                            // real-forge probe in this branch (the resume-path
                            // open-PR check above, `restore_label_to_ready`) —
                            // test fixtures without `gh` credentials never pay
                            // for a forge round trip, and this path stays a
                            // pure no-op with `no_progress` defaulting to
                            // `false` (byte-identical to pre-#4366 behavior)
                            // whenever label-flipping itself is disabled.
                            //
                            // Both forge probes below are FAIL-OPEN, so each arm
                            // demands a POSITIVE verdict rather than accepting the
                            // "probe failed" state:
                            //
                            // - The issue-state arm demands `== Some(false)` ("the
                            //   issue is verifiably OPEN") rather than the weaker
                            //   `!= Some(true)`: a timed-out / rate-limited `gh`
                            //   probe returns `None`, and `None != Some(true)`
                            //   would have been *satisfied*, turning a benign
                            //   self-skip into a counted failed attempt and
                            //   wrongly quarantining an issue during a forge
                            //   outage.
                            // - The open-PR arm (#4452) demands a VERIFIED
                            //   `OpenPrProbe::NoneOpen` rather than the old
                            //   `Option::is_none()`, which conflated "no open
                            //   linked PR" with "the PR probe itself failed". That
                            //   conflation meant a PARTIAL outage (PR probe fails
                            //   while the issue probe answers OPEN) could still
                            //   false-positive; matching `NoneOpen` closes that
                            //   gap — a `ProbeFailed` yields `no_progress = false`.
                            //
                            // Consequently a probe failure on EITHER arm — and a
                            // fortiori a full forge outage — yields
                            // `no_progress == false` (the pre-#4366 behavior), so
                            // an outage can never manufacture quarantine pressure.
                            //
                            // The PR probe is hoisted into its own binding (#6350)
                            // so the `Open(_)` verdict — a legitimate #4123
                            // existing-PR self-skip, deliberately EXEMPT from the
                            // no-progress/quarantine tally below — can still be
                            // read separately by `yielded_open_pr` just below,
                            // without paying for a second forge round trip.
                            let open_pr_probe =
                                if !self.config.skip_label_flip && exit_code == Some(0) {
                                    Some(self.probe_open_linked_pr(issue))
                                } else {
                                    None
                                };
                            let no_progress = open_pr_probe == Some(OpenPrProbe::NoneOpen)
                                && self.issue_is_closed_or_pr(issue) == Some(false);
                            // #6350: a clean exit whose self-skip was a VERIFIED
                            // open linked PR (the #4123 guard's own signature) is
                            // deliberately exempt from `no_progress` above — see
                            // `reaper_open_linked_pr_exempts_clean_exit_from_no_progress`
                            // — because it is legitimate behavior, not a bug.
                            // But "legitimate" is not "free": the live #994
                            // incident (Issue #6350) showed a work-finder tick
                            // cadence re-dispatching such an issue NINE times
                            // before its lease-holding host's sweep finally
                            // yielded on arrival, each attempt burning a spawn, a
                            // token, and a lease comment. Arming the SAME per-issue
                            // dispatch backoff (#4485) as a real failure — without
                            // touching the quarantine tally, which stays exempt —
                            // damps that redispatch rate on this host without
                            // conflating "issue is broken" (quarantine) with
                            // "issue already has an owner elsewhere, retry later"
                            // (backoff).
                            let yielded_open_pr =
                                matches!(open_pr_probe, Some(OpenPrProbe::Open(_)));
                            // Insta-crash quarantine (#3939): a checkpoint-less
                            // death inside the insta-crash window that did NOT
                            // exit cleanly (exit_code != 0, or an unknown
                            // signal-death) never reached real work — the #3938
                            // "missing token pool / import failure" case. Count it
                            // toward quarantine. A clean exit (code 0 — the
                            // legitimate self-skip / no-work path) or a slow death
                            // past the window resets the tally instead.
                            //
                            // Hoisted so the #4386 pre-flight classification below
                            // can consult the same window bool the tally uses —
                            // this branch has no checkpoint at all, so (unlike the
                            // Crashed branch above) there is no "genuine progress"
                            // carve-out to check first.
                            let insta_crash = duration_sec
                                < self.quarantine_config.insta_crash_secs
                                && exit_code != Some(0);
                            let death_class = self.record_preflight_streak(&sweep_id, insta_crash);
                            let is_preflight_death = death_class.is_some();
                            // Issue #5697: this checkpoint-less branch never
                            // consulted `classify_crash` for an account-exhaustion
                            // signature (unlike the Crashed branch above, which
                            // computed `classification` for the bus event). A
                            // credit/plan-exhausted death can land here too (a
                            // wave builder killed before ever writing a
                            // checkpoint), so compute the same best-effort
                            // classification here for the durable outcome
                            // journal's `crash_classification` field.
                            let classification = self
                                .entries
                                .get(&sweep_id)
                                .map(|i| i.log_path.clone())
                                .and_then(|p| tail_lines(&p, EXHAUSTION_LOG_TAIL_LINES).ok())
                                .map(|lines| lines.join("\n"))
                                .and_then(|tail| classify_crash(&tail, exit_code));
                            // #7708: captured before `classification` moves
                            // into the outcome journal below — see this
                            // branch's use further down. The exit-78
                            // token-selection death lands HERE (no checkpoint
                            // was ever written) at least as often as in the
                            // crashed branch above, so both need the carve-out.
                            let pool_dead =
                                classification.as_deref() == Some(NO_USABLE_ACCOUNT_CLASS);
                            // Telemetry `result` classification (#4704),
                            // strongest signal first:
                            //   1. An observed `merge-done` means the sweep
                            //      merged — the schema's `Success` — even when
                            //      no exit code was captured (the kill-probe
                            //      path yields none).
                            //   2. `no_progress` (computed above) flags the
                            //      pathological "clean exit, zero forward
                            //      progress" shape as a failure.
                            //   3. Otherwise a verified clean exit (code 0) is
                            //      the best success signal available without an
                            //      extra forge round trip (a self-skip or a
                            //      completed run); anything else — including an
                            //      UNKNOWN exit status — is a failure, since an
                            //      unobservable exit is not evidence of
                            //      success.
                            let telemetry_result = if self.sampled_reached_merge(&sweep_id) {
                                telemetry::SweepResult::Success
                            } else if no_progress {
                                telemetry::SweepResult::Failure
                            } else if exit_code == Some(0) {
                                telemetry::SweepResult::Success
                            } else {
                                telemetry::SweepResult::Failure
                            };
                            // Durable terminal-outcome record (Issue #4644),
                            // BEFORE the bus emission below moves
                            // `death_class` — see the sibling call in the
                            // Crashed branch above for the rationale.
                            self.append_outcome_journal(
                                issue,
                                &sweep_id,
                                "exited",
                                exit_code,
                                death_class.clone(),
                                classification,
                                duration_sec,
                                telemetry_result,
                            );
                            events_to_emit.push(Event::SweepExited {
                                issue,
                                exit_code,
                                duration_sec,
                                no_progress,
                                death_class,
                                repo: None, // stamped by emit_event (#3929)
                            });
                            events_to_emit.push(Event::SweepGlobalCompleted {
                                sweep_id: sweep_id.clone(),
                                outcome: SweepOutcome::Exited,
                            });
                            // #4485: same rate cap as the crashed branch above,
                            // evaluated before the #4386 pre-flight carve-out so
                            // a pre-flight death still bounds its own retry
                            // cadence. `insta_crash` (fast non-zero death, e.g.
                            // the exit-78 empty-token-pool shape) and
                            // `no_progress` (#4366 clean exit that advanced
                            // nothing) are both failures; `yielded_open_pr`
                            // (#6350) is a legitimate self-skip that is still a
                            // no-*state-change* outcome for redispatch-rate
                            // purposes, so it arms the same window without
                            // joining the quarantine tally below. Only a
                            // genuinely productive exit clears the window.
                            //
                            // #7708: a `no-usable-account` death is exempt
                            // from BOTH arms — it neither arms the ladder (a
                            // pool-wide fault is not the issue's fault, and a
                            // per-issue ladder cannot damp it) nor clears it
                            // (this dispatch proved nothing about the issue,
                            // so an already-armed window must survive). The
                            // host-level pool hold is armed instead; see the
                            // sibling carve-out in the crashed branch above.
                            if pool_dead {
                                crate::work_finder::pool_preflight::note_pool_dead(
                                    &self.config.workspace_root,
                                );
                            } else if insta_crash || no_progress || yielded_open_pr {
                                self.record_dispatch_failure(issue);
                            } else {
                                self.clear_dispatch_backoff(issue);
                            }
                            if !is_preflight_death {
                                // #4366: a separate predicate arm from the
                                // insta-crash window/exit-code check above — a
                                // clean exit 0 with zero lifecycle progress
                                // (`no_progress`) is ALSO a failed attempt, just
                                // a different failure shape (parked-on-monitor
                                // rather than a fast crash). Without this, such
                                // exits fell through to `insta_crash == false`,
                                // which *resets* the tally via
                                // `record_terminal_outcome`, so a repeatedly
                                // parking sweep never quarantines and churns the
                                // dispatch queue forever. Does not touch the
                                // insta-crash window or its `exit_code !=
                                // Some(0)` condition above — this ORs in the new
                                // verdict as a second, independent reason to
                                // count the attempt as failed. A `no_progress`
                                // exit is always exit 0, so `insta_crash` is
                                // false and `death_class` is `None` (#4386's
                                // pre-flight classifier only fires on
                                // `insta_crash`), i.e. this arm is never skipped
                                // by the `is_preflight_death` carve-out.
                                let counted_failure = insta_crash || no_progress;
                                // #4122: re-attribute account-exhaustion deaths to
                                // the spawn account instead of the issue.
                                // #4386: a pre-flight-classified death must not
                                // charge the issue's quarantine tally either (same
                                // carve-out reasoning as exhaustion) — skipped
                                // entirely here. The exhaustion case itself is
                                // NOT skipped (`PreflightOutcome::Unknown` always
                                // yields a `None` death_class, so exhaustion still
                                // reaches — and is handled inside —
                                // `record_insta_crash_outcome`).
                                self.record_insta_crash_outcome(&sweep_id, issue, counted_failure);
                            }
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
                                    repo: None, // stamped by emit_event (#3929)
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
                        // Issue #5342: release each PR-set member's claim lock
                        // now that this sweep is confirmed dead — otherwise
                        // every PR in the set stays locked forever (dispatch
                        // never wrote a machine-level journal entry for a
                        // `PrSet` sweep to prune, so this is the only cleanup
                        // path). No checkpoint/quarantine/outcome-journal
                        // bookkeeping applies here: `PrSet` drives Judge/
                        // Doctor/Merge against PRs a Builder already opened,
                        // not a fresh issue claim, so none of that per-issue
                        // machinery has a coherent PrSet analogue yet.
                        if let SweepKind::PrSet(prs) = &kind {
                            for pr in prs {
                                let _ = self.release_pr_lock_owned(*pr, &sweep_id);
                            }
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
            // Prune the per-SweepId progress latch so it cannot grow unbounded
            // across many dispatches (Issue #4088). Safe because a GC'd entry is
            // terminal — the watchdog only ever consults the latch for
            // Running/Pending entries it still owns a Child handle for.
            self.watchdog_progressed.remove(&id);
            // Prune the per-SweepId phase-transition history for the same
            // reason (Issue #4704): its only consumer is the durable
            // `sweep.outcome` record, which was already written at this
            // entry's terminal transition an hour ago.
            self.phase_history.remove(&id);
            // Prune the per-SweepId opportunistic LOC snapshot for the same
            // reason (Issue #5357): its only consumer is the durable
            // `sweep.outcome` record, already written at this entry's
            // terminal transition an hour ago.
            self.sampled_loc.remove(&id);
            changes += 1;
        }
        ReapOnceOutcome {
            changes,
            pending_resumes,
        }
    }

    /// Shared bookkeeping for a reaper-driven resume dispatch's (#4256)
    /// outcome: logs the attempt (info on success, warn on failure) and
    /// appends the `SweepResumeDispatched` event. Used by both
    /// `reap_once_impl`'s inline resume-dispatch path and
    /// [`reap_once_releasing_poll_lock`]'s deferred one (Issue #6691) so
    /// the observable behavior — log text and event payload — is
    /// byte-identical regardless of which one actually ran the poll.
    fn log_and_emit_resume_result(
        &self,
        issue: u32,
        pr: u32,
        attempt_no: u32,
        resume_phase_check: Option<String>,
        result: Result<DispatchOutcome>,
        events_to_emit: &mut Vec<Event>,
    ) {
        let dispatched = match &result {
            Ok(_) => {
                log::info!(
                    "issue #{issue}: reaper-driven resume dispatched (attempt \
                     {attempt_no}/{MAX_RESUME_ATTEMPTS}, crashed at checkpoint phase \
                     {resume_phase_check:?}, open PR #{pr}) — #4256"
                );
                true
            }
            Err(e) => {
                log::warn!(
                    "issue #{issue}: reaper-driven resume dispatch failed (attempt \
                     {attempt_no}/{MAX_RESUME_ATTEMPTS}, crashed at checkpoint phase \
                     {resume_phase_check:?}, open PR #{pr}): {e} — #4256"
                );
                false
            }
        };
        events_to_emit.push(Event::SweepResumeDispatched {
            issue,
            pr,
            checkpoint_phase: resume_phase_check,
            dispatched,
            repo: None, // stamped by emit_event (#3929)
        });
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
}

/// Test-only synchronization primitive (Issue #6712) for
/// [`reap_once_releasing_poll_lock`]'s guard-chain-complete signal. See the
/// call site's doc comment for why this exists instead of a fixed sleep.
///
/// A single process-wide slot: any test installing a hook via
/// [`test_hooks::set_entering_unlocked_poll_hook`] must be `#[serial]`
/// (matching this file's existing convention for other shared-state tests)
/// and must clear it (a `Drop` guard is the simplest way) so it doesn't leak
/// into an unrelated later test.
#[cfg(test)]
pub(crate) mod test_hooks {
    use std::sync::mpsc::Sender;
    use std::sync::Mutex;

    static ENTERING_UNLOCKED_POLL: Mutex<Option<Sender<()>>> = Mutex::new(None);

    pub(crate) fn set_entering_unlocked_poll_hook(tx: Sender<()>) {
        *ENTERING_UNLOCKED_POLL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tx);
    }

    pub(crate) fn clear_entering_unlocked_poll_hook() {
        *ENTERING_UNLOCKED_POLL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    pub(crate) fn fire_entering_unlocked_poll() {
        let guard = ENTERING_UNLOCKED_POLL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(tx) = guard.as_ref() {
            // Best-effort: a receiver that already dropped (test cleanup
            // raced the hook, or no hook installed) is not this function's
            // problem to report.
            let _ = tx.send(());
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
