//! Bounded `git remote get-url origin` lookup (#9715).
//!
//! The one remote-URL read the daemon-resident sites share
//! (`credential_preflight`, `forge_etag_store`, `forge_events::poll_gate`,
//! `metrics_collector`, `serve`, `tokens_pool::private_workspace::dispatch`),
//! so a `git` that hangs costs at most [`FORGE_CMD_TIMEOUT`] instead of
//! stalling its caller indefinitely.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use super::FORGE_CMD_TIMEOUT;
use crate::cmd_out::{run_command, CmdOutcome, Unavailable};

/// Get the `origin` remote URL from `cwd`, or `None`.
///
/// Bounded by [`FORGE_CMD_TIMEOUT`] with stdin nulled: a `git` that hangs
/// (e.g. walking up from a directory on a blocked external volume) yields
/// `None` after the deadline, with one line on stderr naming the directory.
pub(crate) fn get_remote_url(cwd: &Path) -> Option<String> {
    remote_url_with("git", cwd, FORGE_CMD_TIMEOUT)
}

/// [`get_remote_url`] with the program and deadline injectable, so the
/// timeout path is testable with a stub that sleeps past a short budget.
fn remote_url_with(program: &str, cwd: &Path, timeout: Duration) -> Option<String> {
    let mut cmd = Command::new(program);
    cmd.args(["remote", "get-url", "origin"])
        .current_dir(cwd)
        .stdin(Stdio::null());
    let outcome = run_command(cmd, timeout);
    if let CmdOutcome::Unavailable(Unavailable::TimedOut { after, .. }) = &outcome {
        eprintln!(
            "loom-daemon: `git remote get-url origin` in {} timed out after {after:?}",
            cwd.display()
        );
    }
    let out = outcome.ok_output()?;
    let url = std::str::from_utf8(&out.stdout).ok()?.trim().to_string();
    if url.is_empty() {
        None
    } else {
        Some(url)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// #9715: a `git` that hangs (stub sleeping far past the budget) makes
    /// the remote-URL lookup return `None` within the deadline instead of
    /// blocking its caller. The stub is a file named `git` invoked by path,
    /// so no process-global `PATH` mutation is needed.
    #[cfg(unix)]
    #[test]
    fn remote_url_lookup_times_out_on_hung_git() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let stub = dir.path().join("git");
        std::fs::write(&stub, "#!/bin/sh\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let budget = std::time::Duration::from_millis(500);
        let start = std::time::Instant::now();
        let url = remote_url_with(stub.to_str().unwrap(), dir.path(), budget);
        let elapsed = start.elapsed();

        assert_eq!(url, None);
        assert!(elapsed < std::time::Duration::from_secs(10), "took {elapsed:?}");
    }

    /// The injectable path answers normally when `git` does (#9715 AC3).
    #[cfg(unix)]
    #[test]
    fn remote_url_lookup_returns_trimmed_url() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let stub = dir.path().join("git");
        std::fs::write(&stub, "#!/bin/sh\necho '  git@github.com:o/r.git  '\n").unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let url =
            remote_url_with(stub.to_str().unwrap(), dir.path(), std::time::Duration::from_secs(10));
        assert_eq!(url.as_deref(), Some("git@github.com:o/r.git"));
    }
}
