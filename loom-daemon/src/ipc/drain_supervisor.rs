//! Handling a `DrainAndRestartDaemon` request and supervising the drain it
//! starts (Issue #4090, extended by #4343, #4521, #6007, #6969 and #9588).
//!
//! Moved out of `ipc.rs` because that file is over
//! `.loom/docs/file-size-policy.md`'s threshold and frozen. Re-exported from
//! `crate::ipc` verbatim, so every existing caller is unchanged.

use super::drain_roll::{
    drain_pending_budget, drain_refusal_path, drain_timeout_action, RefusalPath, TimeoutAction,
};
use super::evaluate_drain_tick;
use super::{
    cancel_all_in_flight, count_in_flight_sweeps, detect_supervisor, DrainBegin, DrainOrigin,
    DrainState, DrainTick, RollRefusal, DEFAULT_DRAIN_TIMEOUT_SECS, DRAIN_POLL_INTERVAL,
    EXIT_RESTART, EXIT_SHUTDOWN,
};
use crate::event_bus::EventBus;
use crate::types::Response;
use crate::workspace_pool::WorkspacePool;
use chrono::Utc;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Handle a `DrainAndRestartDaemon` request (Issue #4090): check supervision up
/// front (AC5 — refuse *before* pausing dispatch), then start the drain and
/// spawn its supervisor task. Returns the `DaemonDrain` response to send back.
///
/// Must be called from within a tokio runtime context (the connection handler
/// is) so it can spawn the supervisor.
///
/// Made `pub` for the autonomous self-update loop (#4055): after it rebuilds
/// and provisions a fresh binary, it triggers the roll through this exact drain
/// path — not a bare `RestartDaemon` — so in-flight sweeps finish first and
/// survive in the registry rather than being orphaned. The loop calls it from a
/// blocking thread inside a `tokio::runtime::Handle::enter()` guard so the
/// internal `tokio::spawn` of the supervisor still resolves a runtime.
///
/// `origin` (#9588) records who asked: every IPC request is
/// [`DrainOrigin::Operator`]; only the self-update loop passes
/// [`DrainOrigin::AutoUpdate`]. It decides what a deadline without
/// `--force-after-timeout` does — see [`DrainOrigin`].
#[allow(clippy::too_many_arguments)]
pub fn handle_drain_request(
    drain: &Arc<DrainState>,
    workspace_pool: &Arc<WorkspacePool>,
    fallback_root: &Path,
    event_bus: &Arc<EventBus>,
    timeout_secs: Option<u64>,
    force_after_timeout: bool,
    then_exit: bool,
    origin: DrainOrigin,
) -> Response {
    // AC5 / Finding 4: prove supervision BEFORE entering DRAINING — for the
    // #4090 restart-when-drained case. A `then_exit` drain (#4343) deliberately
    // does NOT want a relaunch, so the supervisor requirement does not apply to
    // it at all: skip the refusal gate entirely and just report whatever
    // supervisor (if any) is detected, informationally.
    let supervisor = if then_exit {
        detect_supervisor()
    } else {
        match detect_supervisor() {
            Some(s) => Some(s),
            None => {
                return Response::DaemonDrain {
                    accepted: false,
                    supervisor: None,
                    in_flight: count_in_flight_sweeps(workspace_pool, fallback_root),
                    message: "refusing to drain: no supervisor detected \
                        (LOOM_DAEMON_SUPERVISOR unset). This daemon was not started under \
                        a recognized supervisor, so nothing would relaunch it after a drain. \
                        Dispatch was NOT paused. Restart manually with loom-daemon-stop.sh && \
                        loom-daemon-start.sh. If this IS a systemd --user service (e.g. a \
                        fleet worker provisioned before #4640), retrofit it instead: mkdir -p \
                        ~/.config/systemd/user/loom-daemon.service.d && printf \
                        '[Service]\\nEnvironment=LOOM_DAEMON_SUPERVISOR=systemd\\nRestart=on-success\\n' \
                        > ~/.config/systemd/user/loom-daemon.service.d/supervisor.conf && \
                        systemctl --user daemon-reload."
                        .to_string(),
                    then_exit,
                };
            }
        }
    };

    let in_flight = count_in_flight_sweeps(workspace_pool, fallback_root);
    let timeout = Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_DRAIN_TIMEOUT_SECS));

    match drain.begin_as(timeout, force_after_timeout, then_exit, origin) {
        DrainBegin::Started {
            generation,
            deadline,
        } => {
            let _ = event_bus.publish_generic(
                "daemon.drain.started",
                serde_json::json!({
                    "in_flight": in_flight,
                    "timeout_secs": timeout.as_secs(),
                    "force_after_timeout": force_after_timeout,
                    "then_exit": then_exit,
                    "deadline": deadline,
                    "origin": origin.as_str(),
                }),
            );
            let drain_task = drain.clone();
            let pool_task = workspace_pool.clone();
            let root_task = fallback_root.to_path_buf();
            let bus_task = event_bus.clone();
            tokio::spawn(async move {
                run_drain_supervisor(
                    drain_task,
                    pool_task,
                    root_task,
                    bus_task,
                    generation,
                    DRAIN_POLL_INTERVAL,
                )
                .await;
            });
            let msg = if then_exit {
                if in_flight == 0 {
                    "drain scheduled (then-exit): 0 in-flight — stopping now (will NOT relaunch)."
                        .to_string()
                } else {
                    format!(
                        "drain scheduled (then-exit): {in_flight} in-flight sweep(s); new dispatch \
                         paused. Will stop (NOT relaunch) when drained, or {} at the deadline.",
                        if force_after_timeout {
                            "cancel stragglers and stop"
                        } else {
                            // #9588: an operator stop never resumes dispatch.
                            "keep dispatch PAUSED, report the stragglers, and still stop once \
                             they finish (dispatch never resumes on its own — `--abort-drain` \
                             resumes it, `--force-after-timeout` cancels them)"
                        }
                    )
                }
            } else if in_flight == 0 {
                format!(
                    "drain scheduled ({}-supervised): 0 in-flight — restarting now.",
                    supervisor.as_deref().unwrap_or("unknown")
                )
            } else {
                // #6007: the non-force deadline no longer "refuses and resumes
                // dispatch" on a roll — promising that is what taught operators
                // to re-run with a bigger --timeout in the first place.
                let deadline_action = if force_after_timeout {
                    "cancel stragglers and restart".to_string()
                } else if origin == DrainOrigin::Operator {
                    // #9588: an operator drain holds, it never gives up.
                    "keep dispatch PAUSED and report the stragglers — the restart still fires \
                     once they finish; dispatch never resumes on its own (`--abort-drain` \
                     resumes it, `--force-after-timeout` cancels them)"
                        .to_string()
                } else {
                    format!(
                        "hold the roll PENDING (dispatch stays paused, the restart re-arms \
                         itself when in-flight reaches zero, for up to {}s total before giving \
                         up and resuming dispatch)",
                        drain_pending_budget(timeout).as_secs()
                    )
                };
                format!(
                    "drain scheduled ({}-supervised): {in_flight} in-flight sweep(s); \
                     new dispatch paused. Will restart when drained, or {deadline_action} at \
                     the deadline.",
                    supervisor.as_deref().unwrap_or("unknown"),
                )
            };
            Response::DaemonDrain {
                accepted: true,
                supervisor,
                in_flight,
                message: msg,
                then_exit,
            }
        }
        // Issue #4521: the ack must describe the **active** drain's terminal
        // action, never the requested one. Echoing the request here is what let
        // an operator's `--drain --then-exit` be acked as "will stop" while an
        // in-progress relaunch-drain (an auto-update roll) exited 0 and launchd
        // brought the daemon straight back up.
        DrainBegin::AlreadyDraining {
            active_then_exit,
            escalated,
            force_escalated,
            origin_promoted,
        } => {
            let _ = event_bus.publish_generic(
                "daemon.drain.already_draining",
                serde_json::json!({
                    "in_flight": in_flight,
                    "requested_then_exit": then_exit,
                    "active_then_exit": active_then_exit,
                    "escalated": escalated,
                    "force_escalated": force_escalated,
                    "origin_promoted": origin_promoted,
                }),
            );
            if escalated {
                log::warn!(
                    "drain escalated to then-exit (Issue #4521): an in-progress relaunch-drain \
                     (e.g. an auto-update roll) will now exit {EXIT_SHUTDOWN} and stay down \
                     instead of relaunching"
                );
            }
            if force_escalated {
                log::warn!(
                    "pending roll escalated to --force-after-timeout (Issue #6007): the \
                     remaining in-flight sweep(s) will be cancelled and the restart will fire on \
                     the next supervisor tick"
                );
            }
            // #6007: a retained (pending) roll must not be acked as if it were a
            // first-attempt drain whose "existing deadline is unchanged" — an
            // operator needs to know the roll already survived a refusal and is
            // waiting on quiescence.
            let snap = drain.snapshot();
            let roll_pending = snap.roll_pending;
            let message = if force_escalated {
                // #9588: any active drain escalates, not only a pending roll.
                let terminal = if active_then_exit {
                    "STOP and stay down"
                } else {
                    "restart"
                };
                let when = snap.deadline.map_or_else(
                    || "on the next supervisor tick".to_string(),
                    |d| {
                        format!(
                            "at {d} (the supervisor checks every {}s)",
                            DRAIN_POLL_INTERVAL.as_secs()
                        )
                    },
                );
                format!(
                    "already draining — ESCALATED to --force-after-timeout{}: the in-progress \
                     drain will CANCEL the {in_flight} remaining in-flight sweep(s) {when}, then \
                     {terminal}, instead of waiting for them to finish.",
                    if escalated { " AND to then-exit" } else { "" }
                )
            } else if escalated {
                format!(
                    "already draining (idempotent) — ESCALATED to then-exit: the in-progress \
                     drain was a relaunch drain (e.g. an auto-update roll) and will now STOP \
                     and stay down when drained, NOT relaunch. {in_flight} in-flight sweep(s); \
                     the existing deadline is unchanged. If a relaunch was wanted after all, \
                     `loom-daemon restart --abort-drain` and re-issue without --then-exit."
                )
            } else if active_then_exit && !then_exit {
                format!(
                    "already draining (idempotent): the in-progress drain is a then-exit \
                     teardown — it will STOP and stay down when drained, NOT relaunch, so this \
                     restart-when-drained request will not be honored (then-exit is never \
                     downgraded). {in_flight} in-flight sweep(s); the existing deadline is \
                     unchanged. Use `loom-daemon restart --abort-drain` to cancel."
                )
            } else if active_then_exit {
                format!(
                    "already draining (idempotent): the in-progress drain will STOP and stay \
                     down when drained (then-exit). {in_flight} in-flight sweep(s); the existing \
                     deadline is unchanged. Use `loom-daemon restart --abort-drain` to cancel."
                )
            } else if roll_pending {
                format!(
                    "already draining (idempotent): a PENDING ROLL is already retained — it \
                     survived its deadline, dispatch stays paused, and the restart re-arms \
                     itself when in-flight reaches zero. {in_flight} in-flight sweep(s); the \
                     re-armed deadline is unchanged. Use `loom-daemon restart --abort-drain` to \
                     cancel it, or `loom-daemon restart --drain --force-after-timeout` to cancel \
                     the stragglers and roll now."
                )
            } else {
                format!(
                    "already draining (idempotent): the in-progress drain will RESTART when \
                     drained. {in_flight} in-flight sweep(s); the existing deadline is \
                     unchanged. Use `loom-daemon restart --abort-drain` to cancel."
                )
            };
            let message = if origin_promoted {
                format!(
                    "{message} The drain was an auto-update roll and is now an OPERATOR drain \
                     (#9588): a timeout keeps dispatch paused instead of resuming it."
                )
            } else {
                message
            };
            Response::DaemonDrain {
                accepted: true,
                supervisor,
                in_flight,
                message,
                then_exit: active_then_exit,
            }
        }
    }
}

/// The exit code a terminal drain tick must use (Issue #4521).
///
/// Load-bearing and deliberately extracted as a pure function so the contract is
/// unit-testable without spawning a process: a **then-exit** drain must exit
/// [`EXIT_SHUTDOWN`] (143, non-zero) so a `KeepAlive:{SuccessfulExit:true}`
/// launchd job stays down — exiting [`EXIT_RESTART`] (0) there is precisely the
/// "drained, then relaunched anyway" failure. A relaunch drain exits
/// [`EXIT_RESTART`] so the supervisor brings it straight back.
#[must_use]
pub fn drain_exit_code(then_exit: bool) -> i32 {
    if then_exit {
        EXIT_SHUTDOWN
    } else {
        EXIT_RESTART
    }
}

/// The operator-facing log line emitted when a drain completes with zero
/// in-flight sweeps (Issue #4090, pinned by Issue #4521).
///
/// The two terminal actions **must** produce visibly different lines — a host
/// log has to say which one fired without the reader guessing. Extracted as a
/// pure function so that distinctness is a test assertion rather than a
/// convention. `supervisor` and `verify_poll_secs` are only interpolated on
/// the relaunch line (`verify_poll_secs` is unused on the `then_exit` branch —
/// there is no relaunch to verify there).
#[must_use]
pub fn drain_complete_log_line(then_exit: bool, supervisor: &str, verify_poll_secs: u64) -> String {
    if then_exit {
        format!(
            "drain complete — 0 in-flight sweeps; exiting {EXIT_SHUTDOWN} and staying down \
             (then_exit — Issue #4343 teardown). No sweep was killed; no orphan left behind."
        )
    } else {
        let note = crate::restart_verify::relaunch_verify_note(supervisor, verify_poll_secs);
        format!(
            "drain complete — 0 in-flight sweeps; exiting {EXIT_RESTART} for a \
             {supervisor}-supervised relaunch. No sweep was killed; no orphan left behind. {note}"
        )
    }
}

/// The operator-facing note recorded (and logged) when a drain times out and
/// refuses the restart (Issue #4090, made actionable by Issue #5340).
///
/// Before #5340 this stopped at "no --force-after-timeout", leaving the
/// operator to guess at a retry — the one they filed #5340 over guessed
/// `loom-daemon drain` (a bare, nonexistent subcommand) and then
/// `loom-daemon fleet drain <ssh_host>` (a *different*, newer remote
/// worker-decommission command that takes a completely different argument).
/// Naming the exact local retry command removes that guesswork: it is always
/// the same `restart --drain` invocation the operator already ran, with
/// `--force-after-timeout` added.
///
/// Extracted as a pure function — same rationale as
/// [`drain_complete_log_line`] just above — so the exact wording is a test
/// assertion rather than something only exercised by driving the full
/// supervisor loop to a real timeout.
#[must_use]
pub fn drain_timeout_refuse_note(in_flight: usize) -> String {
    format!(
        "drain timed out with {in_flight} sweep(s) still in flight — refused restart \
         (no --force-after-timeout); dispatch resumed, daemon stays up. Retry with: \
         `loom-daemon restart --drain --force-after-timeout --timeout <secs>` to force through \
         the remaining sweep(s), or re-run with a larger --timeout if they are simply \
         long-running rather than stuck."
    )
}

/// The operator-facing note recorded when a **relaunch (roll)** drain's deadline
/// passes and the roll is *retained* rather than discarded (Issue #6007).
///
/// This replaces the "dispatch resumed — retry with a bigger number" advice on
/// the roll path, which is precisely the advice that reproduced the livelock: on
/// a busy host every re-run raced the same deadline against a work finder that
/// had just been handed the admission window back. The note therefore says what
/// happens about the **recurrence** — nothing to re-run, the roll re-arms itself
/// — and names the two ways an operator can take over instead.
///
/// Extracted as a pure function (same rationale as
/// [`drain_timeout_refuse_note`]) so the wording is a test assertion rather than
/// something only a real 30-minute timeout exercises.
#[must_use]
pub fn drain_roll_pending_note(
    in_flight: usize,
    attempt: u32,
    window: Duration,
    budget: Duration,
) -> String {
    let window_secs = window.as_secs();
    let budget_secs = budget.as_secs();
    format!(
        "drain deadline passed with {in_flight} sweep(s) still in flight — restart REFUSED \
         (fail-safe: no sweep was cancelled and the pre-update binary keeps running). AUTO-UPDATE \
         ROLL PENDING (retry {attempt}): the roll intent is RETAINED — new dispatch stays PAUSED so \
         the in-flight set can reach zero, and the restart re-arms itself the moment it does. \
         Nothing to re-run: re-issuing `restart --drain` with a larger --timeout is exactly what \
         this replaces. Next deadline in {window_secs}s; total paused-dispatch budget \
         {budget_secs}s, after which the roll is abandoned and dispatch resumes. To give up now \
         and resume dispatch: `loom-daemon restart --abort-drain`. To force through the remaining \
         sweep(s) instead (cancels them): `loom-daemon restart --drain --force-after-timeout`."
    )
}

/// The operator-facing note recorded when a retained (pending) roll finally gives
/// up because its total paused-dispatch budget is spent (Issue #6007).
///
/// Keeps [`drain_timeout_refuse_note`]'s wording as its prefix — `loom-daemon
/// status` renders it the same way and #5340's exact-retry-command contract still
/// holds — then explains the recurrence: sweeps that outlived this much *paused*
/// dispatch are stuck rather than merely long-running, so the fix is to deal with
/// them, not to widen the window again.
#[must_use]
pub fn drain_roll_abandoned_note(in_flight: usize, attempts: u32, elapsed: Duration) -> String {
    let elapsed_secs = elapsed.as_secs();
    format!(
        "drain timed out with {in_flight} sweep(s) still in flight — refused restart \
         (no --force-after-timeout); dispatch resumed, daemon stays up. AUTO-UPDATE ROLL \
         ABANDONED (an operator drain would have stayed paused, #9588). The roll was retained \
         and re-armed {attempts} time(s) across {elapsed_secs}s of PAUSED dispatch and in-flight \
         still never reached zero, so the roll intent is now ABANDONED rather than starve this \
         host of work indefinitely — the provisioned binary was NOT activated. A sweep that \
         outlives {elapsed_secs}s of paused dispatch is stuck, not merely long-running: find it \
         with `loom-daemon list`, cancel it with `loom-daemon cancel --sweep <id>`, and the next \
         roll lands on its own. To force through instead: `loom-daemon restart --drain \
         --force-after-timeout --timeout <secs>`."
    )
}

/// Describe every non-terminal sweep across all managed roots as
/// `<sweep-id> (issue #N | PRs #a,#b)` — the stragglers an operator drain's
/// timeout names (Issue #9588). Same cross-root walk as
/// [`count_in_flight_sweeps`].
#[must_use]
pub fn list_in_flight_sweeps(
    workspace_pool: &Arc<WorkspacePool>,
    fallback_root: &Path,
) -> Vec<String> {
    let workspace_registry =
        crate::workspace_registry::WorkspaceRegistry::load_default().unwrap_or_default();
    let mut out = Vec::new();
    for root in &workspace_registry.effective_roots(fallback_root) {
        let registry = workspace_pool.get_or_provision(root);
        let sr = registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for info in sr.list(None).into_iter().filter(|i| !i.state.is_terminal()) {
            let what = match &info.kind {
                crate::types::SweepKind::Issue(n) => format!("issue #{n}"),
                crate::types::SweepKind::PrSet(prs) => format!(
                    "PRs {}",
                    prs.iter()
                        .map(|p| format!("#{p}"))
                        .collect::<Vec<_>>()
                        .join(",")
                ),
            };
            out.push(format!("{} ({what})", info.sweep_id));
        }
    }
    out
}

/// The note (and log line) an **operator** drain records when its deadline
/// passes without `--force-after-timeout` (Issue #9588): dispatch stays
/// PAUSED, the stragglers are named, and the two operator actions that end the
/// hold are spelled out. Pure, so the wording is a test assertion.
#[must_use]
pub fn drain_timeout_hold_note(then_exit: bool, stragglers: &[String]) -> String {
    let terminal = if then_exit {
        "stop and stay down"
    } else {
        "restart"
    };
    let list = if stragglers.is_empty() {
        "none listed".to_string()
    } else {
        stragglers.join(", ")
    };
    format!(
        "drain deadline passed with {} sweep(s) still in flight — dispatch stays PAUSED \
         (operator drain, #9588: it never resumes on its own). Stragglers: {list}. The daemon \
         will {terminal} as soon as they finish. To cancel them and {terminal} now: \
         `loom-daemon restart --drain --force-after-timeout`. To give up and resume dispatch: \
         `loom-daemon restart --abort-drain`.",
        stragglers.len()
    )
}

/// The drain-supervisor loop (Issue #4090). Polls the cross-root in-flight count
/// and owns the eventual `std::process::exit(EXIT_RESTART)`; on a fail-safe
/// timeout it clears the drain flag and stays up. Stops without exiting if it
/// has been superseded (a new drain) or aborted (generation moved on).
///
/// The terminal action (`then_exit`) is **re-read from [`DrainState`] on every
/// tick** rather than captured at spawn (Issue #4521): an in-progress
/// relaunch-drain can be escalated to stay-down by a later
/// `--drain --then-exit` request, and a supervisor holding a stale `false` would
/// exit `0` and be relaunched by the supervisor anyway.
async fn run_drain_supervisor(
    drain: Arc<DrainState>,
    workspace_pool: Arc<WorkspacePool>,
    fallback_root: PathBuf,
    event_bus: Arc<EventBus>,
    my_generation: u64,
    poll_interval: Duration,
) {
    // Issue #6969: the bound a detached post-exit verifier polls against,
    // resolved once per supervisor rather than per tick — the same knob
    // `restart_verify::verify_and_heal` itself resolves, so the log line and
    // the verifier it describes can never disagree about the bound.
    let verify_poll_secs = crate::restart_verify::resolve_configured_poll_secs();
    loop {
        // Superseded / aborted: a newer drain or an abort bumped the generation,
        // so this supervisor is stale — stop WITHOUT ending the process. This is
        // the "abort then the queue empties anyway" guard (AC6): a stale
        // supervisor must never fire a restart.
        if drain.generation() != my_generation {
            log::info!(
                "drain supervisor (gen {my_generation}) superseded/aborted (current gen {}) — \
                 stopping without restart",
                drain.generation()
            );
            return;
        }

        let in_flight = count_in_flight_sweeps(&workspace_pool, &fallback_root);
        // One consistent read of the live descriptor per tick. `then_exit` comes
        // from here — not from a spawn-time argument — so a mid-drain escalation
        // (relaunch → stay-down, Issue #4521) is honored by this supervisor.
        let (past_deadline, force, then_exit, origin) = {
            let snap = drain.snapshot();
            let past = snap.deadline.is_some_and(|d| Utc::now() >= d);
            (past, snap.force_after_timeout, snap.then_exit, snap.origin)
        };

        match evaluate_drain_tick(in_flight, past_deadline, force) {
            DrainTick::Continue => {
                tokio::time::sleep(poll_interval).await;
            }
            DrainTick::Complete => {
                let _ = event_bus.publish_generic(
                    "daemon.drain.completed",
                    serde_json::json!({ "in_flight": 0, "then_exit": then_exit }),
                );
                // #8652: close the paused interval BEFORE exiting — the exit is
                // what would otherwise lose it.
                drain.close_paused_interval(Utc::now());
                if then_exit {
                    log::warn!("{}", drain_complete_log_line(true, "", verify_poll_secs));
                    crate::observability::shutdown::exit(drain_exit_code(true)).await;
                }
                // This path only runs after `handle_drain_request` proved
                // supervision, so `detect_supervisor()` should still be `Some`
                // here; fall back to a generic label rather than hardcoding
                // launchd if the environment somehow changed underneath us.
                // Issue #6969: also spawns the detached post-exit verifier
                // BEFORE exiting — see `restart_verify::spawn_detached_verifier`'s
                // doc for why it cannot run in-band here. `pre_pid` is this
                // process's own pid: the supervisor's current record of the
                // daemon that is about to exit.
                let sup = crate::restart_verify::detect_and_spawn_verifier(std::process::id());
                log::warn!("{}", drain_complete_log_line(false, &sup, verify_poll_secs));
                crate::observability::shutdown::exit(drain_exit_code(false)).await;
            }
            DrainTick::TimedOutRefuse
                if drain_timeout_action(origin, then_exit) == TimeoutAction::HoldPaused =>
            {
                // Issue #9588. An OPERATOR drain — then-exit or relaunch — never
                // resumes dispatch on its own: hold it paused, name the
                // stragglers, clear the deadline, and keep supervising so the
                // terminal action still fires once in-flight reaches zero.
                let stragglers = list_in_flight_sweeps(&workspace_pool, &fallback_root);
                let note = drain_timeout_hold_note(then_exit, &stragglers);
                let _ = event_bus.publish_generic(
                    "daemon.drain.timeout",
                    serde_json::json!({
                        "in_flight": in_flight,
                        "forced": false,
                        "then_exit": then_exit,
                        "origin": origin.as_str(),
                        "paused": true,
                        "stragglers": stragglers,
                    }),
                );
                log::warn!("{note}");
                drain.hold_after_timeout(note);
                tokio::time::sleep(poll_interval).await;
            }
            DrainTick::TimedOutRefuse => {
                // AUTO-UPDATE roll drains only (#9588 — operator drains hold,
                // above). Issue #6007. A **teardown** (`then_exit`) drain keeps the
                // historical fail-safe byte-for-byte: refuse, resume dispatch,
                // stay up. `fleet drain` orchestrates those over SSH and keys its
                // documented exit-2 contract on the remote reporting
                // `draining: false`, so retaining a pending teardown here would
                // change a remote-decommission contract this issue is not about.
                if drain_refusal_path(then_exit) == RefusalPath::ResumeDispatch {
                    let note = drain_timeout_refuse_note(in_flight);
                    let _ = event_bus.publish_generic(
                        "daemon.drain.timeout",
                        serde_json::json!({
                            "in_flight": in_flight,
                            "forced": false,
                            "then_exit": true,
                            "roll_pending": false,
                            "origin": origin.as_str(),
                            "paused": false,
                        }),
                    );
                    log::warn!("{note}");
                    drain.resolve_timeout(note);
                    return;
                }
                // A **relaunch (roll)** drain retains its intent instead of
                // handing the admission window back to the work finder, which is
                // what made every retry strictly harder to satisfy than the last.
                match drain.refuse_roll_deadline(Utc::now()) {
                    RollRefusal::Deferred {
                        attempt,
                        window,
                        budget,
                        ..
                    } => {
                        let note = drain_roll_pending_note(in_flight, attempt, window, budget);
                        let _ = event_bus.publish_generic(
                            "daemon.drain.roll_pending",
                            serde_json::json!({
                                "in_flight": in_flight,
                                "attempt": attempt,
                                "window_secs": window.as_secs(),
                                "budget_secs": budget.as_secs(),
                            }),
                        );
                        log::warn!("{note}");
                        drain.set_note(note);
                        // Dispatch is still paused and this supervisor is still
                        // the current generation — keep polling so the restart
                        // fires the instant in-flight reaches zero.
                        tokio::time::sleep(poll_interval).await;
                    }
                    RollRefusal::Abandoned {
                        attempts, elapsed, ..
                    } => {
                        let note = drain_roll_abandoned_note(in_flight, attempts, elapsed);
                        let _ = event_bus.publish_generic(
                            "daemon.drain.timeout",
                            serde_json::json!({
                                "in_flight": in_flight,
                                "forced": false,
                                "then_exit": false,
                                "roll_pending": false,
                                "origin": origin.as_str(),
                                "paused": false,
                                "attempts": attempts,
                                "elapsed_secs": elapsed.as_secs(),
                            }),
                        );
                        log::warn!("{note}");
                        // `refuse_roll_deadline` already cleared the flag and
                        // bumped the generation; only the note is left to record.
                        drain.set_note(note);
                        return;
                    }
                }
            }
            DrainTick::TimedOutForce => {
                let cancelled = cancel_all_in_flight(&workspace_pool, &fallback_root);
                drain.close_paused_interval(Utc::now()); // #8652, before either exit.
                let _ = event_bus.publish_generic(
                    "daemon.drain.timeout",
                    serde_json::json!({
                        "in_flight": in_flight,
                        "forced": true,
                        "cancelled": cancelled,
                        "then_exit": then_exit,
                    }),
                );
                if then_exit {
                    log::warn!(
                        "drain timed out with {in_flight} in-flight; --force-after-timeout \
                         cancelled {cancelled} sweep(s); exiting {EXIT_SHUTDOWN} and staying down \
                         (then_exit — Issue #4343 teardown)"
                    );
                    crate::observability::shutdown::exit(drain_exit_code(true)).await;
                }
                // Same detection/fallback as the `DrainTick::Complete` relaunch
                // branch above; this path only reaches here after
                // `handle_drain_request` already proved supervision. Also
                // spawns the same detached post-exit verifier (Issue #6969).
                let sup = crate::restart_verify::detect_and_spawn_verifier(std::process::id());
                log::warn!(
                    "drain timed out with {in_flight} in-flight; --force-after-timeout cancelled \
                     {cancelled} sweep(s); exiting {EXIT_RESTART} for a supervised relaunch. {}",
                    crate::restart_verify::relaunch_verify_note(&sup, verify_poll_secs)
                );
                crate::observability::shutdown::exit(drain_exit_code(false)).await;
            }
        }
    }
}
