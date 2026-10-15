//! B3 (#11906): every step has a timeout, and the reaper enforces it.
//!
//! The sweep watchdogs skip steps (they re-dispatch `/loom:sweep N`), and the
//! reaper's death path acts only on a dead pid, so before this a wedged step
//! held its slot and its claim forever. Each step now gets its role's
//! timeout ([`step_timeout`]); the reaper's tick kills a step past it
//! ([`SweepRegistry::reap_step_timeouts`]) through the ordinary cancel path,
//! whose step arm releases the claim per role (#11921's restore), and the
//! step's end reaps the leases it still listed with `reaped=timeout`
//! (#11931's reap, the one the role runner runs for a timed-out Judge).
//!
//! The deadline is computed from the entry's `started_at`, so it survives a
//! daemon restart: an adopted step keeps the deadline it had.

use super::*;
use std::collections::HashSet;
use std::sync::Mutex;

/// SIGTERM-to-SIGKILL grace for a timed-out step (the role runner's).
pub(crate) const STEP_TIMEOUT_GRACE: Duration = Duration::from_secs(5);

/// How long a step of `role` in the workspace at `root` may run before the
/// reaper kills it.
///
/// Every role the role runner runs has its timeout,
/// `role_runner::DEFAULT_ROLE_TIMEOUT` (one Curator, Judge, Doctor or Champion
/// session). The role runner never runs Builder: a Builder step is one build,
/// so it gets the ceiling the sweep path applies to a whole issue sweep, with
/// that ceiling's own overrides (`LOOM_SWEEP_STALE_AGE_SECS`, then
/// `autonomous.watchdog.staleSweepAgeSecs`, `watchdog::resolve_stale_sweep_age`).
#[must_use]
pub fn step_timeout(root: &Path, role: StepRole) -> Duration {
    match role {
        StepRole::Builder => {
            let config = super::super::watchdog::read_startup_race_config(root);
            super::super::watchdog::resolve_stale_sweep_age(&config)
        }
        StepRole::Curator
        | StepRole::ChampionPromote
        | StepRole::Judge
        | StepRole::Doctor
        | StepRole::ChampionMerge => crate::role_runner::DEFAULT_ROLE_TIMEOUT,
    }
}

/// Whether a step that started at `started_at` is past `timeout` at `now`.
/// Pure.
#[must_use]
pub(crate) fn timed_out(timeout: Duration, started_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    (now - started_at)
        .to_std()
        .is_ok_and(|elapsed| elapsed > timeout)
}

/// Steps the reaper is killing for their timeout, so the cancel path's step
/// arm ends them as timed out rather than cancelled.
static TIMED_OUT: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn mark_timed_out(sweep_id: &str) {
    TIMED_OUT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_or_insert_with(HashSet::new)
        .insert(sweep_id.to_string());
}

/// Whether `sweep_id` was killed for its timeout (consumed).
pub(crate) fn take_timed_out(sweep_id: &str) -> bool {
    TIMED_OUT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_mut()
        .is_some_and(|set| set.remove(sweep_id))
}

impl SweepRegistry {
    /// Kill every live step past its role's timeout (B3), and refresh the
    /// per-role step slots. Returns how many were killed. A step a roll's
    /// pause holds is left alone, as the death path leaves it.
    pub fn reap_step_timeouts(&mut self) -> usize {
        let now = Utc::now();
        let root = &self.config.workspace_root;
        let due: Vec<(SweepId, StepRole, DateTime<Utc>)> = self
            .entries
            .iter()
            .filter(|(id, info)| {
                matches!(info.state, SweepState::Running | SweepState::Pending)
                    && !crate::roll_pause::hold::is_held(id)
            })
            .filter_map(|(id, info)| match info.kind {
                SweepKind::Step { role, .. }
                    if timed_out(step_timeout(root, role), info.started_at, now) =>
                {
                    Some((id.clone(), role, info.started_at))
                }
                SweepKind::Step { .. } | SweepKind::Issue(_) | SweepKind::PrSet(_) => None,
            })
            .collect();
        let mut killed = 0;
        for (sweep_id, role, started_at) in due {
            log::warn!(
                "sweep_registry: {role} step {sweep_id} exceeded its {}s timeout (started {}) — \
                 killing it and releasing its claim (#11906)",
                step_timeout(&self.config.workspace_root, role).as_secs(),
                started_at.to_rfc3339()
            );
            mark_timed_out(&sweep_id);
            match self.cancel(&sweep_id, STEP_TIMEOUT_GRACE) {
                Ok(_) => killed += 1,
                Err(e) => {
                    take_timed_out(&sweep_id);
                    log::warn!("sweep_registry: timed-out step {sweep_id} not cancelled: {e:#}");
                }
            }
        }
        self.publish_step_slots();
        killed
    }
}
