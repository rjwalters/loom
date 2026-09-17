//! The bounded in-band IPC probe (#4398).
//!
//! Process-exists and heartbeat-fresh are both **out-of-band**: neither ever
//! talks to the daemon over the socket it actually serves work on. Two
//! incidents show why that is not enough. In #4381 the installed binary was
//! replaced by a stub that answered `--version` then hung forever — a pid-alive
//! check passes against that indefinitely. On 2026-07-29 the production daemon
//! was alive AND writing a fresh heartbeat while every `status` round-trip
//! hung, because the heartbeat writer and the IPC accept loop are independent
//! `tokio` tasks and one can keep ticking while the other is wedged.
//!
//! So the probe is bounded twice: the CLI bounds its own connect and
//! round-trip, and this wraps the whole invocation in a hard external timeout,
//! so even a CLI that never returns cannot hang the tick.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use super::consts::{DEFAULT_PROBE_ARGS, DEFAULT_PROBE_TIMEOUT_SECS};
use super::env;

/// The exit code a timeout reports, matching `timeout(1)` and the shell
/// `bounded_run` fallback's own `143|137 -> 124` normalisation. Callers
/// distinguish "the CLI is wedged" from "the CLI answered an error".
pub const RC_TIMEOUT: i32 = 124;

/// What one round-trip attempt produced.
pub enum Attempt {
    /// The command ran to completion (or was killed at the deadline, which
    /// reports [`RC_TIMEOUT`]).
    Ran {
        rc: i32,
        output: String,
        bin: String,
    },
    /// The probe could not be attempted at all. Carries the reason verbatim —
    /// it is reported to the operator, and "why we did not look" matters as
    /// much as what we saw.
    Skipped { reason: String },
}

/// The socket's verdict about liveness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketVerdict {
    /// Something served the socket — authoritative evidence of a live daemon.
    Answered,
    /// Nothing is listening. Evidence of absence.
    Unreachable,
    /// The probe proved nothing either way. Explicitly NOT an outage (#5118).
    Indeterminate,
}

#[must_use]
pub fn probe_timeout_secs() -> u64 {
    super::env::num("LOOM_WATCHDOG_STATUS_PROBE_TIMEOUT_SECS", DEFAULT_PROBE_TIMEOUT_SECS)
}

#[must_use]
pub fn probe_args() -> Vec<String> {
    env::var("LOOM_WATCHDOG_IPC_PROBE_ARGS")
        .unwrap_or_else(|| DEFAULT_PROBE_ARGS.to_string())
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// Run the bounded round-trip.
///
/// Every skip reason below is a *degradation*, never a divergence: a watchdog
/// that pages because its own optional helper is missing is worse than one that
/// quietly keeps doing the other two checks.
pub fn attempt(socket_path: &Path, daemon_bin: Option<&Path>) -> Attempt {
    if env::var("LOOM_WATCHDOG_IPC_PROBE").is_some_and(|v| env::is_false(&v)) {
        return Attempt::Skipped {
            reason: "IPC probe disabled via LOOM_WATCHDOG_IPC_PROBE".to_string(),
        };
    }
    let Some(bin) = daemon_bin else {
        return Attempt::Skipped {
            reason: "no loom-daemon binary resolvable (LOOM_DAEMON_BIN unset, not on PATH, \
                     no in-repo build)"
                .to_string(),
        };
    };
    let args = probe_args();
    if args.is_empty() {
        return Attempt::Skipped {
            reason: "LOOM_WATCHDOG_IPC_PROBE_ARGS is empty — nothing to probe with".to_string(),
        };
    }

    let mut cmd = Command::new(bin);
    cmd.args(&args).env("LOOM_SOCKET_PATH", socket_path);
    let timeout = Duration::from_secs(probe_timeout_secs());

    match crate::sweep_registry::output_with_timeout(cmd, timeout) {
        // Killed at the deadline. The shell's bounded_run normalised its
        // SIGTERM/SIGKILL waits to 124 for exactly this case.
        Ok(None) => Attempt::Ran {
            rc: RC_TIMEOUT,
            output: String::new(),
            bin: bin.display().to_string(),
        },
        Ok(Some(out)) => {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            Attempt::Ran {
                // A signalled child has no code; 124 is the honest reading
                // since the shell's fallback mapped its signal waits there too.
                rc: out.status.code().unwrap_or(RC_TIMEOUT),
                output: text,
                bin: bin.display().to_string(),
            }
        }
        Err(e) => Attempt::Skipped {
            reason: format!("could not spawn the probe command: {e}"),
        },
    }
}

/// Classify an attempt into a liveness verdict plus operator-facing detail.
///
/// The ordering matters and is not arbitrary. An application-level error is
/// treated as **Answered**, because a daemon that returns an error has, by
/// definition, served the socket — that is stronger evidence of liveness than a
/// clean reply to a different question would be.
#[must_use]
pub fn classify(
    attempt: &Attempt,
    socket_path: &Path,
    timeout_secs: u64,
) -> (SocketVerdict, String) {
    let (rc, output, bin) = match attempt {
        Attempt::Skipped { reason } => {
            return (SocketVerdict::Indeterminate, format!("could not ask the socket: {reason}"));
        }
        Attempt::Ran { rc, output, bin } => (*rc, output.as_str(), bin.as_str()),
    };

    let base = Path::new(bin)
        .file_name()
        .map_or_else(|| bin.to_string(), |s| s.to_string_lossy().into_owned());
    let label = format!("{base} {}", probe_args().join(" "));
    let socket = socket_path.display();

    match rc {
        0 => {
            return (
                SocketVerdict::Answered,
                format!("'{label}' round-tripped over {socket} within {timeout_secs}s"),
            );
        }
        RC_TIMEOUT => {
            return (
                SocketVerdict::Indeterminate,
                format!(
                    "'{label}' did NOT return within the {timeout_secs}s probe budget — the CLI \
                     itself is wedged, so this tick proves nothing about the daemon either way"
                ),
            );
        }
        2 | 126 | 127 => {
            return (
                SocketVerdict::Indeterminate,
                format!(
                    "probe command '{label}' is unsupported by this binary (exit {rc}) — no \
                     in-band liveness signal available"
                ),
            );
        }
        _ => {}
    }

    let lower = output.to_lowercase();
    let first = output.lines().next().unwrap_or("");
    let has = |needles: &[&str]| needles.iter().any(|n| lower.contains(n));

    if has(&["alive-starting", "socket has not bound"]) {
        (
            SocketVerdict::Answered,
            "the daemon is alive and STARTING (its socket is not bound yet) — alive, not gone"
                .to_string(),
        )
    } else if has(&["alive-but-unresponsive"]) {
        (
            SocketVerdict::Indeterminate,
            "the CLI reports the daemon process as alive-but-unresponsive — alive-vs-gone is \
             UNDETERMINED from this tick"
                .to_string(),
        )
    } else if has(&[
        "could not reach loom-daemon",
        "connect timed out",
        "connect failed",
        "connection refused",
        "no such file",
    ]) {
        (
            SocketVerdict::Unreachable,
            format!("'{label}' could not reach anything at {socket} (exit {rc}): {first}"),
        )
    } else if has(&[
        "round-trip timed out",
        "closed the connection without responding",
    ]) {
        (
            SocketVerdict::Indeterminate,
            format!(
                "'{label}' connected but the round-trip did not complete (exit {rc}) — a wedge, \
                 not a proven absence: {first}"
            ),
        )
    } else {
        (
            SocketVerdict::Answered,
            format!(
                "'{label}' exited {rc} with an application-level error (the daemon ANSWERED, so \
                 the socket is served): {first}"
            ),
        )
    }
}

/// The IPC probe's verdict about **responsiveness**.
///
/// Deliberately a different type from [`SocketVerdict`], and the two classify
/// the SAME command output differently on purpose. They answer different
/// questions:
///
/// | output | [`SocketVerdict`] — "is anything there?" | [`IpcVerdict`] — "is it responsive?" |
/// |---|---|---|
/// | timed out (124) | `Indeterminate` — a wedged CLI is not evidence of absence | `Unresponsive` — a timeout IS unresponsiveness |
/// | `alive-but-unresponsive` | `Indeterminate` — alive-vs-gone unknown | `Unresponsive` — that is the answer |
/// | unrecognised error | `Answered` — it replied, so the socket is served | `Skipped` — no IPC-failure signature, not a hang |
///
/// Collapsing them into one classifier would be an easy and invisible mistake.
/// It is the same shape as #8011, where a port merged two reference parsers
/// that looked interchangeable and shipped three silent divergences. The tests
/// below assert the two DISAGREE on exactly these inputs, so a later
/// simplification cannot quietly unify them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcVerdict {
    /// A clean round-trip.
    Healthy,
    /// The daemon is alive but did not serve the request. Counts toward the
    /// consecutive-failure streak and the rolling window.
    Unresponsive,
    /// Nothing was learned. Explicitly NOT counted as a hang — a watchdog that
    /// escalates because its own optional probe could not run is worse than one
    /// that stays quiet about it.
    Skipped,
}

/// Startup grace: a process younger than this has not necessarily bound its
/// socket yet, so a failed probe against it is not evidence of a wedge (#4213).
pub const DEFAULT_PROBE_GRACE_SECS: u64 = 90;

/// Resolve the grace window through the shell's **two-level** fallback:
/// `LOOM_WATCHDOG_IPC_PROBE_GRACE_SECS`, else `LOOM_DAEMON_STARTUP_GRACE_SECS`,
/// else [`DEFAULT_PROBE_GRACE_SECS`].
///
/// The second tier matters: an operator who widened the daemon's startup grace
/// because their host is slow expects the watchdog's probe to respect the same
/// window. Reading only the first knob would leave the probe firing inside a
/// grace period the operator had explicitly extended, which is the false
/// positive #4213 is about.
#[must_use]
pub fn probe_grace_secs() -> u64 {
    if let Some(v) = super::env::var("LOOM_WATCHDOG_IPC_PROBE_GRACE_SECS")
        .filter(|s| s.bytes().all(|b| b.is_ascii_digit()) && !s.is_empty())
        .and_then(|s| s.parse().ok())
    {
        return v;
    }
    super::env::num("LOOM_DAEMON_STARTUP_GRACE_SECS", DEFAULT_PROBE_GRACE_SECS)
}

/// Classify an attempt as a responsiveness verdict.
///
/// `process_age_secs` gates the whole probe: inside the startup grace window the
/// answer is `Skipped` without even asking, because a daemon that has just
/// relaunched may not have bound its socket yet and counting that as a hang
/// would page on every restart (#4331/#4213).
#[must_use]
pub fn classify_ipc(
    attempt: &Attempt,
    process_age_secs: Option<u64>,
    grace_secs: u64,
    socket_path: &Path,
    timeout_secs: u64,
) -> (IpcVerdict, String) {
    if let Some(age) = process_age_secs {
        if age < grace_secs {
            return (
                IpcVerdict::Skipped,
                format!(
                    "process is only {age}s old (< {grace_secs}s startup grace) — socket may \
                     not be bound yet"
                ),
            );
        }
    }

    let (rc, output, bin) = match attempt {
        Attempt::Skipped { reason } => return (IpcVerdict::Skipped, reason.clone()),
        Attempt::Ran { rc, output, bin } => (*rc, output.as_str(), bin.as_str()),
    };

    let base = Path::new(bin)
        .file_name()
        .map_or_else(|| bin.to_string(), |s| s.to_string_lossy().into_owned());
    let label = format!("{base} {}", probe_args().join(" "));
    let socket = socket_path.display();

    match rc {
        0 => {
            return (
                IpcVerdict::Healthy,
                format!("'{label}' round-tripped over {socket} within {timeout_secs}s"),
            );
        }
        RC_TIMEOUT => {
            return (
                IpcVerdict::Unresponsive,
                format!(
                    "'{label}' did NOT return within the {timeout_secs}s probe budget (the \
                     CLI's own 5s connect + 5s round-trip bounds did not even fire)"
                ),
            );
        }
        2 | 126 | 127 => {
            return (
                IpcVerdict::Skipped,
                format!(
                    "probe command '{label}' is unsupported by this binary (exit {rc}) — \
                     skipping the IPC probe"
                ),
            );
        }
        _ => {}
    }

    let lower = output.to_lowercase();
    let first = output.lines().next().unwrap_or("");
    let has = |needles: &[&str]| needles.iter().any(|n| lower.contains(n));

    if has(&["alive-starting", "socket has not bound"]) {
        (
            IpcVerdict::Skipped,
            "probe reports the daemon is still STARTING (socket not bound yet) — not counted \
             as a hang"
                .to_string(),
        )
    } else if has(&[
        "could not reach loom-daemon",
        "round-trip timed out",
        "connect timed out",
        "connect failed",
        "closed the connection without responding",
        "alive-but-unresponsive",
    ]) {
        (
            IpcVerdict::Unresponsive,
            format!("'{label}' failed the IPC round-trip (exit {rc}): {first}"),
        )
    } else {
        (
            IpcVerdict::Skipped,
            format!(
                "probe exited {rc} without an IPC-failure signature (the daemon answered) — \
                 not counted as a hang: {first}"
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ran(rc: i32, output: &str) -> Attempt {
        Attempt::Ran {
            rc,
            output: output.to_string(),
            bin: "/usr/local/bin/loom-daemon".to_string(),
        }
    }

    fn verdict(a: &Attempt) -> SocketVerdict {
        classify(a, Path::new("/tmp/s.sock"), 15).0
    }

    fn ipc(a: &Attempt) -> IpcVerdict {
        classify_ipc(a, None, 120, Path::new("/tmp/s.sock"), 15).0
    }

    #[test]
    fn the_two_classifiers_deliberately_disagree_on_a_timeout() {
        // "Is anything there?" vs "is it responsive?". A wedged CLI is not
        // evidence the daemon is gone, but it IS a failed round-trip.
        let a = ran(RC_TIMEOUT, "");
        assert_eq!(verdict(&a), SocketVerdict::Indeterminate);
        assert_eq!(ipc(&a), IpcVerdict::Unresponsive);
    }

    #[test]
    fn the_two_classifiers_deliberately_disagree_on_alive_but_unresponsive() {
        let a = ran(1, "state: alive-but-unresponsive");
        assert_eq!(verdict(&a), SocketVerdict::Indeterminate, "alive-vs-gone is unknown");
        assert_eq!(ipc(&a), IpcVerdict::Unresponsive, "responsiveness is exactly what it reports");
    }

    #[test]
    fn the_two_classifiers_deliberately_disagree_on_an_application_error() {
        // It answered, so the socket is served -- but an unrecognised error is
        // not an IPC-failure signature, so it is not counted as a hang.
        let a = ran(1, "error: quarantine is empty");
        assert_eq!(verdict(&a), SocketVerdict::Answered);
        assert_eq!(ipc(&a), IpcVerdict::Skipped);
    }

    #[test]
    fn the_two_classifiers_agree_on_a_clean_round_trip() {
        let a = ran(0, "");
        assert_eq!(verdict(&a), SocketVerdict::Answered);
        assert_eq!(ipc(&a), IpcVerdict::Healthy);
    }

    #[test]
    fn the_grace_window_falls_back_through_two_knobs_to_ninety() {
        // Shell: ${LOOM_WATCHDOG_IPC_PROBE_GRACE_SECS:-${LOOM_DAEMON_STARTUP_GRACE_SECS:-90}}
        // The second tier is not decoration: an operator who widened the
        // daemon's startup grace for a slow host expects the probe to respect
        // the same window, or it fires inside a grace period they extended.
        assert_eq!(DEFAULT_PROBE_GRACE_SECS, 90);
    }

    #[test]
    fn a_young_process_is_never_counted_as_a_hang() {
        // #4213/#4331: a daemon that just relaunched may not have bound its
        // socket. Counting that as a wedge pages on every restart.
        let a = ran(RC_TIMEOUT, "");
        let (v, detail) = classify_ipc(&a, Some(5), 120, Path::new("/tmp/s.sock"), 15);
        assert_eq!(v, IpcVerdict::Skipped);
        assert!(detail.contains("startup grace"), "{detail}");
    }

    #[test]
    fn a_process_past_the_grace_window_is_probed_normally() {
        let a = ran(RC_TIMEOUT, "");
        assert_eq!(
            classify_ipc(&a, Some(500), 120, Path::new("/tmp/s.sock"), 15).0,
            IpcVerdict::Unresponsive
        );
    }

    #[test]
    fn an_unknown_process_age_does_not_grant_grace() {
        // A missing age must not be read as "young". The shell's `=~ ^[0-9]+$`
        // guard fails on an empty value and falls through to the probe.
        let a = ran(RC_TIMEOUT, "");
        assert_eq!(
            classify_ipc(&a, None, 120, Path::new("/tmp/s.sock"), 15).0,
            IpcVerdict::Unresponsive
        );
    }

    #[test]
    fn a_starting_daemon_is_not_counted_as_a_hang_by_either_classifier() {
        let a = ran(1, "state: alive-starting (socket has not bound yet)");
        assert_eq!(verdict(&a), SocketVerdict::Answered, "alive, not gone");
        assert_eq!(ipc(&a), IpcVerdict::Skipped, "starting is not wedged");
    }

    #[test]
    fn a_clean_round_trip_answers() {
        assert_eq!(verdict(&ran(0, "")), SocketVerdict::Answered);
    }

    #[test]
    fn a_timeout_proves_nothing_either_way() {
        // Critically NOT Unreachable: the CLI being wedged is not evidence the
        // daemon is gone. Reporting it as an outage is the #5118 mistake.
        assert_eq!(verdict(&ran(RC_TIMEOUT, "")), SocketVerdict::Indeterminate);
    }

    #[test]
    fn an_unsupported_subcommand_is_not_evidence_of_an_outage() {
        for rc in [2, 126, 127] {
            assert_eq!(verdict(&ran(rc, "")), SocketVerdict::Indeterminate, "rc {rc}");
        }
    }

    #[test]
    fn an_application_error_proves_the_socket_is_served() {
        // The daemon answered. That it answered with an error is irrelevant to
        // the question being asked, and is stronger evidence than silence.
        assert_eq!(verdict(&ran(1, "error: quarantine is empty")), SocketVerdict::Answered);
    }

    #[test]
    fn connection_refused_is_real_evidence_of_absence() {
        assert_eq!(
            verdict(&ran(1, "could not reach loom-daemon: connection refused")),
            SocketVerdict::Unreachable
        );
    }

    #[test]
    fn a_starting_daemon_is_alive_not_gone() {
        assert_eq!(
            verdict(&ran(1, "state: alive-starting (socket has not bound yet)")),
            SocketVerdict::Answered
        );
    }

    #[test]
    fn a_wedged_round_trip_is_not_a_proven_absence() {
        assert_eq!(verdict(&ran(1, "round-trip timed out after 5s")), SocketVerdict::Indeterminate);
    }

    #[test]
    fn a_skipped_probe_is_indeterminate_and_says_why() {
        let a = Attempt::Skipped {
            reason: "IPC probe disabled via LOOM_WATCHDOG_IPC_PROBE".into(),
        };
        let (v, detail) = classify(&a, Path::new("/tmp/s.sock"), 15);
        assert_eq!(v, SocketVerdict::Indeterminate);
        assert!(detail.contains("could not ask the socket"), "{detail}");
        assert!(detail.contains("LOOM_WATCHDOG_IPC_PROBE"), "{detail}");
    }

    #[test]
    fn matching_is_case_insensitive_like_the_shells_grep_i() {
        assert_eq!(
            verdict(&ran(1, "Could Not Reach Loom-Daemon: Connection Refused")),
            SocketVerdict::Unreachable
        );
    }
}
