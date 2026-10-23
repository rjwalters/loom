//! Host maintenance: `loom-daemon restart --pause --then-exit` (#11812).
//!
//! An instance resize, a rebuild or OS work uses the same pause as a version
//! roll: H4 stops every daemon-dispatched agent at a safe point (or requeues
//! it), writes the pause manifest and refreshes the paused items' leases. The
//! only difference is the end: the daemon exits **and stays down** (the
//! `then-exit` exit code and the #9588 operator-stop record), the host is
//! changed, and on the next start H5 resumes the manifest exactly as it
//! resumes a roll's. A bug in recovering a paused agent is fixed once, in the
//! shared path.
//!
//! What it takes over is decided by [`DrainState::begin_host_maintenance`]:
//! an operator drain (in progress, or timed out and held), a startup or fleet
//! hold, or nothing at all, all without reopening dispatch. A pause roll
//! already in H4 is escalated to stay down instead. The H4 run itself is the
//! roll's ([`super::supervise::supervise`]), so the safe points, the manifest,
//! the lease refresh and the `daemon.roll.*` events are the same, tagged
//! `trigger = host-maintenance`.

use std::path::Path;
use std::sync::Arc;

use chrono::Utc;

use super::host::{self, PauseHost};
use super::ledger::RunLedger;
use super::{PausePlan, PauseRollTuning, RollTarget};
use crate::auto_update::pause_manifest::{self, LoadOutcome, Phase};
use crate::event_bus::EventBus;
use crate::ipc::{DrainState, MaintenanceBegin, PauseRollStatus};
use crate::types::Response;
use crate::workspace_pool::WorkspacePool;

/// What [`start`] did, for the IPC reply.
#[derive(Debug, Clone, Default)]
struct Ack {
    accepted: bool,
    manifest_id: Option<String>,
    manifest_path: Option<String>,
    took_over: Option<String>,
    escalated: bool,
    message: String,
}

/// Handle `Request::PauseThenExit`: start the pause and build the reply. The
/// reply goes out before H4 finishes; the CLI then waits for the manifest.
#[must_use]
pub fn handle_request(
    drain: &Arc<DrainState>,
    workspace_pool: &Arc<WorkspacePool>,
    fallback_root: &Path,
    event_bus: &Arc<EventBus>,
) -> Response {
    let in_flight = crate::ipc::count_in_flight_sweeps(workspace_pool, fallback_root);
    let (tuning, _) = PauseRollTuning::resolve(fallback_root);
    let ack = start(drain, workspace_pool, fallback_root, event_bus, tuning);
    Response::HostMaintenancePause {
        accepted: ack.accepted,
        manifest_id: ack.manifest_id,
        manifest_path: ack.manifest_path,
        took_over: ack.took_over,
        escalated: ack.escalated,
        in_flight,
        pause_budget_secs: tuning.pause_budget.as_secs(),
        h4_deadline_secs: tuning.h4_deadline().as_secs(),
        max_age_secs: tuning.lease_ttl.as_secs(),
        resume_window_secs: (tuning.verify_probation + tuning.resume_budget).as_secs(),
        message: ack.message,
    }
}

fn start(
    drain: &Arc<DrainState>,
    workspace_pool: &Arc<WorkspacePool>,
    fallback_root: &Path,
    event_bus: &Arc<EventBus>,
    tuning: PauseRollTuning,
) -> Ack {
    let staged_at = Utc::now();
    let target = RollTarget::host_maintenance();
    let refuse = |state: &str, why: String| {
        log::error!("pause_roll: host-maintenance pause NOT started ({state}): {why}");
        let _ = event_bus.publish_generic(
            "daemon.roll.refused",
            serde_json::json!({
                "state": state,
                "reason": why,
                "target_source": target.source.as_str(),
                "trigger": target.source.as_str(),
            }),
        );
        Ack {
            message: why,
            ..Ack::default()
        }
    };
    let Some(manifest_path) = pause_manifest::manifest_path() else {
        return refuse(
            "pause-failed",
            "no state directory resolves for the pause manifest (no home directory and \
             LOOM_AUTO_UPDATE_STATE_DIR unset); work is never stopped without being recorded. \
             Nothing was changed."
                .to_string(),
        );
    };
    if let LoadOutcome::Loaded(m) = pause_manifest::load(&manifest_path, staged_at) {
        if !matches!(m.phase, Phase::Resumed | Phase::Abandoned) {
            return refuse(
                "resume-pending",
                format!(
                    "pause manifest {} ({}, phase {}) from an earlier pause is still live; it is \
                     resumed or requeued first. Nothing was changed; retry once `loom-daemon \
                     status` shows it finished.",
                    m.manifest_id,
                    manifest_path.display(),
                    m.phase.as_str()
                ),
            );
        }
    }
    let progress = PauseRollStatus {
        budget_secs: tuning.pause_budget.as_secs(),
        min_resumable_age_secs: tuning.min_resumable_age_secs,
        target_source: Some(target.source.as_str().to_string()),
        ..PauseRollStatus::default()
    };
    let path_str = Some(manifest_path.display().to_string());
    match drain.begin_host_maintenance(tuning.pause_budget, progress) {
        MaintenanceBegin::Refused(why) => refuse("resume-in-progress", why),
        MaintenanceBegin::Escalated { manifest_id, .. } => {
            log::warn!(
                "pause_roll: host maintenance: the pause roll in progress now stays down when \
                 its H4 completes (manifest {})",
                manifest_id.as_deref().unwrap_or("not yet written")
            );
            Ack {
                accepted: true,
                manifest_id,
                manifest_path: path_str,
                escalated: true,
                message: "a pause roll was already running H4; it was escalated to then-exit, so \
                          it completes its pause and the daemon stays down"
                    .to_string(),
                ..Ack::default()
            }
        }
        MaintenanceBegin::Started {
            generation,
            took_over,
        } => {
            let plan = PausePlan {
                target: target.clone(),
                tuning,
                manifest_path: manifest_path.clone(),
                from_version: env!("CARGO_PKG_VERSION").to_string(),
                generation,
                staged_at,
                supervisor: crate::ipc::detect_supervisor(),
            };
            let took = took_over.map(|t| t.as_str().to_string());
            let _ = event_bus.publish_generic(
                "daemon.roll.pause_started",
                serde_json::json!({
                    "target_source": target.source.as_str(),
                    "trigger": target.source.as_str(),
                    "took_over": took,
                    "from_version": plan.from_version,
                    "then_exit": true,
                    "pause_budget_secs": tuning.pause_budget.as_secs(),
                    "verify_probation_secs": tuning.verify_probation.as_secs(),
                    "resume_budget_secs": tuning.resume_budget.as_secs(),
                    "min_resumable_age_secs": tuning.min_resumable_age_secs,
                    "max_age_secs": tuning.lease_ttl.as_secs(),
                }),
            );
            log::warn!(
                "pause_roll: H4 pausing for host maintenance (budget {}s{}): every in-flight \
                 agent is stopped at a safe point or requeued, then the daemon exits and stays \
                 down",
                tuning.pause_budget.as_secs(),
                took.as_deref()
                    .map(|t| format!(", took over {t}"))
                    .unwrap_or_default()
            );
            let pause_host: Arc<dyn PauseHost> = Arc::new(host::DaemonPauseHost::new(
                workspace_pool.clone(),
                fallback_root.to_path_buf(),
                event_bus.clone(),
            ));
            let ledger = Arc::new(RunLedger::new(super::new_manifest_id(staged_at), manifest_path));
            let manifest_id = ledger.id().to_string();
            tokio::spawn(super::supervise::supervise(
                drain.clone(),
                workspace_pool.clone(),
                fallback_root.to_path_buf(),
                event_bus.clone(),
                pause_host,
                plan,
                ledger,
            ));
            Ack {
                accepted: true,
                manifest_id: Some(manifest_id),
                manifest_path: path_str,
                took_over: took,
                message: "dispatch is closed; H4 pauses every agent, writes the manifest, \
                          refreshes the paused items' leases, then the daemon exits and stays \
                          down"
                    .to_string(),
                ..Ack::default()
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "maintenance_tests.rs"]
mod tests;
