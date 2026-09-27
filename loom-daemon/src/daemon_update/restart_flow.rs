//! The restart branch itself: `--no-restart`, "was not running", and the
//! three supervisor paths.
//!
//! Split out of `mod.rs` because it is one long straight-line decision tree
//! whose every message is contract, and because `mod.rs` is already the
//! orchestration surface. Nothing here decides *whether* to restart — that is
//! settled before this runs — only *how*, and what to say when the supervisor
//! does not cooperate.

use std::path::Path;
use std::process::Command;

use super::args::Args;
use super::notice::{self, FinalLine};
use super::out;
use super::relaunch;
use super::restart::{self, poll_interval, secs_of};
use super::supervisor::{Detected, Manager};
use super::util;
use super::{args_or_empty, args_or_none, exit};

/// Everything the restart tree needs, resolved by the caller.
pub struct Plan<'a> {
    pub args: &'a Args,
    pub sup: &'a Detected,
    pub manager: Manager,
    pub provision_target: &'a Path,
    pub start_script: &'a Path,
    pub stop_script: &'a Path,
    pub flags_file: &'a Path,
    pub flags_from_file: bool,
    pub flags_source: &'a str,
    pub restart_args: &'a [String],
    pub drain: bool,
    pub drain_defaulted: bool,
    /// The poll window as a STRING, because that is what every message
    /// interpolates. Parsed only where a duration is actually needed.
    pub drain_poll_secs: &'a str,
    pub built_commit: &'a str,
    pub final_line: FinalLine<'a>,
}

/// Never returns.
pub fn run(p: Plan<'_>) -> ! {
    if p.args.no_restart {
        no_restart(&p);
    }
    if !p.sup.was_running {
        out::ok("Rebuilt + provisioned. loom-daemon was not running — nothing to restart.");
        out::say(&format!("Start it with: {} [flags]", p.start_script.display()));
        notice::print_final_installed_line(&p.final_line, p.built_commit);
        exit(0);
    }
    match p.manager {
        Manager::Launchd => launchd_restart(&p),
        Manager::Systemd => systemd_restart(&p),
        _ => pidfile_restart(&p),
    }
}

fn no_restart(p: &Plan<'_>) -> ! {
    out::ok("Rebuilt + provisioned. Skipping restart (--no-restart).");
    if p.sup.was_running {
        let target = p.provision_target.display();
        match p.manager {
            Manager::Launchd => {
                out::say("The running (launchd-managed) daemon is still the PRE-update binary. Restart it with:");
                out::say(&format!("  {target} restart      (graceful: supervised in-place relaunch, in-flight sweeps preserved)"));
                out::say(&format!("  {target} restart --drain   (Issue #5138: pauses dispatch, waits for in-flight sweeps to finish first — no sweep.completed/sweep.outcome telemetry gap, #5084)"));
                out::say("(this two-step --no-restart + manual restart is equivalent to a single 'loom-daemon-update.sh --drain' invocation, which builds + provisions + drain-restarts in one command)");
                out::say("If that binary predates #4077 and refuses the restart, re-render + relaunch under supervision:");
                out::say("  loom-daemon-update.sh --relaunch   (preserves the live plist's LOOM_* env; SIGTERMs the daemon so sweep children reparent)");
                out::say(&format!("'launchctl bootout {}' no longer kills in-flight sweeps on a current build (#5081 — each sweep runs in its own process group and reparents to pid 1), but a hand-run bootout+bootstrap can still race and leave the daemon down (bootout is asynchronous); prefer --relaunch above, which settles/retries/verifies the relaunch safely.", p.sup.launchd_service));
            }
            Manager::Systemd => {
                out::say("The running (systemd-managed) daemon is still the PRE-update binary. Restart it with:");
                // #5119: NOT "in-flight sweeps preserved" here — unlike
                // launchd, a systemd stop job runs over the unit's whole
                // cgroup, so a plain restart's exit(0) reaps every
                // sweep/role-run child with it. The drain variant is the one
                // that genuinely preserves them, which is why it leads.
                out::say(&format!("  {target} restart --drain   (RECOMMENDED, Issue #5138: pauses dispatch, waits for in-flight sweeps to finish, THEN relaunches — the preserving variant; an immediate restart here can kill sweeps and land the unit in 'failed', #5119)"));
                out::say(&format!("  {target} restart      (immediate/non-drained — in-flight sweeps AND role runs in the unit cgroup ARE terminated; only if you have confirmed nothing is in flight)"));
                out::say("(this two-step --no-restart + manual restart is equivalent to a single 'loom-daemon-update.sh --drain' invocation, which builds + provisions + drain-restarts in one command — drain is also the DEFAULT on systemd for a plain 'loom-daemon-update.sh' run, no flag needed)");
                out::say("If that binary predates #4267 and refuses the restart, re-render + relaunch under supervision:");
                out::say("  loom-daemon-update.sh --relaunch   (preserves the live unit's LOOM_* env; SIGTERMs the daemon so sweep children reparent)");
                out::say(&format!("Do NOT 'systemctl --user stop {}' by hand — stop tears down the whole cgroup and KILLS in-flight sweeps (they are direct children of the unit).", p.sup.systemd_unit));
            }
            _ => {
                out::say(
                    "The running daemon is still the PRE-update binary. Restart manually with:",
                );
                out::say(&format!(
                    "  {} && {} {}",
                    p.stop_script.display(),
                    p.start_script.display(),
                    args_or_empty(p.restart_args)
                ));
            }
        }
    }
    notice::print_final_installed_line(&p.final_line, p.built_commit);
    exit(0);
}

/// The poll window, its human note, the kickstart window and the interval.
fn poll_windows(p: &Plan<'_>, default_note: &str) -> (String, String, String, std::time::Duration) {
    let kickstart = util::env_non_empty("LOOM_DAEMON_RESTART_KICKSTART_POLL_SECS")
        .unwrap_or_else(|| "15".to_string());
    let interval = poll_interval();
    // An explicit override always wins, drain or not — an operator (or a test)
    // who asked for a specific poll window gets exactly that.
    if let Some(explicit) = util::env_non_empty("LOOM_DAEMON_RESTART_POLL_SECS") {
        return (explicit, default_note.to_string(), kickstart, interval);
    }
    if p.drain {
        // A drain can legitimately take up to its own --timeout before it
        // relaunches — the fast default would false-negative on every real
        // drain.
        return (
            p.drain_poll_secs.to_string(),
            "(Issue #5138 drain window)".to_string(),
            kickstart,
            interval,
        );
    }
    ("30".to_string(), default_note.to_string(), kickstart, interval)
}

/// The #5138 fail-safe report, shared by both supervised branches.
///
/// A drain that timed out WITHOUT `--force-after-timeout` is the fail-safe
/// working exactly as designed: the daemon refused the restart and resumed
/// dispatch on its CURRENT (pre-update) binary rather than cancelling
/// in-flight sweeps. The self-heal must NEVER run in that case.
fn report_drain_failsafe(p: &Plan<'_>, cur_pid: &str, poll_secs: &str) -> ! {
    out::warn(&format!(
        "Drain timed out after {poll_secs}s without --force-after-timeout — the FAIL-SAFE held: loom-daemon is STILL RUNNING its PRE-update binary (pid {cur_pid}). No in-flight sweep was cancelled or killed."
    ));
    out::warn(&format!(
        "The freshly-built binary IS provisioned at {} but was NOT activated this run.",
        p.provision_target.display()
    ));
    // #6007: on a #6007+ daemon the roll is RETAINED rather than discarded, so
    // the recurrence advice is "do nothing", not "re-run" — re-running on a
    // busy host is what reproduced this.
    if restart::drain_roll_still_armed(p.provision_target) {
        out::warn("The daemon reports its drain STILL IN PROGRESS past the deadline — it has KEPT THE ROLL PENDING (#6007): new dispatch is still paused and the restart re-arms itself the moment the in-flight set reaches zero. Nothing to re-run — this host converges onto the provisioned binary on its own (or resumes dispatch and says so once the pending roll's paused-dispatch budget is spent).");
        out::warn("Watch it with 'loom-daemon status' (the line under 'Drain: DRAINING …' explains the pending roll). To take over: 'loom-daemon restart --abort-drain' gives up and resumes dispatch now, or 'loom-daemon restart --drain --force-after-timeout' cancels the remaining sweep(s) and rolls immediately.");
    } else {
        out::warn("Re-run this script (or 'loom-daemon restart --drain' by hand) once the in-flight sweep(s) finish, or re-run with --force-after-timeout to force the roll through.");
    }
    exit(8);
}

fn launchd_restart(p: &Plan<'_>) -> ! {
    out::say(&format!("loom-daemon is launchd-managed (label {}).", p.sup.launchd_label));
    let invoke = restart::build_restart_invoke_args(
        p.drain,
        p.args.drain_timeout.as_deref(),
        p.args.force_after_timeout,
    );
    if p.drain {
        out::say(&format!(
            "Restarting via the supervised DRAIN restart primitive: {} {} (Issue #5138 / #4090) — pausing dispatch, waiting for in-flight sweeps to finish (preserving sweep.completed/sweep.outcome telemetry, #5084), then relaunching.",
            p.provision_target.display(),
            invoke.join(" ")
        ));
    } else {
        out::say(&format!(
            "Restarting via the supervised restart primitive: {} restart",
            p.provision_target.display()
        ));
    }
    out::say("(.daemon.flags is NOT consulted — the plist's EnvironmentVariables carries the equivalent config.)");

    // Capture the pre-restart pid BEFORE the request so the poll below can
    // tell "launchd relaunched onto a new pid" apart from "the same job never
    // moved".
    let pre_restart_pid = p.sup.launchd_job_pid().unwrap_or_default();
    let pre_shown = if pre_restart_pid.is_empty() {
        "<none>".to_string()
    } else {
        pre_restart_pid.clone()
    };

    if restart::invoke_restart(p.provision_target, &invoke) {
        // The RUNNING (old) binary accepted the request — but that ack is the
        // daemon's promise, not proof launchd honoured it (#4232). Verify a
        // NEW, live pid before reporting success; the success line below is
        // intentionally the ONLY "restart scheduled"-style line in this
        // branch, and it is unreachable until verification passes.
        let (poll_secs, kind_note, kickstart_secs, interval) = poll_windows(p, "(#4232)");
        out::say(&format!(
            "Restart request accepted (pre-restart pid: {pre_shown}). Verifying launchd relaunches onto a NEW, live pid within {poll_secs}s before reporting success {kind_note}..."
        ));

        if let Some(new_pid) = restart::wait_for_new_launchd_pid(
            p.sup,
            &pre_restart_pid,
            secs_of(&poll_secs),
            interval,
        ) {
            out::ok(&format!(
                "loom-daemon restart scheduled — launchd relaunched it onto the freshly-provisioned binary (new pid {new_pid}, verified within {poll_secs}s)."
            ));
            notice::print_final_installed_line(&p.final_line, p.built_commit);
            exit(0);
        }

        // Detect the fail-safe by the pid being UNCHANGED (still alive, still
        // the pre-restart pid) — anything else (pid gone, or some other
        // unrecognised shape) falls through to the ordinary self-heal path.
        if p.drain && !p.args.force_after_timeout {
            let cur = p.sup.launchd_job_pid().unwrap_or_default();
            if !cur.is_empty()
                && cur == pre_restart_pid
                && cur.parse::<i32>().is_ok_and(util::pid_alive)
            {
                report_drain_failsafe(p, &cur, &poll_secs);
            }
        }

        out::warn(&format!(
            "launchd did NOT relaunch within {poll_secs}s of the restart ack — no new, live pid observed (pre-restart pid was {pre_shown})."
        ));
        restart::log_launchd_diagnostics(p.sup);
        out::warn(&format!(
            "Falling back to 'launchctl kickstart {}' (plain — NEVER -k — so a daemon that DID relaunch during the race window above is never killed).",
            p.sup.launchd_service
        ));
        restart::launchctl_kickstart(p.sup);

        if let Some(new_pid) = restart::wait_for_new_launchd_pid(
            p.sup,
            &pre_restart_pid,
            secs_of(&kickstart_secs),
            interval,
        ) {
            out::ok(&format!(
                "loom-daemon restart scheduled — launchd's own relaunch did not occur within {poll_secs}s, but the 'launchctl kickstart' fallback relaunched it (new pid {new_pid}, verified within {kickstart_secs}s). Remediation note: the kickstart fallback was required (#4232) — investigate why launchd did not relaunch the job on its own."
            ));
            notice::print_final_installed_line(&p.final_line, p.built_commit);
            exit(0);
        }

        out::err("loom-daemon restart FAILED: no new, live pid was observed even after the 'launchctl kickstart' fallback.");
        restart::log_launchd_diagnostics(p.sup);
        out::err(&format!(
            "The freshly-built binary IS provisioned, but the daemon's live status is NOT confirmed (pre-restart pid was {pre_shown})."
        ));
        out::err(&format!("Investigate manually: launchctl print {}", p.sup.launchd_service));
        exit(7);
    }

    // The restart request is served by the RUNNING (old) binary. A pre-#4077
    // daemon has no RestartDaemon handler (and an unsupervised/dead socket
    // also fails), so the request was refused. Refuse loudly rather than claim
    // a half-update success.
    out::err("loom-daemon restart FAILED: the running daemon did not accept the restart request.");
    out::err("This is expected on the FIRST roll onto a #4077-capable binary — the currently-running binary predates the 'restart' IPC command (or its socket is dead).");
    out::err("The freshly-built binary IS provisioned, but the OLD (unsupervised) binary is still running.");

    if p.args.relaunch {
        exit(relaunch::perform_relaunch(p.sup, p.start_script));
    }

    let pid_hint = p.sup.launchd_job_pid().unwrap_or_default();
    let pid_hint = if pid_hint.is_empty() {
        "<daemon-pid>".to_string()
    } else {
        pid_hint
    };
    out::err("");
    out::err("To finish the roll, re-render the plist and relaunch under launchd supervision");
    out::err("(this installs KeepAlive:{SuccessfulExit:true} + LOOM_DAEMON_SUPERVISOR=launchd so");
    out::err("the NEXT roll can use the supervised path) while preserving the live plist's LOOM_*");
    out::err("autonomy env — run:");
    out::err("  loom-daemon-update.sh --relaunch      (or: LOOM_DAEMON_UPDATE_RELAUNCH=1 loom-daemon-update.sh)");
    out::err("");
    out::err(&format!(
        "NOTE (#5081): a bare 'launchctl bootout {}' no longer terminates",
        p.sup.launchd_service
    ));
    out::err("in-flight sweeps on a current build — every sweep runs in its own process group");
    out::err("(process_group(0), #3800), which bootout's job-tree teardown does not reach; it");
    out::err("reparents to pid 1 and keeps running. --relaunch above is still the recommended");
    out::err("path, but for a different reason: hand-running bootout immediately followed by a");
    out::err("plain bootstrap can race (bootout is asynchronous) and fail with 'Bootstrap");
    out::err("failed: 5: Input/output error', leaving the daemon down until a retry — start.sh");
    out::err("settles after bootout, retries on that race, and verifies the relaunched job's");
    out::err("live pid + env before reporting success, none of which a hand-typed sequence gets.");
    out::err("If you must relaunch by hand anyway, prefer the graceful sequence below:");
    out::err(&format!(
        "  kill -TERM {pid_hint}   # daemon exits non-zero; sweep children reparent regardless; not relaunched (stale plist KeepAlive=false)"
    ));
    out::err(&format!(
        "  {}                                  # re-render + reload the supervised plist (settles/retries/verifies, #5081)",
        p.start_script.display()
    ));
    exit(6);
}

fn systemd_restart(p: &Plan<'_>) -> ! {
    out::say(&format!("loom-daemon is systemd-managed (unit {}).", p.sup.systemd_unit));
    let invoke = restart::build_restart_invoke_args(
        p.drain,
        p.args.drain_timeout.as_deref(),
        p.args.force_after_timeout,
    );
    if p.drain {
        if p.drain_defaulted {
            out::say(&format!(
                "Restarting via the supervised DRAIN restart primitive: {} {} — this is now the DEFAULT on systemd (Issue #5138): an immediate restart here can kill in-flight sweeps and land the unit in 'failed' (#5119). Pass --restart-now to opt out.",
                p.provision_target.display(),
                invoke.join(" ")
            ));
        } else {
            out::say(&format!(
                "Restarting via the supervised DRAIN restart primitive: {} {} (Issue #5138 / #4090) — pausing dispatch, waiting for in-flight sweeps to finish (preserving sweep.completed/sweep.outcome telemetry, #5084), then relaunching.",
                p.provision_target.display(),
                invoke.join(" ")
            ));
        }
    } else {
        out::say(&format!(
            "--restart-now given: restarting via the IMMEDIATE (non-drained) supervised restart primitive: {} restart",
            p.provision_target.display()
        ));
    }
    out::say("(.daemon.flags is NOT consulted — the unit's Environment= lines carry the equivalent config.)");

    let pre_restart_pid = p.sup.systemd_unit_pid().unwrap_or_default();
    let pre_shown = if pre_restart_pid.is_empty() {
        "<none>".to_string()
    } else {
        pre_restart_pid.clone()
    };

    if restart::invoke_restart(p.provision_target, &invoke) {
        let (poll_secs, kind_note, kickstart_secs, interval) = poll_windows(p, "(#4950)");
        out::say(&format!(
            "Restart request accepted (pre-restart pid: {pre_shown}). Verifying systemd relaunches onto a NEW, live MainPID within {poll_secs}s before reporting success {kind_note}..."
        ));

        if let Some(new_pid) = restart::wait_for_new_systemd_pid(
            p.sup,
            &pre_restart_pid,
            secs_of(&poll_secs),
            interval,
        ) {
            out::ok(&format!(
                "loom-daemon restart scheduled — systemd relaunched it onto the freshly-provisioned binary (new pid {new_pid}, verified within {poll_secs}s)."
            ));
            notice::print_final_installed_line(&p.final_line, p.built_commit);
            exit(0);
        }

        // The unit stays `active` throughout a fail-safe drain — it was never
        // told to stop — so an unchanged, live MainPID is the signature.
        if p.drain && !p.args.force_after_timeout {
            let cur = p.sup.systemd_unit_pid().unwrap_or_default();
            if !cur.is_empty()
                && cur != "0"
                && cur == pre_restart_pid
                && cur.parse::<i32>().is_ok_and(util::pid_alive)
            {
                report_drain_failsafe(p, &cur, &poll_secs);
            }
        }

        out::warn(&format!(
            "systemd did NOT relaunch within {poll_secs}s of the restart ack — no new, live MainPID observed (pre-restart pid was {pre_shown})."
        ));
        restart::log_systemd_diagnostics(p.sup);
        let mut active_state = p.sup.systemd_unit_active_state();
        let mut unit_result = p.sup.systemd_unit_result();

        // #5119: the unit may still be mid-teardown when the #4950 pid poll
        // expires. The pre-#5119 code only self-healed a unit ALREADY
        // `failed`, so a `deactivating` snapshot fell through to "refusing to
        // guess" and left the daemon DOWN until an operator ran
        // reset-failed+start by hand.
        if restart::is_transitional(&active_state) {
            let settle_secs = util::env_non_empty("LOOM_DAEMON_STOP_SETTLE_SECS")
                .unwrap_or_else(|| "100".to_string());
            out::warn(&format!(
                "Unit is still transitioning (ActiveState={active_state}) — its stop job has not finished (a stale unit predating #4862's KillMode=mixed drags the SIGTERM→SIGKILL teardown of in-cgroup sweep/role children out to the default 90s TimeoutStopSec). Waiting up to {settle_secs}s for it to settle before recovering (#5119)."
            ));
            let (settled, _) =
                restart::wait_for_systemd_stop_settle(p.sup, secs_of(&settle_secs), interval);
            active_state = settled;
            unit_result = p.sup.systemd_unit_result();
            out::warn(&format!(
                "Unit settled to ActiveState={active_state} (Result={}) after its stop transition.",
                none_or_unknown(&unit_result)
            ));
        }

        // A unit that is NOT `active` after that settle will NOT come back on
        // its own: `Restart=on-success` fires only for a clean-exit relaunch,
        // never for a stop-timeout escalation (`failed`, Result=timeout) nor a
        // completed stop (`inactive`). Only a genuinely `active` unit on a pid
        // the poll simply failed to observe is left alone — touching that
        // would risk bouncing a healthy daemon.
        if active_state != "active" {
            out::warn(&format!(
                "Unit is in a non-running state (ActiveState={}, Result={}) — systemd will NOT auto-relaunch it (Restart=on-success does not fire for a failed/stopped unit). Self-healing via 'systemctl --user reset-failed {2} && systemctl --user start {2}'.",
                none_or_unknown(&active_state),
                none_or_unknown(&unit_result),
                p.sup.systemd_unit
            ));
            restart::systemd_self_heal(p.sup);

            if let Some(new_pid) = restart::wait_for_new_systemd_pid(
                p.sup,
                &pre_restart_pid,
                secs_of(&kickstart_secs),
                interval,
            ) {
                out::ok(&format!(
                    "loom-daemon restart scheduled — systemd's own relaunch did not occur within {poll_secs}s (unit settled to '{active_state}', Result={}), but 'systemctl --user reset-failed && start' recovered it (new pid {new_pid}, verified within {kickstart_secs}s). Remediation note: the reset-failed+start fallback was required (#4950/#5119) — investigate why the unit's stop sequence exceeded TimeoutStopSec (a live unit that predates #4862's KillMode=mixed fix — never re-rendered by a plain restart — is the most likely cause; re-render it with 'loom-daemon-update.sh --relaunch').",
                    none_or_unknown(&unit_result)
                ));
                notice::print_final_installed_line(&p.final_line, p.built_commit);
                exit(0);
            }

            out::err("loom-daemon restart FAILED: no new, live MainPID was observed even after 'systemctl --user reset-failed && start'.");
        } else {
            out::err("loom-daemon restart FAILED: the unit is not confirmed relaunched, yet its ActiveState is 'active' on an unchanged/unobserved pid — refusing to bounce a possibly-healthy daemon.");
        }
        restart::log_systemd_diagnostics(p.sup);
        out::err(&format!(
            "The freshly-built binary IS provisioned, but the daemon's live status is NOT confirmed (pre-restart pid was {pre_shown})."
        ));
        out::err(&format!("Investigate manually: systemctl --user status {}", p.sup.systemd_unit));
        exit(7);
    }

    out::err("loom-daemon restart FAILED: the running daemon did not accept the restart request.");
    out::err("This is expected on the FIRST roll onto a #4267-capable binary — the currently-running binary predates the 'restart' IPC command (or its socket is dead).");
    out::err("The freshly-built binary IS provisioned, but the OLD (unsupervised) binary is still running.");

    if p.args.relaunch {
        exit(relaunch::perform_systemd_relaunch(p.sup, p.start_script));
    }

    let pid_hint = p.sup.systemd_unit_pid().unwrap_or_default();
    let pid_hint = if pid_hint.is_empty() {
        "<daemon-pid>".to_string()
    } else {
        pid_hint
    };
    out::err("");
    out::err("To finish the roll, re-render the unit and relaunch under systemd supervision");
    out::err("(this installs Restart=on-success + LOOM_DAEMON_SUPERVISOR=systemd so");
    out::err("the NEXT roll can use the supervised path) while preserving the live unit's LOOM_*");
    out::err("autonomy env — run:");
    out::err("  loom-daemon-update.sh --relaunch      (or: LOOM_DAEMON_UPDATE_RELAUNCH=1 loom-daemon-update.sh)");
    out::err("");
    out::err(&format!(
        "WARNING: do NOT 'systemctl --user stop {}' by hand to force this.",
        p.sup.systemd_unit
    ));
    out::err("stop tears down the whole cgroup, and in-flight sweep children are DIRECT");
    out::err("children of the unit, so it TERMINATES every running sweep — stranding");
    out::err("loom:building labels and leaving worktrees behind. --relaunch above instead stops");
    out::err("the daemon gracefully (SIGTERM) so sweep children reparent and keep working.");
    out::err("If you must relaunch by hand, prefer the graceful sequence over stop+enable:");
    out::err(&format!(
        "  kill -TERM {pid_hint}   # daemon exits by signal; children reparent; not relaunched (Restart=on-success does not fire)"
    ));
    out::err(&format!(
        "  {}                                  # re-render + enable --now the supervised unit",
        p.start_script.display()
    ));
    exit(6);
}

fn pidfile_restart(p: &Plan<'_>) -> ! {
    if p.flags_from_file {
        out::say(&format!(
            "Restarting with the flags persisted at the last start ({}): {}",
            p.flags_file.display(),
            args_or_none(p.restart_args)
        ));
    } else {
        out::warn(&format!(
            "No {} found — restarting FLAGS-OFF (bare) rather than guessing the prior autonomy flags.",
            p.flags_file.display()
        ));
    }

    out::say("Stopping loom-daemon...");
    // `--restarting` preserves the autonomy-desired marker + watchdog across
    // this internal stop (#4011): a self-update is NOT operator intent to
    // stop, so the detector must NOT be disarmed — otherwise every self-update
    // would silently turn off the very autonomy-loss detection this closes.
    let stopped = Command::new(p.stop_script)
        .arg("--restarting")
        .status()
        .is_ok_and(|s| s.success());
    if !stopped {
        out::err("loom-daemon-stop.sh failed — NOT starting the new binary on top of a still-running old one.");
        exit(1);
    }

    out::say(&format!(
        "Starting loom-daemon with preserved flags: {}",
        args_or_none(p.restart_args)
    ));
    let mut cmd = Command::new(p.start_script);
    if !p.restart_args.is_empty() {
        cmd.args(p.restart_args);
    } else if p.flags_from_file {
        // Issue #5429: a persisted-flags FILE that EXISTS but is empty is a
        // CONFIRMED prior FLAGS-OFF state, distinct from the "no file at all"
        // branch below, which is genuinely a guess. A bare invocation here
        // would leave WANT_WORK_FINDER/WANT_HEALTH_GATE unset, which the start
        // wrapper's autonomy-downgrade refusal (#4011/#5409) cannot
        // distinguish from a silent, un-intentional default — and the
        // autonomy-desired marker it checks is written on ANY successful start,
        // so it is already present on every restart of a daemon that was
        // already running. That mismatch made this exact restart refuse with
        // exit 1 whenever the marker pre-existed. Passing the two explicit
        // negatives states the confirmed intent (byte-identical resulting
        // LOOM_WORK_FINDER=0/LOOM_MAIN_HEALTH_GATE=0 env) while satisfying the
        // downgrade check's "an explicit ask is not silent" exemption.
        cmd.args(["--no-work-finder", "--no-health-gate"]);
    }
    let start_rc = cmd.status().ok().and_then(|s| s.code()).unwrap_or(1);
    if start_rc == 0 {
        notice::print_final_installed_line(&p.final_line, p.built_commit);
    }
    exit(start_rc)
}

fn none_or_unknown(value: &str) -> &str {
    if value.is_empty() {
        "unknown"
    } else {
        value
    }
}
