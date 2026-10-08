//! Issue #10348: the `sweep-lease-renew.sh start` command a dispatched sweep
//! runs, extracted from `dispatch.rs` to hold that file at its file-size
//! ratchet. The `LOOM_SWEEP_LEASE_RENEW_SOURCE=dispatch` marker lets the loop
//! skip its per-cycle issue-state read.

use std::path::Path;
use std::process::{Command, Stdio};

/// Builds the `sweep-lease-renew.sh start` command for a dispatched sweep.
pub(super) fn lease_renewal_start_command(
    script: &Path,
    issue: u32,
    sweep_id: &str,
    child_pid: u32,
    host: &str,
    workspace_root: &Path,
) -> Command {
    let mut cmd = Command::new(script);
    cmd.arg("start")
        .arg(issue.to_string())
        .arg("--watch-pid")
        .arg(child_pid.to_string())
        // Exact-match targeting (#6485): without BOTH of these the loop
        // falls back to "newest lease wins" and can spend the sweep
        // renewing a PEER dispatcher's lease comment while this claim's
        // own `updated_at` never advances. The daemon knows both values
        // exactly — it published them itself in `write_lease_comment`.
        .arg("--host")
        .arg(host)
        .arg("--sweep-id")
        .arg(sweep_id)
        // #10348: marks a daemon-dispatched start. Its watched pid is the
        // sweep child, which already bounds the loop, so the loop skips the
        // per-cycle issue-state read (+12 calls/h per lease).
        .env("LOOM_SWEEP_LEASE_RENEW_SOURCE", "dispatch")
        // Same workspace every other forge mutation in this registry runs
        // in, so `gh` resolves this repo in a multi-workspace daemon
        // (#3928/#3937).
        .current_dir(workspace_root)
        .stdin(Stdio::null())
        // Piped and read by `run_lease_renewal_start` purely to capture the
        // loop pid `start` prints. Safe to read to EOF: the detached loop
        // redirects its OWN stdout to /dev/null, so nothing holds this pipe
        // open past `start`'s return.
        .stdout(Stdio::piped());
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatched_start_sets_source_marker() {
        let cmd = lease_renewal_start_command(
            Path::new("/w/.loom/scripts/sweep-lease-renew.sh"),
            42,
            "sweep-issue-42-1",
            999,
            "h",
            Path::new("/w"),
        );
        let marker = cmd
            .get_envs()
            .find(|(k, _)| *k == "LOOM_SWEEP_LEASE_RENEW_SOURCE")
            .and_then(|(_, v)| v);
        assert_eq!(marker, Some(std::ffi::OsStr::new("dispatch")));
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[..4], ["start", "42", "--watch-pid", "999"]);
    }
}
