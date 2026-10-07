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
//! The operator CLI inherits these budgets too. So that a first, slow image
//! pull does not fail `accounts session start` at [`DOCKER_RUN_TIMEOUT`],
//! the operator commands pull a missing image first, under the longer
//! [`DOCKER_PULL_TIMEOUT`] ([`ensure_image_for_operator`], #10661). The
//! reconcile pass does not: a pull there would hold the whole pass.
//!
//! Ctrl-C/SIGTERM sent to an operator command is forwarded to the running
//! `docker` child ([`super::operator_interrupt`], #10661); for the daemon,
//! which never installs that handler, nothing changes.

use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};

use super::operator_interrupt::{self, Interrupted};
use crate::proc_exec::{self, Completion};

/// Budget for a read-only or quick `docker` call (`inspect`, `start`, `top`,
/// `rm`, `exec tmux …`).
pub const DOCKER_CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// Budget for `docker run`, which may first pull the session image.
pub const DOCKER_RUN_TIMEOUT: Duration = Duration::from_secs(600);
/// Budget for `docker stop` (its own `-t` grace plus headroom).
pub const DOCKER_STOP_TIMEOUT: Duration = Duration::from_secs(120);
/// Budget for the operator's explicit pull of a missing session image.
pub const DOCKER_PULL_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// How long a `docker` child gets to exit after an operator's signal is
/// forwarded to it, before its group is killed.
pub const FORWARD_GRACE: Duration = Duration::from_secs(10);

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
///
/// An operator's SIGINT/SIGTERM ([`operator_interrupt`]) is forwarded to the
/// child, and fails the call with [`Interrupted`]; once one is pending, no
/// further call starts.
pub fn run_bounded(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<(bool, String, String)> {
    run_bounded_with(program, args, timeout, &operator_interrupt::pending)
}

/// [`run_bounded`] with an explicit pending-signal probe (tests).
pub(crate) fn run_bounded_with(
    program: &str,
    args: &[&str],
    timeout: Duration,
    pending: &dyn Fn() -> Option<i32>,
) -> Result<(bool, String, String)> {
    if let Some(signal) = pending() {
        return Err(Interrupted(signal).into());
    }
    let mut command = Command::new(program);
    command.args(args).stdin(Stdio::null());
    let subcommand = args.first().copied().unwrap_or_default();
    let completion = proc_exec::run_bounded_forwarding(command, timeout, FORWARD_GRACE, pending);
    // A call that still succeeded stands; one that failed (or was killed
    // after the grace) once a signal was forwarded failed because of it.
    if let (Some(signal), false) = (pending(), completion.as_ref().is_ok_and(Completion::succeeded))
    {
        return Err(Interrupted(signal).into());
    }
    match completion.map_err(|e| anyhow!("failed to run `{program} {subcommand}`: {e}"))? {
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

/// Operator `session start`/`shell` only (#10661): make sure `image` is
/// present before the bounded `docker run`, pulling it under
/// [`DOCKER_PULL_TIMEOUT`] when it is not, with a note on stderr.
///
/// # Errors
/// When Docker cannot be asked, or the pull fails or times out.
pub fn ensure_image_for_operator(image: &str) -> Result<()> {
    ensure_image_with("docker", image, &operator_interrupt::pending, DOCKER_PULL_TIMEOUT)
}

pub(crate) fn ensure_image_with(
    program: &str,
    image: &str,
    pending: &dyn Fn() -> Option<i32>,
    pull_timeout: Duration,
) -> Result<()> {
    let probe = ["image", "inspect", "--format", "{{.Id}}", image];
    let (present, _, _) = run_bounded_with(program, &probe, DOCKER_CALL_TIMEOUT, pending)?;
    if present {
        return Ok(());
    }
    eprintln!(
        "note: pulling {image} before starting the session (not present on this host; allowing \
         up to {} min)",
        pull_timeout.as_secs() / 60
    );
    let (pulled, _, stderr) = run_bounded_with(program, &["pull", image], pull_timeout, pending)?;
    if !pulled {
        bail!("docker pull {image} failed: {}", stderr.trim());
    }
    Ok(())
}

#[cfg(test)]
#[path = "docker_cli_interrupt_tests.rs"]
mod interrupt_tests;

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
