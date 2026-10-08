//! ETA authority gating for the tracker (#10498): only the fleet's one ETA
//! authority computes and emits ETA records. Every other host drops its
//! pending store, runs no pass, and emits nothing. See
//! [`crate::eta::authority`] for the resolution rule.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::eta::coverage::{self, Declared, State};
use crate::eta::repo_priority::FleetMember;

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

/// #10897: the roster repos a **non-authority** host emits for because the
/// authority is not known to cover them. `None` on the authority itself and
/// on a silent non-authority host.
static FALLBACK: Mutex<Option<BTreeSet<String>>> = Mutex::new(None);

/// Whether `slug` is in this pass's scope: always on the authority, only the
/// fallback scope on a non-authority host that keeps emitting.
pub(super) fn in_scope(slug: &str) -> bool {
    FALLBACK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .is_none_or(|scope| scope.contains(&slug.to_ascii_lowercase()))
}

/// The fleet roster's current members from the cached fleet-store history:
/// a file read, never a network call. `None` when unknown (no `fleet.repo`,
/// cold cache).
fn cached_roster(root: &Path) -> Option<Vec<FleetMember>> {
    let now = chrono::Utc::now();
    let history = crate::eta::roster_history::load_for(root, now).0?;
    crate::eta::repo_priority::revision_at(&history, now).map(|r| r.members.clone())
}

/// The fallback scope for this host (pure): `None` for the authority itself,
/// else [`coverage::fallback_scope`].
pub(super) fn scope_for(
    is_authority: bool,
    roster: Option<&[FleetMember]>,
    declared: &Declared,
) -> Option<BTreeSet<String>> {
    if is_authority {
        None
    } else {
        coverage::fallback_scope(roster, declared)
    }
}

/// Resolve whether this host runs the ETA pass, and for which repos: the
/// authority for every repo it manages, any other host only for the roster
/// repos the authority is not declared to cover (#10897). Stores the scope.
fn decide(root: &Path, is_authority: bool) -> bool {
    let scope = if is_authority {
        None
    } else {
        scope_for(
            false,
            cached_roster(root).as_deref(),
            &crate::config_resolver::fleet_eta_authority_covers(root),
        )
    };
    let fallback = scope.is_some();
    if fallback {
        log_fallback(scope.as_ref());
    }
    *FALLBACK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = scope;
    is_authority || fallback
}

static LAST_FALLBACK: Mutex<Option<String>> = Mutex::new(None);

fn log_fallback(scope: Option<&BTreeSet<String>>) {
    let line = scope.map_or_else(String::new, |s| {
        format!("{} roster repo(s) the authority is not declared to cover", s.len())
    });
    let mut last = LAST_FALLBACK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if last.as_deref() != Some(line.as_str()) {
        log::warn!(
            "eta: this host is not the ETA authority but keeps emitting for {line}; declare \
             fleet.etaAuthorityCovers=\"all\" once the authority covers the roster (#10897)"
        );
        *last = Some(line);
    }
}

static LAST_COVERAGE: Mutex<Option<String>> = Mutex::new(None);

/// The authority's coverage check (#10897), run on every authority pass with
/// the slugs the pass covered: one `error` per change and, while the roster is
/// not fully covered, a `loom.daemon.task_faults{reason=eta_authority_coverage}`
/// count each pass (the critical `eta.authority.coverage` signal). An unknown
/// roster raises nothing. A fallback host skips it: it is not the authority.
pub(super) fn check_coverage(root: &Path, host_id: &str, covered: &[String]) {
    let scoped = FALLBACK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_some();
    if scoped {
        return;
    }
    coverage::write_last_pass(
        root,
        &coverage::LastPass {
            host: host_id.to_string(),
            covered: covered.to_vec(),
        },
    );
    let Some(roster) = cached_roster(root) else {
        return;
    };
    report_coverage(&coverage::coverage(&roster, covered), host_id);
}

pub(super) fn report_coverage(c: &coverage::Coverage, host_id: &str) {
    let mut last = LAST_COVERAGE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if c.state != State::Short {
        *last = None;
        return;
    }
    let line = c.describe();
    if last.as_deref() != Some(line.as_str()) {
        log::error!(
            "eta.authority.coverage: authority {host_id} covers {line}; other hosts keep \
             emitting the uncovered repos (#10897)"
        );
        *last = Some(line);
    }
    crate::observability::ops::liveness::fault(
        crate::task_liveness::ETA_PASS,
        crate::observability::ops::liveness::Fault::EtaAuthorityCoverage,
    );
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
    let is_authority = decide(workspace_root, resolution.is_authority());
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
    let is_authority = decide(workspace_root, resolution.is_authority());
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
