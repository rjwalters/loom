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
                    Ok(mut r) => {
                        let changed = r.reap_once();
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
                        changed
                    }
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
    #[allow(clippy::too_many_lines)]
    pub fn reap_once(&mut self) -> usize {
        let mut changes = 0usize;

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
                                        let dispatched =
                                            match self.dispatch_resume_after_crash(issue, pr) {
                                                Ok(_) => {
                                                    log::info!(
                                                        "issue #{issue}: reaper-driven resume \
                                                         dispatched (attempt \
                                                         {attempt_no}/{MAX_RESUME_ATTEMPTS}, \
                                                         crashed at checkpoint phase \
                                                         {resume_phase_check:?}, open PR #{pr}) \
                                                         — #4256"
                                                    );
                                                    true
                                                }
                                                Err(e) => {
                                                    log::warn!(
                                                        "issue #{issue}: reaper-driven resume \
                                                         dispatch failed (attempt \
                                                         {attempt_no}/{MAX_RESUME_ATTEMPTS}, \
                                                         crashed at checkpoint phase \
                                                         {resume_phase_check:?}, open PR #{pr}): \
                                                         {e} — #4256"
                                                    );
                                                    false
                                                }
                                            };
                                        events_to_emit.push(Event::SweepResumeDispatched {
                                            issue,
                                            pr,
                                            checkpoint_phase: resume_phase_check.clone(),
                                            dispatched,
                                            repo: None, // stamped by emit_event (#3929)
                                        });
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
                            // #4463: skip the restore when a newer sweep owns
                            // the lock (superseded) — it holds the live claim.
                            if !self.config.skip_label_flip && !produced_pr && !superseded {
                                let _ = self.restore_label_to_ready(issue);
                                self.note_label_flip(issue); // #4485 flap detection
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
                            let no_progress = !self.config.skip_label_flip
                                && exit_code == Some(0)
                                && self.probe_open_linked_pr(issue) == OpenPrProbe::NoneOpen
                                && self.issue_is_closed_or_pr(issue) == Some(false);
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
                            // nothing) are both failures; a genuinely productive
                            // exit clears the window.
                            if insta_crash || no_progress {
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

    /// Issue #4111 (the consumer-side regression this issue is really about):
    /// a checkpoint-less, clean (`exit 0`) sweep death for a daemon-dispatched
    /// self-claim check — i.e. exactly the "deliberate skip" shape the #3939
    /// insta-crash guard is SUPPOSED to exempt — must NOT increment the
    /// insta-crash tally when the reaper retains the real `Child` handle
    /// (poll_liveness observes `exit_code == Some(0)`, not the `None`
    /// fallback the other insta-crash fixtures in this file simulate via a
    /// dead/unretained PID). This exercises the reaper's `exit_code !=
    /// Some(0)` guard (`sweep_registry.rs`) against a REAL spawned process
    /// rather than a synthetic dead-PID fixture, closing the gap the issue's
    /// Finding 1 flagged: every other insta-crash test in this file only ever
    /// observes `exit_code = None` (no retained handle), so a real Some(0)
    /// exit was never actually exercised before.
    #[test]
    fn reaper_real_clean_exit_does_not_count_as_insta_crash() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        // A real, fast, clean-exit child (mirrors what a #4111 self-skip
        // looks like on the wire: no checkpoint written, exits 0 quickly).
        let child = Command::new("true")
            .spawn()
            .expect("spawn `true` fixture child");
        let pid = child.id();
        // Give the OS a moment to actually finish the process before we poll.
        std::thread::sleep(Duration::from_millis(50));

        let sweep_id = "sweep-issue-4111-clean-exit".to_string();
        registry.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                pgid: None,
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(41_110),
                pid,
                token_name: "unknown".into(),
                runtime: "unknown".into(),
                runtime_source: None,
                log_path: registry.compute_log_path(41_110),
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
        // Retain the handle (mirrors `dispatch()`'s `self.children.insert`) so
        // `poll_liveness` uses the real exit code instead of the no-handle
        // `kill(pid, 0)` fallback that always yields `exit_code = None`.
        registry.children.insert(sweep_id.clone(), child);

        registry.reap_once();

        assert_eq!(
            registry.insta_crash_count(41_110),
            0,
            "a real, handle-observed clean (exit 0) death must not count toward quarantine"
        );
        assert!(
            !registry.is_quarantined(41_110),
            "a single clean exit must never quarantine the issue"
        );
        let info = registry.get(&sweep_id).unwrap();
        assert!(
            matches!(info.state, SweepState::Exited { code: Some(0), .. }),
            "expected an Exited{{code: Some(0)}} terminal state; got: {:?}",
            info.state
        );
    }

    /// AC: exit 0 + no checkpoint + no open linked PR + issue open → counted
    /// as a failed attempt, and 3 consecutive occurrences quarantine exactly
    /// like 3 consecutive insta-crashes would — the whole point of the
    /// backstop is that this failure shape must NOT be exempt from the tally
    /// just because the exit code was 0.
    #[test]
    fn reaper_counts_no_progress_clean_exit_and_quarantines_at_threshold() {
        let dir = tempdir().unwrap();
        let mut registry = no_progress_test_registry(dir.path(), "OPEN", "", false);
        assert_eq!(registry.quarantine_config().threshold, 3);

        for seq in 0..3 {
            insert_clean_exit_running(&mut registry, 43_660, seq);
            let changed = registry.reap_once();
            assert!(changed >= 1, "reap_once should observe the dead fixture child");
        }

        assert_eq!(
            registry.insta_crash_count(43_660),
            3,
            "3 consecutive no-progress clean exits must accrue exactly like insta-crashes"
        );
        assert!(
            registry.is_quarantined(43_660),
            "3rd consecutive no-progress exit must quarantine the issue"
        );
    }

    /// AC: exit 0 with an open linked PR is NOT counted — the #4123 open-PR
    /// dispatch-guard self-skip is a legitimate zero-progress-this-run
    /// outcome, not a parked-on-monitor failure.
    #[test]
    fn reaper_open_linked_pr_exempts_clean_exit_from_no_progress() {
        let dir = tempdir().unwrap();
        let mut registry = no_progress_test_registry(dir.path(), "OPEN", "4400", false);

        insert_clean_exit_running(&mut registry, 43_661, 0);
        registry.reap_once();

        assert_eq!(
            registry.insta_crash_count(43_661),
            0,
            "a clean exit with an open linked PR must not count toward quarantine"
        );
        assert!(!registry.is_quarantined(43_661));
    }

    /// AC: exit 0 with the issue already closed is NOT counted — a legitimate
    /// curator close-as-not-planned (or already-done) self-skip is a valid
    /// zero-PR, zero-checkpoint outcome.
    #[test]
    fn reaper_closed_issue_exempts_clean_exit_from_no_progress() {
        let dir = tempdir().unwrap();
        let mut registry = no_progress_test_registry(dir.path(), "CLOSED", "", false);

        insert_clean_exit_running(&mut registry, 43_662, 0);
        registry.reap_once();

        assert_eq!(
            registry.insta_crash_count(43_662),
            0,
            "a clean exit on an already-closed issue must not count toward quarantine"
        );
        assert!(!registry.is_quarantined(43_662));
    }

    /// AC (PR #4408 judge feedback): the no-progress predicate must FAIL OPEN
    /// when the forge probes themselves fail. [`Self::issue_is_closed_or_pr`] returns
    /// `None` on a missing/failed/timed-out/unparseable `gh` answer and its
    /// contract says callers MUST treat that as "don't punish" — the original
    /// `!= Some(true)` spelling was *satisfied* by `None`, so a rate-limited or
    /// timed-out probe during a forge outage silently converted every benign
    /// self-skip into a counted failed attempt and wrongly quarantined the
    /// issue. Requiring a positive `== Some(false)` ("the issue is verifiably
    /// open") verdict means a probe failure yields `no_progress == false`.
    ///
    /// The fixture answers `issue view` with an unparseable `"WEDGED"` state
    /// (the same shape a truncated/garbled `gh` response has), which makes
    /// `issue_is_closed_or_pr` return `None`. Three consecutive clean exits under
    /// that condition — enough to trip the quarantine threshold if any of them
    /// counted — must leave the tally at 0 and the issue un-quarantined.
    #[test]
    fn reaper_probe_failure_fails_open_and_does_not_count_no_progress() {
        let dir = tempdir().unwrap();
        let mut registry = no_progress_test_registry(dir.path(), "WEDGED", "", false);
        assert_eq!(registry.quarantine_config().threshold, 3);

        for seq in 0..3 {
            insert_clean_exit_running(&mut registry, 43_665, seq);
            let changed = registry.reap_once();
            assert!(changed >= 1, "reap_once should observe the dead fixture child");
        }

        assert_eq!(
            registry.insta_crash_count(43_665),
            0,
            "an unresolvable issue-state probe must fail open — a clean exit during a forge \
             outage must never accrue toward quarantine"
        );
        assert!(
            !registry.is_quarantined(43_665),
            "3 consecutive probe-failure clean exits must not quarantine the issue"
        );
    }

    /// #4452 regression: a PARTIAL forge outage — the open-linked-PR probe fails
    /// (`api graphql` exits non-zero) while `issue view` still answers OPEN —
    /// must NOT count a clean exit toward quarantine. The old `Option<u32>`
    /// return conflated "the PR probe failed" with "verified no open PR", so
    /// `first_open_linked_pr(issue).is_none()` was satisfied and the predicate
    /// wrongly fired. With the three-state [`OpenPrProbe`], the predicate now
    /// requires a VERIFIED `NoneOpen`, so a `ProbeFailed` yields
    /// `no_progress == false`. Three consecutive clean exits under this
    /// condition — enough to trip the threshold if any counted — must leave the
    /// tally at 0 and the issue un-quarantined.
    #[test]
    fn reaper_pr_probe_failure_with_open_issue_fails_open_and_does_not_count() {
        let dir = tempdir().unwrap();
        let mut registry = no_progress_pr_probe_fail_registry(dir.path(), "OPEN");
        assert_eq!(registry.quarantine_config().threshold, 3);

        for seq in 0..3 {
            insert_clean_exit_running(&mut registry, 43_666, seq);
            let changed = registry.reap_once();
            assert!(changed >= 1, "reap_once should observe the dead fixture child");
        }

        assert_eq!(
            registry.insta_crash_count(43_666),
            0,
            "a PR-probe failure (partial outage) with the issue still OPEN must fail open — a \
             clean exit must never accrue toward quarantine (#4452)"
        );
        assert!(
            !registry.is_quarantined(43_666),
            "3 consecutive PR-probe-failure clean exits must not quarantine the issue (#4452)"
        );
    }

    /// AC: `skip_label_flip` disables the whole no-progress probe (no `gh`
    /// call at all, matching every other real-forge probe in this branch) —
    /// a clean exit under `skip_label_flip` never counts, regardless of what
    /// the (unconsulted) forge state would have been. Uses the plain
    /// `fixture_registry` (no `gh_bin` configured at all) so a regression
    /// that removed the gate would fail loudly (an unconfigured `gh` binary
    /// erroring out, not silently answering "no PR / open issue").
    #[test]
    fn reaper_no_progress_probe_is_noop_under_skip_label_flip() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        assert!(registry.config.skip_label_flip, "fixture_registry sets skip_label_flip = true");

        let sweep_id = insert_clean_exit_running(&mut registry, 43_663, 0);
        registry.reap_once();

        assert_eq!(
            registry.insta_crash_count(43_663),
            0,
            "skip_label_flip must disable the no-progress probe entirely"
        );
        assert!(!registry.is_quarantined(43_663));
        let info = registry.get(&sweep_id).unwrap();
        assert!(matches!(info.state, SweepState::Exited { code: Some(0), .. }));
    }

    /// AC: the `SweepExited` event carries `no_progress: true` for the
    /// failure shape and `no_progress: false` for the exempted shapes, so
    /// operators (and #4137 durable telemetry) can distinguish the failure
    /// class without re-deriving it from `exit_code` + `duration_sec` alone.
    #[tokio::test]
    async fn reaper_sweep_exited_event_carries_no_progress_classification() {
        use crate::event_bus::EventBus;

        let dir = tempdir().unwrap();
        let mut registry = no_progress_test_registry(dir.path(), "OPEN", "", false);
        let bus = Arc::new(EventBus::new());
        registry.set_event_bus(bus.clone());
        let mut sub = bus.subscribe::<[&str; 0], &str>([]);

        insert_clean_exit_running(&mut registry, 43_664, 0);
        registry.reap_once();

        let mut saw_no_progress_true = false;
        for _ in 0..4 {
            let Ok(ev) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await else {
                break;
            };
            if let Event::SweepExited {
                issue, no_progress, ..
            } = ev.unwrap()
            {
                assert_eq!(issue, 43_664);
                assert!(
                    no_progress,
                    "expected no_progress=true for a parked-on-monitor clean exit"
                );
                saw_no_progress_true = true;
            }
        }
        assert!(saw_no_progress_true, "expected a sweep.issue.43664.exited event");
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
                pgid: None,
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(55),
                pid: 2_147_483_640,
                token_name: "unknown".into(),
                runtime: "unknown".into(),
                runtime_source: None,
                log_path: registry.compute_log_path(55),
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
                    ..
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

    /// Issue #4255: the reaper's `sweep.issue.{N}.crashed` payload carries a
    /// best-effort error classification derived from the dead sweep's log tail,
    /// alongside `checkpoint_phase`. A log whose terminal output is the chronic
    /// `Execution error` string is labeled `execution-error`.
    #[tokio::test]
    async fn reaper_crashed_event_carries_classification() {
        use crate::event_bus::EventBus;

        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        let bus = Arc::new(EventBus::new());
        registry.set_event_bus(bus.clone());
        let mut sub = bus.subscribe::<[&str; 0], &str>([]);

        let cp_dir = registry.config.checkpoint_dir();
        std::fs::create_dir_all(&cp_dir).unwrap();
        std::fs::write(cp_dir.join("issue-57.json"), r#"{"phase":"builder","issue":57}"#).unwrap();

        // Write a sweep log whose terminal line is the bare `Execution error`
        // fatal — the exact death mode this issue targets.
        let log_path = registry.compute_log_path(57);
        std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        std::fs::write(&log_path, "spawn-claude: preamble\nExecution error\n").unwrap();

        let sweep_id = "sweep-issue-57-test".to_string();
        registry.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                pgid: None,
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(57),
                pid: 2_147_483_641,
                token_name: "unknown".into(),
                runtime: "unknown".into(),
                runtime_source: None,
                log_path,
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

        let changed = registry.reap_once();
        assert!(changed >= 1);

        let mut saw_classified = false;
        for _ in 0..2 {
            let ev = sub.recv().await.unwrap();
            if let Event::SweepCrashed {
                issue,
                checkpoint_phase,
                classification,
                ..
            } = ev
            {
                assert_eq!(issue, 57);
                assert_eq!(checkpoint_phase.as_deref(), Some("builder"));
                assert_eq!(
                    classification.as_deref(),
                    Some("execution-error"),
                    "expected the crashed event to carry the execution-error classification"
                );
                saw_classified = true;
            }
        }
        assert!(saw_classified, "expected a classified sweep.issue.57.crashed event");
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
                pgid: None,
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(66),
                pid: 2_147_483_640,
                token_name: "unknown".into(),
                runtime: "unknown".into(),
                runtime_source: None,
                log_path: registry.compute_log_path(66),
                idempotency_key: None,
                started_at: Utc::now() - chrono::Duration::seconds(10),
                state: SweepState::Running,
                latest_phase: None,
                pr_number: None,
                model: None,
                effort: None,
                depends_on: None,
                repo: None,
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
                pgid: None,
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(21),
                pid: 2_147_483_640, // ~i32::MAX, almost certainly dead
                token_name: "unknown".into(),
                runtime: "unknown".into(),
                runtime_source: None,
                log_path: registry.compute_log_path(21),
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

        let changed = registry.reap_once();
        assert!(changed >= 1);
        let info = registry.get(&sweep_id).unwrap();
        assert!(matches!(info.state, SweepState::Exited { .. }));
    }

    /// The reaper-driven resume path (#4256) is exempt: it re-dispatches an
    /// issue's OWN open PR and is already bounded by `MAX_RESUME_ATTEMPTS`, so the
    /// backoff must not block it (which would strand a PR at review).
    #[test]
    fn reaper_resume_dispatch_bypasses_the_backoff() {
        let dir = tempdir().unwrap();
        let mut registry = backoff_registry(dir.path(), 60, 900);
        registry.record_dispatch_failure(35);

        assert!(
            registry
                .dispatch(&SweepKind::Issue(35), None, None, None, None)
                .is_err(),
            "an ordinary dispatch is refused"
        );
        assert!(
            registry.dispatch_resume_after_crash(35, 777).is_ok(),
            "the bounded #4256 resume path is exempt"
        );
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
                pgid: None,
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(77),
                pid: 2_147_483_640, // ~i32::MAX, almost certainly dead
                token_name: "unknown".into(),
                runtime: "unknown".into(),
                runtime_source: None,
                log_path: registry.compute_log_path(77),
                idempotency_key: None,
                started_at: Utc::now(),
                state: SweepState::Running,
                latest_phase: None,
                pr_number: None, // no PR produced -> recoverable claim
                model: None,
                effort: None,
                depends_on: None,
                repo: None,
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
                pgid: None,
                sweep_id: sweep_id.clone(),
                kind: kind.clone(),
                pid: 2_147_483_640, // ~i32::MAX, almost certainly dead
                token_name: "unknown".into(),
                runtime: "unknown".into(),
                runtime_source: None,
                log_path: registry.compute_log_path(88),
                idempotency_key: None,
                started_at,
                state: SweepState::Running,
                latest_phase: None,
                pr_number: None, // no PR produced -> recoverable claim
                model: None,
                effort: None,
                depends_on: None,
                repo: None,
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
                pgid: None,
                sweep_id: sweep_id.clone(),
                kind: kind.clone(),
                pid: 2_147_483_640,
                token_name: "unknown".into(),
                runtime: "unknown".into(),
                runtime_source: None,
                log_path: registry.compute_log_path(99),
                idempotency_key: None,
                started_at,
                state: SweepState::Running,
                latest_phase: None,
                pr_number: Some(456), // PR opened -> must NOT reset the label
                model: None,
                effort: None,
                depends_on: None,
                repo: None,
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
                pgid: None,
                sweep_id: sweep_id.clone(),
                kind: kind.clone(),
                pid: 2_147_483_640,
                token_name: "unknown".into(),
                runtime: "unknown".into(),
                runtime_source: None,
                log_path: registry.compute_log_path(0),
                idempotency_key: None,
                started_at,
                state: SweepState::Running,
                latest_phase: None,
                pr_number: None,
                model: None,
                effort: None,
                depends_on: None,
                repo: None,
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
                pgid: None,
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(33),
                pid: 2_147_483_640,
                token_name: "unknown".into(),
                runtime: "unknown".into(),
                runtime_source: None,
                log_path: registry.compute_log_path(33),
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

        let changed = registry.reap_once();
        assert!(changed >= 1);
        let info = registry.get(&sweep_id).unwrap();
        assert!(matches!(info.state, SweepState::Crashed { .. }));
    }

    /// Regression (Test Plan item 3): `finish_cancel` on a stale entry whose
    /// issue lock is owned by a NEWER live sweep must leave that lock intact AND
    /// must not restore the label out from under the newer sweep.
    #[test]
    fn cancel_preserves_lock_and_skips_restore_when_superseded() {
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
        config.skip_label_flip = false; // real restore path enabled but must NOT fire
        let mut registry = SweepRegistry::new(config);

        // A newer live sweep owns the lock.
        let lock = write_lock_owner(&registry, 8801, "sweep-issue-8801-newer", std::process::id());

        // The OLDER sweep being cancelled.
        let kind = SweepKind::Issue(8801);
        let started_at = Utc::now();
        let sweep_id = "sweep-issue-8801-older".to_string();
        registry.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                pgid: None,
                sweep_id: sweep_id.clone(),
                kind: kind.clone(),
                pid: 2_147_483_640,
                token_name: "unknown".into(),
                runtime: "unknown".into(),
                runtime_source: None,
                log_path: registry.compute_log_path(8801),
                idempotency_key: None,
                started_at,
                state: SweepState::Running,
                latest_phase: None,
                pr_number: None,
                model: None,
                effort: None,
                depends_on: None,
                repo: None,
            },
        );

        let outcome = registry.finish_cancel(&sweep_id, 2_147_483_640, &kind, started_at, true);
        assert!(outcome.was_running);

        // The newer sweep's lock survives untouched.
        assert!(lock.exists(), "cancelling an older sweep must not free the newer sweep's lock");
        let owner: LockOwner =
            serde_json::from_str(&std::fs::read_to_string(lock.join("owner.json")).unwrap())
                .unwrap();
        assert_eq!(owner.sweep_id, "sweep-issue-8801-newer");

        // The label must NOT be restored — the newer sweep still holds the claim.
        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            !gh_calls.contains("--remove-label loom:building"),
            "a superseded cancel must not restore loom:building -> loom:issue; got: {gh_calls:?}"
        );
    }

    /// Issue #5017/#5282 regression: the LOCAL lock check alone cannot see a
    /// cross-host race — `.loom/locks/issue-<N>` is host-local, so a peer
    /// host's live claim on the same issue leaves the local check believing
    /// nothing else owns the lock (`Released`, not `Superseded`). This test
    /// simulates exactly that: no local lock at all (mirrors the real
    /// incident, where the cancelling host's own `.loom/locks/` never
    /// recorded the OTHER host's claim), but the forge's `loom:building`
    /// labeled-event timeline shows the label was (re-)applied AFTER this
    /// sweep's own `started_at` — the cross-host claim-ownership signal.
    /// `finish_cancel` MUST leave the label alone in that case; restoring it
    /// would repeat the loom#5270 incident (cancelling a losing duplicate
    /// destroyed a live peer host's claim).
    #[test]
    fn cancel_skips_restore_when_forge_shows_a_newer_claim_from_another_host() {
        let dir = tempdir().unwrap();
        let gh_log = dir.path().join("gh-invocations.log");
        let fake_gh = dir.path().join("fake-gh.sh");
        // Any `gh api ... /timeline ...` call answers with a labeling
        // timestamp comfortably AFTER this sweep's `started_at` (set below to
        // `now - 1h`); every other `gh` call (the label-restore edit itself)
        // behaves like the other fixtures: logged, empty stdout, exit 0. A
        // real `restore_label_to_ready` call would show up in the log as a
        // `--remove-label loom:building` invocation, so asserting its absence
        // is sufficient to prove the forge check vetoed the restore.
        let script = format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{}\"\nif [[ \"$1\" == \"api\" ]]; then\n  echo \"\\\"$(date -u '+%Y-%m-%dT%H:%M:%SZ')\\\"\"\nfi\nexit 0\n",
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
        config.skip_label_flip = false; // real restore path enabled but must NOT fire
        let mut registry = SweepRegistry::new(config);

        // No local lock at all — models the cross-host case where this
        // host's `.loom/locks/issue-<N>` never recorded the other host's
        // claim (release_lock_owned would answer `Released`, not
        // `Superseded`, with no forge-side check).
        let kind = SweepKind::Issue(8802);
        let started_at = Utc::now() - chrono::Duration::hours(1);
        let sweep_id = "sweep-issue-8802-loser".to_string();
        registry.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                pgid: None,
                sweep_id: sweep_id.clone(),
                kind: kind.clone(),
                pid: 2_147_483_640,
                token_name: "unknown".into(),
                runtime: "unknown".into(),
                runtime_source: None,
                log_path: registry.compute_log_path(8802),
                idempotency_key: None,
                started_at,
                state: SweepState::Running,
                latest_phase: None,
                pr_number: None,
                model: None,
                effort: None,
                depends_on: None,
                repo: None,
            },
        );

        let outcome = registry.finish_cancel(&sweep_id, 2_147_483_640, &kind, started_at, true);
        assert!(outcome.was_running);

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls.contains("api repos/{owner}/{repo}/issues/8802/timeline"),
            "expected finish_cancel to consult the forge-side claim timeline; got: {gh_calls:?}"
        );
        assert!(
            !gh_calls.contains("--remove-label loom:building"),
            "a cancel whose forge timeline shows a newer claim must NOT restore \
             loom:building -> loom:issue (would destroy a peer host's live claim, #5017/#5282); \
             got gh invocations: {gh_calls:?}"
        );
    }

    /// Issue #5017/#5282: the SAME cross-host claim-ownership check must
    /// apply to the reaper's natural-exit/crash label-restore path
    /// (`reap_once`'s checkpoint-present branch), not just `finish_cancel` —
    /// the Curator's "Suspected Cause (unverified)" flagged this as the
    /// likely-but-unconfirmed second call site by code-path symmetry; this
    /// test confirms it.
    #[test]
    fn reap_skips_restore_when_forge_shows_a_newer_claim_from_another_host() {
        let dir = tempdir().unwrap();
        let (mut registry, gh_log) = fixture_registry(dir.path());
        // Override the fixture's fake `gh` so `gh api .../timeline` answers
        // with a labeling timestamp AFTER `started_at` (set below).
        let fake_gh = dir.path().join("fake-gh-timeline.sh");
        let script = format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{}\"\nif [[ \"$1\" == \"api\" ]]; then\n  echo \"\\\"$(date -u '+%Y-%m-%dT%H:%M:%SZ')\\\"\"\nfi\nexit 0\n",
            gh_log.display()
        );
        std::fs::write(&fake_gh, &script).unwrap();
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();
        registry.config.gh_bin = Some(fake_gh);
        registry.config.skip_label_flip = false;

        // Checkpoint present so the reaper picks the Crashed/label-restore
        // branch under test.
        let cp_dir = registry.config.checkpoint_dir();
        std::fs::create_dir_all(&cp_dir).unwrap();
        std::fs::write(cp_dir.join("issue-8803.json"), r#"{"phase":"builder","issue":8803}"#)
            .unwrap();

        let started_at = Utc::now() - chrono::Duration::hours(1);
        let sweep_id = "sweep-issue-8803-loser".to_string();
        registry.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                pgid: None,
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(8803),
                pid: 2_147_483_640, // near-certainly dead
                token_name: "unknown".into(),
                runtime: "unknown".into(),
                runtime_source: None,
                log_path: registry.compute_log_path(8803),
                idempotency_key: None,
                started_at,
                state: SweepState::Running,
                latest_phase: None,
                pr_number: None,
                model: None,
                effort: None,
                depends_on: None,
                repo: None,
            },
        );

        let changed = registry.reap_once();
        assert!(changed >= 1);

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls.contains("api repos/{owner}/{repo}/issues/8803/timeline"),
            "expected reap_once to consult the forge-side claim timeline; got: {gh_calls:?}"
        );
        assert!(
            !gh_calls.contains("--remove-label loom:building"),
            "a reap whose forge timeline shows a newer claim must NOT restore \
             loom:building -> loom:issue (would destroy a peer host's live claim, #5017/#5282); \
             got gh invocations: {gh_calls:?}"
        );
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
                pgid: None,
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(42),
                pid: 1234,
                token_name: "agent-1.token".into(),
                runtime: "unknown".into(),
                runtime_source: None,
                log_path: registry.compute_log_path(42),
                idempotency_key: None,
                started_at: Utc::now(),
                state: SweepState::Running,
                latest_phase: Some("builder".into()),
                pr_number: None,
                model: None,
                effort: None,
                depends_on: None,
                repo: None,
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
                pgid: None,
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(99),
                pid: 1,
                token_name: "unknown".into(),
                runtime: "unknown".into(),
                runtime_source: None,
                log_path: log_path.clone(),
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
                pgid: None,
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(11),
                pid: 1,
                token_name: "unknown".into(),
                runtime: "unknown".into(),
                runtime_source: None,
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
                repo: None,
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
                pgid: None,
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(22),
                pid: 2_147_483_640, // ~i32::MAX, almost certainly dead
                token_name: "unknown".into(),
                runtime: "unknown".into(),
                runtime_source: None,
                log_path: registry.compute_log_path(22),
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
                pgid: None,
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(77),
                pid,
                token_name: "unknown".into(),
                runtime: "unknown".into(),
                runtime_source: None,
                log_path: registry.compute_log_path(77),
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
                    pgid: None,
                    sweep_id: target.clone(),
                    kind: SweepKind::Issue(880),
                    pid: target_pid,
                    token_name: "unknown".into(),
                    runtime: "unknown".into(),
                    runtime_source: None,
                    log_path: target_log,
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
            reg.entries.insert(
                other.clone(),
                SweepInfo {
                    pgid: None,
                    sweep_id: other.clone(),
                    kind: SweepKind::Issue(881),
                    pid: 2_147_483_640, // ~i32::MAX, harmless dead pid
                    token_name: "unknown".into(),
                    runtime: "unknown".into(),
                    runtime_source: None,
                    log_path: other_log,
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
                pgid: None,
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(88),
                pid: 2_147_483_640,
                token_name: "unknown".into(),
                runtime: "unknown".into(),
                runtime_source: None,
                log_path: registry.compute_log_path(88),
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

        let gc_pid = read_pid_file(&gc_pidfile, FIXTURE_CHILD_WAIT_MS)
            .expect("grandchild pid should be recorded");
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
        assert!(
            wait_until_dead(leader_pid, FIXTURE_CHILD_WAIT_MS),
            "leader still alive after cancel"
        );
        assert!(
            wait_until_dead(gc_pid, FIXTURE_CHILD_WAIT_MS),
            "grandchild survived cancel — group-kill did not reach it (single-PID regression)"
        );

        let info = registry.get(&sweep_id).unwrap();
        assert!(matches!(info.state, SweepState::Exited { .. }));
    }

    /// Issue #4980, the crux regression test: cancelling a **reconstructed**
    /// entry must still tear down the whole process group.
    ///
    /// [`cancel_terminates_whole_process_group_including_grandchild`] above only
    /// proves group-kill works while the *spawning* registry still holds the
    /// in-memory `Child` handle. That was the entire gate on the group path
    /// (`if self.children.contains_key(sweep_id)`), so the two situations an
    /// operator actually hits at 3am both silently degraded to a single-PID kill:
    /// a daemon that has since restarted (this test, via `reconstruct()`), and
    /// **every** `loom-daemon cancel` invocation, which runs in a fresh process
    /// that never held a handle at all.
    ///
    /// The second registry here models both: same workspace, same on-disk lock,
    /// zero retained handles. The grandchild assertion is what fails on a
    /// regression — a single-PID kill leaves it running, which is precisely the
    /// zombie-agent shape of the 2026-08-03 incident.
    #[test]
    #[serial]
    fn cancel_reconstructed_entry_kills_whole_group() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let gc_pidfile = workspace.join("grandchild.pid");

        let script = format!(
            "#!/usr/bin/env bash\nsleep 300 &\necho \"$!\" > \"{gc}\"\nsleep 300\n",
            gc = gc_pidfile.display()
        );
        let mut spawner = lifecycle_registry(workspace, &script);

        let outcome = spawner
            .dispatch(&SweepKind::Issue(4980), None, None, None, None)
            .expect("dispatch should succeed");
        let leader_pid = outcome.pid;
        let sweep_id = outcome.sweep_id.clone();

        let gc_pid = read_pid_file(&gc_pidfile, FIXTURE_CHILD_WAIT_MS)
            .expect("grandchild pid should be recorded");
        assert!(is_pid_alive(leader_pid), "leader should be running post-dispatch");
        assert!(is_pid_alive(gc_pid), "grandchild should be running post-dispatch");

        // The pgid must have been persisted to the claim lock at spawn time —
        // this is what survives the daemon, and the OS cannot re-derive it once
        // the leader dies.
        let owner_path = spawner
            .config
            .locks_dir()
            .join("issue-4980")
            .join("owner.json");
        let owner: LockOwner =
            serde_json::from_str(&std::fs::read_to_string(&owner_path).unwrap()).unwrap();
        assert_eq!(owner.owner_pid, leader_pid);
        assert_eq!(
            owner.pgid,
            Some(leader_pid),
            "owner.json must record the child's process group (#4980)"
        );

        // Simulate the daemon restart / fresh CLI process: a brand-new registry
        // over the same workspace, rebuilt from disk. It has NO `Child` handle
        // for this sweep — the exact condition that used to disable group-kill.
        let mut restarted = lifecycle_registry(workspace, &script);
        let admitted = restarted.reconstruct().expect("reconstruct should succeed");
        assert_eq!(admitted, 1, "the live lock should be admitted as a Running entry");
        assert!(
            !restarted.children.contains_key(&sweep_id),
            "a reconstructed entry must not have a retained Child handle — that is the \
             whole point of this test"
        );
        assert_eq!(
            restarted.get(&sweep_id).unwrap().pgid,
            Some(leader_pid),
            "reconstruct() must restore the persisted process group"
        );

        let cancel = restarted
            .cancel(&sweep_id, Duration::from_secs(1))
            .expect("cancel should succeed");
        assert!(cancel.was_running);

        // THE assertion: the grandchild is not a direct child of anything this
        // registry tracks, so only a group-scoped signal can reach it.
        assert!(
            wait_until_dead(gc_pid, FIXTURE_CHILD_WAIT_MS),
            "grandchild survived a cancel of a RECONSTRUCTED entry — the group-kill degraded \
             to a single-PID kill (#4980 regression)"
        );

        // The leader was SIGKILLed but is still a child of THIS test process
        // (the original registry spawned it), so it lingers as a zombie until
        // someone `wait()`s it — in production the old daemon is gone and init
        // reaps it. Reap it through the spawner's retained handle, then assert
        // it is genuinely gone.
        let _ = spawner.reap_handle(&sweep_id);
        assert!(
            wait_until_dead(leader_pid, FIXTURE_CHILD_WAIT_MS),
            "leader still alive after cancel"
        );
    }

    /// Issue #4980 edge case: an `owner.json` written by a **pre-#4980** daemon
    /// binary has no `pgid` key at all. It must still deserialize (a parse
    /// failure is read everywhere as "no owner", which would drop a *live*
    /// sweep's lock), reconstruct into an entry with `pgid: None`, and cancel
    /// via a documented single-PID fallback rather than panicking.
    #[test]
    #[serial]
    fn reconstruct_tolerates_pre_pgid_owner_json_and_degrades_gracefully() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let mut registry = lifecycle_registry(workspace, "#!/usr/bin/env bash\nsleep 300\n");

        // A long-lived process to stand in for the live sweep leader. Spawned
        // WITHOUT `process_group(0)`, exactly like a pre-#4980 daemon's child
        // would appear to a registry that has no record of its group.
        let mut child = Command::new("bash")
            .arg("-c")
            .arg("sleep 300")
            .spawn()
            .expect("spawn fixture child");
        let pid = child.id();

        // Byte-for-byte the old on-disk schema: four keys, no `pgid`.
        let locks = registry.config.locks_dir();
        let lock = locks.join("issue-4979");
        std::fs::create_dir_all(&lock).unwrap();
        std::fs::write(
            lock.join("owner.json"),
            format!(
                r#"{{"issue":4979,"owner_pid":{pid},"acquired_at":"{}","sweep_id":"sweep-legacy"}}"#,
                Utc::now().to_rfc3339()
            ),
        )
        .unwrap();

        let admitted = registry
            .reconstruct()
            .expect("a pre-#4980 owner.json must not fail reconstruction");
        assert_eq!(admitted, 1, "the legacy lock must still be admitted");
        let info = registry.get("sweep-legacy").expect("legacy entry admitted");
        assert_eq!(info.pid, pid);
        assert_eq!(info.pgid, None, "no group is recorded in a pre-#4980 owner.json");

        // Cancel must fall back to single-PID delivery — no panic, no
        // group-signal against an unknown group.
        let outcome = registry
            .cancel("sweep-legacy", Duration::from_secs(1))
            .expect("cancel of a legacy entry should succeed");
        assert!(outcome.was_running);
        let _ = child.wait();
        assert!(wait_until_dead(pid, FIXTURE_CHILD_WAIT_MS), "legacy child should be gone");
    }

    /// Issue #4980: a recorded pgid that the OS contradicts (PID recycled across
    /// a daemon restart, so the live `owner_pid` leads some *other* group) must
    /// be discarded, not trusted. Trusting it would aim a later `kill(-pgid, 9)`
    /// at a stranger's process group.
    #[test]
    #[serial]
    fn reconstruct_discards_a_pgid_the_os_contradicts() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let mut registry = lifecycle_registry(workspace, "#!/usr/bin/env bash\nsleep 300\n");

        let mut child = Command::new("bash")
            .arg("-c")
            .arg("sleep 300")
            .spawn()
            .expect("spawn fixture child");
        let pid = child.id();

        // This child was NOT spawned as a group leader, so `getpgid(pid)` is the
        // test harness's group — never `pid` itself. Claim otherwise on disk.
        let lock = registry.config.locks_dir().join("issue-4978");
        std::fs::create_dir_all(&lock).unwrap();
        let owner = LockOwner {
            issue: 4978,
            owner_pid: pid,
            acquired_at: Utc::now().to_rfc3339(),
            sweep_id: "sweep-stale-pgid".to_string(),
            pgid: Some(pid),
        };
        std::fs::write(lock.join("owner.json"), serde_json::to_string_pretty(&owner).unwrap())
            .unwrap();

        registry.reconstruct().expect("reconstruct should succeed");
        assert_eq!(
            registry.get("sweep-stale-pgid").unwrap().pgid,
            None,
            "a pgid the OS does not confirm must be discarded (#4980)"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    /// Issue #4980 acceptance criterion 2 (the crash path): a sweep whose
    /// **leader is already dead** but whose process group still holds live
    /// members must have that group reaped — not left running unclaimed work.
    ///
    /// This is the incident shape verbatim: the registry showed `in_flight: 0`
    /// while a surviving `claude` agent kept mutating a repo whose claim had
    /// already been returned to the queue. `signal_sweep` cannot help here (the
    /// OS will not report a dead pid's group), which is exactly why the pgid is
    /// persisted while the leader is alive.
    #[test]
    #[serial]
    fn reaper_reaps_the_surviving_group_of_a_dead_leader() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        let gc_pidfile = dir.path().join("orphan.pid");

        // A group leader that forks a long-lived background child and then
        // exits: the leader dies, the group lives on with an orphan in it.
        let mut leader = {
            let mut cmd = Command::new("bash");
            cmd.arg("-c").arg(format!(
                "sleep 300 &\necho \"$!\" > \"{gc}\"\nexit 0\n",
                gc = gc_pidfile.display()
            ));
            #[cfg(unix)]
            cmd.process_group(0);
            cmd.spawn().expect("spawn fixture leader")
        };
        let leader_pid = leader.id();
        let orphan_pid = read_pid_file(&gc_pidfile, FIXTURE_CHILD_WAIT_MS)
            .expect("orphan pid should be recorded");
        // Reap the leader so it is genuinely dead (not a zombie that
        // `kill(pid, 0)` would still report as alive).
        let _ = leader.wait();
        assert!(wait_until_dead(leader_pid, FIXTURE_CHILD_WAIT_MS), "leader should be gone");
        assert!(is_pid_alive(orphan_pid), "orphan should have survived its leader");

        let sweep_id = "sweep-orphaned-group".to_string();
        registry.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(4977),
                pid: leader_pid,
                // Persisted at spawn time; the ONLY remaining handle on the
                // survivors now that the leader is gone.
                pgid: Some(leader_pid),
                token_name: "unknown".into(),
                runtime: "unknown".into(),
                runtime_source: None,
                log_path: registry.compute_log_path(4977),
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

        registry.reap_once();

        assert!(
            wait_until_dead(orphan_pid, FIXTURE_CHILD_WAIT_MS),
            "the dead leader's surviving process group was not reaped — a zombie agent would \
             keep running unclaimed work (#4980)"
        );
    }

    /// Issue #4980: a group that ignores the crash-path SIGTERM is escalated to
    /// SIGKILL on a later reaper tick. The escalation is deliberately deferred
    /// rather than slept through inline — `reap_once` runs on the `ListSweeps`
    /// read path under the registry mutex, where blocking is the 2026-07-26
    /// wedge shape — so this test drives the two ticks directly.
    #[test]
    #[serial]
    fn pending_group_reap_escalates_to_sigkill_on_a_later_tick() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        let gc_pidfile = dir.path().join("stubborn.pid");

        // The background child traps (ignores) SIGTERM, so only SIGKILL ends it.
        let mut leader = {
            let mut cmd = Command::new("bash");
            cmd.arg("-c").arg(format!(
                "bash -c 'trap \"\" TERM; sleep 300' &\necho \"$!\" > \"{gc}\"\nexit 0\n",
                gc = gc_pidfile.display()
            ));
            #[cfg(unix)]
            cmd.process_group(0);
            cmd.spawn().expect("spawn fixture leader")
        };
        let leader_pid = leader.id();
        let orphan_pid = read_pid_file(&gc_pidfile, FIXTURE_CHILD_WAIT_MS)
            .expect("orphan pid should be recorded");
        let _ = leader.wait();
        // Give the inner bash a moment to install its TERM trap.
        std::thread::sleep(Duration::from_millis(200));

        assert!(
            registry.reap_orphaned_group("sweep-stubborn", Some(4976), leader_pid),
            "a group with live members must be reaped"
        );
        assert!(
            registry.pending_group_reaps.contains_key("sweep-stubborn"),
            "a SIGKILL escalation must be registered"
        );
        // The SIGTERM is ignored, so the orphan is still there.
        std::thread::sleep(Duration::from_millis(200));
        assert!(is_pid_alive(orphan_pid), "trap-TERM orphan should have survived SIGTERM");

        // Bring the deadline forward rather than sleeping out the real grace.
        registry
            .pending_group_reaps
            .get_mut("sweep-stubborn")
            .unwrap()
            .escalate_at = Instant::now();
        registry.escalate_pending_group_reaps();

        assert!(
            wait_until_dead(orphan_pid, FIXTURE_CHILD_WAIT_MS),
            "the stubborn orphan survived the SIGKILL escalation (#4980)"
        );
        assert!(
            registry.pending_group_reaps.is_empty(),
            "a completed escalation must be dropped from the pending map"
        );
    }

    /// Issue #4980 safety floor: a recorded pgid naming **this process's own
    /// group** must never be signalled. `kill(-our_pgid, 9)` would take down the
    /// daemon and every sweep it owns — the one mistake in this area that is
    /// worse than the bug being fixed.
    #[test]
    #[serial]
    fn group_signalling_refuses_this_processs_own_group() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        let own_group = current_process_group().expect("unix test host");

        assert!(
            !registry.reap_orphaned_group("sweep-self", None, own_group),
            "reap_orphaned_group must refuse our own process group"
        );
        assert!(registry.pending_group_reaps.is_empty());

        // `signal_sweep` falls back to single-PID delivery rather than group
        // delivery. Signal 0 (liveness probe) keeps the test harmless: it proves
        // which target was chosen without killing anything.
        let sweep_id = "sweep-self-entry".to_string();
        registry.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(4975),
                pid: std::process::id(),
                pgid: Some(own_group),
                token_name: "unknown".into(),
                runtime: "unknown".into(),
                runtime_source: None,
                log_path: registry.compute_log_path(4975),
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
        assert!(
            registry.signal_sweep(&sweep_id, std::process::id(), 0),
            "the single-PID fallback should still reach the (live) pid"
        );
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
        assert!(wait_until_alive(pid, FIXTURE_CHILD_WAIT_MS), "child should have started");
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
            wait_until_dead(pid, FIXTURE_CHILD_WAIT_MS),
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

        // Phase 1 (#4044): wait generously for the fixture child to actually
        // exit. Under host exec-latency pressure (syspolicyd, AV scanners),
        // launching the child and running it to `exit 0` can itself take far
        // longer than the promptness bound below — that latency is a host
        // condition, not the property under test, so it gets the same
        // generous ceiling as every other fixture-child wait.
        assert!(
            wait_until_dead(outcome.pid, FIXTURE_CHILD_WAIT_MS),
            "fixture child did not exit within the wait budget"
        );

        // Phase 2: reap-on-read reconciles liveness via the retained handle's
        // `try_wait()`. Bound THIS loop to ~2s to prove "prompt" — a healthy
        // implementation transitions on the first reconcile once the child is
        // confirmed dead (`try_wait` reaps the zombie and yields the exit
        // status). Because Phase 1 already confirmed death, this bound now
        // measures reap promptness from confirmed death, not from dispatch —
        // it can no longer be falsely reddened by the child's own launch
        // latency.
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
    // Read-path forge I/O is bounded (Issue #3973)
    // ===================================================================
    //
    // The reaper's `gh` shell-outs run on the `ListSweeps` / `GetSweepStatus`
    // read path via `reap_liveness`. During the 2026-07-26 incident a wedged
    // `gh`/XPC blocked that read under the registry mutex for ~15 minutes.
    // These tests pin the bounded-subprocess fix.

    #[test]
    fn output_with_timeout_returns_output_for_a_fast_command() {
        let mut cmd = Command::new("/bin/echo");
        cmd.arg("hi");
        let out = output_with_timeout(cmd, Duration::from_secs(5))
            .expect("spawn should succeed")
            .expect("a fast command must complete inside the window");
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
    }

    #[test]
    fn output_with_timeout_kills_a_hung_command() {
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("30");
        let start = Instant::now();
        let out =
            output_with_timeout(cmd, Duration::from_millis(300)).expect("spawn should succeed");
        let elapsed = start.elapsed();
        assert!(out.is_none(), "a command exceeding the timeout must be killed and yield None");
        assert!(
            elapsed < Duration::from_secs(5),
            "kill-on-timeout should be prompt, took {elapsed:?}"
        );
    }

    #[test]
    #[serial]
    fn reap_gh_timeout_honors_env_override() {
        std::env::set_var(REAP_GH_TIMEOUT_ENV, "3");
        assert_eq!(reap_gh_timeout(), Duration::from_secs(3));
        // Zero and non-numeric both fall back to the compiled default.
        std::env::set_var(REAP_GH_TIMEOUT_ENV, "0");
        assert_eq!(reap_gh_timeout(), REAP_GH_TIMEOUT);
        std::env::set_var(REAP_GH_TIMEOUT_ENV, "notanumber");
        assert_eq!(reap_gh_timeout(), REAP_GH_TIMEOUT);
        std::env::remove_var(REAP_GH_TIMEOUT_ENV);
        assert_eq!(reap_gh_timeout(), REAP_GH_TIMEOUT);
    }

    /// End-to-end: a wedged `gh` must NOT block the `ListSweeps` /
    /// `GetSweepStatus` read path (`reap_liveness`) indefinitely, and the
    /// in-memory liveness transition must still complete when the forge label
    /// flip is killed for exceeding its timeout (Issue #3973).
    #[test]
    #[serial]
    fn read_path_reap_is_bounded_when_gh_wedges() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let scripts_dir = workspace.join(".loom").join("scripts");
        std::fs::create_dir_all(&scripts_dir).unwrap();
        // This test runs with `skip_label_flip = false`, so it exercises the
        // real #4027 workspace-commands guard too — install the marker so
        // dispatch proceeds to the gh-wedge scenario under test.
        touch_sweep_command(workspace);

        // A fake `gh` that hangs far longer than the reap timeout, simulating
        // the wedged gh/XPC from the incident.
        let fake_gh = scripts_dir.join("gh-hang.sh");
        std::fs::write(&fake_gh, "#!/usr/bin/env bash\nsleep 60\n").unwrap();
        let mut ghp = std::fs::metadata(&fake_gh).unwrap().permissions();
        ghp.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, ghp).unwrap();

        // A fake spawn that exits immediately so the read-path reap finds a
        // dead child and attempts the forge label restore.
        let spawn = scripts_dir.join("spawn-claude.sh");
        std::fs::write(&spawn, "#!/usr/bin/env bash\nexit 0\n").unwrap();
        let mut sp = std::fs::metadata(&spawn).unwrap().permissions();
        sp.set_mode(0o755);
        std::fs::set_permissions(&spawn, sp).unwrap();

        let mut config = SweepRegistryConfig::new(workspace.to_path_buf());
        config.spawn_bin = Some(spawn);
        config.gh_bin = Some(fake_gh);
        // Force the reaper's `gh` shell-out (byte-for-byte the incident path).
        config.skip_label_flip = false;
        config.journal_path = Some(workspace.join("test-sweeps-journal.json"));
        let mut registry = SweepRegistry::new(config);

        // Bound each reaper gh call tightly so the test is fast.
        std::env::set_var(REAP_GH_TIMEOUT_ENV, "1");

        let outcome = registry
            .dispatch(&SweepKind::Issue(4243), None, None, None, None)
            .expect("dispatch should succeed");
        let sweep_id = outcome.sweep_id.clone();

        // Ensure the child has actually exited before we reap-on-read. This
        // gate is generous (#4044) — it is not part of the bounded-gh
        // property under test below, which starts timing only after this
        // point.
        assert!(
            wait_until_dead(outcome.pid, FIXTURE_CHILD_WAIT_MS),
            "fake spawn child did not exit within the wait budget"
        );

        // The read-path reap must return well under the ~15-minute hang. It
        // kills the wedged gh at the 1s bound; generous headroom covers poll
        // slack and any second bounded call.
        let start = Instant::now();
        registry.reap_liveness();
        let elapsed = start.elapsed();
        std::env::remove_var(REAP_GH_TIMEOUT_ENV);

        assert!(
            elapsed < Duration::from_secs(15),
            "reap_liveness on the read path took {elapsed:?} — a wedged gh must not block it \
             indefinitely (#3973)"
        );
        // The liveness transition still completes despite the killed gh.
        assert!(
            matches!(
                registry.get(&sweep_id).unwrap().state,
                SweepState::Exited { .. } | SweepState::Crashed { .. }
            ),
            "exited child should transition to a terminal state even when the forge label flip \
             is killed for exceeding its timeout"
        );
    }

    /// THE CORE REGRESSION (#4444): a checkpoint-resume dispatch — which
    /// deliberately bypasses the 2.6 open-PR guard for its OWN PR — must still be
    /// refused by the 2.7 park guard when the issue was parked after the crash.
    /// This is the path that overrode a `loom:blocked` park on #4366.
    ///
    /// The refusal must be failure-visible, not silent: the reaper still emits
    /// `SweepResumeDispatched { dispatched: false }` (and logs the refusal, whose
    /// message names the park label), and no fresh `Running` entry is created.
    #[tokio::test]
    #[serial]
    async fn reaper_resume_refused_when_issue_parked_after_crash() {
        use crate::event_bus::EventBus;

        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::set_var("LOOM_REPO", "rjwalters/loom");
        // Open linked PR #4501 (so the resume path engages and the 2.6 bypass
        // matches) AND a `loom:blocked` park applied after the crash.
        let (mut reg, gh_log) = park_guard_registry(ws, "loom:blocked", 0, "4501", false);
        let bus = Arc::new(EventBus::new());
        reg.set_event_bus(bus.clone());
        let mut sub = bus.subscribe::<[&str; 0], &str>([]);

        write_checkpoint(&reg, 4366, "builder-done");
        insert_dead_running_entry(&mut reg, 4366, "sweep-issue-4366-crashed");

        let changed = reg.reap_once();
        assert!(changed >= 1);

        let mut saw_resume_false = false;
        for _ in 0..8 {
            let Ok(ev) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await else {
                break;
            };
            if let Event::SweepResumeDispatched {
                issue,
                pr,
                dispatched,
                ..
            } = ev.unwrap()
            {
                assert_eq!(issue, 4366);
                assert_eq!(pr, 4501);
                assert!(
                    !dispatched,
                    "a park applied after the crash must refuse the resume dispatch"
                );
                saw_resume_false = true;
            }
        }
        assert!(
            saw_resume_false,
            "the park refusal must stay failure-visible on the resume path (not silent)"
        );

        assert!(
            running_issue_sweep_id(&reg, 4366).is_none(),
            "a refused resume must not create a fresh Running entry"
        );
        let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            calls.contains("api repos/rjwalters/loom/issues/4366 --jq .labels[].name"),
            "the resume dispatch went through the central 2.7 REST probe; got: {calls:?}"
        );
        assert_eq!(
            calls
                .lines()
                .filter(|l| l.contains("api repos/rjwalters/loom/issues/4366 --jq .labels[].name"))
                .count(),
            1,
            "exactly ONE park-label probe per resume dispatch (deduped with the old \
             call-site-only check); got: {calls:?}"
        );
        // The park survives: the crash-path restore removed the stale claim but
        // did NOT re-add `loom:issue` (#4206), and no fresh claim was flipped on.
        assert!(
            !calls.contains("--add-label loom:issue"),
            "the operator's park must not be clobbered back to loom:issue; got: {calls:?}"
        );
        std::env::remove_var("LOOM_REPO");
    }

    /// Companion to the test above: with the SAME fixture but no park label, the
    /// resume dispatch succeeds — proving the refusal above is caused by the park
    /// label and not by some other property of the fixture.
    #[tokio::test]
    #[serial]
    async fn reaper_resume_succeeds_when_issue_not_parked() {
        use crate::event_bus::EventBus;

        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::set_var("LOOM_REPO", "rjwalters/loom");
        let (mut reg, _gh_log) = park_guard_registry(ws, "loom:curated", 0, "4502", false);
        let bus = Arc::new(EventBus::new());
        reg.set_event_bus(bus.clone());
        let mut sub = bus.subscribe::<[&str; 0], &str>([]);

        write_checkpoint(&reg, 4367, "builder-done");
        insert_dead_running_entry(&mut reg, 4367, "sweep-issue-4367-crashed");

        let changed = reg.reap_once();
        assert!(changed >= 1);

        let mut saw_resume_true = false;
        for _ in 0..8 {
            let Ok(ev) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await else {
                break;
            };
            if let Event::SweepResumeDispatched { dispatched, .. } = ev.unwrap() {
                assert!(dispatched, "an unparked issue must still resume normally");
                saw_resume_true = true;
            }
        }
        assert!(saw_resume_true, "expected a successful resume dispatch");
        assert!(running_issue_sweep_id(&reg, 4367).is_some());
        std::env::remove_var("LOOM_REPO");
    }

    /// AC (Test Plan item 1): a crashed sweep whose checkpoint reads
    /// `builder-done` AND whose issue has an open linked PR is resumed by the
    /// reaper — the resume dispatch bypasses the #4123 open-PR guard (a
    /// second `issue edit ... loom:building` shows up in the gh log for a
    /// FRESH sweep entry), and a `SweepResumeDispatched` event is published so
    /// the recovery attempt is visible on the event bus (AC: "not silent").
    #[tokio::test]
    #[serial]
    async fn reaper_resumes_crashed_sweep_at_builder_done_with_open_pr() {
        use crate::event_bus::EventBus;

        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::set_var("LOOM_REPO", "rjwalters/loom");
        let (mut reg, gh_log) = open_pr_guard_registry(ws, "4300", 0, false);
        let bus = Arc::new(EventBus::new());
        reg.set_event_bus(bus.clone());
        let mut sub = bus.subscribe::<[&str; 0], &str>([]);

        write_checkpoint(&reg, 4256, "builder-done");
        insert_dead_running_entry(&mut reg, 4256, "sweep-issue-4256-crashed");

        let changed = reg.reap_once();
        assert!(changed >= 1);

        let mut saw_resume = false;
        let mut saw_crashed = false;
        // Crashed, GlobalCompleted, GlobalDispatch (resume spawn), plus our
        // ResumeDispatched — drain generously and match by variant.
        for _ in 0..8 {
            let Ok(ev) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await else {
                break;
            };
            match ev.unwrap() {
                Event::SweepResumeDispatched {
                    issue,
                    pr,
                    checkpoint_phase,
                    dispatched,
                    ..
                } => {
                    assert_eq!(issue, 4256);
                    assert_eq!(pr, 4300);
                    assert_eq!(checkpoint_phase.as_deref(), Some("builder-done"));
                    assert!(dispatched, "the resume dispatch itself must have succeeded");
                    saw_resume = true;
                }
                Event::SweepCrashed { issue, .. } => {
                    assert_eq!(issue, 4256);
                    saw_crashed = true;
                }
                _ => {}
            }
        }
        assert!(saw_crashed, "expected the normal sweep.issue.4256.crashed event too");
        assert!(saw_resume, "expected a sweep.issue.4256.resume_dispatched event");

        // A fresh Running entry exists for issue 4256 under a NEW sweep_id
        // (the original crashed one is retained, terminal).
        let resumed_id = running_issue_sweep_id(&reg, 4256);
        assert!(resumed_id.is_some(), "resume dispatch must have created a new Running entry");
        assert_ne!(resumed_id.unwrap(), "sweep-issue-4256-crashed");

        let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            calls.contains("issue edit 4256") && calls.contains("loom:building"),
            "the resume dispatch must flip the label like an ordinary dispatch; got: {calls:?}"
        );
        std::env::remove_var("LOOM_REPO");
    }

    /// Core regression (Issue #4463, Test Plan item 1): reaping an OLD dead
    /// sweep for issue N while a NEWER live sweep owns N's lock must (a) leave
    /// the lock intact and (b) fire NO resume dispatch — even though the
    /// checkpoint phase (`builder-done`) and an open linked PR would otherwise
    /// make this exactly the resume-eligible case. Without the ownership gate,
    /// the reaper would delete the live sweep's lock and re-dispatch a second
    /// sweep into the same worktree (the 43s-apart double-dispatch incident).
    #[test]
    #[serial]
    fn reap_dead_sweep_preserves_newer_sweep_lock_and_skips_resume() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::set_var("LOOM_REPO", "rjwalters/loom");
        // graphql returns a PR ⇒ `probe_open_linked_pr` WOULD report one, so the
        // ONLY thing that can prevent a resume dispatch is the #4463 gate.
        let (mut reg, gh_log) = open_pr_guard_registry(ws, "9300", 0, false);

        // A newer, still-live sweep B owns the issue lock (our own PID ⇒ alive).
        let lock = write_lock_owner(&reg, 9256, "sweep-issue-9256-newer", std::process::id());
        reg.entries.insert(
            "sweep-issue-9256-newer".to_string(),
            SweepInfo {
                pgid: None,
                sweep_id: "sweep-issue-9256-newer".to_string(),
                kind: SweepKind::Issue(9256),
                pid: std::process::id(), // alive
                token_name: "unknown".into(),
                runtime: "unknown".into(),
                runtime_source: None,
                log_path: reg.compute_log_path(9256),
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

        // A resume-eligible checkpoint + the OLD dead sweep A that lost the lock.
        write_checkpoint(&reg, 9256, "builder-done");
        insert_dead_running_entry(&mut reg, 9256, "sweep-issue-9256-dead");

        let before = reg.entries.len();
        reg.reap_once();

        // (a) The live sweep's lock is intact and still owned by sweep B.
        assert!(
            lock.exists(),
            "a newer live sweep's lock must survive reaping the old dead sweep"
        );
        let owner: LockOwner =
            serde_json::from_str(&std::fs::read_to_string(lock.join("owner.json")).unwrap())
                .unwrap();
        assert_eq!(owner.sweep_id, "sweep-issue-9256-newer");

        // (b) No resume: the superseded gate short-circuits BEFORE the open-PR
        // probe and BEFORE any label flip, and creates no fresh entry.
        let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            !calls.contains("api graphql"),
            "a superseded reap must not probe for a resume PR; got: {calls:?}"
        );
        assert!(
            !calls.contains("issue edit"),
            "a superseded reap must not flip or restore any label; got: {calls:?}"
        );
        assert_eq!(
            reg.entries.len(),
            before,
            "no new sweep entry may be dispatched on a superseded reap"
        );

        // Exactly one live sweep remains (sweep B); sweep A is terminal.
        assert!(matches!(
            reg.get("sweep-issue-9256-dead").unwrap().state,
            SweepState::Crashed { .. }
        ));
        assert!(matches!(reg.get("sweep-issue-9256-newer").unwrap().state, SweepState::Running));

        std::env::remove_var("LOOM_REPO");
    }

    /// AC (Test Plan item 3): a crashed sweep whose checkpoint is
    /// `curator-done` (pre-PR) is NOT resumed even though `probe_open_linked_pr`
    /// would report one — resume is gated on a Builder-or-later checkpoint
    /// phase, and a pre-PR crash gets ONLY the normal crash handling.
    #[tokio::test]
    #[serial]
    async fn reaper_does_not_resume_pre_builder_checkpoint() {
        use crate::event_bus::EventBus;

        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::set_var("LOOM_REPO", "rjwalters/loom");
        let (mut reg, gh_log) = open_pr_guard_registry(ws, "4301", 0, false);
        let bus = Arc::new(EventBus::new());
        reg.set_event_bus(bus.clone());
        let mut sub = bus.subscribe::<[&str; 0], &str>([]);

        write_checkpoint(&reg, 4257, "curator-done");
        insert_dead_running_entry(&mut reg, 4257, "sweep-issue-4257-crashed");

        let changed = reg.reap_once();
        assert!(changed >= 1);

        for _ in 0..2 {
            let ev = sub.recv().await.unwrap();
            assert!(
                !matches!(ev, Event::SweepResumeDispatched { .. }),
                "a pre-Builder checkpoint must never trigger a resume dispatch"
            );
        }

        assert!(
            running_issue_sweep_id(&reg, 4257).is_none(),
            "no resume dispatch means no fresh Running entry for the issue"
        );
        let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            !calls.contains("api graphql"),
            "resume-eligibility check must not even probe the closes-graph for a \
             pre-Builder checkpoint phase; got: {calls:?}"
        );
        std::env::remove_var("LOOM_REPO");
    }

    /// Edge case (Test Plan item 4): the crashed sweep's PR already
    /// merged/closed by the time the reaper ticks — `probe_open_linked_pr`
    /// returns `NoneOpen` (empty post-`--jq` output), so no resume is attempted
    /// and no error surfaces; only the normal crash handling fires.
    #[tokio::test]
    #[serial]
    async fn reaper_skips_resume_when_pr_already_closed() {
        use crate::event_bus::EventBus;

        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::set_var("LOOM_REPO", "rjwalters/loom");
        // Empty graphql stdout ⇒ no OPEN-state linked PR.
        let (mut reg, gh_log) = open_pr_guard_registry(ws, "", 0, false);
        let bus = Arc::new(EventBus::new());
        reg.set_event_bus(bus.clone());
        let mut sub = bus.subscribe::<[&str; 0], &str>([]);

        write_checkpoint(&reg, 4258, "judge-done");
        insert_dead_running_entry(&mut reg, 4258, "sweep-issue-4258-crashed");

        let changed = reg.reap_once();
        assert!(changed >= 1);

        for _ in 0..2 {
            let ev = sub.recv().await.unwrap();
            assert!(
                !matches!(ev, Event::SweepResumeDispatched { .. }),
                "no open PR means no resume dispatch, even at a resumable phase"
            );
        }

        assert!(
            running_issue_sweep_id(&reg, 4258).is_none(),
            "no resume dispatch means no fresh Running entry for the issue"
        );
        let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            calls.contains("api graphql"),
            "the resume-eligibility check DID probe the closes-graph; got: {calls:?}"
        );
        std::env::remove_var("LOOM_REPO");
    }

    /// Bounded resume attempts (#4256, Judge residual-risk backstop): once an
    /// issue has accumulated `MAX_RESUME_ATTEMPTS` consecutive checkpoint-less
    /// resume crashes, the reaper stops resuming it — it emits a
    /// failure-visible `SweepResumeDispatched { dispatched: false }` event,
    /// creates NO fresh Running entry, and adds NO labels beyond the ones
    /// already present. This is the replacement for the #4123 open-PR backstop
    /// that the resume path deliberately bypasses, closing the ~2s..stall
    /// infinite-resume window. The checkpoint is deliberately made STALE
    /// (mtime before this run's `started_at`) so the reset-on-progress branch
    /// does not clear the seeded tally — the exact pathology the cap bounds.
    #[tokio::test]
    #[serial]
    async fn reaper_stops_resuming_after_attempt_cap() {
        use crate::event_bus::EventBus;

        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::set_var("LOOM_REPO", "rjwalters/loom");
        let (mut reg, gh_log) = open_pr_guard_registry(ws, "4310", 0, false);
        let bus = Arc::new(EventBus::new());
        reg.set_event_bus(bus.clone());
        let mut sub = bus.subscribe::<[&str; 0], &str>([]);

        write_checkpoint(&reg, 4260, "builder-done");
        insert_dead_running_entry(&mut reg, 4260, "sweep-issue-4260-crashed");
        // Stale inherited checkpoint: `started_at` AFTER the checkpoint mtime,
        // so `checkpoint_written_by_run` is false and the reset-on-progress
        // branch never clears the seeded tally.
        reg.entries
            .get_mut("sweep-issue-4260-crashed")
            .unwrap()
            .started_at = Utc::now() + chrono::Duration::seconds(30);
        // Pre-seed the issue exactly AT the cap — the next resume is refused.
        reg.resume_attempt_counts.insert(4260, MAX_RESUME_ATTEMPTS);

        let changed = reg.reap_once();
        assert!(changed >= 1);

        let mut saw_resume_false = false;
        for _ in 0..8 {
            let Ok(ev) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await else {
                break;
            };
            if let Event::SweepResumeDispatched {
                issue,
                pr,
                dispatched,
                ..
            } = ev.unwrap()
            {
                assert_eq!(issue, 4260);
                assert_eq!(pr, 4310);
                assert!(
                    !dispatched,
                    "at the attempt cap the reaper must NOT resume (dispatched:false)"
                );
                saw_resume_false = true;
            }
        }
        assert!(
            saw_resume_false,
            "exhaustion must still emit a failure-visible resume_dispatched event (not silent)"
        );

        // No resume dispatch ⇒ no fresh Running entry for the issue.
        assert!(
            running_issue_sweep_id(&reg, 4260).is_none(),
            "no resume dispatch at the cap means no fresh Running entry"
        );
        // The cap is not exceeded.
        assert_eq!(
            reg.resume_attempt_counts.get(&4260).copied(),
            Some(MAX_RESUME_ATTEMPTS),
            "an exhausted issue's counter stays pinned at the cap, never grows"
        );
        // No extra label was applied on exhaustion (the PR is left as-is for
        // the periodic Judge role / operator).
        // No NEW label is applied on exhaustion. `restore_label_to_ready`
        // still flips loom:building→loom:issue (existing behavior, not a new
        // label), and `issue_has_blocked_label` mentions "loom:blocked" only
        // inside its read-only `--jq` query — so assert specifically that no
        // `--add-label loom:blocked` (quarantine) edit was issued.
        let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            !calls.contains("add-label loom:blocked"),
            "exhaustion must NOT add labels beyond the existing ones; got: {calls:?}"
        );
        std::env::remove_var("LOOM_REPO");
    }

    /// Boundary companion to `reaper_stops_resuming_after_attempt_cap`: one
    /// below the cap the resume still fires and ticks the per-issue counter up
    /// to exactly `MAX_RESUME_ATTEMPTS`, so the cap value itself is locked (the
    /// Nth attempt succeeds; only the (N+1)th is refused). Uses the same stale
    /// checkpoint so the seeded tally survives into the resume decision.
    #[tokio::test]
    #[serial]
    async fn reaper_still_resumes_one_below_attempt_cap() {
        use crate::event_bus::EventBus;

        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::set_var("LOOM_REPO", "rjwalters/loom");
        let (mut reg, _gh_log) = open_pr_guard_registry(ws, "4311", 0, false);
        let bus = Arc::new(EventBus::new());
        reg.set_event_bus(bus.clone());
        let mut sub = bus.subscribe::<[&str; 0], &str>([]);

        write_checkpoint(&reg, 4261, "judge-rejected");
        insert_dead_running_entry(&mut reg, 4261, "sweep-issue-4261-crashed");
        reg.entries
            .get_mut("sweep-issue-4261-crashed")
            .unwrap()
            .started_at = Utc::now() + chrono::Duration::seconds(30);
        reg.resume_attempt_counts
            .insert(4261, MAX_RESUME_ATTEMPTS - 1);

        let changed = reg.reap_once();
        assert!(changed >= 1);

        let mut saw_resume_true = false;
        for _ in 0..8 {
            let Ok(ev) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await else {
                break;
            };
            if let Event::SweepResumeDispatched {
                issue, dispatched, ..
            } = ev.unwrap()
            {
                assert_eq!(issue, 4261);
                assert!(dispatched, "one below the cap the resume must still fire");
                saw_resume_true = true;
            }
        }
        assert!(saw_resume_true, "expected a successful resume dispatch below the cap");
        assert!(
            running_issue_sweep_id(&reg, 4261).is_some(),
            "a below-cap resume must create a fresh Running entry"
        );
        assert_eq!(
            reg.resume_attempt_counts.get(&4261).copied(),
            Some(MAX_RESUME_ATTEMPTS),
            "the resume attempt must tick the per-issue counter up to the cap"
        );
        std::env::remove_var("LOOM_REPO");
    }

    /// Deterministic-no-op guard (Issue #5614) — the label-flap regression.
    ///
    /// Reproduces the #5565 shape exactly: a resumable checkpoint phase
    /// (`judge-done`) plus an open linked PR, where the sweep exited **cleanly**
    /// (`exit_code == Some(0)`, via a REAL retained child, not the no-handle
    /// `None` fallback) without touching the checkpoint. In production that is
    /// a sweep reporting "PR held under `loom:operator` — human merge decision
    /// required" and stopping on purpose. Pre-#5614 the reaper read the
    /// surviving checkpoint as a crash and resumed it, and because the resume
    /// path bypasses the #4123 open-PR guard AND the #4485 dispatch backoff,
    /// each cycle re-claimed the issue ~3s after the reaper released it —
    /// ~10 `loom:issue`/`loom:building` transitions in 7 minutes.
    ///
    /// The guard must: refuse the resume, say so visibly
    /// (`SweepResumeDispatched { dispatched: false }`), create no fresh
    /// `Running` entry (no re-claim, no flap), and — unlike the attempt-cap
    /// refusal — leave the resume runway untouched, since a human-gated pause
    /// is not a failed attempt.
    #[tokio::test]
    #[serial]
    async fn reaper_does_not_resume_a_clean_exit_that_made_no_checkpoint_progress() {
        use crate::event_bus::EventBus;

        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::set_var("LOOM_REPO", "rjwalters/loom");
        let (mut reg, _gh_log) = open_pr_guard_registry(ws, "5569", 0, false);
        let bus = Arc::new(EventBus::new());
        reg.set_event_bus(bus.clone());
        let mut sub = bus.subscribe::<[&str; 0], &str>([]);

        // Checkpoint written BEFORE the entry's `started_at`, so
        // `checkpoint_written_by_run` is false: this run inherited the
        // checkpoint and left it exactly as it found it.
        write_checkpoint(&reg, 5565, "judge-done");
        let sweep_id = insert_clean_exit_running(&mut reg, 5565, 1);

        let changed = reg.reap_once();
        assert!(changed >= 1);

        let mut saw_resume_false = false;
        for _ in 0..8 {
            let Ok(ev) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await else {
                break;
            };
            if let Event::SweepResumeDispatched {
                issue,
                pr,
                dispatched,
                ..
            } = ev.unwrap()
            {
                assert_eq!(issue, 5565);
                assert_eq!(pr, 5569);
                assert!(
                    !dispatched,
                    "a clean exit that made no checkpoint progress must NOT be resumed (#5614)"
                );
                saw_resume_false = true;
            }
        }
        assert!(
            saw_resume_false,
            "the refusal must be failure-visible on the bus, not a silent skip"
        );

        // The load-bearing assertion: no fresh Running entry means no resume
        // dispatch, and therefore no `loom:issue` -> `loom:building` re-claim
        // seconds after the reaper restored the label — i.e. no flap.
        assert!(
            running_issue_sweep_id(&reg, 5565).is_none(),
            "no resume dispatch means no fresh Running entry (and no re-claim flap)"
        );
        // A human-gated pause is not a failed attempt: the runway is untouched,
        // so clearing the hold leaves the full #4256 resume budget available.
        assert_eq!(
            reg.resume_attempt_counts.get(&5565).copied(),
            None,
            "the deterministic-no-op refusal must not consume a resume attempt"
        );
        // Sanity: the fixture really did observe a clean exit, not the
        // no-handle `None` fallback that the pre-#5614 behavior still allows.
        let info = reg.entries.get(&sweep_id).unwrap();
        assert!(
            matches!(info.state, SweepState::Crashed { .. }),
            "a surviving checkpoint still classifies the entry as Crashed; got: {:?}",
            info.state
        );
        std::env::remove_var("LOOM_REPO");
    }

    /// Narrowness companion to the test above (#5614): the guard keys on a
    /// **clean** exit, so #4256's actual remit — a genuine crash whose exit
    /// code is not `Some(0)` — must still resume on the first attempt. The
    /// no-handle reap path reports `exit_code == None`, which is exactly the
    /// reconstructed-entry / signal-death case the guard deliberately fails
    /// open on.
    #[tokio::test]
    #[serial]
    async fn reaper_still_resumes_a_non_clean_exit_with_no_checkpoint_progress() {
        use crate::event_bus::EventBus;

        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::set_var("LOOM_REPO", "rjwalters/loom");
        let (mut reg, _gh_log) = open_pr_guard_registry(ws, "5570", 0, false);
        let bus = Arc::new(EventBus::new());
        reg.set_event_bus(bus.clone());
        let mut sub = bus.subscribe::<[&str; 0], &str>([]);

        write_checkpoint(&reg, 5566, "judge-done");
        insert_dead_running_entry(&mut reg, 5566, "sweep-issue-5566-crashed");
        // Same stale-checkpoint setup as the clean-exit test: the ONLY
        // difference between the two is the exit code.
        reg.entries
            .get_mut("sweep-issue-5566-crashed")
            .unwrap()
            .started_at = Utc::now() + chrono::Duration::seconds(30);

        let changed = reg.reap_once();
        assert!(changed >= 1);

        let mut saw_resume_true = false;
        for _ in 0..8 {
            let Ok(ev) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await else {
                break;
            };
            if let Event::SweepResumeDispatched {
                issue, dispatched, ..
            } = ev.unwrap()
            {
                assert_eq!(issue, 5566);
                assert!(
                    dispatched,
                    "a non-clean exit is a real crash — #4256's resume must still fire"
                );
                saw_resume_true = true;
            }
        }
        assert!(
            saw_resume_true,
            "expected the #4256 resume to still dispatch for a genuine crash"
        );
        assert!(
            running_issue_sweep_id(&reg, 5566).is_some(),
            "a genuine crash resume must still create a fresh Running entry"
        );
        assert_eq!(
            reg.resume_attempt_counts.get(&5566).copied(),
            Some(1),
            "a genuine crash resume still consumes one attempt from the runway"
        );
        std::env::remove_var("LOOM_REPO");
    }
}
