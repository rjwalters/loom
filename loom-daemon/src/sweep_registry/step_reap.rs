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

/// Slack between a Builder step's locally recorded start and the forge's
/// `labeled` time. GitHub stamps whole seconds and host clocks drift, so the
/// claim label this step's own dispatch applied just before the spawn can
/// read as up to a clock-skew later than the start. Only a label applied
/// beyond the slack is another claimant's. (Slack in the other direction,
/// `started_at - 60s`, would read the step's own label as foreign and leave
/// every dead Builder step's claim stranded.)
pub(crate) const LABEL_CLOCK_SKEW_SECS: i64 = 60;

/// Whether a claim label stamped `labeled_at` was re-applied by another
/// claimant after a step that started at `started_at`. Pure.
#[must_use]
pub(crate) fn label_reapplied_after(labeled_at: DateTime<Utc>, started_at: DateTime<Utc>) -> bool {
    labeled_at > started_at + chrono::Duration::seconds(LABEL_CLOCK_SKEW_SECS)
}

impl SweepRegistry {
    /// Release a finished step's claim. Returns `true` when the claim is held
    /// elsewhere, in which case no label is written:
    ///
    /// - a newer local owner of the step's claim lock;
    /// - for a Builder step, `loom:building` re-applied on the forge more than
    ///   [`LABEL_CLOCK_SKEW_SECS`] after the step started (the issue sweep's
    ///   cross-host check, with slack for the forge's whole-second stamps and
    ///   host drift);
    /// - for every other step, the role's earliest live lease on the item
    ///   held by another sweep ([`Self::step_lease_held_elsewhere`]). The
    ///   label is no evidence there: the daemon pre-takes only the role-keyed
    ///   lease, and the role prompt applies the label itself, later.
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
                && match role {
                    StepRole::Builder => self
                        .fetch_label_applied_at(item, "loom:building")
                        .is_some_and(|at| label_reapplied_after(at, started_at)),
                    StepRole::Curator
                    | StepRole::ChampionPromote
                    | StepRole::Judge
                    | StepRole::Doctor
                    | StepRole::ChampionMerge => {
                        self.step_lease_held_elsewhere(item, role, sweep_id)
                    }
                });
        self.publish_step_claim(peer_claims::ClaimKind::Retract, item, role);
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

    /// Whether a PR step's claim on `pr` is still the paused run `sweep_id`'s
    /// to give back after a roll (#11898): no other sweep holds the role's
    /// earliest live lease, and the claim label was not re-applied after the
    /// run started (`started_at`, with the clock-skew slack). A peer Judge or
    /// Doctor that claimed the PR during the roll window keeps it.
    ///
    /// # Errors
    /// Why the claim is no longer this run's.
    pub(crate) fn roll_pr_step_claim_still_ours(
        &self,
        pr: u32,
        role: StepRole,
        sweep_id: &str,
        started_at: Option<DateTime<Utc>>,
    ) -> std::result::Result<(), String> {
        if self.config.skip_label_flip {
            return Ok(());
        }
        if self.step_lease_held_elsewhere(pr, role, sweep_id) {
            return Err(format!("another sweep holds the live {role} lease"));
        }
        if let (Some(label), Some(started_at)) = (role.claim_label(), started_at) {
            if self
                .fetch_label_applied_at(pr, label)
                .is_some_and(|at| label_reapplied_after(at, started_at))
            {
                return Err(format!("`{label}` was re-applied after this run started"));
            }
        }
        Ok(())
    }

    /// Publish a step's `(item, step)` peer claim (#11898): `Advertise` while
    /// it runs, `Retract` when it ends. Fail-open and a no-op without a
    /// publisher, like every peer-claim send. A Champion merge step's
    /// advertisement is the repo-keyed merge signal peers read through
    /// `PeerClaimView::merge_in_flight_at`.
    pub(crate) fn publish_step_claim(
        &self,
        kind: peer_claims::ClaimKind,
        item: u32,
        role: StepRole,
    ) {
        let Some(tx) = &self.peer_claim_publisher else {
            return;
        };
        let ad = ClaimAd::step(
            kind,
            item,
            role,
            peer_claims::repo_slug(&self.config.workspace_root),
            host_identity(),
            std::process::id(),
            Utc::now().to_rfc3339(),
        );
        if let Err(e) = tx.try_send(ad) {
            log::debug!("sweep_registry: {role} step claim for #{item} dropped ({e})");
        }
    }

    /// Re-advertise every live step's claim on the reaper's heartbeat, as
    /// [`Self::readvertise_peer_claims`] does for issue sweeps. Returns the
    /// number advertised.
    pub(crate) fn readvertise_step_claims(&self) -> usize {
        let live: Vec<(u32, StepRole)> = self
            .entries
            .values()
            .filter(|info| matches!(info.state, SweepState::Running | SweepState::Pending))
            .filter_map(|info| match info.kind {
                SweepKind::Step { item, role } => Some((item, role)),
                SweepKind::Issue(_) | SweepKind::PrSet(_) => None,
            })
            .collect();
        for (item, role) in &live {
            self.publish_step_claim(peer_claims::ClaimKind::Advertise, *item, *role);
        }
        live.len()
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
