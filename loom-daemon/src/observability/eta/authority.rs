//! ETA authority gating for the tracker (#10498): only the fleet's one ETA
//! authority computes and emits ETA records. Every other host drops its
//! pending store, runs no pass, and emits nothing. See
//! [`crate::eta::authority`] for the resolution rule.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{
    append_journal, deliver, lock, pending_path, read_pending, sink, Delivered, Emission,
    JournalEntry, Provenance, QueueSink, Registry, Resolved, Tracker,
};

/// This process's last resolution. `true` until the first resolve says
/// otherwise, so an unresolved host never goes silent by default.
static AUTHORITY: AtomicBool = AtomicBool::new(true);

/// Whether this host is currently the ETA authority.
pub(super) fn active() -> bool {
    AUTHORITY.load(Ordering::Relaxed)
}

/// Forget every pending estimate and delete the persisted store. Idempotent.
/// Returns how many in-memory estimates were dropped.
pub(super) fn drop_pending(tracker: &mut Tracker, path: &Path) -> usize {
    let dropped = tracker.pending().len();
    // An empty store: the registry filters nothing, so the built-in one does.
    tracker.restore_pending(Vec::new(), &Registry::builtin());
    if let Err(error) = std::fs::remove_file(path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            log::warn!("eta: removing the pending store {} failed: {error}", path.display());
        }
    }
    dropped
}

/// Startup: the authority restores its pending store, any other host drops it
/// (the first run after an upgrade), so no stale outcome is ever scored there.
/// Returns how many restored estimates were dropped for an unregistered
/// heuristic (#10484); always 0 on a non-authority host.
pub(super) fn restore(tracker: &mut Tracker, workspace_root: &Path, registry: &Registry) -> usize {
    let resolution = crate::eta::authority::resolve(workspace_root);
    crate::eta::authority::log_change(&resolution);
    let is_authority = resolution.is_authority();
    AUTHORITY.store(is_authority, Ordering::Relaxed);
    let path = pending_path(workspace_root);
    if is_authority {
        tracker.restore_pending(read_pending(&path), registry)
    } else {
        drop_pending(tracker, &path);
        0
    }
}

/// Re-resolve the authority (every pass, so a config edit needs no restart)
/// and apply it: a host that is not the authority drops its pending store, at
/// most once. Returns whether this host is the authority.
pub(super) fn refresh(workspace_root: &Path) -> bool {
    let resolution = crate::eta::authority::resolve(workspace_root);
    crate::eta::authority::log_change(&resolution);
    let is_authority = resolution.is_authority();
    let was = AUTHORITY.swap(is_authority, Ordering::Relaxed);
    if !is_authority {
        if let Some(state) = lock().as_mut() {
            let path = pending_path(&state.workspace_root);
            let dropped = drop_pending(&mut state.tracker, &path);
            if was || dropped > 0 {
                log::info!(
                    "eta: this host is not the ETA authority; dropped {dropped} pending \
                     estimate(s) and emits no ETA records (#10498)"
                );
            }
        }
    }
    is_authority
}

/// A non-authority host's bus event: the stage journal still records what this
/// host saw, but nothing is estimated, scored or emitted. `true` when it
/// applied (the caller returns).
pub(super) fn journal_only(root: &Path, rows: &[JournalEntry]) -> bool {
    if active() {
        return false;
    }
    append_journal(root, rows);
    true
}

/// [`deliver`] behind the authority gate. The passes never reach it as a
/// non-authority host; if something does, the records are dropped and a
/// `loom.daemon.task_faults{reason=eta_non_authority_emit}` fault is counted,
/// the signal fleet alerts read.
pub(super) fn gate_delivery(
    authority: bool,
    emissions: Vec<Emission>,
    outcomes: Vec<Resolved>,
    loom: &Provenance,
    host_id: &str,
    dry_run: bool,
    sink: Option<&dyn QueueSink>,
) -> Delivered {
    if authority {
        return deliver(emissions, outcomes, loom, host_id, dry_run, sink);
    }
    if !emissions.is_empty() || !outcomes.is_empty() {
        log::warn!(
            "eta: a non-authority host reached the ETA sink with {} estimate(s) and {} \
             outcome(s); dropped (#10498)",
            emissions.len(),
            outcomes.len()
        );
        crate::observability::ops::liveness::fault(
            crate::task_liveness::ETA_PASS,
            crate::observability::ops::liveness::Fault::EtaNonAuthorityEmit,
        );
    }
    Delivered::default()
}

/// [`gate_delivery`] with this process's authority flag.
pub(super) fn deliver_checked(
    emissions: Vec<Emission>,
    outcomes: Vec<Resolved>,
    host_id: &str,
    dry_run: bool,
) -> Delivered {
    let loom = Provenance::current();
    gate_delivery(active(), emissions, outcomes, &loom, host_id, dry_run, sink())
}
