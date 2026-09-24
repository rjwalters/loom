//! Host-side autonomy-loss detector for the raw `loom-daemon` process (#4011),
//! ported from `defaults/scripts/cli/loom-daemon-watchdog.sh` (#8086, epic
//! #7810).
//!
//! # The contract this inherits
//!
//! `.loom/scripts/cli/loom-daemon-watchdog.sh` is invoked **by that path** by a
//! launchd/systemd timer, the `loom` dispatcher and operators, so the script
//! name survives as a stub and this module inherits its CLI surface whole: six
//! flags, roughly forty `LOOM_WATCHDOG_*` environment knobs, the
//! `<ts> [<LEVEL>] <msg>` log line shape, and the exit codes its callers branch
//! on (0 healthy / deliberate stop, 1 divergence or state mismatch, 3 liveness
//! undetermined).
//!
//! # Why this port mostly *deletes* logic rather than translating it
//!
//! The shell had to re-derive, in bash, primitives the daemon already owns —
//! and its own comments say so. `resolve_pid_file()` states it "mirrors the
//! daemon's own `daemon_pidfile::resolve_pid_file_path_from` EXACTLY so the two
//! ends can never mean different files", and records that the #5118 incident
//! was possible *only because each side derived its own path*.
//! `parse_etime_secs()` says it "mirrors the Rust probe's `parse_etime` exactly
//! (`daemon_install_state.rs`, #4368) so the two never disagree".
//!
//! Two implementations kept in agreement by comment is the defect source those
//! comments describe. Here they are one implementation: this module calls
//! [`crate::daemon_pidfile`], [`crate::daemon_install_state`] and
//! [`crate::autonomy_marker`] directly, so the agreement is structural.
//!
//! # The equivalence proof
//!
//! `defaults/scripts/tests/test-loom-daemon-watchdog.sh` (188 assertions, plus
//! 4 retired records) is a retained black-box suite: it invokes the stub **by
//! path** with environment overrides and asserts on stdout, the log file and
//! exit codes. The peer-coordination dedup window's own equivalence suite,
//! `test-loom-daemon-watchdog-dedup.sh`, was retired in #8587 once this
//! module's `peer_coord` submodule (with its own `#[cfg(test)]` coverage)
//! became the only implementation — its pre-port shell counterpart had
//! already been orphaned by this port.
//!
//! They run against this code with two harness changes and no altered
//! expectations: `LOOM_DAEMON_SELF_BIN` pins the binary that implements the
//! stub (#8134), and four assertions about the SHELL's source text are retired
//! under §6's three-part test, recorded in the suite rather than deleted.
//!
//! "206" was the count before the stub existed — i.e. the suite measuring the
//! shell it was supposed to be replacing. That number is the reason §6 now
//! opens by saying a green figure must be made red on purpose before it is
//! believed.
//!
//! Per #8011 a retained suite is necessary and not sufficient — it proves only
//! what its author thought to write down. See
//! `defaults/docs/verification-recipes.md` §6 for the differential step.

use std::path::{Path, PathBuf};

pub mod config;
pub mod consts;
pub mod env;
pub mod escalate;
pub mod heartbeat;
pub mod liveness;
pub mod load_avg;
pub mod locate;
pub mod marker;
pub mod peer_coord;
pub mod probe;
pub mod probe_state;
pub mod recovery;
pub mod remediation;
pub mod report;
pub mod supervisor;
pub mod supervisor_cmd;

/// The `--help` banner, kept verbatim from the shell's head-comment block.
///
/// It is the CLI contract (the retained suite greps it for the marker /
/// `StartInterval` rationale and for the #4398 and #5944 knob names) and the
/// design record for every incident that shaped this detector. It lives beside
/// the code as prose rather than inside a string literal so it stays readable
/// and diffable.
pub const HELP_BANNER: &str = include_str!("help.txt");

/// Run one watchdog tick, returning the process exit code.
///
/// The codes are contract — a supervisor and the retained suite both branch on
/// them:
///
/// | code | meaning |
/// |---|---|
/// | 0 | healthy, or the marker is absent so nothing is expected |
/// | 1 | a divergence, or a state mismatch between intent and reality |
/// | 2 | usage error (an unknown flag) |
/// | 3 | liveness undetermined — deliberately NOT an outage (#5118) |
#[must_use]
pub fn tick(verbose: bool) -> i32 {
    let paths = config::Paths::from_env();
    let reporter = report::Reporter::new(paths.log.clone(), verbose);
    let state = consts::StateFiles::resolve(&paths.loom_dir);

    if !paths.marker.exists() {
        return marker_absent(&paths, &reporter, &state);
    }
    marker_present(&paths, &reporter, &state)
}

/// Sections 2-7: intent is on record, so compare it against reality.
fn marker_present(
    paths: &config::Paths,
    reporter: &report::Reporter,
    state: &consts::StateFiles,
) -> i32 {
    let sup = supervisor::resolve_with_marker(
        &paths.marker,
        cfg!(target_os = "macos"),
        supervisor::systemctl_available(),
    );
    let mut snap = liveness::probe(&paths.loom_dir, &paths.marker, &sup);

    // #5118: the out-of-band signals can only ever be a hint. Before declaring
    // an outage, ask the socket — a served socket is authoritative and an
    // absent pid file is no evidence at all.
    let mut socket_proved_healthy = false;
    if !snap.alive() {
        let corroboration = socket_corroboration(paths, reporter, &mut snap);
        if let Some(code) = corroboration.exit {
            return code;
        }
        socket_proved_healthy = corroboration.healthy;
    }

    if !snap.alive() {
        return outage(paths, reporter, state, &snap, &sup);
    }

    // A healthy tick ends any outage episode: the next real outage must start
    // from a fresh attempt budget rather than inheriting a spent one, and from
    // no escalation sentinel.
    recovery::clear(&state.recovery, &state.escalation_sentinel);

    // Sections 12-22: the bounded in-band probe and its two signals.
    let probe_outcome = ipc_probe(paths, reporter, state, &snap);
    if let Some(code) = probe_outcome.exit {
        return code;
    }

    // The 50-series: only on a tick whose probe came back healthy. Asking a
    // daemon that is not answering produces no information, and a "could not
    // determine" reported alongside a confirmed hang is noise on a real signal.
    // `probe_outcome.healthy`, NOT `!reporter.diverged()`. A probe that was
    // disabled or skipped also fails to diverge, and this check must never
    // become a new hang surface for a tick that deliberately spent no
    // round-trip — `LOOM_WATCHDOG_IPC_PROBE=0` against a never-returning
    // binary took 15s here before this distinction existed.
    if probe_outcome.healthy || socket_proved_healthy {
        peer_coordination(paths, reporter, state, &snap);
    }

    let heartbeat_file = snap.heartbeat_file.clone().unwrap_or_else(|| {
        paths
            .loom_dir
            .join("daemon.heartbeat")
            .display()
            .to_string()
    });
    let verdict = heartbeat::decide(
        snap.heartbeat,
        &snap.detail,
        &heartbeat_file,
        snap.heartbeat_age_secs,
        snap.heartbeat_stale_threshold_secs,
        snap.process_age_secs,
    );
    heartbeat::emit(&verdict, reporter)
}

/// Sections 1 and 1b — there is no operator intent on record.
///
/// The marker's lifetime IS that intent, which is why the detector keys on it
/// rather than on "is the pid file / launchd job present". `loom-daemon-stop.sh`
/// boots out the job AND deletes the pid file, so after ANY stop those would be
/// gone — making a deliberately-stopped daemon and a silently-dead one
/// byte-identical. A detector built on them pages on every intentional stop, or
/// never pages at all.
///
/// Absent intent, a daemon that IS running is still worth a word: it is running
/// UNSUPERVISED, and nothing will revive it when it dies.
fn marker_absent(
    paths: &config::Paths,
    reporter: &report::Reporter,
    state: &consts::StateFiles,
) -> i32 {
    let sup = supervisor::resolve_without_marker(
        cfg!(target_os = "macos"),
        supervisor::systemctl_available(),
    );
    let pid_file = marker::resolve_pid_file(
        env::var("LOOM_PID_FILE"),
        None,
        env::var("LOOM_MACHINE_CHECKOUT"),
        env::var("LOOM_WORKSPACE"),
        None,
        Some(paths.loom_dir.clone()),
    );

    let liveness = crate::daemon_install_state::check_liveness_with_systemd(
        sup.use_launchd,
        &sup.label,
        pid_file.as_deref(),
        env::var("LOOM_LAUNCHD_DOMAIN").as_deref(),
        sup.use_systemd.then_some(sup.systemd_unit.as_str()),
    );

    let mut alive = liveness.alive;
    let mut detail = liveness.detail;

    // The out-of-band signals can only ever be a hint here. Ask the socket
    // before concluding anything, because a served socket is authoritative and
    // an absent pid file is not (#5118).
    if !alive {
        let bin = locate::daemon_bin(None);
        let attempt = probe::attempt(&paths.socket_path, bin.as_deref());
        let (verdict, socket_detail) =
            probe::classify(&attempt, &paths.socket_path, probe::probe_timeout_secs());
        if verdict == probe::SocketVerdict::Answered {
            alive = true;
            detail = format!(
                "a daemon ANSWERS on {} ({socket_detail}); the out-of-band signal disagreed: \
                 {detail}",
                paths.socket_path.display()
            );
        }
    }

    if alive {
        reporter.report(
            report::Level::Warn,
            &format!(
                "STATE MISMATCH: no autonomy-desired marker at {}, but a daemon IS running \
                 ({detail}). Crash protection is DISARMED — if this daemon dies the watchdog \
                 will NOT revive it. Heal it by restarting the daemon (it self-heals the marker \
                 at startup, #4331) or re-running ./.loom/scripts/cli/loom-daemon-start.sh; if \
                 the daemon should NOT be running, stop it with \
                 ./.loom/scripts/cli/loom-daemon-stop.sh.",
                paths.marker.display()
            ),
        );
        return 1;
    }

    // A deliberate stop ends any outage episode: the next real start must begin
    // from a clean slate rather than inheriting a spent attempt budget -- and
    // the escalation sentinel goes with it, or the next outage reads as
    // already-reported and is never escalated.
    recovery::clear(&state.recovery, &state.escalation_sentinel);
    reporter.report(
        report::Level::Ok,
        &format!(
            "RULE: marker absent -> deliberate stop, not reviving (#6388): no autonomy-desired \
             marker at {} — no daemon expected; nothing to check.",
            paths.marker.display()
        ),
    );
    0
}

/// Sections 8-10: intent says a daemon should be running and none is.
///
/// The order is deliberate and each step is bounded:
///
/// 1. **Supervisor-level remediation** first, when the job is LOADED with a
///    clean last exit. That is a narrow, well-understood shape — the supervisor
///    accepted the job and failed to relaunch it — and `kickstart` addresses it
///    without starting a second daemon.
/// 2. **Bounded recovery** otherwise: a full start, under a spending limit with
///    exponential backoff behind a circuit breaker.
/// 3. **Escalation** once the budget is spent, because an outage nobody can see
///    is the failure this detector exists to prevent.
fn outage(
    paths: &config::Paths,
    reporter: &report::Reporter,
    state: &consts::StateFiles,
    snap: &liveness::Snapshot,
    sup: &supervisor::Supervisor,
) -> i32 {
    let now = now_secs();
    let started_at = snap.started_at.clone().unwrap_or_default();

    // Step 1: the supervisor may be able to fix this itself.
    if snap.job_loaded {
        if let Some(code) = supervisor_remediation(paths, reporter, sup, snap, &started_at) {
            if code == 0 {
                recovery::clear(&state.recovery, &state.escalation_sentinel);
            }
            return code;
        }
    }

    // Step 2: bounded recovery. Load or open the episode first, so the report
    // can say how long this has been going on rather than only that it is.
    let mut episode = recovery::read(&state.recovery);
    if episode.down_since == 0 {
        episode.down_since = now;
    }
    episode.ticks += 1;
    let outage_secs = now.saturating_sub(episode.down_since);

    let limits = recovery::Limits::from_env();

    // Resolve what a recovery would RUN before deciding whether to run it: a
    // host with nothing runnable is report-only, and the report should say so
    // rather than implying an attempt was made and failed.
    let cli_dir = cli_dir();
    let pid_file = marker::get_nonempty(&paths.marker, "pid_file").map(PathBuf::from);
    let argv = recovery::resolve_argv(
        env::var("LOOM_WATCHDOG_RECOVER_CMD").as_deref(),
        &cli_dir,
        pid_file.as_deref(),
    );

    // #6388: a signal-shaped exit is NAMED, never used to refuse. Only marker
    // absence means a deliberate stop, and the marker is present here.
    let signal_note = snap
        .exit_signal_detail
        .as_deref()
        .map(remediation::signal_rule_note)
        .unwrap_or_default();

    let decision = match &argv {
        recovery::Argv::Unavailable { .. } => recovery::Decision::Disabled,
        recovery::Argv::Run { .. } => recovery::decide(&episode, &limits, now),
    };

    // An attempt actually runs here, rather than being reported and skipped.
    if let (recovery::Decision::Attempt { attempt }, recovery::Argv::Run { argv, detail }) =
        (&decision, &argv)
    {
        episode.attempts = *attempt;
        episode.last_attempt = now;
        recovery::write(&state.recovery, &episode);

        reporter.report(
            report::Level::Divergence,
            &format!(
                "A daemon is EXPECTED (autonomy-desired marker present, started {started_at}) \
                 but is NOT running: {}. Autonomous dispatch has stopped (down {outage_secs}s \
                 across {} consecutive watchdog ticks).{signal_note} AUTO-RECOVERING now — \
                 bounded attempt {attempt} of {} (#5391), running: {detail}",
                snap.detail, episode.ticks, limits.max_attempts
            ),
        );

        let timeout =
            std::time::Duration::from_secs(env::num("LOOM_WATCHDOG_RECOVER_TIMEOUT_SECS", 120));
        let mut cmd = std::process::Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        let recover_rc = crate::sweep_registry::output_with_timeout(cmd, timeout)
            .ok()
            .flatten()
            .and_then(|o| o.status.code());

        // Confirm with the SAME liveness question the outage was declared on.
        let recheck = supervisor_cmd::Recheck::from_env();
        let back = supervisor_cmd::recheck_alive(&recheck, || {
            let l = liveness::probe(&paths.loom_dir, &paths.marker, sup);
            l.alive().then_some(l.pid).flatten()
        });

        if back.is_some() {
            reporter.report(
                report::Level::Ok,
                &format!(
                    "auto-recovery SUCCEEDED on attempt {attempt} of {} after a {outage_secs}s \
                     outage: {}. Recovery command exited {} (#5391).",
                    limits.max_attempts,
                    snap.detail,
                    recover_rc.map_or_else(|| "?".to_string(), |c| c.to_string())
                ),
            );
            recovery::clear(&state.recovery, &state.escalation_sentinel);
            return 0;
        }

        let next = attempt + 1;
        let tail = if *attempt < limits.max_attempts {
            format!(
                " The next attempt is backed off by {}s.",
                recovery::backoff_for(next, limits.backoff_secs, limits.backoff_cap_secs)
            )
        } else {
            format!(
                // Wording is contract: the retained suite greps for the
                // literal phrase "CIRCUIT BREAKER OPEN" (#5391). A paraphrase
                // ("is now OPEN") reads the same to a human and silently fails
                // the assertion, which is the whole reason the suite is run
                // unchanged against the port.
                " CIRCUIT BREAKER OPEN: the attempt budget is spent, so no further automatic \
                 attempts will be made until a tick observes a healthy daemon or {} is deleted.",
                state.recovery.display()
            )
        };
        let escalation = if *attempt >= limits.max_attempts {
            escalation_note(
                paths,
                reporter,
                state,
                snap,
                &format!("the circuit breaker is OPEN after {attempt} attempts"),
                Some(detail),
            )
        } else {
            String::new()
        };
        reporter.report(
            report::Level::Divergence,
            &format!(
                "Bounded recovery attempt {attempt} of {} RAN ('{detail}', exit {}) and the \
                 daemon is STILL not confirmed running.{tail}{escalation} Recover with: \
                 ./.loom/scripts/cli/loom-daemon-start.sh [flags]  (or 'loom-daemon status' to \
                 inspect).",
                limits.max_attempts,
                recover_rc.map_or_else(|| "?".to_string(), |c| c.to_string())
            ),
        );
        return 1;
    }

    let recover_note = match decision {
        recovery::Decision::Disabled => match &argv {
            recovery::Argv::Unavailable { detail } => format!(
                "NO auto-recovery was attempted: {detail}. This watchdog is therefore \
                 REPORT-ONLY on this host — an installed watchdog job here means DETECTION, not \
                 self-healing — until that is fixed."
            ),
            // The shell has TWO distinct messages here, not one
            // (loom-daemon-watchdog.sh:2158 and :2161), and both say
            // REPORT-ONLY. The port had paraphrased this one down to
            // "Automatic recovery is disabled on this host", dropping the
            // "nothing will bring the daemon back but you" the header promises
            // is stated explicitly.
            //
            // The retained assertion for it passed anyway, because `cli_dir`
            // pointed at the binary's directory where no start script exists,
            // so argv was ALWAYS Unavailable and the other branch answered.
            // Fixing that bug is what exposed this one.
            recovery::Argv::Run { .. } => {
                "NO auto-recovery was attempted: it is DISABLED on this host \
                 (LOOM_WATCHDOG_AUTO_RECOVER=0). This watchdog is REPORT-ONLY for this outage — \
                 an installed watchdog job here means DETECTION, not self-healing, and nothing \
                 will bring the daemon back but you."
                    .to_string()
            }
        },
        recovery::Decision::BackingOff {
            next_attempt,
            remaining,
        } => format!(
            "Attempt {next_attempt} of {} is backed off for another {remaining}s.",
            limits.max_attempts
        ),
        recovery::Decision::BreakerOpen { attempts } => format!(
            "CIRCUIT BREAKER OPEN: {attempts} bounded recovery attempts (budget {}) have \
             already been spent on this outage and none restored the daemon. NO further \
             automatic attempts will be made until a tick observes a healthy daemon or {} is \
             deleted — deliberately, so a genuinely broken binary is restarted a bounded number \
             of times instead of forever.",
            limits.max_attempts,
            state.recovery.display()
        ),
        // Unreachable: an Attempt is handled above, where it actually runs.
        recovery::Decision::Attempt { .. } => String::new(),
    };

    // Step 3: escalate once nothing automatic is left to try.
    //
    // TWO conditions, not one. The shell (loom-daemon-watchdog.sh:2224-2228)
    // escalates when the breaker has tripped OR when no attempt is even
    // POSSIBLE on this host and the outage has persisted for the same number
    // of consecutive ticks. Porting only the first left the second silent:
    // with auto-recovery disabled, or no runnable recovery command, a
    // confirmed outage filed no forge issue at all — just a log line every
    // tick, forever. That is the 2026-07-26 shape the watchdog exists to
    // prevent, and `help.txt` still promised the behaviour.
    //
    // No retained assertion covers it: the harness pins AUTO_RECOVER=0 with
    // ESCALATE=0, so no case ever enables escalation while recovery is
    // impossible.
    //
    // The reason strings are the shell's verbatim — they are posted into a
    // forge issue body, and a paraphrase is a silent divergence in text an
    // operator reads during an outage.
    // `Disabled` covers both of the shell's `recover_possible != true` cases:
    // recovery switched off, and no runnable recovery argv — the call site
    // maps `Argv::Unavailable` onto it.
    let escalate_for = escalation_trigger(&decision, episode.ticks, limits.max_attempts);

    let escalation_note = match escalate_for {
        Some(EscalationTrigger::BreakerOpen) => escalation_note(
            paths,
            reporter,
            state,
            snap,
            &format!(
                "the circuit breaker is OPEN — {} bounded recovery attempts were spent and the \
                 daemon is still down",
                episode.attempts
            ),
            None,
        ),
        Some(EscalationTrigger::RecoveryImpossible) => escalation_note(
            paths,
            reporter,
            state,
            snap,
            &format!(
                "automatic recovery is not possible on this host, and the outage has persisted \
                 for {} consecutive watchdog ticks ({outage_secs}s)",
                episode.ticks
            ),
            None,
        ),
        None => String::new(),
    };

    recovery::write(&state.recovery, &episode);
    reporter.report(
        report::Level::Divergence,
        &format!(
            "A daemon is EXPECTED (autonomy-desired marker present, started {started_at}) but is \
             NOT running: {}. Autonomous dispatch has stopped (down {outage_secs}s across {} \
             consecutive watchdog ticks).{signal_note} {recover_note}{escalation_note} Recover \
             with: \
             ./.loom/scripts/cli/loom-daemon-start.sh [flags]  (or 'loom-daemon status' to \
             inspect).",
            snap.detail, episode.ticks
        ),
    );
    1
}

/// The `#4232`/`#4862` gate. `None` when it does not apply, so the caller falls
/// through to bounded recovery.
fn supervisor_remediation(
    paths: &config::Paths,
    reporter: &report::Reporter,
    sup: &supervisor::Supervisor,
    snap: &liveness::Snapshot,
    started_at: &str,
) -> Option<i32> {
    let service = snap.supervisor_service.as_deref()?;
    if !matches!(
        remediation::gate(snap.job_loaded, snap.last_exit_status),
        remediation::Gate::Remediate
    ) {
        // A crash, a SIGTERM, or an unreadable status. Deliberately not
        // remediated here; the caller proceeds to bounded recovery, which is
        // the path that has a spending limit.
        return None;
    }

    reporter.report(
        report::Level::Divergence,
        &format!(
            "A daemon is EXPECTED (autonomy-desired marker present, started {started_at}) but is \
             NOT running: {}. Last exit status was 0 — the restart-primitive's own exit-0 \
             contract (#4054/#4077) — which the supervisor failed to honor. Auto-remediating \
             {service} (PLAIN kickstart, never -k, so a daemon that is mid-relaunch is never \
             killed) (#4232).",
            snap.detail
        ),
    );

    if sup.use_launchd {
        let _ = supervisor_cmd::run_bounded(&supervisor_cmd::launchd_kickstart_argv(service));
    } else if sup.use_systemd {
        for argv in supervisor_cmd::systemd_restart_argvs(service) {
            let _ = supervisor_cmd::run_bounded(&argv);
        }
    } else {
        return None;
    }

    // Ask the SAME liveness question the outage was declared on, so a
    // relaunch is confirmed by the same evidence that declared it missing.
    let recheck = supervisor_cmd::Recheck::from_env();
    let alive = supervisor_cmd::recheck_alive(&recheck, || {
        let l = crate::daemon_install_state::check_liveness_with_systemd(
            sup.use_launchd,
            &sup.label,
            None,
            env::var("LOOM_LAUNCHD_DOMAIN").as_deref(),
            sup.use_systemd.then_some(sup.systemd_unit.as_str()),
        );
        l.alive.then_some(l.pid).flatten()
    });

    match alive {
        Some(pid) => {
            reporter.report(
                report::Level::Ok,
                &format!("auto-remediation succeeded: relaunched {service} (new pid {pid})."),
            );
            Some(0)
        }
        None => {
            reporter.report(
                report::Level::Divergence,
                &format!(
                    "Auto-remediation attempted on {service} but the daemon is STILL not \
                     confirmed running. Escalate manually: inspect the supervisor, or run \
                     ./.loom/scripts/cli/loom-daemon-start.sh [flags]. Watchdog log: {}.",
                    paths.log.display()
                ),
            );
            Some(1)
        }
    }
}

/// Attempt escalation and render the note the divergence line carries.
///
/// The note is written from what was OBSERVED, never from what was intended. An
/// escalation that says an issue was filed when none was is worse than silence:
/// it tells the reader to go look somewhere nothing exists.
fn escalation_note(
    paths: &config::Paths,
    reporter: &report::Reporter,
    state: &consts::StateFiles,
    snap: &liveness::Snapshot,
    reason: &str,
    recovery_detail: Option<&str>,
) -> String {
    match escalate::decide(&state.escalation_sentinel) {
        escalate::Decision::AlreadyEscalated => format!(
            " This outage has ALREADY been escalated out-of-band (sentinel {}).",
            state.escalation_sentinel.display()
        ),
        escalate::Decision::Disabled => format!(
            " Out-of-band escalation is disabled, so THIS LOGFILE IS THE ONLY SIGNAL for this \
             outage — {}.",
            paths.log.display()
        ),
        escalate::Decision::Escalate => {
            let repo_root = marker::get_nonempty(&paths.marker, "repo_root").map(PathBuf::from);
            let cli_dir = cli_dir();
            let fallback = env::var("LOOM_WATCHDOG_CREATE_ISSUE_FALLBACK_DIR").map(PathBuf::from);

            let script =
                escalate::resolve_issue_script(repo_root.as_deref(), &cli_dir, fallback.as_deref());

            let hostname = escalate::hostname();
            let ctx = escalate::Context {
                hostname: &hostname,
                socket_path: &paths.socket_path,
                marker: &paths.marker,
                liveness_detail: &snap.detail,
                reason,
                recovery_argv_detail: recovery_detail,
                watchdog_log: &paths.log,
                recovery_state: &state.recovery,
                sentinel: &state.escalation_sentinel,
            };

            match script {
                Some(s) if escalate::file_issue(&s, &ctx, &state.escalation_sentinel, reporter) => {
                    " ESCALATED out-of-band: filed a forge tracking issue so this outage is not \
                     confined to a logfile nobody tails (#5391)."
                        .to_string()
                }
                _ => format!(
                    " Out-of-band escalation was NOT possible (no create-issue.sh reachable, or \
                     the forge call failed), so THIS LOGFILE IS THE ONLY SIGNAL for this outage \
                     — {}.",
                    paths.log.display()
                ),
            }
        }
    }
}

/// Recompute heartbeat freshness for a snapshot that has just been flipped to
/// alive.
///
/// `liveness.rs` computes freshness only when the process was ALREADY known
/// alive — reasonable on its own, since freshness is meaningless about a
/// process that does not exist. But `socket_corroboration` flips the snapshot
/// to alive AFTERWARDS, and nothing re-asked.
///
/// The result was a fail-OPEN: no pid file, the socket answers, a heartbeat
/// file 1000s old against a 300s threshold — and the tick reported "no
/// heartbeat file … (heartbeat disabled or not yet written) — liveness-only
/// OK" and exited 0, while the file existed and was stale. The shell's section
/// 4 ran on HEARTBEAT_FILE regardless of how liveness was established.
fn refresh_heartbeat(snap: &mut liveness::Snapshot) {
    let Some(hb) = snap.heartbeat_file.clone() else {
        return;
    };
    let Some(threshold) = snap.heartbeat_stale_threshold_secs else {
        return;
    };
    let (freshness, age) = crate::daemon_install_state::check_heartbeat(
        std::path::Path::new(&hb),
        threshold,
        snap.process_age_secs,
    );
    snap.heartbeat = Some(freshness);
    snap.heartbeat_age_secs = age;
}

/// Why this tick escalates out-of-band, if it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscalationTrigger {
    /// The bounded recovery budget is spent and the daemon is still down.
    BreakerOpen,
    /// No attempt is even possible on this host, and the outage has persisted
    /// for as many ticks as the budget would have allowed attempts.
    RecoveryImpossible,
}

/// The shell escalates on TWO conditions (loom-daemon-watchdog.sh:2224-2228),
/// and porting only the first left the second silent: with auto-recovery
/// disabled, or no runnable recovery command, a confirmed outage filed no
/// forge issue at all — just a log line every tick, forever. That is the
/// 2026-07-26 shape this watchdog exists to prevent.
///
/// Extracted as a predicate because the retained suite cannot reach it: the
/// harness pins `AUTO_RECOVER=0` together with `ESCALATE=0`, so no case ever
/// enables escalation while recovery is impossible.
fn escalation_trigger(
    decision: &recovery::Decision,
    ticks: u64,
    max_attempts: u64,
) -> Option<EscalationTrigger> {
    match decision {
        recovery::Decision::BreakerOpen { .. } => Some(EscalationTrigger::BreakerOpen),
        // `Disabled` covers both of the shell's `recover_possible != true`
        // cases — recovery switched off, and no runnable argv — because the
        // caller maps `Argv::Unavailable` onto it.
        recovery::Decision::Disabled if ticks >= max_attempts => {
            Some(EscalationTrigger::RecoveryImpossible)
        }
        _ => None,
    }
}

/// The directory the ENTRY POINT lives in — not the binary's.
///
/// Sibling scripts (`loom-daemon-start.sh` for bounded recovery,
/// `create-issue.sh` for the tier-3 escalation fallback) are resolved relative
/// to the script, which is what `$_LOOM_WATCHDOG_CLI_DIR` meant in the shell.
///
/// `current_exe()` is the wrong answer and was silently wrong on every normal
/// install: it points at `~/.local/bin/loom-daemon`, where no sibling scripts
/// exist, so bounded recovery degraded to REPORT-ONLY — "no readable
/// loom-daemon-start.sh beside this watchdog" — while the real sibling sat
/// next to the stub that invoked it. The retained suite never caught it
/// because it always pins `LOOM_WATCHDOG_RECOVER_CMD`.
///
/// The stub exports `LOOM_WATCHDOG_CLI_DIR`; the fallback keeps a direct
/// `loom-daemon daemon-watchdog` invocation working, where there is no stub
/// and therefore no sibling scripts to find anyway.
fn cli_dir() -> PathBuf {
    cli_dir_from(env::var("LOOM_WATCHDOG_CLI_DIR").as_deref())
}

/// The decision half of [`cli_dir`], split out so it can be tested without
/// mutating process environment — which is shared across threads and makes an
/// env-setting test order-dependent against every other test in the binary.
fn cli_dir_from(exported: Option<&str>) -> PathBuf {
    if let Some(d) = exported.filter(|v| !v.is_empty()).map(PathBuf::from) {
        if d.is_dir() {
            return d;
        }
    }
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod high_fix_tests;

/// Unix seconds.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// What the bounded in-band probe concluded.
///
/// `healthy` is NOT `exit.is_none()`. A probe that was disabled, skipped, or
/// never ran (no pid to ask about) also produces no exit code, and treating
/// that as healthy is what let the 50-series peer-coordination check spend a
/// SECOND round-trip on a tick that had deliberately spent none — turning
/// `LOOM_WATCHDOG_IPC_PROBE=0` into a 15-second hang against a mock that never
/// returns. The shell gates that check on `probe_verdict == healthy`; this is
/// that value, carried explicitly rather than inferred.
struct ProbeOutcome {
    /// An exit code the tick must return immediately.
    exit: Option<i32>,
    /// The round-trip actually succeeded this tick.
    healthy: bool,
}

/// Sections 12-22: the bounded in-band IPC probe.
///
/// Returns `Some(code)` only for a CONFIRMED hang, which exits immediately.
/// Every other outcome falls through to the heartbeat section, possibly having
/// set [`report::Reporter::probe_diverged`] — which is what makes an OK-shaped
/// heartbeat line render as `DEGRADED` and owns the tick's exit code (#5790).
///
/// Two signals, deliberately distinct:
///
/// - **Consecutive** (#4398): N failures in a row against the same pid is a
///   confirmed hang. Resets on any clean round-trip, because a transient
///   failure under concurrent-sweep load is the common case (#4279).
/// - **Windowed** (#5944): N failures among the last M ticks. An intermittent
///   probe — fail, succeed, fail, succeed — never reaches 3 consecutive, so the
///   first signal never fires for it, yet failing half your ticks is not a
///   clean bill of health either.
fn ipc_probe(
    paths: &config::Paths,
    reporter: &report::Reporter,
    state: &consts::StateFiles,
    snap: &liveness::Snapshot,
) -> ProbeOutcome {
    let Some(pid) = snap.pid else {
        return ProbeOutcome {
            exit: None,
            healthy: false,
        };
    };

    let repo_root = marker::get_nonempty(&paths.marker, "repo_root").map(PathBuf::from);
    let bin = locate::daemon_bin(repo_root.as_deref());
    let attempt = probe::attempt(&paths.socket_path, bin.as_deref());
    let (verdict, detail) = probe::classify_ipc(
        &attempt,
        snap.process_age_secs,
        probe::probe_grace_secs(),
        &paths.socket_path,
        probe::probe_timeout_secs(),
    );

    let window_ticks =
        env::num("LOOM_WATCHDOG_IPC_PROBE_WINDOW_TICKS", consts::DEFAULT_PROBE_WINDOW_TICKS);
    let window_threshold = env::num(
        "LOOM_WATCHDOG_IPC_PROBE_WINDOW_FAIL_THRESHOLD",
        consts::DEFAULT_PROBE_WINDOW_FAIL_THRESHOLD,
    );
    let fail_threshold =
        env::num("LOOM_WATCHDOG_IPC_PROBE_FAIL_THRESHOLD", consts::DEFAULT_PROBE_FAIL_THRESHOLD);

    match verdict {
        probe::IpcVerdict::Healthy => {
            probe_state::clear_fail_streak(&state.probe_fail_count);
            let w = probe_state::record(&state.probe_window, pid, false, window_ticks);

            if w.fails >= window_threshold {
                let load = load_avg::sample();
                reporter.report(
                    report::Level::Degraded,
                    &format!(
                        "daemon IPC round-trip OK this tick ({detail}), but {} of the last {} \
                         watchdog ticks failed the same probe (window threshold \
                         {window_threshold}/{window_ticks}, #5944) — none of them were 3 \
                         CONSECUTIVE, so neither the same-tick (#5790) nor sustained-CONFIRMED \
                         (#4398) signal fired for this pattern, but failures this frequent are \
                         not a clean bill of health either. Host load average at probe time: \
                         {load}. NOT a confirmed hang (this tick's own round-trip answered) and \
                         no remediation is attempted; the window ages out on its own as old \
                         ticks roll off, and a run of clean ticks lets it clear naturally.",
                        w.fails, w.len
                    ),
                );
                reporter.set_diverged(format!(
                    "the IPC probe has failed {} of the last {} watchdog ticks (see the \
                     DEGRADED line above, #5944) — dispatch may be intermittently degraded \
                     despite THIS tick's own round-trip succeeding; the exit code for this tick \
                     reflects that windowed/rate signal, not this line.",
                    w.fails, w.len
                ));
            } else {
                reporter.report(report::Level::Ok, &format!("IPC probe OK: {detail}."));
            }
            ProbeOutcome {
                exit: None,
                healthy: true,
            }
        }

        probe::IpcVerdict::Unresponsive => {
            let streak = probe_state::fail_streak(&state.probe_fail_count, pid) + 1;
            probe_state::write_fail_streak(&state.probe_fail_count, pid, streak);
            probe_state::record(&state.probe_window, pid, true, window_ticks);
            let load = load_avg::sample();

            if streak >= fail_threshold {
                reporter.report(
                    report::Level::Divergence,
                    &format!(
                        "daemon IPC UNRESPONSIVE (CONFIRMED): the process is alive ({}) — and \
                         its heartbeat may well look FRESH — but the bounded socket round-trip \
                         has now failed on {streak} CONSECUTIVE watchdog ticks (threshold \
                         {fail_threshold}). {detail}. Host load average at probe time: {load}. \
                         The heartbeat writer and the IPC accept loop are independent tokio \
                         tasks, so a fresh heartbeat does NOT prove the daemon can still serve \
                         work: autonomous dispatch is effectively DEAD while this holds. No \
                         automatic kill/restart is attempted (#4398 — there is no provably-safe \
                         unattended remediation for a wedged-but-alive process). RECOVER: \
                         'loom-daemon restart' (note: the restart primitive travels over this \
                         same wedged socket and may itself hang), else \
                         ./.loom/scripts/cli/loom-daemon-stop.sh && \
                         ./.loom/scripts/cli/loom-daemon-start.sh [flags]. Diagnose with: \
                         LOOM_SOCKET_PATH={} {}s-bounded 'loom-daemon status'; sample the \
                         process with 'sample {pid}' (macOS) or 'gdb -p {pid}' to capture the \
                         wedge before killing it.",
                        snap.detail,
                        paths.socket_path.display(),
                        probe::probe_timeout_secs()
                    ),
                );
                return ProbeOutcome {
                    exit: Some(1),
                    healthy: false,
                };
            }

            reporter.report(
                report::Level::Divergence,
                &format!(
                    "daemon IPC probe FAILED while the process is alive ({}): {detail}. This is \
                     consecutive failure {streak} of {fail_threshold} — NOT yet a confirmed hang \
                     (a single failure can be transient contention under concurrent-sweep load, \
                     #4279) and no remediation is attempted. Host load average at probe time: \
                     {load}. If the next watchdog tick round-trips cleanly the streak resets.",
                    snap.detail
                ),
            );
            reporter.mark_diverged();
            ProbeOutcome {
                exit: None,
                healthy: false,
            }
        }

        // Nothing was learned. Explicitly not counted as a hang, and the window
        // is NOT recorded — an unobserved tick is not a healthy one, and
        // recording it as either would bias the rate signal.
        probe::IpcVerdict::Skipped => {
            reporter.report(report::Level::Ok, &format!("IPC probe skipped: {detail}."));
            ProbeOutcome {
                exit: None,
                healthy: false,
            }
        }
    }
}

/// The 50-series: peer-claim coordination health (#6222/#7258/#7664).
///
/// A degraded receive path is silent by construction — each host still believes
/// it is coordinating while actually working alone, so two can claim the same
/// issue and neither notices. That is why this is asked explicitly rather than
/// waited for.
///
/// Reports only; it never exits. Coordination degradation is a fleet-level
/// concern, and failing this tick would conflate it with the daemon-liveness
/// question the exit code answers.
fn peer_coordination(
    paths: &config::Paths,
    reporter: &report::Reporter,
    state: &consts::StateFiles,
    snap: &liveness::Snapshot,
) {
    if !peer_coord::enabled() {
        return;
    }
    let repo_root = marker::get_nonempty(&paths.marker, "repo_root").map(PathBuf::from);
    let Some(bin) = locate::daemon_bin(repo_root.as_deref()) else {
        return;
    };

    let mut cmd = std::process::Command::new(&bin);
    cmd.args(["peer-claims", "--json"])
        .env("LOOM_SOCKET_PATH", &paths.socket_path);
    let timeout =
        std::time::Duration::from_secs(env::num("LOOM_WATCHDOG_PEER_COORD_TIMEOUT_SECS", 15));
    let Some(out) = crate::sweep_registry::output_with_timeout(cmd, timeout)
        .ok()
        .flatten()
    else {
        return;
    };
    if !out.status.success() {
        return;
    }
    // `None` is "could not determine", which is deliberately NOT reported.
    // Saying nothing is correct here; saying "healthy" would be a claim, and
    // saying "unknown" every tick on a host without peers is noise.
    let Some(health) = peer_coord::parse(&String::from_utf8_lossy(&out.stdout)) else {
        return;
    };

    match health.verdict {
        peer_coord::Verdict::Degraded => {
            let note = if state.peer_coord_sentinel.exists() {
                reporter.report(
                    report::Level::Ok,
                    &format!(
                        "peer-coordination degradation already escalated out-of-band (sentinel \
                         {}).",
                        state.peer_coord_sentinel.display()
                    ),
                );
                return;
            } else if let Some(remaining) = peer_coord::cooldown_remaining(
                &state.peer_coord_cooldown,
                now_secs(),
                peer_coord::cooldown_secs(),
            ) {
                // #7258: a path that recovers and re-degrades within minutes is
                // ONE episode to an operator, not two. Filing per flap buries
                // the signal under duplicates of itself.
                format!(
                    "Suppressing a duplicate tracking issue: a previous episode on this host \
                     recovered within the last {}s cooldown window (#7258) — {remaining}s \
                     remaining before a repeat degradation would file fresh again. {} is the \
                     signal for this flap.",
                    peer_coord::cooldown_secs(),
                    paths.log.display()
                )
            } else if env::var("LOOM_WATCHDOG_ESCALATE").is_some_and(|v| env::is_false(&v)) {
                format!(
                    "Out-of-band escalation is disabled — THIS LOGFILE IS THE ONLY SIGNAL for \
                     this degradation, {}.",
                    paths.log.display()
                )
            } else if let Some(note) = peer_coord::read_cooldown(&state.peer_coord_cooldown)
                .and_then(|c| {
                    // #7664: the cooldown has ELAPSED, but this is still the
                    // same episode if we are inside the (longer) dedup window
                    // and have a usable prior issue reference. Comment on that
                    // issue and reopen it rather than filing a duplicate.
                    //
                    // This path existed as `repeat_action`/`flap_comment` with
                    // unit tests and NO callers — the fifth instance in this
                    // port of code that a comment claimed was wired. The
                    // CI-wired dedup suite is what proved it: 10 of its 27
                    // assertions exercise exactly this, and I had not run it.
                    match peer_coord::repeat_action(
                        Some(&c),
                        now_secs(),
                        peer_coord::dedup_window_secs(),
                    ) {
                        peer_coord::Repeat::CommentOn { flap, .. } => peer_coord::dedup_comment(
                            &c,
                            &state.peer_coord_sentinel,
                            &state.peer_coord_cooldown,
                            &escalate::hostname(),
                            &health.summary,
                            flap,
                            reporter,
                        ),
                        peer_coord::Repeat::FileFresh => None,
                    }
                })
            {
                note
            } else {
                // #6222: file a forge tracking issue so the degradation is not
                // confined to a logfile nobody tails.
                let cli_dir = cli_dir();
                let fallback =
                    env::var("LOOM_WATCHDOG_CREATE_ISSUE_FALLBACK_DIR").map(PathBuf::from);
                let script = escalate::resolve_issue_script(
                    repo_root.as_deref(),
                    &cli_dir,
                    fallback.as_deref(),
                );
                let hostname = escalate::hostname();
                match script.as_deref().and_then(|s| {
                    peer_coord::file(s, &hostname, &health, &paths.log, &state.peer_coord_sentinel)
                }) {
                    Some(issue_ref) => {
                        peer_coord::write_sentinel(
                            &state.peer_coord_sentinel,
                            &issue_ref,
                            reporter,
                        );
                        "ESCALATED out-of-band: filed a forge tracking issue so this degradation \
                         is not confined to a logfile nobody tails (#6222)."
                            .to_string()
                    }
                    None => format!(
                        "Out-of-band escalation was NOT possible (no create-issue.sh reachable, \
                         or the forge call failed) — THIS LOGFILE IS THE ONLY SIGNAL for this \
                         degradation, {}.",
                        paths.log.display()
                    ),
                }
            };
            reporter.report(
                report::Level::Divergence,
                &format!("peer-claim coordination is DEGRADED: {}. {note}", health.summary),
            );
        }
        peer_coord::Verdict::Green => {
            // Only worth a line when there was something to recover FROM.
            if state.peer_coord_sentinel.exists() {
                let hostname = escalate::hostname();
                if peer_coord::recover(
                    &state.peer_coord_sentinel,
                    &state.peer_coord_cooldown,
                    &hostname,
                    &health.summary,
                ) {
                    // Level and wording are contract: the shell reports OK
                    // here (not WARN) and the retained assertion greps for
                    // "cleared the escalation sentinel". A paraphrase reads
                    // the same to a human and silently fails the assertion.
                    reporter.report(
                        report::Level::Ok,
                        &format!(
                            "peer-claim coordination has RECOVERED ({}) — closed the tracking \
                             issue and cleared the escalation sentinel (#6222).",
                            health.summary
                        ),
                    );
                } else {
                    // Best-effort by design: a missing `gh`, no forge auth, or
                    // a failed close leaves the sentinel so a LATER healthy
                    // tick retries, rather than losing track of an open issue.
                    reporter.report(
                        report::Level::Warn,
                        &format!(
                            "peer-claim coordination has RECOVERED ({}) but closing/commenting \
                             the tracking issue failed — the sentinel is left in place so a \
                             later healthy tick retries (#6222).",
                            health.summary
                        ),
                    );
                }
            }
        }
    }
    let _ = snap;
}

/// #5118: ask the socket before concluding a daemon is gone.
///
/// The incident this closes: the pid file alone was treated as sufficient to
/// declare an outage, and it is the WEAKEST of the three signals — a path
/// disagreement between the writer and the reader made an absent file look like
/// a dead daemon. An absent file is **no evidence at all**, which is a different
/// thing from evidence of absence, and conflating them paged for a daemon that
/// was serving traffic the whole time.
///
/// Returns `exit: Some(code)` when this tick's answer is settled here; `None`
/// when the caller should continue to the outage path. May flip `snap` to
/// alive.
///
/// `healthy` reports that THIS round-trip answered — the shell's
/// SOCKET_ONLY_LIVENESS path, which sets `probe_verdict=healthy` directly.
/// The later in-band probe cannot re-derive it, because on this path there is
/// no pid to ask about and it returns before probing at all.
fn socket_corroboration(
    paths: &config::Paths,
    reporter: &report::Reporter,
    snap: &mut liveness::Snapshot,
) -> ProbeOutcome {
    let repo_root = marker::get_nonempty(&paths.marker, "repo_root").map(PathBuf::from);
    let bin = locate::daemon_bin(repo_root.as_deref());
    let attempt = probe::attempt(&paths.socket_path, bin.as_deref());
    let (verdict, socket_detail) =
        probe::classify(&attempt, &paths.socket_path, probe::probe_timeout_secs());
    let socket = paths.socket_path.display();

    match verdict {
        probe::SocketVerdict::Answered => {
            if snap.source == liveness::Source::PidFile {
                // The pid-file hint was unusable and the socket answers. The
                // socket wins: it is in-band evidence of the thing actually
                // being asked about.
                snap.state = crate::daemon_install_state::InstallState::AliveButUnresponsive;

                // Recompute heartbeat freshness now that liveness is
                // established. `liveness.rs` computes it only when the process
                // was ALREADY known alive — reasonable on its own, since
                // freshness is meaningless about a process that does not exist
                // — but this path flips the snapshot to alive afterwards, and
                // nothing re-asked.
                //
                // The result was a fail-OPEN: no pid file, socket answers,
                // heartbeat file 1000s old against a 300s threshold, and the
                // tick reported "no heartbeat file … (heartbeat disabled or
                // not yet written) — liveness-only OK" and exited 0, while the
                // file existed and was stale. The shell's section 4 ran on
                // HEARTBEAT_FILE regardless of how liveness was established.
                refresh_heartbeat(snap);

                snap.detail = format!(
                    "daemon ANSWERS on {socket} ({socket_detail}) — authoritative; the pid-file \
                     hint was unusable ({}), which is NOT evidence of an outage (#5118)",
                    snap.detail
                );
                reporter.report(
                    report::Level::Ok,
                    &format!("daemon healthy via the in-band socket round-trip: {}.", snap.detail),
                );
                ProbeOutcome {
                    exit: None,
                    healthy: true,
                }
            } else {
                // A supervisor says the job is gone, yet something serves the
                // socket. Dispatch is running, so this is NOT an outage — but
                // nothing will relaunch it when it dies, and relaunching into a
                // served socket would only be refused by the singleton guard.
                reporter.report(
                    report::Level::Warn,
                    &format!(
                        "STATE MISMATCH: {}, yet a daemon ANSWERS on {socket} ({socket_detail}). \
                         Dispatch is still running, so this is NOT an autonomy outage — but the \
                         daemon is UNSUPERVISED: nothing will relaunch it if it dies, and no \
                         auto-remediation is attempted here (relaunching into a served socket \
                         would only be refused by the singleton guard). Heal it by rolling the \
                         daemon through ./.loom/scripts/cli/loom-daemon-stop.sh && \
                         ./.loom/scripts/cli/loom-daemon-start.sh [flags] so the supervisor owns \
                         it again.",
                        snap.detail
                    ),
                );
                ProbeOutcome {
                    exit: Some(1),
                    healthy: false,
                }
            }
        }

        probe::SocketVerdict::Unreachable => {
            // Two independent signals agree. That is a real outage.
            snap.detail =
                format!("{}; and the in-band probe confirms it: {socket_detail}", snap.detail);
            ProbeOutcome {
                exit: None,
                healthy: false,
            }
        }

        probe::SocketVerdict::Indeterminate => {
            if snap.source == liveness::Source::PidFile
                && snap.pidfile_evidence == liveness::PidfileEvidence::Absent
            {
                // No evidence either way, from the weakest signal. Exit 3 is a
                // distinct code precisely so this is not counted as an outage:
                // "we could not tell" and "it is down" call for different
                // responses, and #5118 is what happens when they share one.
                reporter.report(
                    report::Level::Unknown,
                    &format!(
                        "LIVENESS UNDETERMINED: a daemon is EXPECTED (autonomy-desired marker \
                         present, started {}) but this tick found NO evidence either way — {}, \
                         and the in-band socket probe could not answer: {socket_detail}. This is \
                         deliberately NOT reported as an outage (#5118): the pid file alone is \
                         too weak a signal to declare one. Restore the in-band probe (make a \
                         loom-daemon binary resolvable — LOOM_DAEMON_BIN / PATH / ~/.local/bin — \
                         and leave LOOM_WATCHDOG_IPC_PROBE enabled), then re-check with \
                         'loom-daemon health'.",
                        snap.started_at.as_deref().unwrap_or(""),
                        snap.detail
                    ),
                );
                return ProbeOutcome {
                    exit: Some(3),
                    healthy: false,
                };
            }
            snap.detail = format!(
                "{}; the in-band probe could not corroborate: {socket_detail}",
                snap.detail
            );
            ProbeOutcome {
                exit: None,
                healthy: false,
            }
        }
    }
}
