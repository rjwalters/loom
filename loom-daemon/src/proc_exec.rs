//! Shared bounded subprocess execution (epic #7810, PR 1).
//!
//! Every later shell-to-Rust port needs reliable evidence about what actually
//! ran. This module is that evidence layer: one place that runs a child under a
//! deadline and reports, without collapsing, which of four things happened.
//!
//! # Why this exists as a module rather than a new runner
//!
//! It does not introduce a second executor. `sweep_registry::reaper::output_with_timeout`
//! was already the de facto shared one (~30 call sites across 10 files), and its
//! `Result<Option<Output>>` shape already preserved raw bytes and `ExitStatus`.
//! This extracts that behaviour and closes two gaps it could not close while its
//! contract was "best-effort `gh` call from the reaper".
//!
//! # The two gaps
//!
//! **Draining.** The original polled `try_wait()` and only called
//! `wait_with_output()` *after* the child had already exited. A child that
//! filled the pipe blocked on write, never exited, and the deadline fired — so
//! output size was indistinguishable from a hang. Measured on the original, with
//! a 3s budget: 64 KiB completed, 128 KiB reported a timeout. Its doc comment
//! stated this as a precondition ("the reaper's `gh` invocations emit a tiny
//! payload"), which is sound for the reaper and unsound for a shared executor.
//! Here both pipes are drained by dedicated threads that run *concurrently* with
//! the wait, so the child can never block on a full pipe.
//!
//! **Descendants.** `Child::kill()` signals the immediate child only. A child
//! that had forked its own children left them running past the deadline
//! (verified by pid, not by process name). Here the child is placed in its own
//! process group at spawn and the deadline terminates that **group**.
//!
//! # Process-group safety
//!
//! [`run_bounded`] calls `process_group(0)` before spawning, so the child
//! becomes the leader of a brand-new group whose id equals its pid. Termination
//! therefore signals a group this module created and whose only members are the
//! child and its descendants. It never signals pgid 0 or a negative pid derived
//! from anything but that spawn, so an inherited group — the daemon's own, or a
//! caller's — can never be hit. That distinction is the whole reason the group
//! is created here rather than reusing whatever the child inherited.

use std::io;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long to let the reader threads finish after the process group has been
/// **terminated** (timeout, cancellation, unanswered forwarded signal), before
/// giving up on them.
///
/// After a `SIGKILL` to the group every writer is gone, so the pipes reach EOF
/// promptly; this is a bound on that, not an expected wait. A reader delivers
/// its bytes only once, at EOF, so a reader that misses this window contributes
/// **no** bytes for its stream — not a partial prefix. That is acceptable only
/// on these paths, whose result already says the answer is missing
/// ([`Completion::TimedOut`] or [`ExecError::Collect`]); blocking instead would
/// reintroduce exactly the class of hang this module exists to bound.
///
/// It is deliberately **not** used after a normal exit (issue #10799): there a
/// missed drain used to surface as `Exited` with empty stdout — a successful
/// command that "printed nothing". The normal-exit drain uses the call's
/// remaining budget instead, and fails with [`ExecError::Collect`] if it cannot
/// complete (see [`drain_after_exit`]). This constant doubles as that drain's
/// minimum budget, so a child that exits right at the deadline still gets one.
const DRAIN_GRACE: Duration = Duration::from_millis(500);

/// Poll cadence while waiting for the child to exit.
///
/// Matches the reaper's original `REAP_GH_POLL_INTERVAL` so migrated callers see
/// the same latency characteristics.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// What a bounded execution did, when it got far enough to say.
///
/// The two variants are deliberately not collapsible into one: a completed
/// process — even one that exited nonzero or died on a signal — answered the
/// question. A timeout did not. Callers that need "unknown" semantics must be
/// able to tell those apart, which is the property `restart_verify`'s
/// `.ok().flatten()` and `GhResult`'s `success: bool` both destroy.
#[derive(Debug)]
pub enum Completion {
    /// The child ran to completion inside the deadline.
    ///
    /// Carries `std::process::Output` unchanged — raw `stdout`/`stderr` bytes
    /// (never lossily decoded) and the real `ExitStatus`, so a nonzero exit and
    /// a signal death remain distinguishable via
    /// `std::os::unix::process::ExitStatusExt::signal`.
    Exited(Output),

    /// The deadline elapsed. The child's process group was terminated.
    ///
    /// Whatever had been read before the deadline is preserved rather than
    /// discarded: a probe that timed out after emitting a diagnostic is easier
    /// to act on than one that reports nothing. There is deliberately no
    /// `ExitStatus` here — the child did not choose its fate, this module did,
    /// and reporting the `SIGKILL` we ourselves sent as if it were the
    /// program's own outcome would be a lie of exactly the kind this module
    /// exists to stop telling.
    TimedOut { stdout: Vec<u8>, stderr: Vec<u8> },
}

impl Completion {
    /// The completed `Output`, or `None` if the deadline fired.
    ///
    /// A convenience for migrating callers that genuinely only care about the
    /// success path; prefer matching on the variants where "timed out" and
    /// "ran and failed" should be handled differently.
    #[must_use]
    pub fn output(self) -> Option<Output> {
        match self {
            Completion::Exited(o) => Some(o),
            Completion::TimedOut { .. } => None,
        }
    }

    /// Whether the child ran to completion and exited zero.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        matches!(self, Completion::Exited(o) if o.status.success())
    }
}

/// Why a bounded execution produced no answer at all.
///
/// Split from [`Completion`] because these mean different things to a caller
/// and should not share a code path: `Spawn` says the command could not start
/// (missing binary, bad permissions, bad working directory) and will fail the
/// same way next time; `Collect` says it *did* start and something went wrong
/// reading its result, so the side effects may well have happened.
#[derive(Debug)]
pub enum ExecError {
    /// The child could not be started. Nothing ran; no side effects occurred.
    Spawn(io::Error),

    /// The child started but its status or output could not be collected.
    ///
    /// Distinct from `Spawn` because the command may have run to completion and
    /// had its effects — treating this as "did not run" is how a completed
    /// write gets retried or reported as never having happened.
    Collect(io::Error),
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecError::Spawn(e) => write!(f, "spawn failed: {e}"),
            ExecError::Collect(e) => write!(f, "output collection failed: {e}"),
        }
    }
}

impl std::error::Error for ExecError {}

/// Run `cmd` to completion under `timeout`.
///
/// `stdout` and `stderr` are forced to `piped()` and drained concurrently, so
/// output volume can never be mistaken for a hang. `stdin` is left to the
/// caller — `restart_verify`'s probes null it, and a caller that needs to feed
/// input still can.
///
/// On timeout the child's own process group (created at spawn, see the module
/// docs) is terminated, so descendants do not outlive the deadline.
///
/// # Errors
///
/// [`ExecError::Spawn`] if the child could not be started; [`ExecError::Collect`]
/// if it started but waiting on it failed, or if it exited but its output could
/// not be fully drained within the remaining budget (`ErrorKind::TimedOut`,
/// e.g. a descendant still holds the pipe) — never `Exited` with missing output.
pub fn run_bounded(cmd: Command, timeout: Duration) -> Result<Completion, ExecError> {
    run_bounded_cancellable(cmd, timeout, || false)
}

#[cfg(unix)]
fn place_in_own_process_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // pgid := the child's own pid. Every later signal targets THIS group, which
    // did not exist before this call, so no inherited group can be reached.
    cmd.process_group(0);
}

#[cfg(not(unix))]
fn place_in_own_process_group(_cmd: &mut Command) {}

/// Terminate the group led by `pid`, then the pid itself as a fallback.
///
/// `pid` is always a pid this module spawned with `process_group(0)`, so it is
/// the leader of a group created here and `killpg` cannot reach an inherited
/// group. The direct `kill` afterwards covers the window where the child has
/// been created but `setpgid` has not yet taken effect.
#[cfg(unix)]
fn terminate_group(pid: u32) {
    let pid = pid as libc::pid_t;
    // SAFETY: `pid` is a child this module spawned into its own process group;
    // killpg on its own pgid cannot escape to an inherited group. A failure
    // (ESRCH — already gone) is expected and ignored.
    unsafe {
        libc::killpg(pid, libc::SIGKILL);
        libc::kill(pid, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn terminate_group(_pid: u32) {}

/// Forward `signal` to the group led by `pid` (and to `pid` itself, for the
/// window before `setpgid` takes effect). Same safety argument as
/// [`terminate_group`]: `pid` leads a group this module created.
#[cfg(unix)]
fn signal_group(pid: u32, signal: i32) {
    let pid = pid as libc::pid_t;
    // SAFETY: see `terminate_group`; ESRCH (already gone) is ignored.
    unsafe {
        libc::killpg(pid, signal);
        libc::kill(pid, signal);
    }
}

#[cfg(not(unix))]
fn signal_group(_pid: u32, _signal: i32) {}

/// Run with cooperative cancellation, terminating the owned process group.
/// Cancellation returns `Collect(Interrupted)` because side effects may have run.
pub fn run_bounded_cancellable(
    cmd: Command,
    timeout: Duration,
    cancelled: impl Fn() -> bool,
) -> Result<Completion, ExecError> {
    run_bounded_inner(cmd, timeout, cancelled, |_| {}, None)
}

/// [`run_bounded`], forwarding a signal the **caller** received to the
/// child's process group (issue #10661). The group this module creates is
/// not the terminal's foreground group, so a Ctrl-C at the terminal reaches
/// only the caller; an interactive caller that traps SIGINT/SIGTERM reports
/// it through `pending`, polled while the child runs.
///
/// The first signal `pending` returns is sent to the group once. If the child
/// then exits on its own within `grace`, its real [`Completion::Exited`] is
/// returned (the caller decides what an interrupted exit means). If not, the
/// group is killed and the result is `Collect(Interrupted)`, as for
/// cancellation. With `pending` always `None` this is exactly [`run_bounded`].
/// `spawned` observes the child's pid (= its process group id) once it runs,
/// as in [`run_bounded_observed`].
pub fn run_bounded_forwarding(
    cmd: Command,
    timeout: Duration,
    grace: Duration,
    pending: &dyn Fn() -> Option<i32>,
    spawned: impl FnOnce(u32),
) -> Result<Completion, ExecError> {
    run_bounded_inner(cmd, timeout, || false, spawned, Some((pending, grace)))
}

/// Observe the actual child PID before waiting, while retaining the shared
/// process-group deadline and pipe-draining behavior. The observer must return
/// promptly and must not panic; it is intended for local lifecycle bookkeeping.
pub fn run_bounded_observed(
    cmd: Command,
    timeout: Duration,
    spawned: impl FnOnce(u32),
) -> Result<Completion, ExecError> {
    run_bounded_inner(cmd, timeout, || false, spawned, None)
}

/// A caller's pending-signal probe and how long to wait after forwarding it.
type Forward<'a> = Option<(&'a dyn Fn() -> Option<i32>, Duration)>;

fn run_bounded_inner(
    mut cmd: Command,
    timeout: Duration,
    cancelled: impl Fn() -> bool,
    spawned: impl FnOnce(u32),
    forward: Forward<'_>,
) -> Result<Completion, ExecError> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    place_in_own_process_group(&mut cmd);

    let mut child = cmd.spawn().map_err(ExecError::Spawn)?;
    let pid = child.id();
    spawned(pid);

    // Drain both pipes on their own threads, concurrently with the wait below.
    // This is the fix for the original's deadlock: the child can fill the pipe
    // to any size without blocking, because somebody is always reading.
    let stdout_rx = spawn_reader(child.stdout.take());
    let stderr_rx = spawn_reader(child.stderr.take());

    let mut deadline = Instant::now() + timeout;
    let mut forwarded = false;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // The child exited on its own. Its pipes reach EOF once every
                // writer is gone — which a lingering descendant can delay — so
                // drain against the call's remaining budget, and fail rather
                // than report success with missing output (#10799).
                let (stdout, stderr) = drain_after_exit(&stdout_rx, &stderr_rx, deadline)?;
                return Ok(Completion::Exited(Output {
                    status,
                    stdout,
                    stderr,
                }));
            }
            Ok(None) => {}
            Err(e) => {
                // It started, so this is a collection failure, not a spawn one.
                terminate_group(pid);
                let _ = child.wait();
                return Err(ExecError::Collect(e));
            }
        }

        if let (false, Some((pending, grace))) = (forwarded, forward) {
            if let Some(signal) = pending() {
                signal_group(pid, signal);
                forwarded = true;
                deadline = deadline.min(Instant::now() + grace);
            }
        }
        if cancelled() || (forwarded && Instant::now() >= deadline) {
            terminate_group(pid);
            let _ = child.wait();
            let _ = collect(&stdout_rx, DRAIN_GRACE);
            let _ = collect(&stderr_rx, DRAIN_GRACE);
            return Err(ExecError::Collect(io::Error::new(
                io::ErrorKind::Interrupted,
                "execution cancelled; process group terminated",
            )));
        }
        if Instant::now() >= deadline {
            terminate_group(pid);
            let _ = child.wait();
            return Ok(Completion::TimedOut {
                stdout: collect(&stdout_rx, DRAIN_GRACE),
                stderr: collect(&stderr_rx, DRAIN_GRACE),
            });
        }

        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Read `pipe` to EOF on a dedicated thread, delivering the bytes once.
///
/// Returns `None` when there is no pipe to read (only reachable if the handle
/// was already taken), which `collect` renders as empty output.
fn spawn_reader<R: io::Read + Send + 'static>(pipe: Option<R>) -> Option<mpsc::Receiver<Vec<u8>>> {
    let mut pipe = pipe?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        // A read error yields whatever was read before it; the caller's job is
        // to report what the process produced, not to fail the whole execution
        // because the tail was unreadable.
        let _ = io::Read::read_to_end(&mut pipe, &mut buf);
        let _ = tx.send(buf);
    });
    Some(rx)
}

/// Collect both streams after a **normal** exit, or fail (issue #10799).
///
/// The budget is whatever remains of the call's `deadline` (already shortened
/// by a forwarded signal's grace, if any), floored at [`DRAIN_GRACE`] so a child
/// that exits right at the deadline still gets a drain. It is **one** budget
/// shared by both streams, not one per stream, so the call overruns its
/// deadline by at most the floor.
///
/// A reader that has not reached EOF by then (a descendant still holds the
/// write end, or the host is too loaded to schedule the reader) yields
/// [`ExecError::Collect`] with [`io::ErrorKind::TimedOut`]: the command ran,
/// but its output is not known, and reporting `Exited` with an empty stream
/// would let a caller read "succeeded, printed nothing" as "no items". The
/// descendants are left alone, as before — the leader has been reaped, so its
/// pid no longer safely names a group this module owns.
fn drain_after_exit(
    stdout_rx: &Option<mpsc::Receiver<Vec<u8>>>,
    stderr_rx: &Option<mpsc::Receiver<Vec<u8>>>,
    deadline: Instant,
) -> Result<(Vec<u8>, Vec<u8>), ExecError> {
    let now = Instant::now();
    let drain_deadline = now + deadline.saturating_duration_since(now).max(DRAIN_GRACE);
    let take = |rx: &Option<mpsc::Receiver<Vec<u8>>>, stream: &str| match rx {
        None => Ok(Vec::new()),
        Some(rx) => rx
            .recv_timeout(drain_deadline.saturating_duration_since(Instant::now()))
            .map_err(|e| {
                ExecError::Collect(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "child exited but its {stream} did not reach EOF within the \
                         call's remaining budget ({e}); output incomplete"
                    ),
                ))
            }),
    };
    let stdout = take(stdout_rx, "stdout")?;
    let stderr = take(stderr_rx, "stderr")?;
    Ok((stdout, stderr))
}

/// Take a reader thread's bytes, waiting at most `grace`.
///
/// Only for the paths that have **terminated** the process group. Returns empty
/// on timeout rather than blocking: after the group has been killed the writers
/// are gone and EOF is immediate, so exceeding `grace` means a pipe write end
/// escaped the group. Blocking there would reintroduce the unbounded wait this
/// module exists to eliminate. A normal exit uses [`drain_after_exit`] instead,
/// because there an empty miss is indistinguishable from real empty output.
fn collect(rx: &Option<mpsc::Receiver<Vec<u8>>>, grace: Duration) -> Vec<u8> {
    match rx {
        Some(rx) => rx.recv_timeout(grace).unwrap_or_default(),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests;
