//! `loom-daemon restart` handler, including the `--drain`/`--abort-drain`
//! variants (Issue #4712 — split out of `main.rs`).

use anyhow::Result;
use std::path::Path;

use loom_daemon::types::{Request, Response};

use super::common::{query_daemon, resolve_socket_path};

/// Handle the `restart` subcommand (Issue #4054 — the supervised restart
/// primitive). Connects to the running daemon over its Unix socket and sends a
/// single `RestartDaemon` request.
///
/// When the daemon is supervised (launchd) it replies `DaemonRestart {
/// scheduled: true }` and then exits 0 for a `KeepAlive:SuccessfulExit`
/// relaunch — we print the ack and exit 0. When it is unsupervised (nohup /
/// Linux / `--foreground`) it replies `DaemonRestart { scheduled: false }` and
/// keeps running — we print the refusal and exit non-zero, so an operator or
/// Phase 3 can detect that no restart happened rather than assuming it did.
pub(crate) async fn handle_restart_command(
    drain: bool,
    timeout: Option<u64>,
    force_after_timeout: bool,
    abort_drain: bool,
    then_exit: bool,
) -> Result<()> {
    let socket_path = resolve_socket_path()?;

    if then_exit && !drain {
        eprintln!("--then-exit requires --drain (there is nothing to drain-then-exit without it)");
        std::process::exit(1);
    }

    // Issue #4995: on a launchd host, warn LOUDLY — before scheduling
    // anything — if the currently-bootstrapped launchd job's environment has
    // drifted from what the on-disk plist now declares. A plain restart's
    // `KeepAlive` relaunch reuses the already-bootstrapped job spec, so a
    // hand-edited plist would otherwise be picked up silently — never.
    // Best-effort: returns `None` (no output) on every non-launchd /
    // nothing-to-compare / no-drift outcome, and never blocks or fails the
    // restart itself.
    if let Some(warning) = loom_daemon::launchd_env_drift::check_launchd_env_drift() {
        eprintln!();
        eprintln!("{warning}");
        eprintln!();
    }

    // Drain-mode variants (Issue #4090) speak `DrainAndRestartDaemon` /
    // `AbortDrain` and expect a `DaemonDrain` reply; the plain restart keeps its
    // #4054 `RestartDaemon` / `DaemonRestart` contract byte-for-byte.
    if abort_drain {
        return handle_drain_reply(&socket_path, &Request::AbortDrain, DrainReplyKind::Abort).await;
    }
    if drain {
        return handle_drain_reply(
            &socket_path,
            &Request::DrainAndRestartDaemon {
                timeout_secs: timeout,
                force_after_timeout,
                then_exit,
            },
            // Issue #4521: the outcome line is rendered from the *reply*, not
            // from this flag — see `handle_drain_reply`.
            DrainReplyKind::Drain {
                requested_then_exit: then_exit,
            },
        )
        .await;
    }

    match query_daemon(&socket_path, &Request::RestartDaemon).await {
        Ok(Response::DaemonRestart {
            scheduled,
            supervisor,
            message,
        }) => {
            if scheduled {
                println!(
                    "loom-daemon restart scheduled (supervisor: {}).",
                    supervisor.as_deref().unwrap_or("unknown")
                );
                println!("{message}");
                Ok(())
            } else {
                eprintln!("loom-daemon did NOT restart: {message}");
                std::process::exit(1);
            }
        }
        Ok(Response::Error { message }) => {
            eprintln!("Daemon error: {message}");
            std::process::exit(1);
        }
        Ok(other) => {
            eprintln!("Unexpected response from daemon: {other:?}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Could not reach loom-daemon at {}: {e}", socket_path.display());
            eprintln!();
            eprintln!("Is the daemon running? Start it with:");
            eprintln!("  ./.loom/scripts/cli/loom-daemon-start.sh");
            std::process::exit(1);
        }
    }
}

/// Which drain-mode variant [`handle_drain_reply`] is rendering (Issue #4521).
///
/// Carries the *requested* `then_exit` only so the reply can be compared against
/// it and a version-skew warning raised — never so the outcome line can be
/// rendered from the request.
#[derive(Debug, Clone, Copy)]
enum DrainReplyKind {
    /// `AbortDrain` — no `then_exit` semantics at all.
    Abort,
    /// `DrainAndRestartDaemon` with the `--then-exit` flag as requested.
    Drain { requested_then_exit: bool },
}

/// Render the accepted-ack prefix for a `DaemonDrain` reply (Issue #4521).
///
/// **Load-bearing: derived from `reply_then_exit` (what the daemon says the
/// active drain will do), never from the requested flag.** The original defect
/// picked this line from the CLI's own `--then-exit` argument, so both a stale
/// daemon (which dropped the unknown wire field) and an already-in-progress
/// relaunch-drain produced a confident "will stop, not restart" promise for a
/// daemon that then exited 0 and was relaunched by launchd.
///
/// Pure so the request/reply divergence cases are unit-testable.
fn drain_accepted_prefix(kind: DrainReplyKind, reply_then_exit: bool) -> &'static str {
    match kind {
        DrainReplyKind::Abort => "loom-daemon drain aborted",
        DrainReplyKind::Drain { .. } => {
            if reply_then_exit {
                "loom-daemon drain-then-exit scheduled (will stop, not restart, once drained)"
            } else {
                "loom-daemon drain scheduled (will RESTART once drained)"
            }
        }
    }
}

/// The loud version-skew / escalation warning to print when the reply's
/// `then_exit` contradicts what was requested (Issue #4521), or `None` when they
/// agree (or there is nothing to compare, i.e. `--abort-drain`).
///
/// Pure for testability; the caller prints it to stderr.
fn drain_then_exit_mismatch_warning(
    kind: DrainReplyKind,
    reply_then_exit: bool,
) -> Option<&'static str> {
    let DrainReplyKind::Drain {
        requested_then_exit,
    } = kind
    else {
        return None;
    };
    if requested_then_exit == reply_then_exit {
        return None;
    }
    if requested_then_exit {
        Some(
            "WARNING: --then-exit was requested but the daemon reports it will RESTART when \
             drained, not stay down.\n\
             \x20 This host will come back up. Two known causes:\n\
             \x20   1. Version skew: the running daemon predates the --then-exit support and \
             silently dropped the flag — roll the daemon onto the current binary and re-issue.\n\
             \x20   2. A drain was already in progress and could not be escalated.\n\
             \x20 Verify with `loom-daemon status` before assuming this host is torn down.",
        )
    } else {
        Some(
            "WARNING: a plain --drain was requested but the daemon reports a then-exit drain is \
             active — it will STOP and stay down when drained, NOT restart.\n\
             \x20 then-exit is never downgraded. Use `loom-daemon restart --abort-drain` and \
             re-issue if a relaunch was intended.",
        )
    }
}

/// Shared reply handler for the drain-mode restart variants (Issue #4090): both
/// `DrainAndRestartDaemon` and `AbortDrain` answer with a `DaemonDrain` frame.
/// A refused request (unsupervised host, or abort with no drain in progress)
/// exits non-zero so a script can detect that nothing happened.
async fn handle_drain_reply(
    socket_path: &Path,
    request: &Request,
    kind: DrainReplyKind,
) -> Result<()> {
    let refused_prefix = match kind {
        DrainReplyKind::Abort => "no drain was in progress",
        DrainReplyKind::Drain { .. } => "loom-daemon did NOT drain",
    };
    match query_daemon(socket_path, request).await {
        Ok(Response::DaemonDrain {
            accepted,
            supervisor,
            in_flight,
            message,
            then_exit,
        }) => {
            if accepted {
                let sup_note = if then_exit {
                    "then-exit — no relaunch".to_string()
                } else {
                    supervisor.as_deref().unwrap_or("unknown").to_string()
                };
                let accepted_prefix = drain_accepted_prefix(kind, then_exit);
                println!("{accepted_prefix} (supervisor: {sup_note}, {in_flight} in-flight).");
                println!("{message}");
                if let Some(warning) = drain_then_exit_mismatch_warning(kind, then_exit) {
                    eprintln!();
                    eprintln!("{warning}");
                }
                Ok(())
            } else {
                eprintln!("{refused_prefix}: {message}");
                std::process::exit(1);
            }
        }
        Ok(Response::Error { message }) => {
            eprintln!("Daemon error: {message}");
            std::process::exit(1);
        }
        Ok(other) => {
            eprintln!("Unexpected response from daemon: {other:?}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Could not reach loom-daemon at {}: {e}", socket_path.display());
            eprintln!();
            eprintln!("Is the daemon running? Start it with:");
            eprintln!("  ./.loom/scripts/cli/loom-daemon-start.sh");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod drain_reply_render_tests {
    //! Issue #4521 AC2: the drain outcome line is rendered from the daemon's
    //! REPLY, and a request/reply divergence raises a loud warning. The
    //! divergence cases are exactly the two ways an operator gets lied to:
    //! version skew (a stale daemon drops the unknown `then_exit` wire field and
    //! defaults it to `false`) and an already-in-progress drain with a different
    //! terminal action.
    use super::{drain_accepted_prefix, drain_then_exit_mismatch_warning, DrainReplyKind};

    const DRAIN_THEN_EXIT: DrainReplyKind = DrainReplyKind::Drain {
        requested_then_exit: true,
    };
    const DRAIN_RELAUNCH: DrainReplyKind = DrainReplyKind::Drain {
        requested_then_exit: false,
    };

    #[test]
    fn prefix_follows_the_reply_not_the_request() {
        // Requested --then-exit, daemon confirms it: "will stop".
        assert!(drain_accepted_prefix(DRAIN_THEN_EXIT, true).contains("will stop, not restart"));
        // Requested --then-exit but the daemon reports a relaunch (stale daemon,
        // or an un-escalatable active drain): the CLI must NOT promise a stop.
        let skewed = drain_accepted_prefix(DRAIN_THEN_EXIT, false);
        assert!(skewed.contains("will RESTART"), "got: {skewed}");
        assert!(!skewed.contains("will stop"), "got: {skewed}");
        // Plain drain against an active teardown drain renders the stop wording.
        assert!(drain_accepted_prefix(DRAIN_RELAUNCH, true).contains("will stop, not restart"));
        assert!(drain_accepted_prefix(DRAIN_RELAUNCH, false).contains("will RESTART"));
    }

    #[test]
    fn abort_prefix_is_unchanged_and_never_warns() {
        assert_eq!(
            drain_accepted_prefix(DrainReplyKind::Abort, false),
            "loom-daemon drain aborted"
        );
        assert_eq!(drain_accepted_prefix(DrainReplyKind::Abort, true), "loom-daemon drain aborted");
        assert!(drain_then_exit_mismatch_warning(DrainReplyKind::Abort, false).is_none());
        assert!(drain_then_exit_mismatch_warning(DrainReplyKind::Abort, true).is_none());
    }

    #[test]
    fn agreement_produces_no_warning() {
        assert!(drain_then_exit_mismatch_warning(DRAIN_THEN_EXIT, true).is_none());
        assert!(drain_then_exit_mismatch_warning(DRAIN_RELAUNCH, false).is_none());
    }

    /// The incident shape: `--then-exit` requested, daemon replies `false`. The
    /// warning must name the version-skew cause explicitly — that is what a
    /// deploy operator needs in order to roll the daemon and re-issue rather
    /// than assume the host is torn down.
    #[test]
    fn requested_then_exit_but_reply_says_restart_warns_loudly() {
        let warning =
            drain_then_exit_mismatch_warning(DRAIN_THEN_EXIT, false).expect("divergence must warn");
        assert!(warning.starts_with("WARNING:"));
        assert!(warning.contains("Version skew"));
        assert!(warning.contains("RESTART"));
        assert!(warning.contains("loom-daemon status"));
    }

    #[test]
    fn plain_drain_against_active_teardown_warns() {
        let warning =
            drain_then_exit_mismatch_warning(DRAIN_RELAUNCH, true).expect("divergence must warn");
        assert!(warning.starts_with("WARNING:"));
        assert!(warning.contains("never downgraded"));
        assert!(warning.contains("--abort-drain"));
    }
}
