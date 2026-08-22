//! `loom-daemon restart` handler, including the `--drain`/`--abort-drain`
//! variants (Issue #4712 — split out of `main.rs`).

use anyhow::Result;
use std::path::Path;

use loom_daemon::launchd_reload;
use loom_daemon::restart_verify::{self, RelaunchOutcome, Supervisor};
use loom_daemon::types::{Request, Response};

use super::common::{query_daemon, resolve_socket_path};

/// Handle the `restart` subcommand (Issue #4054 — the supervised restart
/// primitive). Connects to the running daemon over its Unix socket and sends a
/// single `RestartDaemon` request.
///
/// When the daemon is supervised (launchd / systemd) it replies `DaemonRestart
/// { scheduled: true }` and then exits 0 for a `KeepAlive:SuccessfulExit` /
/// `Restart=on-success` relaunch — we print the ack and exit 0. When it is
/// unsupervised (nohup / Linux without a unit / `--foreground`) it replies
/// `DaemonRestart { scheduled: false }` and keeps running — we print the refusal
/// and exit non-zero, so an operator or Phase 3 can detect that no restart
/// happened rather than assuming it did.
///
/// Issue #5119 adds two things around that exchange, because the ack alone
/// proved not to be enough on a busy systemd host: a **pre-restart stale-unit
/// warning**, and a **post-restart relaunch verification with supervisor
/// self-heal** ([`verify_relaunch`]).
pub(crate) async fn handle_restart_command(
    drain: bool,
    timeout: Option<u64>,
    force_after_timeout: bool,
    abort_drain: bool,
    then_exit: bool,
    reload_supervisor: bool,
) -> Result<()> {
    // Issue #6682: `--reload-supervisor` is a local, launchctl-shelling CLI
    // operation — it boots the launchd job out and back in so a hand-edited
    // plist's EnvironmentVariables actually takes effect (a plain restart's
    // KeepAlive relaunch never re-reads the plist — see launchd_env_drift.rs).
    // It never talks to the running daemon's IPC socket at all: the whole
    // point is bootstrapping a FRESH process from a FRESH plist read,
    // independent of whatever the currently-running daemon can report over
    // its socket — so this branches out before any of the socket-based logic
    // below even resolves a socket path.
    if reload_supervisor {
        return handle_reload_supervisor_command(drain, abort_drain, then_exit);
    }

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

    // Issue #5119: capture the supervisor's pre-restart pid BEFORE the request,
    // so the post-restart poll can tell "relaunched onto a NEW pid" apart from
    // "the old process never moved". Cheap (one bounded `systemctl show` /
    // `launchctl print`) and best-effort — `None` never blocks the restart.
    //
    // The supervisor is *guessed* locally here only because the ack has not
    // arrived yet (and this CLI process, an operator's shell, usually has no
    // `LOOM_DAEMON_SUPERVISOR` of its own); the ack's own `supervisor` field is
    // the authority for the verification itself.
    let probe_supervisor = restart_verify::probe_host_supervisor();
    let pre_restart_pid = probe_supervisor.and_then(restart_verify::pre_restart_pid);
    if let (Some(sup), Some(_)) = (probe_supervisor, pre_restart_pid) {
        // #5119: a live unit still on the pre-#4862 `KillMode=control-group` is
        // exactly what turned the 2026-08-03 roll into a four-minute outage —
        // say so BEFORE exiting, while the operator can still choose `--drain`.
        // Gated on a resolved pre-restart pid so a host with no such unit at all
        // (where `systemctl show` happily reports the *default* KillMode for a
        // unit that does not exist) can never produce a phantom warning.
        restart_verify::warn_if_stale_unit(sup);
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
                verify_relaunch(supervisor.as_deref(), pre_restart_pid).await;
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

/// Handle `restart --reload-supervisor` (Issue #6682) — a local
/// `launchctl bootout` + bounded-retry `launchctl bootstrap`, never a request
/// to the running daemon (see [`loom_daemon::launchd_reload`]).
///
/// Refuses the drain-mode flags outright: they only make sense against the
/// socket-based restart request this variant deliberately bypasses.
fn handle_reload_supervisor_command(drain: bool, abort_drain: bool, then_exit: bool) -> Result<()> {
    if drain || abort_drain || then_exit {
        eprintln!(
            "--reload-supervisor cannot be combined with --drain/--abort-drain/--then-exit — it \
             is a local launchd bootout+bootstrap, not a request to the running daemon over its \
             socket. In-flight sweeps survive a launchd bootout/bootstrap on their own (they \
             reparent to launchd, ppid=1), so no drain is required first."
        );
        std::process::exit(1);
    }
    let outcome = launchd_reload::reload_launchd_supervisor();
    eprintln!("{}", outcome.render());
    if outcome.is_ok() {
        Ok(())
    } else {
        std::process::exit(1);
    }
}

/// Verify the supervisor actually relaunched the daemon after an accepted
/// `RestartDaemon`, and self-heal if it did not (Issue #5119).
///
/// # Why this does not change the exit code
///
/// **`loom-daemon restart`'s exit status means "the running daemon accepted the
/// request", and two callers already branch on exactly that**:
/// `loom-daemon-update.sh` treats a non-zero as "the running daemon did NOT
/// accept the restart request" (its #4118 refused-restart fallback, which
/// re-renders the plist/unit), and `scripts/loom`'s `loom_cmd_restart` treats
/// it as "supervised restart unavailable" and falls back to stop-then-start.
/// Turning an *accepted-but-unverified* restart into a non-zero exit would send
/// both down a path whose diagnosis is simply wrong.
///
/// So the verdict is reported loudly on stderr instead — and, far more
/// importantly, the **self-heal has already run** by the time it is printed.
/// `loom-daemon-update.sh` keeps its own richer post-restart verification (it
/// additionally reports the provisioned commit) and remains the authority on
/// the update path's exit status; this one exists so every OTHER caller of the
/// primitive stops being fire-and-forget.
async fn verify_relaunch(supervisor: Option<&str>, pre_restart_pid: Option<u32>) {
    if !restart_verify::verification_enabled(
        std::env::var(restart_verify::VERIFY_ENV).ok().as_deref(),
    ) {
        return;
    }
    let Some(sup) = supervisor.and_then(Supervisor::parse) else {
        // A `scheduled: true` ack from a supervisor this build does not know how
        // to probe. Nothing to verify against — never treat that as a failure.
        return;
    };
    let poll_secs = restart_verify::resolve_secs(
        std::env::var(restart_verify::POLL_SECS_ENV).ok().as_deref(),
        restart_verify::DEFAULT_POLL_SECS,
    );
    // The poll sleeps for up to poll_secs + recovery_secs (default 45s) and
    // shells out repeatedly, so it goes on a blocking thread rather than
    // stalling the runtime worker this CLI is running on.
    let outcome = match tokio::task::spawn_blocking(move || {
        restart_verify::verify_and_heal(sup, pre_restart_pid)
    })
    .await
    {
        Ok(outcome) => outcome,
        Err(e) => RelaunchOutcome::NotRelaunched {
            detail: format!("the verification task itself failed to complete: {e}"),
        },
    };
    let rendered = outcome.render(sup.as_str(), poll_secs);
    match outcome {
        RelaunchOutcome::Relaunched { healed: false, .. } | RelaunchOutcome::Skipped { .. } => {
            println!("{rendered}");
        }
        _ => {
            eprintln!();
            eprintln!("{rendered}");
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
