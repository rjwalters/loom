//! A step's terminal transition (Issue #11898; design #11894 §5).
//!
//! An issue sweep's death runs a long recovery chain (checkpoint resume,
//! quarantine, the outcome journal, `sweep.issue.{N}.*` events). None of it
//! applies to a step: a step is one role session, its outcome belongs to the
//! step ledger (#11868's classes, consumed by a later child), and a dead step
//! is recovered by putting its item back where the draw finds it again. So the
//! reaper and the cancel path hand a step here, which:
//!
//! 1. releases the step's claim lock, ownership-checked like a sweep's: an
//!    issue step holds `issue-<N>` (the lock an issue sweep holds, so the two
//!    never run on one issue at once), a PR step holds `pr-<N>` (the PR-set
//!    lock namespace, with the same exclusion);
//! 2. unless the lock or the forge says a newer claimant holds the item,
//!    restores the claim per role ([`SweepRegistry::restore_step_claim`]);
//! 3. marks the entry `Exited` and reports `sweep.global.completed`.

use super::*;
use crate::step_role::StepRole;

impl SweepRegistry {
    /// Release a finished step's claim. Returns `true` when the claim is held
    /// elsewhere (a newer local lock owner, or a claim label re-applied on the
    /// forge after this step started), in which case no label is written.
    pub(crate) fn release_dead_step(
        &mut self,
        sweep_id: &str,
        item: u32,
        role: StepRole,
        started_at: DateTime<Utc>,
    ) -> bool {
        let retained = if role.acts_on_pr() {
            self.release_pr_lock_owned(item, sweep_id).retained()
        } else {
            self.release_lock_owned(item, sweep_id).retained()
        };
        let held_elsewhere = retained
            || (!self.config.skip_label_flip
                && role.claim_label().is_some_and(|label| {
                    self.fetch_label_applied_at(item, label)
                        .is_some_and(|at| at > started_at)
                }));
        if held_elsewhere {
            log::info!(
                "sweep_registry: {role} step {sweep_id} on #{item} ended, but its claim is held \
                 elsewhere now — leaving the labels alone (#11898)"
            );
            return true;
        }
        if !self.config.skip_label_flip {
            match self.restore_step_claim(item, role, sweep_id) {
                Ok(release) => log::info!(
                    "sweep_registry: {role} step {sweep_id} on #{item} ended — claim release: \
                     {release:?} (#11898)"
                ),
                Err(e) => log::warn!(
                    "sweep_registry: {role} step {sweep_id} on #{item} ended — claim release \
                     failed: {e:#}"
                ),
            }
            self.note_label_flip(item);
        }
        false
    }

    /// The terminal transition of a step the reaper (or a cancel) found dead:
    /// release, mark `Exited` with `exit_code`, and return the completion event.
    pub(crate) fn finish_dead_step(
        &mut self,
        sweep_id: &str,
        item: u32,
        role: StepRole,
        started_at: DateTime<Utc>,
        exit_code: Option<i32>,
    ) -> Event {
        self.release_dead_step(sweep_id, item, role, started_at);
        if let Some(info) = self.entries.get_mut(sweep_id) {
            info.state = SweepState::Exited {
                code: exit_code,
                at: Utc::now(),
            };
        }
        Event::SweepGlobalCompleted {
            sweep_id: sweep_id.to_string(),
            outcome: SweepOutcome::Exited,
        }
    }
}

#[cfg(test)]
#[path = "step_reap_tests.rs"]
mod tests;
