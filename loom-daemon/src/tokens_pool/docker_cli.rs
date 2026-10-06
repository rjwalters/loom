//! Bounded `docker` CLI invocations (issue #10453).
//!
//! `ProcessContainerRunner` used to run `docker <args>` with no deadline, so a
//! wedged Docker daemon stalled the caller forever — for the periodic session
//! reconcile pass that meant the loop never ran again until the Loom daemon
//! restarted. Every captured `docker` call now has a wall-clock budget; on
//! expiry the local client is killed and the call fails, which callers treat
//! exactly like "Docker unavailable" (the reconcile pass backs off and takes
//! no action).

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

/// Budget for a read-only or quick `docker` call (`inspect`, `start`, `top`,
/// `rm`, `exec tmux …`).
pub const DOCKER_CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// Budget for `docker run`, which may first pull the session image.
pub const DOCKER_RUN_TIMEOUT: Duration = Duration::from_secs(600);
/// Budget for `docker stop` (its own `-t` grace plus headroom).
pub const DOCKER_STOP_TIMEOUT: Duration = Duration::from_secs(120);

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
/// once `timeout` elapses (the child is killed and reaped). Returns
/// `(success, stdout, stderr)`.
pub fn run_bounded(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<(bool, String, String)> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run `{program} {}`", args.join(" ")))?;
    // Drain both pipes concurrently so a chatty child can never block on a
    // full pipe while we poll for its exit.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            String::from_utf8_lossy(&bytes).into_owned()
        })
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "`{program} {}` timed out after {}s (container runtime unresponsive)",
                args.first().copied().unwrap_or_default(),
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    Ok((status.success(), stdout, stderr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hung_command_times_out_instead_of_stalling() {
        let started = Instant::now();
        let error = run_bounded("sleep", &["30"], Duration::from_millis(200)).unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");
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
