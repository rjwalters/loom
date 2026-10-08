//! The self-update loop's runtime wiring: one tick ([`run_tick`]) and the
//! spawned loop around it ([`spawn_auto_update_task`]).
//!
//! Moved out of `auto_update.rs` (over the file-size ratchet threshold) by
//! Issue #10414, which also hardened the loop. On 2026-10-05 both AWS workers
//! stopped rolling at ~01:30Z with the daemon still up, and nothing said why:
//!
//! - **A panicking tick no longer ends the loop.** Before #10414 a panic inside
//!   `run_tick` reached the loop as a `JoinError`. The loop logged one `error!`
//!   and returned, so the host stopped updating for the rest of the process's
//!   life, and status still showed the last good tick. Each tick now runs under
//!   `catch_unwind`. A panic is published as the tick's status note, emitted as
//!   an `auto_update.tick` record with `decision = panic` and counted as a
//!   `loom.daemon.task_faults{task=auto_update, reason=panic}` fault. The loop
//!   keeps going.
//! - **A tick that never returns is visible.** The tick's blocking thread cannot
//!   be cancelled. Instead the loop warns once per interval while it waits, and
//!   counts one `overrun` fault past [`TICK_OVERRUN`]. It never starts a second
//!   tick beside the first: the state is moved into the running one. The
//!   `task_alive` gauge drops to `0` once the tick's silence passes the
//!   liveness window.
//! - **Every tick is reported.** One `auto_update.tick` record per tick
//!   ([`super::tick_telemetry`]) carries the decision, the installed and target
//!   versions, the defer reason and the drain state.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;

use super::tick_telemetry::{self, TickSummary};
use super::*;
use crate::observability::ops::liveness::{self, Fault};
use crate::task_liveness::{self as task_liveness, AUTO_UPDATE};
use crate::telemetry::kinds::auto_update_tick::TickDecisionKind;

/// How long one tick may run before it counts as an overrun: the update
/// script's own timeout (a fetch or rebuild is killed at that point) plus five
/// minutes for resolution and the drain trigger.
pub const TICK_OVERRUN: Duration = DEFAULT_REBUILD_TIMEOUT.saturating_add(Duration::from_secs(300));

/// The liveness window for the loop: two intervals and the grace, plus the
/// longest a single tick may legitimately run.
#[must_use]
pub fn liveness_window(interval: Duration) -> Duration {
    task_liveness::default_stale_after(interval).saturating_add(TICK_OVERRUN)
}

/// Run one full tick: publish `last_check`, probe, decide, and — on a `Rebuild`
/// decision — rebuild and (on success) trigger the drain-and-restart. Pure of
/// spawning concerns so tests can drive it directly. Returns what the tick
/// decided, for the `auto_update.tick` record (#10414).
pub(super) fn run_tick<P: AutoUpdateProbe, T: DrainTrigger>(
    state: &mut AutoUpdateState,
    status: &AutoUpdateStatus,
    probe: &mut P,
    trigger: &T,
    settle: Duration,
    defer_deadline: Duration,
) -> TickSummary {
    let now = Instant::now();
    let last_check = Utc::now();
    let settle = state.window.begin_tick(last_check, trigger, settle);
    // Issue #8998: before either cooperating with an armed roll or arming a new
    // one, ask whether the condition it waits for (in-flight reaches zero) is
    // reachable at all. The in-flight count is read only when there is an
    // episode to advance — a roll is armed, or one was declared unsatisfiable
    // and we are waiting for the host to go quiet — so an ordinary up-to-date
    // tick pays nothing extra for this.
    let armed = trigger.armed_roll();
    let mut summary = TickSummary::new(armed.as_ref());
    if armed.is_some() || state.roll_stall_active() {
        let stall_in_flight = probe.in_flight_sweeps();
        summary.in_flight = Some(stall_in_flight);
        let stall = state.observe_roll_stall(last_check, armed.as_ref(), stall_in_flight);
        // Issue #9010: a standing declaration that has stood for its full cooldown
        // is dropped inside `observe` (returning `None`, so this tick falls through
        // and arms a roll normally). Surface that as its own WARN rather than
        // letting the suppression end silently — an operator watching a host that
        // stopped updating needs to see the retry being spent as well as the
        // declaration that preceded it.
        if let Some(retry) = state.take_roll_stall_retry_note() {
            log::warn!("auto_update: {retry}");
        }
        if let Some(report) = stall {
            let note = report.note();
            log::warn!("auto_update: {note}");
            // Only ever a roll this loop armed and labelled with an artifact
            // target. `observe` already refuses to advance — or to re-report — an
            // episode while a teardown or an untargeted operator drain is armed,
            // but this is the call that actually reaches `DrainState::abort()`,
            // so it re-states the ownership test rather than trusting an
            // invariant asserted one module away. Gating on `armed.is_some()`
            // instead is what let a latched declaration cancel an operator's
            // teardown within one tick (the defect PR #9004 shipped first).
            if armed
                .as_ref()
                .is_some_and(|roll| !roll.then_exit && roll.target.is_some())
            {
                trigger.abandon_roll(&note);
            }
            let armed_artifact = probe.resolve_artifact();
            status.publish(state.snapshot(true, last_check, note.clone(), &armed_artifact));
            return summary.finish(TickDecisionKind::RollStall, note, &armed_artifact, None);
        }
    }
    // Issue #6007: cooperate with the drain rather than racing it. A roll that is
    // already armed — including one *retained* across a refused deadline
    // (dispatch paused, restart re-arming itself at quiescence) — needs no second
    // rebuild; the binary is provisioned and the restart is coming.
    //
    // Issue #8514 narrows that skip by exactly one case: a **pending** roll
    // whose artifact has since been overtaken by a newer release is superseded
    // rather than waited out, so the host does not spend its paused-dispatch
    // budget converging on a binary that is already stale. Every other shape —
    // a first-attempt drain, a teardown, an untargeted operator drain, no newer
    // artifact — still skips, byte-for-byte as before.
    if trigger.roll_in_progress() {
        let armed_artifact = probe.resolve_artifact();
        match supersede::decide_armed_roll(armed.as_ref(), &armed_artifact) {
            supersede::ArmedRollAction::Skip(note) => {
                log::info!("auto_update: {note}");
                status.publish(state.snapshot(true, last_check, note.clone(), &armed_artifact));
                return summary.finish(TickDecisionKind::DrainWait, note, &armed_artifact, None);
            }
            supersede::ArmedRollAction::Supersede { from, to } => {
                let note = supersede::supersede_note(&from, &to);
                state.window.allow_retarget();
                log::warn!("auto_update: {note}");
                if !trigger.supersede_roll(&from, &to) {
                    // The roll completed or was abandoned between the two reads
                    // — nothing to supersede, and this tick has no armed roll to
                    // cooperate with any more. Fall through and decide normally.
                    log::info!(
                        "auto_update: the pending roll to {from} ended on its own before it could \
                         be superseded — continuing this tick normally"
                    );
                }
            }
        }
    }
    // Issue #7609: the artifact question is asked FIRST and answered
    // independently of the source checkout — it is the whole point that a
    // missing/dirty/stale checkout says nothing about whether a newer signed
    // binary exists.
    let artifact = probe.resolve_artifact();
    let check = probe.check();
    let tree_clean = probe.is_tree_clean().unwrap_or(false);
    // Only pay for the in-flight count once a roll is actually on the table (it
    // loads the workspace registry from disk); a cheap pre-filter avoids that
    // read on every up-to-date tick.
    let in_flight = if artifact.is_actionable() || check.update_available == Some(true) {
        let n = probe.in_flight_sweeps();
        summary.in_flight = Some(n);
        n
    } else {
        0
    };
    summary.check = Some(check.clone());

    // Issue #6261: the 2026-08-14 incident's diagnostic gap wasn't just the
    // gate-reset bugs above — it was that NOTHING surfaced the staleness
    // proactively. `daemon status`'s `Self-update:` line (client-side,
    // `crate::self_update::check()`) already renders this when queried live,
    // but a day-long incident needs a signal that reaches the daemon's own
    // log without anyone asking. Logged every tick the threshold is crossed
    // (bounded by `interval`, default 900s — not spammy).
    if let Some(warning) =
        crate::self_update::staleness_warning_default(check.commits_behind, check.hours_behind)
    {
        log::warn!("auto_update: {warning}");
    }

    let inputs = TickInputs {
        artifact: &artifact,
        check: &check,
        tree_clean,
        in_flight,
    };
    let decision = state.decide(now, &inputs, settle, defer_deadline);
    let (kind, outcome, note) = match state.window.gate(last_check, decision) {
        TickDecision::Skip(reason) => {
            // Issue #7608: name the offending paths behind a dirty-tree
            // refusal — the generic reason alone gave no way to tell an
            // actual tracked-input change from unrelated untracked litter
            // (both looked identical in the log before #7608), which is how
            // a stray `pnpm-lock.yaml` stalled unattended rebuilds for weeks.
            // `ends_with` rather than `==` because the source path's reasons
            // are now prefixed with the artifact-resolution failure (#7609).
            let reason = if !tree_clean && reason.ends_with(DIRTY_TREE_REASON) {
                with_dirty_paths(&reason, probe.tree_dirty_paths())
            } else {
                reason
            };
            // Issue #6261: every tick's decision is now logged, not just a
            // `Rebuild`'s — the 2026-08-14 incident's daemon log had ZERO
            // evidence of why the loop never rolled across a 20-merge day,
            // because a `Skip` only ever reached `daemon status`'s
            // latest-tick `note` field (overwritten every tick, useless
            // unless read live at exactly the right moment).
            log::info!("auto_update: {reason}");
            // #10414: a skip with a target being tracked is a roll some gate
            // held back (settle, backoff, terminal, in-flight, roll window).
            let kind = if state.tracked_target.is_some() {
                TickDecisionKind::Defer
            } else {
                TickDecisionKind::Skip
            };
            (kind, None, reason)
        }
        TickDecision::SkipWarn(reason) => {
            // Issue #8513: a stale-repo resolution is "do nothing" like a
            // `Skip`, but it is NOT healthy — WARN rather than INFO, so it
            // reaches the daemon's own log at a level an operator actually
            // notices, exactly like the staleness warning above.
            log::warn!("auto_update: {reason}");
            (TickDecisionKind::StaleRepo, None, reason)
        }
        TickDecision::Rebuild { low_priority } => {
            if low_priority {
                log::info!(
                    "auto_update: source is stale and settled but the host has stayed busy past \
                     the gate-4 deadline ({in_flight} in-flight) — rebuilding at reduced priority"
                );
            } else {
                log::info!(
                    "auto_update: source is stale and settled with 0 in-flight sweeps — rebuilding"
                );
            }
            let outcome = probe.rebuild(low_priority);
            // `None`: a source-path roll has no release-artifact identity, so it
            // is never a supersede candidate (#8514).
            let drain_accepted =
                matches!(outcome, RebuildOutcome::Success) && trigger.trigger_for(None);
            summary.roll_armed = drain_accepted;
            let mut note = state.record_rebuild(now, &outcome, drain_accepted);
            if low_priority {
                note = format!(
                    "{note} [forced past the in-flight gate after the defer deadline; built at \
                     reduced priority]"
                );
            }
            note = relaunch_verify_note::with_relaunch_verify_note(note, drain_accepted);
            log_roll_outcome(&outcome, &note);
            (TickDecisionKind::Rebuild, Some(outcome_str(&outcome)), note)
        }
        TickDecision::FetchArtifact {
            version,
            tag,
            why,
            low_priority,
        } => {
            // Issue #7609's per-decision log line: names the path (artifact),
            // the cause (`why`), and — because this is the line an operator
            // reads when a host is stuck — that no source build can happen
            // here even if the checkout is dirty or absent.
            log::info!(
                "auto_update: {why} — fetching release artifact {tag} ({version}) with the \
                 source-rebuild fallback disabled{}",
                if low_priority {
                    // Issue #8252: a busy host no longer postpones the fetch
                    // behind the rebuild-stampede gate — it only nices it.
                    " (host busy; fetching now at reduced priority rather than deferring — the \
                     build-stampede gate applies to rebuilds only)"
                } else {
                    ""
                }
            );
            let outcome = probe.fetch_artifact(&tag, low_priority);
            let info = match &artifact {
                ArtifactResolution::Resolved(info) => info.clone(),
                // Unreachable: a `FetchArtifact` decision is only ever
                // produced from a `Resolved` artifact.
                ArtifactResolution::Unresolved(_) => ArtifactInfo::default(),
            };
            // #8514: label the roll with the artifact identity it is rolling to,
            // so a later tick can tell a still-current pending roll from one a
            // newer release has overtaken.
            let drain_accepted = matches!(outcome, RebuildOutcome::Success)
                && trigger.trigger_for(Some(&supersede::artifact_roll_target(&info)));
            summary.roll_armed = drain_accepted;
            let mut note = state.record_artifact_roll(now, &outcome, drain_accepted, &info);
            if low_priority {
                note = format!(
                    "{note} [{in_flight} in-flight sweep(s): fetched immediately at reduced \
                     priority — the in-flight gate defers rebuilds only]"
                );
            }
            note = relaunch_verify_note::with_relaunch_verify_note(note, drain_accepted);
            log_roll_outcome(&outcome, &note);
            (TickDecisionKind::Fetch, Some(outcome_str(&outcome)), note)
        }
    };
    // #10712: a host below the fleet floor says so on whatever the tick
    // decided (the floor-driven cause, or the unsatisfiable-floor alert). The
    // alert is never a gate: nothing above was held back for it and dispatch
    // is untouched.
    let note = format!("{note}{}", state.floor.note_suffix());
    if let Some(stall) = state.floor.stall() {
        log::error!("auto_update: {}", stall.note());
        summary.floor_stall = Some(stall.note());
    }

    status.publish(state.snapshot(true, last_check, note.clone(), &artifact));
    summary.finish(kind, note, &artifact, outcome)
}

/// The `outcome` field's spelling of a roll outcome.
fn outcome_str(outcome: &RebuildOutcome) -> &'static str {
    match outcome {
        RebuildOutcome::Success => "success",
        RebuildOutcome::Retryable(_) => "retryable",
        RebuildOutcome::Terminal(_) => "terminal",
    }
}

/// Log a roll's outcome at the severity its kind warrants — a terminal failure
/// is an error, everything else a warning (a successful roll is a warning
/// because it means the daemon is about to restart).
fn log_roll_outcome(outcome: &RebuildOutcome, note: &str) {
    match outcome {
        RebuildOutcome::Success | RebuildOutcome::Retryable(_) => {
            log::warn!("auto_update: {note}");
        }
        RebuildOutcome::Terminal(_) => log::error!("auto_update: {note}"),
    }
}

/// The text of a caught panic payload.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_string())
}

/// One tick under `catch_unwind`, emitting its `auto_update.tick` record. A
/// panic is published as the status note and reported as `decision = panic`;
/// the caller's loop continues either way (#10414).
pub(super) fn guarded_tick<P: AutoUpdateProbe, T: DrainTrigger>(
    state: &mut AutoUpdateState,
    status: &AutoUpdateStatus,
    probe: &mut P,
    trigger: &T,
    tuning: &TickTuning,
) -> TickSummary {
    let started_at = Utc::now();
    let started = Instant::now();
    // #10712: the fleet floor in force (updated by fleet-sync without a
    // restart) against the version this process is running. Read here, not in
    // `run_tick`, so tick tests set the basis themselves.
    state
        .floor
        .set_basis(crate::fleet_sync::loom_min_version(), env!("CARGO_PKG_VERSION"));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_tick(state, status, probe, trigger, tuning.settle, tuning.defer_deadline)
    }));
    let summary = result.unwrap_or_else(|payload| {
        let note = format!(
            "tick panicked ({}); recorded, and the loop keeps running — the next tick retries",
            panic_message(payload.as_ref())
        );
        log::error!("auto_update: {note}");
        liveness::fault(AUTO_UPDATE, Fault::Panic);
        let unresolved = ArtifactResolution::Unresolved("tick panicked".to_string());
        status.publish(state.snapshot(true, started_at, note.clone(), &unresolved));
        TickSummary::new(None).finish(TickDecisionKind::Panic, note, &unresolved, None)
    });
    tick_telemetry::emit(&summary, state.consecutive_failures, started_at, started.elapsed());
    summary
}

/// Spawn the **single** process-global auto-update loop on the shared daemon
/// runtime (Issue #4055). Registers `status` as the process-global so
/// `loom-daemon status` can render it, then ticks every `tuning.interval`, moving the
/// per-tick blocking work (git/cargo subprocesses, registry reads) onto
/// `spawn_blocking` so it never parks a runtime worker.
///
/// Unlike the sibling autonomous loops this is **not** a `spawn_multi_*`
/// per-workspace fan-out: the daemon has one binary and one source checkout, so
/// exactly one loop runs regardless of how many workspaces are registered.
///
/// Issue #10414: each tick runs under `catch_unwind` ([`guarded_tick`]), the
/// loop beats [`crate::task_liveness`] after every tick, and a tick that does
/// not return is reported while the loop waits for it (see the module doc).
pub fn spawn_auto_update_task<P, T>(
    probe: P,
    trigger: T,
    status: Arc<AutoUpdateStatus>,
    tuning: TickTuning,
) -> tokio::task::JoinHandle<()>
where
    P: AutoUpdateProbe + Send + 'static,
    T: DrainTrigger + Send + Sync + 'static,
{
    register_global_status(status.clone());
    log::info!("auto_update: starting loop ({})", tuning.describe());
    task_liveness::register(AUTO_UPDATE, tuning.interval, liveness_window(tuning.interval));
    let mut state = AutoUpdateState::new()
        .with_roll_stall_deadlines(tuning.roll_stall_deadlines)
        .with_roll_stall_cooldown(tuning.roll_stall_cooldown);
    state.window = roll_window::WindowGate::new(tuning.roll_window);
    tokio::spawn(run_loop(state, probe, trigger, status, tuning))
}

/// The loop [`spawn_auto_update_task`] runs: tick every `tuning.interval`,
/// beat liveness after each tick, and stop (visibly) only if a tick's task is
/// lost outright.
pub(super) async fn run_loop<P, T>(
    mut state: AutoUpdateState,
    mut probe: P,
    mut trigger: T,
    status: Arc<AutoUpdateStatus>,
    tuning: TickTuning,
) where
    P: AutoUpdateProbe + Send + 'static,
    T: DrainTrigger + Send + Sync + 'static,
{
    let mut ticker = tokio::time::interval(tuning.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let status_task = status.clone();
        let handle = tokio::task::spawn_blocking(move || {
            guarded_tick(&mut state, &status_task, &mut probe, &trigger, &tuning);
            (state, probe, trigger)
        });
        match wait_for_tick(handle, tuning.interval, TICK_OVERRUN).await {
            Ok((s, p, t)) => {
                state = s;
                probe = p;
                trigger = t;
                task_liveness::beat(AUTO_UPDATE, tuning.interval);
            }
            Err(e) => {
                // Unreachable for a panic (`guarded_tick` catches it); a
                // cancelled blocking task or an abort-on-panic build is all
                // that is left. The state moved into the tick is gone, so the
                // loop cannot continue — but it no longer dies silently.
                let note = format!(
                    "tick task failed ({e}); the loop has STOPPED — this host will not \
                     self-update until the daemon restarts"
                );
                log::error!("auto_update: {note}");
                liveness::fault(AUTO_UPDATE, Fault::Exit);
                task_liveness::mark_dead(AUTO_UPDATE, &note);
                let mut snap = status.snapshot();
                snap.note = Some(note);
                status.publish(snap);
                return;
            }
        }
    }
}

/// Await a tick's blocking task, warning once per `warn_every` while it runs
/// and counting one `overrun` fault once it has run longer than `overrun`.
/// Never cancels it (a blocking thread cannot be) and never starts another.
pub(super) async fn wait_for_tick<R>(
    mut handle: tokio::task::JoinHandle<R>,
    warn_every: Duration,
    overrun: Duration,
) -> Result<R, tokio::task::JoinError> {
    let started = tokio::time::Instant::now();
    let mut overrun_reported = false;
    loop {
        match tokio::time::timeout(warn_every, &mut handle).await {
            Ok(joined) => return joined,
            Err(_) => {
                let elapsed = started.elapsed();
                log::warn!(
                    "auto_update: tick still running after {}s — no new tick starts until it \
                     returns; task_alive{{task=auto_update}} drops to 0 if it never does",
                    elapsed.as_secs()
                );
                if !overrun_reported && elapsed >= overrun {
                    overrun_reported = true;
                    liveness::fault(AUTO_UPDATE, Fault::Overrun);
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "runner_tests.rs"]
mod tests;
