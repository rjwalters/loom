//! The deferred publisher behind a declined `lease ensure` (#10570).
//!
//! ## The incident this closes
//!
//! rjwalters/loom#10161, 2026-10-06. A fleet sweep's lease comment was last
//! renewed at 12:14:54Z; the daemon released that sweep's claim at 12:19:28Z
//! but its lease record stayed inside the 15-minute TTL until 12:29:54Z. An
//! attended Builder claimed `loom:building` at 12:24:24Z. Any `lease ensure`
//! at that moment gets `sweep-lease-publish.sh` exit 4 (a "live peer" holds a
//! fresh lease) and, before this module, gave up for good: no lease comment
//! from the attended claim ever appeared. Once the leftover lease aged out,
//! another host's orphan recovery found a label older than its grace period,
//! no linked PR, no local claim file and only a STALE lease, and reclaimed it
//! at 12:35:51Z (`no_spawn_loop_entry`). Every recovery gate behaved
//! correctly; what failed was the attended path's publication.
//!
//! ## What this does
//!
//! On a retryable decline ([`Outcome::is_retryable`]) `lease ensure` spawns
//! itself detached with `--deferred`. That child re-runs the SAME
//! [`LeaseEnsureArgs::ensure`] — the same publish script, trusted lease format
//! and peer rules, then the same bounded renewer — on a fixed cadence until a
//! publish settles, the watched session dies, or the age cap passes. A live
//! peer's lease is never superseded: while it stays fresh, every attempt is
//! declined exactly as before. There is no new lease format or protocol.
//!
//! One deferred publisher owns the retry per issue per checkout (an exclusive
//! `flock`), and at most one exists per watched session. A different
//! session's caller is never turned away by the owner's lock: its own
//! publisher waits for that lock, and an owner whose session dies releases it
//! within one poll ([`run_with`]). Every outcome is appended to `.loom/logs/lease-ensure/issue-<N>.log`,
//! because `worktree.sh` callers rarely read stderr and the detached child
//! has none.

use super::{LeaseEnsureArgs, Outcome, SessionEnv, DEFAULT_MAX_AGE_SECS};
use std::io::Write as _;
use std::os::unix::io::AsRawFd as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Seconds between deferred attempts while a peer lease could be a leftover.
/// A minute keeps the gap between a leftover's expiry and this claim's own
/// lease well inside orphan recovery's grace period.
pub(crate) const DEFAULT_RETRY_INTERVAL_SECS: u64 = 60;

/// After a peer lease has stayed fresh this long past its TTL it is being
/// renewed, i.e. a genuinely live peer: slow to the renewer's own cadence.
const BACKOFF_FACTOR: u64 = 5;

/// How a deferred publisher ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeferredEnd {
    /// An attempt settled; `Renewing` is the success case.
    Settled(Outcome),
    /// The watched session process is gone: nothing left to lease.
    SessionEnded,
    /// The age cap passed with every attempt still declined.
    CapReached,
}

impl DeferredEnd {
    pub(crate) fn describe(&self, issue: u64) -> String {
        match self {
            Self::Settled(o) => format!("deferred publisher settled: {}", o.describe(issue)),
            Self::SessionEnded => format!(
                "deferred publisher for issue #{issue} stopped: the watched session ended before \
                 a lease could be published"
            ),
            Self::CapReached => format!(
                "deferred publisher for issue #{issue} stopped at its age cap with every publish \
                 still declined — the claim has no lease"
            ),
        }
    }
}

/// Where this checkout records `lease ensure` outcomes for `issue`.
pub(crate) fn log_path(root: &Path, issue: u64) -> PathBuf {
    root.join(".loom/logs/lease-ensure")
        .join(format!("issue-{issue}.log"))
}

/// The single-owner lock of the deferred publisher for `issue`: whoever
/// holds it is the one publisher retrying for this issue in this checkout.
pub(super) fn lock_path(root: &Path, issue: u64) -> PathBuf {
    log_path(root, issue).with_extension("deferred.lock")
}

/// The per-session lock: at most one deferred publisher (owning or waiting)
/// per watched session per issue, so a session's repeated `lease ensure`
/// calls do not stack publishers.
fn session_lock_path(root: &Path, issue: u64, watch_pid: u32) -> PathBuf {
    let key = super::session_sweep_id(watch_pid).unwrap_or_else(|| format!("s{watch_pid}"));
    log_path(root, issue).with_extension(format!("deferred.{key}.lock"))
}

/// How often a deferred publisher re-checks its watched session while asleep,
/// and how often a waiting one re-probes the issue lock. This bounds the
/// handoff gap when the owning session dies (#10570 review): the owner
/// releases the issue lock within one poll of its session ending, not after a
/// full retry interval of up to 300 s.
const DEFAULT_POLL: Duration = Duration::from_secs(5);

/// Append one timestamped outcome line. Best effort: a log is diagnostics.
pub(crate) fn record(workspace: &str, issue: u64, line: &str) {
    let Ok(root) = loom_daemon::repo_root::resolve_repo_root(workspace) else {
        return;
    };
    let path = log_path(&root, issue);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{} {line}", chrono::Utc::now().to_rfc3339());
    }
}

/// Spawn the detached deferred publisher; returns the line to report.
pub(crate) fn spawn(args: &LeaseEnsureArgs) -> String {
    spawn_with(args, launch_process)
}

/// [`spawn`] with the launch injected, so tests drive the real ownership
/// decision without exec'ing the daemon binary.
///
/// Only a publisher for the SAME watched session suppresses a launch. One
/// owned by a different session is no proof this session has a retry
/// scheduled: that owner may be asleep with its session already dead, and
/// would exit without ever publishing for this one (#10570 review). So a
/// different session always gets its own publisher, which waits for the
/// issue lock.
pub(crate) fn spawn_with(
    args: &LeaseEnsureArgs,
    launch: impl FnOnce(&LeaseEnsureArgs) -> std::io::Result<u32>,
) -> String {
    let issue = args.issue;
    let mut waits = false;
    if let Ok(root) = loom_daemon::repo_root::resolve_repo_root(&args.workspace) {
        // Probe (and immediately release) both locks.
        if try_lock(&session_lock_path(&root, issue, args.watch_pid)).is_none() {
            return format!(
                "issue #{issue}: a deferred publisher for this session (pid {}) is already \
                 waiting for it",
                args.watch_pid
            );
        }
        waits = try_lock(&lock_path(&root, issue)).is_none();
    }
    match launch(args) {
        Ok(pid) => format!(
            "issue #{issue}: deferred publisher {pid} will retry every {}s while pid {} lives{} \
             (log: .loom/logs/lease-ensure/issue-{issue}.log)",
            args.retry_interval,
            args.watch_pid,
            if waits {
                ", once another session's deferred publisher for this issue stops"
            } else {
                ""
            }
        ),
        Err(e) => format!("could not start a deferred publisher for issue #{issue}: {e}"),
    }
}

fn launch_process(args: &LeaseEnsureArgs) -> std::io::Result<u32> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["lease", "ensure", &args.issue.to_string(), "--deferred"])
        .args(["--watch-pid", &args.watch_pid.to_string()])
        .args(["--max-age", &args.max_age.to_string()])
        .args(["--retry-interval", &args.retry_interval.to_string()])
        .args(["--workspace", &args.workspace])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if args.force {
        command.arg("--force");
    }
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    super::mark_inherited_fds_cloexec();
    command.spawn().map(|child| child.id())
}

/// The `--deferred` entry point.
pub(crate) fn run(args: &LeaseEnsureArgs) {
    run_with(args, &SessionEnv::from_process(), DEFAULT_POLL);
}

/// Take this session's lock, wait (bounded) for the issue's owner lock, then
/// retry. `None` when a publisher for this session already exists.
///
/// The wait is bounded by this session's own liveness and the age cap, and an
/// owner whose session has died releases within one `poll` (its sleep is
/// sliced), so a live session's declined publish is never orphaned behind a
/// dead one. While the owner's session lives it keeps the lock: one publisher
/// per issue, and a live peer's lease is still never superseded.
pub(crate) fn run_with(
    args: &LeaseEnsureArgs,
    env: &SessionEnv,
    poll: Duration,
) -> Option<DeferredEnd> {
    let root = loom_daemon::repo_root::resolve_repo_root(&args.workspace).ok()?;
    let note = |line: &str| record(&args.workspace, args.issue, line);
    let Some(_session) = try_lock(&session_lock_path(&root, args.issue, args.watch_pid)) else {
        note("deferred publisher already waiting for this issue in this session; exiting");
        return None;
    };
    let pid = args.watch_pid;
    let ident = super::super::lease_renewer::start_identity(pid);
    let alive = || {
        loom_daemon::live_claim::pid_is_live_process(pid)
            && (ident.is_none() || super::super::lease_renewer::start_identity(pid) == ident)
    };
    let cap = if args.max_age == 0 {
        DEFAULT_MAX_AGE_SECS
    } else {
        args.max_age
    };
    let started = Instant::now();
    let mut announced = false;
    let _owner = loop {
        if let Some(f) = try_lock(&lock_path(&root, args.issue)) {
            break f;
        }
        if !announced {
            note(&format!(
                "deferred publisher for pid {pid} waiting for another session's publisher to stop"
            ));
            announced = true;
        }
        let end = if !alive() {
            Some(DeferredEnd::SessionEnded)
        } else if started.elapsed().as_secs() >= cap {
            Some(DeferredEnd::CapReached)
        } else {
            None
        };
        if let Some(end) = end {
            note(&end.describe(args.issue));
            return Some(end);
        }
        std::thread::sleep(poll);
    };
    note(&format!("deferred publisher for pid {pid} owns issue #{}'s retry", args.issue));
    // Time spent waiting comes off the cap, so the claim stays inside it.
    let remaining = cap.saturating_sub(started.elapsed().as_secs());
    if remaining == 0 {
        note(&DeferredEnd::CapReached.describe(args.issue));
        return Some(DeferredEnd::CapReached);
    }
    let owned = LeaseEnsureArgs {
        max_age: remaining,
        workspace: args.workspace.clone(),
        ..*args
    };
    let since = Instant::now();
    let end = retry_loop(
        &owned,
        env,
        || since.elapsed().as_secs(),
        |secs| sleep_while(Duration::from_secs(secs), poll, alive),
        alive,
        |o| note(&format!("deferred attempt: {}", o.describe(args.issue))),
    );
    note(&end.describe(args.issue));
    Some(end)
}

/// Sleep for `total`, waking every `poll` to stop early once `alive` fails.
fn sleep_while(total: Duration, poll: Duration, alive: impl Fn() -> bool) {
    let deadline = Instant::now() + total;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if left.is_zero() || !alive() {
            return;
        }
        std::thread::sleep(left.min(poll));
    }
}

pub(super) fn try_lock(path: &Path) -> Option<std::fs::File> {
    std::fs::create_dir_all(path.parent()?).ok()?;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .ok()?;
    // SAFETY: flock only takes an advisory lock on an fd this function owns.
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    (rc == 0).then_some(f)
}

/// The retry decision, with time, sleep and liveness injected so it is
/// deterministic under test. `elapsed` is seconds since the decline.
pub(crate) fn retry_loop(
    args: &LeaseEnsureArgs,
    env: &SessionEnv,
    mut elapsed: impl FnMut() -> u64,
    mut sleep: impl FnMut(u64),
    mut session_alive: impl FnMut() -> bool,
    mut on_attempt: impl FnMut(&Outcome),
) -> DeferredEnd {
    // A deferred publisher is always bounded, even under `--max-age 0`.
    let cap = if args.max_age == 0 {
        DEFAULT_MAX_AGE_SECS
    } else {
        args.max_age
    };
    let ttl_secs = (loom_daemon::claim_reconciliation::resolve_lease_ttl_minutes() * 60.0) as u64;
    let interval = args.retry_interval.max(1);
    loop {
        let wait = if elapsed() < ttl_secs + interval {
            interval
        } else {
            interval * BACKOFF_FACTOR
        };
        sleep(wait);
        if !session_alive() {
            return DeferredEnd::SessionEnded;
        }
        let now = elapsed();
        if now >= cap {
            return DeferredEnd::CapReached;
        }
        let attempt = LeaseEnsureArgs {
            // The renewer inherits what is left, so the whole claim stays
            // inside the original cap.
            max_age: cap - now,
            deferred: false,
            workspace: args.workspace.clone(),
            ..*args
        };
        let outcome = attempt.ensure(env);
        on_attempt(&outcome);
        if !outcome.is_retryable() {
            return DeferredEnd::Settled(outcome);
        }
    }
}
