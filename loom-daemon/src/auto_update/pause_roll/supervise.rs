//! H3 → H4 entry and the pause supervisor (issue #10831): start a pause roll,
//! run H4 off the async runtime, enforce its deadline, and act on how it ended.
//!
//! Split from `pause_roll.rs` so the orchestrator (`run_h4_with`) and the
//! code that watches it are separate. The watching matters (Judge findings on
//! #10974): H4 runs on a blocking thread that can fail or hang, and before
//! this module handled both, a pause that failed uncommitted left its pause
//! requests and reaper holds behind, and one that hung after the commit left
//! dispatch paused, `--abort-drain` refused and no restart. See
//! [`end_unfinished`].

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use super::host::{self, PauseHost};
use super::ledger::RunLedger;
use super::{refusal, run_h4_with, H4Outcome, PausePlan, PauseRollTuning, RollTarget};
use crate::auto_update::pause_manifest::{self, LoadOutcome, Phase};
use crate::event_bus::EventBus;
use crate::ipc::{DrainBegin, DrainState, PauseOwnership, PauseRollStatus};
use crate::workspace_pool::WorkspacePool;

/// How long the supervisor waits for [`end_unfinished`] itself. It only
/// touches local files and sends signals; this bounds a dead disk.
const END_UNFINISHED_TIMEOUT: Duration = Duration::from_secs(30);

/// H3 Staged → H4: start a pause roll to `target` (design §7 "H3 Staged").
///
/// Returns `true` when a restart is now coming: this call started the pause,
/// or a drain was already in progress (an operator drain wins; the staged
/// binary is picked up when that drain's restart fires, and a then-exit
/// teardown stops the daemon instead, which is logged).
///
/// Returns `false`, having paused nothing, when the roll cannot proceed. There
/// is **no drain fallback**: a roll that cannot pause never waits for the
/// in-flight count to reach zero. It is alerted (H7) and retried on a later
/// tick.
///
/// Must be called inside a tokio runtime context: it spawns the supervisor.
pub fn start_pause_roll(
    drain: &Arc<DrainState>,
    workspace_pool: &Arc<WorkspacePool>,
    fallback_root: &Path,
    event_bus: &Arc<EventBus>,
    target: &RollTarget,
) -> bool {
    let staged_at = Utc::now();
    // `key` is what the alert is rate-limited on (`refusal`): the first
    // refusal for a key is an ERROR and an event, a repeat within five
    // minutes is a debug line.
    let refuse_as = |state: &str, key: &str, why: String| {
        if !refusal::should_alert(key) {
            log::debug!("pause_roll: roll still not started (H7 {state}): {why}");
            return false;
        }
        log::error!("pause_roll: roll NOT started (H7 {state}): {why}");
        let _ = event_bus.publish_generic(
            "daemon.roll.refused",
            serde_json::json!({
                "state": state,
                "reason": why,
                "target_source": target.source.as_str(),
                "to_version": target.to_version,
            }),
        );
        false
    };
    let refuse = |state: &str, why: String| refuse_as(state, state, why);
    let Some(supervisor) = crate::ipc::detect_supervisor() else {
        return refuse(
            "unsupervised",
            "no supervisor detected (LOOM_DAEMON_SUPERVISOR unset), so nothing would relaunch the \
             daemon after the pause. Dispatch was NOT paused. Restart manually to run the staged \
             binary."
                .to_string(),
        );
    };
    let Some(manifest_path) = pause_manifest::manifest_path() else {
        return refuse(
            "pause-failed",
            "no state directory resolves for the pause manifest (no home directory and \
             LOOM_AUTO_UPDATE_STATE_DIR unset); work is never stopped without being recorded"
                .to_string(),
        );
    };
    // A manifest from an earlier pause that is still within its lease window
    // belongs to a resume that has not finished (or, before #10832, to
    // restart recovery that is still running). Do not pause again on top of it.
    match pause_manifest::load(&manifest_path, staged_at) {
        LoadOutcome::Loaded(m) if !matches!(m.phase, Phase::Resumed | Phase::Abandoned) => {
            // Refused on every tick until the manifest is resumed or ages
            // out: alerted once per manifest, then at most every five minutes.
            return refuse_as(
                "resume-pending",
                &format!("resume-pending:{}", m.manifest_id),
                format!(
                    "pause manifest {} ({}, phase {}) from the previous roll is still live; the \
                     next roll waits until it is resumed or older than its {}s max age (this \
                     alert repeats at most every 5 minutes while that manifest stands)",
                    m.manifest_id,
                    manifest_path.display(),
                    m.phase.as_str(),
                    m.roll.max_age_secs
                ),
            );
        }
        _ => {}
    }
    // #10832: the last roll to this same target did not take (the host came
    // back on the old binary). Do not pause every agent for it again at once.
    // Refused on every tick until the retry time, so rate-limited per target.
    let held_back = manifest_path.parent().and_then(|dir| {
        crate::auto_update::pause_resume::attempt::gate(
            dir,
            target.to_version.as_deref(),
            target.to_artifact_sha256.as_deref(),
            staged_at,
        )
    });
    if let Some(why) = held_back {
        let key =
            format!("roll-attempt-backoff:{}", target.to_version.as_deref().unwrap_or_default());
        return refuse_as("roll-attempt-backoff", &key, why);
    }
    let (tuning, rejected) = PauseRollTuning::resolve(fallback_root);
    if let Some(why) = rejected {
        log::error!("pause_roll: {why}");
        let _ = event_bus
            .publish_generic("daemon.roll.config_rejected", serde_json::json!({ "reason": why }));
    }
    let progress = PauseRollStatus {
        budget_secs: tuning.pause_budget.as_secs(),
        min_resumable_age_secs: tuning.min_resumable_age_secs,
        target_source: Some(target.source.as_str().to_string()),
        to_version: target.to_version.clone(),
        ..PauseRollStatus::default()
    };
    match drain.begin_pause_roll(tuning.pause_budget, progress) {
        DrainBegin::Started { generation, .. } => {
            drain.set_roll_target(target.label.clone());
            let plan = PausePlan {
                target: target.clone(),
                tuning,
                manifest_path,
                from_version: env!("CARGO_PKG_VERSION").to_string(),
                generation,
                staged_at,
                supervisor: Some(supervisor),
            };
            let _ = event_bus.publish_generic(
                "daemon.roll.pause_started",
                serde_json::json!({
                    "target_source": target.source.as_str(),
                    "to_version": target.to_version,
                    "from_version": plan.from_version,
                    "pause_budget_secs": tuning.pause_budget.as_secs(),
                    "verify_probation_secs": tuning.verify_probation.as_secs(),
                    "resume_budget_secs": tuning.resume_budget.as_secs(),
                    "min_resumable_age_secs": tuning.min_resumable_age_secs,
                }),
            );
            if drain.pause_ownership(generation) == PauseOwnership::Promoted {
                // #10979: dispatch was already held (a fleet-state `paused`
                // hold). The roll replaced the hold with a supervised drain
                // that keeps it held; nothing is paused by H4, and the daemon
                // restarts as soon as nothing is in flight.
                log::warn!(
                    "pause_roll: dispatch was already held (fleet state `paused`), so the {} roll \
                     to {} runs as a supervised drain: the daemon restarts as soon as nothing is \
                     in flight, and comes back held",
                    target.source.as_str(),
                    target.to_version.as_deref().unwrap_or("the rebuilt binary")
                );
            } else {
                log::warn!(
                    "pause_roll: H4 pausing for a {} roll to {} (budget {}s): every in-flight \
                     agent is stopped at a safe point or requeued, then the daemon restarts",
                    target.source.as_str(),
                    target.to_version.as_deref().unwrap_or("the rebuilt binary"),
                    tuning.pause_budget.as_secs()
                );
            }
            let host: Arc<dyn PauseHost> = Arc::new(host::DaemonPauseHost::new(
                workspace_pool.clone(),
                fallback_root.to_path_buf(),
                event_bus.clone(),
            ));
            let ledger = Arc::new(RunLedger::new(
                super::new_manifest_id(staged_at),
                plan.manifest_path.clone(),
            ));
            tokio::spawn(supervise(
                drain.clone(),
                workspace_pool.clone(),
                fallback_root.to_path_buf(),
                event_bus.clone(),
                host,
                plan,
                ledger,
            ));
            true
        }
        DrainBegin::AlreadyDraining {
            active_then_exit, ..
        } => {
            if active_then_exit {
                log::warn!(
                    "pause_roll: an operator then-exit (teardown) drain is in progress and wins: \
                     the daemon will STOP when drained and will NOT relaunch into the staged \
                     binary. Start it again to pick up the update."
                );
            } else {
                log::warn!(
                    "pause_roll: a drain is already in progress; it wins, and its restart picks \
                     up the staged binary. No pause was started."
                );
            }
            true
        }
    }
}

/// End an H4 run that will not end by itself: its deadline passed (the
/// thread is stuck) or its task failed (the thread is gone). `why` says which.
///
/// The drain decides, under its lock, which side of the commit point the run
/// is on, and the two sides never both apply:
///
/// - **Not committed** (no agent stopped): the pause is ended
///   ([`DrainState::pause_abort_uncommitted`]) and dispatch resumes. Everything
///   the run created is removed ([`RunLedger::undo`]): its pause requests (so
///   parked calls are released instead of denied), its reaper holds (so those
///   sweeps reap normally again) and its manifest. Alerted as H7
///   `pause-failed`.
/// - **Committed**: agents are already stopped, so the restart is the only way
///   they are picked up again. Every item not recorded as stopped has its
///   process group killed, the manifest is written with what is known
///   ([`RunLedger::force_finish`]) and the outcome is `Paused`: the caller
///   exits for the relaunch. Dispatch is never left paused with an abort
///   refused.
/// - **Promoted or gone**: someone else owns the drain. This run's artefacts
///   are removed and it stands down, as it would have by itself.
pub(crate) fn end_unfinished(
    drain: &DrainState,
    host: &dyn PauseHost,
    plan: &PausePlan,
    ledger: &RunLedger,
    why: &str,
) -> H4Outcome {
    let generation = plan.generation;
    let note = format!(
        "PAUSE FAILED (H7 pause-failed): {why} before any agent was stopped. Dispatch resumed; \
         this host stays on {} until the next roll attempt.",
        plan.from_version
    );
    if drain.pause_abort_uncommitted(generation, note.clone()) {
        ledger.undo(host);
        host.close_dispatch(false, ledger.id());
        log::error!("pause_roll: {note}");
        host.emit(
            "daemon.roll.pause_failed",
            serde_json::json!({ "manifest_id": ledger.id(), "error": why }),
        );
        return H4Outcome::Failed(note);
    }
    match drain.pause_ownership(generation) {
        PauseOwnership::Ours => {
            let (manifest_id, forced) = ledger.force_finish(why, &plan.from_version);
            log::error!(
                "pause_roll: {why} AFTER agents were stopped. Finishing the pause by force: \
                 {forced} remaining item(s) had their process group killed and are recorded \
                 `requeue` / `planned` ({}); manifest {manifest_id} is written and the daemon \
                 restarts. The next start finishes the requeues.",
                super::REASON_BUDGET_MISSED
            );
            host.emit(
                "daemon.roll.forced",
                serde_json::json!({
                    "manifest_id": manifest_id,
                    "reason": why,
                    "forced_items": forced,
                    "h4_deadline_secs": plan.tuning.h4_deadline().as_secs(),
                }),
            );
            H4Outcome::Paused {
                manifest_id,
                then_exit: drain.then_exit(),
            }
        }
        owner => {
            ledger.undo(host);
            host.close_dispatch(false, ledger.id());
            H4Outcome::StoodDown {
                promoted: owner == PauseOwnership::Promoted,
            }
        }
    }
}

/// The pause supervisor: run H4 off the runtime under its deadline, then act
/// on how it ended.
async fn supervise(
    drain: Arc<DrainState>,
    workspace_pool: Arc<WorkspacePool>,
    fallback_root: PathBuf,
    event_bus: Arc<EventBus>,
    host: Arc<dyn PauseHost>,
    plan: PausePlan,
    ledger: Arc<RunLedger>,
) {
    let generation = plan.generation;
    let deadline = plan.tuning.h4_deadline();
    let (h4_drain, h4_host, h4_plan, h4_ledger) =
        (drain.clone(), host.clone(), plan.clone(), ledger.clone());
    let task =
        tokio::task::spawn_blocking(move || run_h4_with(&h4_drain, h4_host, &h4_plan, &h4_ledger));
    let unfinished = match tokio::time::timeout(deadline, task).await {
        Ok(Ok(outcome)) => Err(outcome),
        Ok(Err(e)) => Ok(format!("the pause task failed ({e})")),
        Err(_) => Ok(format!(
            "the pause did not finish within its {}s H4 deadline",
            deadline.as_secs()
        )),
    };
    let outcome = match unfinished {
        Err(outcome) => outcome,
        Ok(why) => {
            let (d, h, p, l) = (drain.clone(), host.clone(), plan.clone(), ledger.clone());
            let ending =
                tokio::task::spawn_blocking(move || end_unfinished(&d, h.as_ref(), &p, &l, &why));
            match tokio::time::timeout(END_UNFINISHED_TIMEOUT, ending).await {
                Ok(Ok(outcome)) => outcome,
                // Even the cleanup is stuck (or failed). `end_unfinished`
                // settles the drain before it touches a file, so the drain
                // already says which side this is: a committed pause still
                // restarts (its `pausing` manifest is what the next start
                // finishes), anything else has had dispatch resumed.
                _ => {
                    let committed = drain.pause_ownership(generation) == PauseOwnership::Ours
                        && drain.snapshot().pause.is_some_and(|p| p.stopped);
                    log::error!(
                        "pause_roll: the pause could not be cleaned up within {}s ({})",
                        END_UNFINISHED_TIMEOUT.as_secs(),
                        if committed {
                            "agents were stopped: restarting anyway"
                        } else {
                            "nothing was stopped"
                        }
                    );
                    if !committed {
                        return;
                    }
                    H4Outcome::Paused {
                        manifest_id: ledger.id().to_string(),
                        then_exit: drain.then_exit(),
                    }
                }
            }
        }
    };
    match outcome {
        H4Outcome::Paused {
            manifest_id,
            then_exit,
        } => {
            // #8652: close the paused interval BEFORE exiting — the exit is
            // what would otherwise lose it.
            drain.close_paused_interval(Utc::now());
            if then_exit {
                log::warn!(
                    "pause_roll: pause complete (manifest {manifest_id}); an operator then-exit \
                     won, so exiting {} and staying down. The next start resumes or requeues the \
                     recorded agents.",
                    crate::ipc::drain_exit_code(true)
                );
                crate::observability::shutdown::exit(crate::ipc::drain_exit_code(true)).await;
            }
            let sup = crate::restart_verify::detect_and_spawn_verifier(std::process::id());
            log::warn!(
                "pause_roll: pause complete (manifest {manifest_id}); exiting {} for a \
                 {sup}-supervised relaunch onto the staged binary",
                crate::ipc::drain_exit_code(false)
            );
            crate::observability::shutdown::exit(crate::ipc::drain_exit_code(false)).await;
        }
        H4Outcome::StoodDown { promoted: true } => {
            // Rule 1 of the operator interplay: the drain is now an operator
            // drain with no supervisor of its own. Supervise it.
            crate::ipc::drain_supervisor::run_drain_supervisor(
                drain,
                workspace_pool,
                fallback_root,
                event_bus,
                generation,
                crate::ipc::DRAIN_POLL_INTERVAL,
            )
            .await;
        }
        H4Outcome::StoodDown { promoted: false } | H4Outcome::Failed(_) => {}
    }
}
