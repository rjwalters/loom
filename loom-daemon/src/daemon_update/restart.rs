//! The restart half: the supervised `restart` IPC primitive, the
//! verification that the supervisor ACTUALLY relaunched, the self-heal
//! fallbacks, and the bare pid-file/nohup stop+start.
//!
//! Every success line in here is unreachable until a NEW, live pid has been
//! observed, and that is the whole point. Both supervisors can honour the
//! daemon's `restart` ack and then fail to relaunch:
//!
//! * **launchd (#4232)**: on 2026-07-28 the ack was honest (the supervised
//!   daemon exited 0 per its #4054 contract) but `KeepAlive:SuccessfulExit`
//!   never fired. The script reported success while the daemon stayed down
//!   for ~4 minutes until an operator ran `launchctl kickstart` by hand.
//! * **systemd (#4950/#5119)**: on 2026-08-02 the daemon exited 0, but the
//!   unit's stop job exceeded `TimeoutStopSec` reaping sweep children still in
//!   the cgroup, so systemd marked it `failed (Result: timeout)` — and
//!   `Restart=on-success` does not match `Result=timeout`. The host was
//!   daemonless until a hand `reset-failed && start`.
//!
//! The drain fail-safe (#5138) is the one case where NOT relaunching is the
//! correct outcome: a drain that times out without `--force-after-timeout`
//! leaves the pre-update binary running rather than cancelling in-flight
//! sweeps, and the self-heal must NEVER fire there — kickstarting would force
//! exactly the sweep-cancelling restart the fail-safe exists to prevent.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::out;
use super::supervisor::Detected;
use super::util;

/// `build_restart_invoke_args` — `restart`, plus the drain passthrough.
///
/// All drain SEMANTICS (pause dispatch, wait for in-flight sweeps, the
/// fail-safe refusal on timeout) already live in `loom-daemon restart --drain`
/// (#4090). This only decides WHETHER to pass `--drain` and threads the
/// operator's own `--timeout` / `--force-after-timeout` through unchanged.
#[must_use]
pub fn build_restart_invoke_args(
    drain: bool,
    drain_timeout: Option<&str>,
    force_after_timeout: bool,
) -> Vec<String> {
    let mut args = vec!["restart".to_string()];
    if drain {
        args.push("--drain".to_string());
        if let Some(t) = drain_timeout {
            args.push("--timeout".to_string());
            args.push(t.to_string());
        }
        if force_after_timeout {
            args.push("--force-after-timeout".to_string());
        }
    }
    args
}

/// `${LOOM_DAEMON_RESTART_POLL_INTERVAL:-1}` — may be fractional (e.g. `0.5`),
/// matching `sleep`'s own support. An unparseable value falls back to 1s, as
/// `sleep` would have errored and the loop would have spun.
#[must_use]
pub fn poll_interval() -> Duration {
    let raw =
        util::env_non_empty("LOOM_DAEMON_RESTART_POLL_INTERVAL").unwrap_or_else(|| "1".to_string());
    Duration::from_secs_f64(raw.parse::<f64>().unwrap_or(1.0).max(0.0))
}

/// Parse a poll-window string for timing. The STRING is what the messages
/// interpolate, so it is carried separately and never re-rendered from this.
#[must_use]
pub fn secs_of(raw: &str) -> u64 {
    raw.parse::<u64>().unwrap_or(0)
}

/// `wait_for_new_launchd_pid <pre_pid> <timeout_secs> <interval>`.
///
/// A pid that merely DIFFERS but is already dead (a race artifact), or that
/// still equals `pre_pid` (the old process lingering mid-teardown during the
/// poll window), must NEVER be mistaken for a successful relaunch.
pub fn wait_for_new_launchd_pid(
    sup: &Detected,
    pre_pid: &str,
    timeout_secs: u64,
    interval: Duration,
) -> Option<String> {
    poll_for_new_pid(timeout_secs, interval, || {
        let cur = sup.launchd_job_pid().unwrap_or_default();
        accept_pid(&cur, pre_pid, false)
    })
}

/// `wait_for_new_systemd_pid` — the same contract, plus: a reported `0` (not
/// running) never counts.
pub fn wait_for_new_systemd_pid(
    sup: &Detected,
    pre_pid: &str,
    timeout_secs: u64,
    interval: Duration,
) -> Option<String> {
    poll_for_new_pid(timeout_secs, interval, || {
        let cur = sup.systemd_unit_pid().unwrap_or_default();
        accept_pid(&cur, pre_pid, true)
    })
}

fn accept_pid(cur: &str, pre_pid: &str, reject_zero: bool) -> Option<String> {
    if cur.is_empty() || cur == pre_pid {
        return None;
    }
    if reject_zero && cur == "0" {
        return None;
    }
    let alive = cur.parse::<i32>().is_ok_and(util::pid_alive);
    if alive {
        Some(cur.to_string())
    } else {
        None
    }
}

/// The shared loop shape: CHECK first, then test the deadline, then sleep —
/// so a zero-second window still performs exactly one check.
fn poll_for_new_pid<F>(timeout_secs: u64, interval: Duration, mut probe: F) -> Option<String>
where
    F: FnMut() -> Option<String>,
{
    let start = Instant::now();
    loop {
        if let Some(pid) = probe() {
            return Some(pid);
        }
        if start.elapsed().as_secs() >= timeout_secs {
            return None;
        }
        std::thread::sleep(interval);
    }
}

/// `wait_for_systemd_stop_settle <timeout_secs> <interval>` — poll
/// `ActiveState` until it SETTLES out of a transitional state.
///
/// On a busy host a `loom-daemon restart` exits the daemon 0, but the unit's
/// stop job can sit in `deactivating (stop-sigterm)` for the full
/// `TimeoutStopSec` while it SIGTERMs — then SIGKILLs — the sweep/role
/// children still in the service cgroup. A STALE unit (rendered before
/// #4862's `KillMode=mixed` fix) drags that out to systemd's 90s default, far
/// past the #4950 pid poll's 30s. Reading `ActiveState` ONCE right after that
/// poll expired saw `deactivating` (not yet `failed`) and fell through to
/// "refusing to guess", leaving the daemon down.
///
/// Returns the settled state, and whether it settled within the window.
pub fn wait_for_systemd_stop_settle(
    sup: &Detected,
    timeout_secs: u64,
    interval: Duration,
) -> (String, bool) {
    let start = Instant::now();
    loop {
        let state = sup.systemd_unit_active_state();
        if !is_transitional(&state) {
            return (state, true);
        }
        if start.elapsed().as_secs() >= timeout_secs {
            return (state, false);
        }
        std::thread::sleep(interval);
    }
}

/// The transitional `ActiveState` values the settle-wait keeps waiting on.
#[must_use]
pub fn is_transitional(state: &str) -> bool {
    matches!(
        state,
        "deactivating"
            | "activating"
            | "reloading"
            | "deactivating-sigterm"
            | "deactivating-sigkill"
    )
}

/// `log_launchd_diagnostics` — the breadcrumb that tells "launchd never
/// relaunched" apart from "the daemon crashed immediately after relaunching".
pub fn log_launchd_diagnostics(sup: &Detected) {
    out::warn(&format!("launchctl print {} diagnostic snapshot:", sup.launchd_service));
    for line in sup.launchctl_print_combined().lines() {
        out::warn(&format!("  {line}"));
    }
}

/// `log_systemd_diagnostics` — including the `Active:`/`Result:` line the
/// #4950 incident's journal excerpt was read off of.
pub fn log_systemd_diagnostics(sup: &Detected) {
    out::warn(&format!("systemctl --user status {} diagnostic snapshot:", sup.systemd_unit));
    for line in sup.systemctl_status_combined().lines() {
        out::warn(&format!("  {line}"));
    }
}

/// `drain_roll_still_armed <bin>` (#6007) — does the still-running daemon
/// report a drain STILL in progress after our pid poll expired?
///
/// The poll window is the drain's own timeout + 60s, so by here the deadline
/// has passed: a pre-#6007 daemon would have cleared the flag and be reporting
/// `draining: false`. Still-draining therefore means the daemon RETAINED the
/// roll — it keeps dispatch paused and re-arms the restart when the in-flight
/// set reaches zero. That changes the exit-8 advice completely: "re-run once
/// the sweeps finish" is exactly the operator babysitting #6007 removed, and
/// on a busy host re-running reproduces the same outcome.
///
/// Deliberately conservative: any failure to reach or parse the status
/// (pre-#6007 daemon, no `--json`, absent binary) answers `false`, so the
/// message falls back to the historical wording rather than promising a
/// convergence the running daemon may not implement. Uses a textual scan, not
/// a JSON parse — the shell used `grep`, never `jq`, because `jq` cannot be
/// assumed on a fleet worker.
#[must_use]
pub fn drain_roll_still_armed(bin: &Path) -> bool {
    if !util::is_executable(bin) {
        return false;
    }
    let Some(out) = Command::new(bin)
        .args(["status", "--json"])
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
    else {
        return false;
    };
    let json = String::from_utf8_lossy(&out.stdout).to_string();
    if json.is_empty() {
        return false;
    }
    // `tr -d ' \n'` then `grep -q '"drain":{[^}]*"draining":true'`.
    let squeezed: String = json.chars().filter(|c| *c != ' ' && *c != '\n').collect();
    let Some(idx) = squeezed.find("\"drain\":{") else {
        return false;
    };
    let rest = &squeezed[idx + "\"drain\":{".len()..];
    // `[^}]*"draining":true` — the flag must appear inside the `drain` object,
    // i.e. before the first `}` closes it. A plain `contains` over the whole
    // document would also match a `"draining":true` in some later object.
    let block_end = rest.find('}').unwrap_or(rest.len());
    rest[..block_end].contains("\"draining\":true")
}

/// Run the provisioned binary's `restart` subcommand with stdio inherited.
#[must_use]
pub fn invoke_restart(provision_target: &Path, args: &[String]) -> bool {
    Command::new(provision_target)
        .args(args)
        .status()
        .is_ok_and(|s| s.success())
}

/// `launchctl kickstart <service>` — plain, NEVER `-k`, so a daemon that DID
/// relaunch during the race window above is never killed.
pub fn launchctl_kickstart(sup: &Detected) {
    let _ = Command::new("launchctl")
        .arg("kickstart")
        .arg(&sup.launchd_service)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// `systemctl --user reset-failed <unit> && systemctl --user start <unit>`.
pub fn systemd_self_heal(sup: &Detected) {
    for verb in ["reset-failed", "start"] {
        let _ = Command::new("systemctl")
            .args(["--user", verb, &sup.systemd_unit])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_drain_passthrough_is_exactly_what_the_operator_asked_for() {
        assert_eq!(build_restart_invoke_args(false, Some("5"), true), vec!["restart"]);
        assert_eq!(build_restart_invoke_args(true, None, false), vec!["restart", "--drain"]);
        assert_eq!(
            build_restart_invoke_args(true, Some("5"), true),
            vec![
                "restart",
                "--drain",
                "--timeout",
                "5",
                "--force-after-timeout"
            ]
        );
    }

    #[test]
    fn a_dead_or_unchanged_pid_is_never_a_relaunch() {
        // The current process is alive, so it stands in for "a new, live pid".
        let me = std::process::id().to_string();
        assert_eq!(accept_pid(&me, "4242", false).as_deref(), Some(me.as_str()));
        assert_eq!(accept_pid(&me, &me, false), None, "unchanged pid");
        assert_eq!(accept_pid("", "4242", false), None, "no pid reported");
        assert_eq!(accept_pid("0", "4242", true), None, "systemd MainPID=0");
        // A pid that differs but is not alive: 2^31-1 is not a live process.
        assert_eq!(accept_pid("2147483647", "4242", false), None);
    }

    #[test]
    fn the_transitional_set_is_the_one_the_settle_wait_keys_off() {
        for s in [
            "deactivating",
            "activating",
            "reloading",
            "deactivating-sigterm",
            "deactivating-sigkill",
        ] {
            assert!(is_transitional(s), "{s}");
        }
        for s in ["active", "inactive", "failed", ""] {
            assert!(!is_transitional(s), "{s}");
        }
    }
}
