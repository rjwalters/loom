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
//! One deferred publisher per issue per checkout (an exclusive `flock`), and
//! every outcome is appended to `.loom/logs/lease-ensure/issue-<N>.log`,
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

/// The single-owner lock of the deferred publisher for `issue`.
fn lock_path(root: &Path, issue: u64) -> PathBuf {
    log_path(root, issue).with_extension("deferred.lock")
}

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
    let issue = args.issue;
    // Probe (and immediately release) the single-owner lock, so a second
    // call reports the publisher already waiting instead of a redundant one.
    if let Ok(root) = loom_daemon::repo_root::resolve_repo_root(&args.workspace) {
        if try_lock(&lock_path(&root, issue)).is_none() {
            return format!("issue #{issue}: a deferred publisher is already waiting for it");
        }
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => return format!("could not start a deferred publisher for issue #{issue}: {e}"),
    };
    let mut command = Command::new(exe);
    command
        .args(["lease", "ensure", &issue.to_string(), "--deferred"])
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
    match command.spawn() {
        Ok(child) => format!(
            "issue #{issue}: deferred publisher {} will retry every {}s while pid {} lives \
             (log: .loom/logs/lease-ensure/issue-{issue}.log)",
            child.id(),
            args.retry_interval,
            args.watch_pid
        ),
        Err(e) => format!("could not start a deferred publisher for issue #{issue}: {e}"),
    }
}

/// The `--deferred` entry point: take the per-issue lock, then retry.
pub(crate) fn run(args: &LeaseEnsureArgs) {
    let Ok(root) = loom_daemon::repo_root::resolve_repo_root(&args.workspace) else {
        return;
    };
    let Some(_held) = try_lock(&lock_path(&root, args.issue)) else {
        record(
            &args.workspace,
            args.issue,
            "deferred publisher already waiting for this issue; exiting",
        );
        return;
    };
    let ident = super::super::lease_renewer::start_identity(args.watch_pid);
    let pid = args.watch_pid;
    let started = Instant::now();
    let end = retry_loop(
        args,
        &SessionEnv::from_process(),
        || started.elapsed().as_secs(),
        |secs| std::thread::sleep(Duration::from_secs(secs)),
        || {
            loom_daemon::live_claim::pid_is_live_process(pid)
                && (ident.is_none() || super::super::lease_renewer::start_identity(pid) == ident)
        },
        |o| {
            record(
                &args.workspace,
                args.issue,
                &format!("deferred attempt: {}", o.describe(args.issue)),
            )
        },
    );
    record(&args.workspace, args.issue, &end.describe(args.issue));
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
