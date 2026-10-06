//! Bounded `docker` CLI invocations (issue #10453).
//!
//! `ProcessContainerRunner` used to run `docker <args>` with no deadline, so a
//! wedged Docker daemon stalled the caller forever — for the periodic session
//! reconcile pass that meant the loop never ran again until the Loom daemon
//! restarted. Every captured `docker` call now has a wall-clock budget,
//! enforced by the crate's shared [`crate::proc_exec::run_bounded`] (which
//! also kills the child's process group). On expiry the call fails with the
//! typed [`DockerTimedOut`], which the reconcile pass treats exactly like
//! "Docker unavailable" — whether the timed-out call was a read or a
//! `docker start`/`run` — so it ends the pass instead of spending the budget
//! once per account.
//!
//! The operator CLI inherits these budgets too: `accounts session start` on
//! a first, slow image pull now fails after [`DOCKER_RUN_TIMEOUT`] (it was
//! unbounded); `docker pull` the image first if that is a risk.

use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, Result};

use crate::proc_exec::{self, Completion};

/// Budget for a read-only or quick `docker` call (`inspect`, `start`, `top`,
/// `rm`, `exec tmux …`).
pub const DOCKER_CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// Budget for `docker run`, which may first pull the session image.
pub const DOCKER_RUN_TIMEOUT: Duration = Duration::from_secs(600);
/// Budget for `docker stop` (its own `-t` grace plus headroom).
pub const DOCKER_STOP_TIMEOUT: Duration = Duration::from_secs(120);

/// A `docker` call hit its deadline: the runtime is unresponsive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerTimedOut {
    pub subcommand: String,
    pub secs: u64,
}

impl std::fmt::Display for DockerTimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "`docker {}` timed out after {}s (container runtime unresponsive)",
            self.subcommand, self.secs
        )
    }
}

impl std::error::Error for DockerTimedOut {}

/// The budget for `docker <args>`, by subcommand.
#[must_use]
pub fn timeout_for(args: &[&str]) -> Duration {
    match args.first().copied() {
        Some("run" | "pull") => DOCKER_RUN_TIMEOUT,
        Some("stop") => DOCKER_STOP_TIMEOUT,
        _ => DOCKER_CALL_TIMEOUT,
    }
}

/// Run `program <args>` with stdin closed, capturing stdout/stderr, failing
/// with [`DockerTimedOut`] once `timeout` elapses. Returns
/// `(success, stdout, stderr)`.
pub fn run_bounded(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<(bool, String, String)> {
    let mut command = Command::new(program);
    command.args(args).stdin(Stdio::null());
    let subcommand = args.first().copied().unwrap_or_default();
    match proc_exec::run_bounded(command, timeout)
        .map_err(|e| anyhow!("failed to run `{program} {subcommand}`: {e}"))?
    {
        Completion::Exited(output) => Ok((
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )),
        Completion::TimedOut { .. } => Err(DockerTimedOut {
            subcommand: subcommand.to_string(),
            secs: timeout.as_secs(),
        }
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn a_hung_command_times_out_instead_of_stalling() {
        let started = Instant::now();
        let error = run_bounded("sleep", &["30"], Duration::from_millis(200)).unwrap_err();
        assert!(error.downcast_ref::<DockerTimedOut>().is_some(), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn output_and_status_are_captured() {
        let (ok, out, err) =
            run_bounded("sh", &["-c", "echo out; echo err >&2; exit 3"], DOCKER_CALL_TIMEOUT)
                .unwrap();
        assert!(!ok);
        assert_eq!(out.trim(), "out");
        assert_eq!(err.trim(), "err");
    }

    #[test]
    fn budgets_by_subcommand() {
        assert_eq!(timeout_for(&["run", "x"]), DOCKER_RUN_TIMEOUT);
        assert_eq!(timeout_for(&["stop", "-t", "15", "c"]), DOCKER_STOP_TIMEOUT);
        assert_eq!(timeout_for(&["inspect", "c"]), DOCKER_CALL_TIMEOUT);
    }
}
