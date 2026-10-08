//! Wiring H5 into daemon startup (issue #10832; design §7 H5 "Entry").
//!
//! Two calls, because two things must happen at different points of boot:
//!
//! 1. [`arm_at_startup`], **before** the first sweep registry is built: it
//!    arms [`crate::roll_pause::suppress`] for the manifest's items, so
//!    `reconstruct`, the startup claim reconciliation, the journal capacity
//!    seed and the reapers all leave the paused agents alone from their very
//!    first pass.
//! 2. [`Startup::spawn`], once the drain state, the workspace pool and the role
//!    runner's guard exist and **before** any dispatch producer is spawned: it
//!    places the dispatch hold synchronously, then runs H5 off the runtime.
//!
//! [`crate::roll_pause::suppress::host_verified`] is `false` between the two
//! and until H5 finishes: the signal a startup step that must not run on an
//! unverified binary waits on.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::Utc;

use super::host::{DaemonResumeHost, ResumeHost};
use super::{PauseResumeStatus, ResumePlan, ResumeTuning};
use crate::auto_update::pause_manifest::{self, ItemKind, ItemStatus, LoadOutcome, Phase};
use crate::auto_update::pause_roll::PauseRollTuning;
use crate::event_bus::EventBus;
use crate::ipc::DrainState;
use crate::role_runner::InProgressGuard;
use crate::roll_pause::suppress::{self, HeldItem};
use crate::workspace_pool::WorkspacePool;

static STATUS: Mutex<Option<PauseResumeStatus>> = Mutex::new(None);

/// Record the status `status --json` reports.
pub(super) fn publish(status: &PauseResumeStatus) {
    *STATUS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(status.clone());
}

/// The pause/resume state of the manifest this process found at startup:
/// what H5 is doing, or did. `None` when this process started without one.
#[must_use]
pub fn status() -> Option<PauseResumeStatus> {
    STATUS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// The items of `manifest` restart recovery must leave alone: everything not
/// already resumed.
fn held_items(manifest: &pause_manifest::PauseManifest) -> Vec<HeldItem> {
    manifest
        .items
        .iter()
        .filter(|i| i.status != ItemStatus::Resumed)
        .map(|i| HeldItem {
            id: i.id.clone(),
            repo: PathBuf::from(&i.repo),
            issue: (i.kind == ItemKind::Sweep).then_some(i.issue).flatten(),
        })
        .collect()
}

/// What [`arm_at_startup`] found: whether a manifest file exists (whatever
/// its state), i.e. whether [`Startup::spawn`] has work to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "call `.spawn(..)` once the drain state exists, or H5 never runs"]
pub struct Startup(bool);

/// Arm the recovery suppression for a live pause manifest. Call before any
/// sweep registry is reconstructed.
pub fn arm_at_startup() -> Startup {
    Startup(manifest_found())
}

fn manifest_found() -> bool {
    let Some(path) = pause_manifest::manifest_path() else {
        return false;
    };
    match pause_manifest::load(&path, Utc::now()) {
        LoadOutcome::Missing => false,
        LoadOutcome::Loaded(m) | LoadOutcome::Stale(m) => {
            if !matches!(m.phase, Phase::Resumed | Phase::Abandoned) {
                let items = held_items(&m);
                log::warn!(
                    "pause_resume: found pause manifest {} (phase {}, {} item(s)) from a roll to \
                     {}; restart recovery is held off its {} unfinished item(s) until H5 has \
                     resumed or requeued them (#10832)",
                    m.manifest_id,
                    m.phase.as_str(),
                    m.items.len(),
                    m.roll.to_version,
                    items.len()
                );
                suppress::arm(&m.manifest_id, items);
            }
            true
        }
        // Reported once by H5; nothing can be held from a manifest that
        // cannot be read.
        LoadOutcome::Corrupt(_) | LoadOutcome::UnknownVersion(_) => true,
    }
}

impl Startup {
    /// Place the dispatch hold for a live manifest and run H5. A no-op when
    /// [`arm_at_startup`] found no manifest. Must be called inside a tokio
    /// runtime context, before the dispatch producers are spawned.
    pub fn spawn(
        self,
        drain: &Arc<DrainState>,
        pool: &Arc<WorkspacePool>,
        fallback_root: &Path,
        bus: &Arc<EventBus>,
        in_progress: &InProgressGuard,
    ) {
        if let (true, Some(manifest_path)) = (self.0, pause_manifest::manifest_path()) {
            spawn_h5(
                manifest_path,
                drain,
                pool,
                fallback_root.to_path_buf(),
                bus,
                in_progress.clone(),
            );
        }
    }
}

fn spawn_h5(
    manifest_path: PathBuf,
    drain: &Arc<DrainState>,
    pool: &Arc<WorkspacePool>,
    fallback_root: PathBuf,
    bus: &Arc<EventBus>,
    in_progress: InProgressGuard,
) {
    let (tuning, rejected) = PauseRollTuning::resolve(&fallback_root);
    if let Some(why) = rejected {
        log::error!("pause_resume: {why}");
    }
    let host: Arc<dyn ResumeHost> = Arc::new(DaemonResumeHost::new(
        pool.clone(),
        fallback_root,
        bus.clone(),
        drain.clone(),
        in_progress,
    ));
    // Synchronously, so no producer spawned after this call ever ticks with
    // dispatch open. H5 itself re-checks and waits when another hold is in
    // force.
    if suppress::is_armed() {
        host.hold_dispatch();
    }
    let plan = ResumePlan {
        manifest_path,
        running_version: env!("CARGO_PKG_VERSION").to_string(),
        tuning: ResumeTuning::from_pause(&tuning),
    };
    tokio::task::spawn_blocking(move || {
        let outcome = super::run_h5(host, &plan);
        log::info!("pause_resume: H5 ended: {outcome:?}");
    });
}

#[cfg(test)]
mod tests {
    /// The startup order H5 depends on (design §7 H5 "Entry", #10979), pinned
    /// on the daemon's own startup source: recovery is held off before the
    /// first registry is reconstructed; the drain state (with any fleet
    /// `paused` hold the startup fleet-sync pass found) exists before H5 is
    /// spawned; and H5 holds dispatch before any dispatch producer starts.
    #[test]
    fn startup_arms_before_recovery_and_holds_before_any_producer() {
        let src = include_str!("../../daemon_service.rs");
        let at = |needle: &str| {
            src.find(needle)
                .unwrap_or_else(|| panic!("daemon_service.rs no longer contains `{needle}`"))
        };
        let arm = at("auto_update::pause_resume::arm_at_startup()");
        let spawn = at("h5.spawn(");
        assert!(arm < at("sweep.reconstruct()"), "armed before the first reconstruct");
        assert!(arm < at("spawn_startup_passes("), "and before startup claim reconciliation");
        assert!(arm < at("seed_capacity_from_journal("), "and before the journal capacity seed");
        assert!(at("fleet_state::wire(") < spawn, "the fleet hold is applied before H5 starts");
        for producer in [
            "epic_supervisor::spawn_multi_supervisor_thread(",
            "spawn_multi_work_finder_task(",
            "spawn_multi_role_task(",
            "spawn_auto_update_task(",
        ] {
            assert!(spawn < at(producer), "H5 holds dispatch before `{producer}`");
        }
    }
}
