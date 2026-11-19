//! The daemon's process-global [`InProgressGuard`] handle and its read-backs:
//! the total live role-agent count (`loom-daemon status`, #6102) and the
//! per-role count behind `loom.dispatch.busy_slots` (#11847). Split out of
//! `role_runner.rs` so the per-role read did not grow that file.

use std::collections::BTreeMap;
use std::sync::{OnceLock, PoisonError};

use super::{active_run_count, InProgressGuard};

/// The daemon's single [`InProgressGuard`], registered once at startup so
/// out-of-band readers (`loom-daemon status` via [`crate::ipc`]) can sample the
/// live role-agent count without threading the `Arc` through the IPC server —
/// the same process-global read-back shape
/// `crate::admission_brake::register_global` (removed, #12406) uses for the brake.
static GLOBAL_IN_PROGRESS: OnceLock<InProgressGuard> = OnceLock::new();

/// Register the process-global [`InProgressGuard`] handle. Idempotent: only the
/// first registration wins (there is exactly one guard per daemon process).
pub fn register_global_in_progress(set: InProgressGuard) {
    let _ = GLOBAL_IN_PROGRESS.set(set);
}

/// Live count of role invocations in flight across every managed workspace,
/// read from the process-global guard (#6102).
///
/// `0` when no guard has been registered (role runner never spawned, or a
/// non-daemon process such as the `calibrate` CLI) — honestly "no role agents
/// observed here", the same zero-behavior-change contract
/// `crate::admission_brake::global_is_holding` (removed, #12406) has.
///
/// This is the count that [`crate::types::DaemonStatusReport::active_role_agents`]
/// reports, so `loom-daemon status` shows total agent load — sweeps *and* role
/// agents — from one place rather than making an operator run `pgrep`.
#[must_use]
pub fn global_active_run_count() -> usize {
    GLOBAL_IN_PROGRESS.get().map_or(0, active_run_count)
}

/// Role invocations in flight per role across every managed workspace (each
/// lane counts once), or `None` when no guard is registered: no role runner
/// in this process, so there is nothing measured to report (#11847).
#[must_use]
pub fn global_active_runs_by_role() -> Option<BTreeMap<&'static str, usize>> {
    GLOBAL_IN_PROGRESS.get().map(runs_by_role)
}

/// [`global_active_runs_by_role`] over an explicit guard.
#[must_use]
pub fn runs_by_role(set: &InProgressGuard) -> BTreeMap<&'static str, usize> {
    let mut counts = BTreeMap::new();
    for (_, role, _) in set.lock().unwrap_or_else(PoisonError::into_inner).iter() {
        *counts.entry(*role).or_insert(0) += 1;
    }
    counts
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::runs_by_role;

    #[test]
    fn runs_are_counted_per_role_across_roots_and_lanes() {
        let set = crate::role_runner::new_in_progress_guard();
        {
            let mut held = set
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            held.insert((PathBuf::from("/w/a"), "judge", 0));
            held.insert((PathBuf::from("/w/b"), "judge", 0));
            held.insert((PathBuf::from("/w/a"), "doctor", 0));
            held.insert((PathBuf::from("/w/a"), "doctor", 1));
            held.insert((PathBuf::from("/w/a"), "curator", 0));
        }
        let counts = runs_by_role(&set);
        assert_eq!(counts.get("judge"), Some(&2));
        assert_eq!(counts.get("doctor"), Some(&2));
        assert_eq!(counts.get("curator"), Some(&1));
        assert_eq!(counts.values().sum::<usize>(), crate::role_runner::active_run_count(&set));
    }
}
