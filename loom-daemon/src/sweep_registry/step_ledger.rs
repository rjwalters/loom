//! The registry side of the step-outcome ledger (#11900; design: #11894 §5).
//!
//! [`crate::step_dispatch::outcome_ledger`] decides; this applies. One call
//! per finished step, [`SweepRegistry::record_step_outcome`]:
//!
//! | Effect | Applied here as |
//! |---|---|
//! | `Advance` | the item's local history and cooldowns are dropped; so is every peer copy in this host's view, and every peer is told to drop its own (a zero-length `StepCooldownArmed` for **every** step of the item, whoever armed it); a Champion verdict also removes its step's forge ladder |
//! | `Landed` | the same, plus the #11853 landed cooldown on the issue (fleet-wide, every consumer) and a 24 h Builder cooldown on `(item, builder)` |
//! | `Cooldown` | the window, broadcast |
//! | `FastFailLadder` (Builder steps only) | [`SweepRegistry::charge_fast_failure`] (the #11854 ladder and park), and the ladder's window mirrored onto `(item, step)` and broadcast |
//! | `FastFailWindow` (every other step: PR steps, Curator, Champion promotion) | the ledger's own doubling window, broadcast; **no** forge write: the park's release restores `loom:issue`, which is only right for an issue Builder was building (#11969) |
//! | `ForgeLadder` | the next rung written to the item's body ([`reeval_ladder::record`]), its due time mirrored locally (capped at 24 h) and broadcast |
//! | `Redraw` | counted; a second in a row on the pair arms the ledger's doubling refusal cooldown, broadcast (#11969) |
//! | `Reclassify` | nothing for the first; a second identical state refusal in a row arms the ledger's doubling refusal cooldown, broadcast. Not a repeat-cap run |
//! | `Failed` (slow) | counted toward the repeat cap |
//!
//! A Builder step's `noop` also feeds the issue's no-op hold (#12024,
//! [`SweepRegistry::note_builder_step_noop`]). The ledger's own history
//! cannot bound a promote / Builder-no-op cycle: Champion's promotion is a
//! verdict, and a verdict clears every step's history and cooldown on the
//! item. The hold's streak lives outside the ledger, is counted across the
//! fleet, and survives the promotion (the fingerprint ignores the
//! `loom:curated` / `loom:issue` lane churn), so the third no-op on an
//! unchanged issue parks it on `loom:blocked` + a `no-op hold` park record,
//! which the step draw passes (`Blocked`) until its fingerprint changes.
//!
//! Whenever the repeat cap arms, its window is broadcast like any other,
//! except a refusal's: a pool gate or a claim race is this host's own, so a
//! `Refused` outcome's window stays local (#11909).
//! Nothing broadcast is longer than 24 h: the peer receiver caps it too.

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use super::{host_identity, SweepRegistry, LANDED_COOLDOWN};
use crate::peer_claims::{self, ClaimAd};
use crate::step_dispatch::outcome_ledger::{
    CooldownCause, Effect, LedgerCounters, OutcomeClass, Recorded, StepCooldowns, StepOutcome,
};
use crate::step_dispatch::reeval_ladder::{self, BodyForge};
use crate::step_role::StepRole;

/// One finished step, as the reaper hands it to the ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepRun {
    /// The number the step ran on (the PR for a PR step).
    pub item: u32,
    pub step: StepRole,
    /// The step's sweep id (the fast-fail ladder's no-op carve-out reads it).
    pub sweep_id: String,
    /// The classified outcome (`outcome_ledger::classify`).
    pub outcome: StepOutcome,
    /// How long the session ran.
    pub ran_for: Duration,
    /// The reaper's failure class, when it has one; the outcome's reason
    /// otherwise.
    pub failure_class: Option<String>,
}

impl SweepRegistry {
    /// Record a finished step's outcome and apply its effect (module doc).
    /// `forge` reads and writes the item's body for the Champion ladder.
    pub fn record_step_outcome<F: BodyForge + ?Sized>(
        &mut self,
        run: &StepRun,
        forge: &mut F,
    ) -> Recorded {
        let now = Utc::now();
        let (item, step) = (run.item, run.step);
        let rec = self
            .step_ledger
            .record(item, step, &run.outcome, run.ran_for, now);
        if step == StepRole::Builder && run.outcome.class == OutcomeClass::Noop {
            self.note_builder_step_noop(item, &run.sweep_id, &run.outcome.reason);
        }
        let champion = step.role_name() == "champion";
        if matches!(rec.effect, Effect::Advance | Effect::Landed) {
            self.lift_item_step_cooldowns(item);
            if champion {
                if let Err(e) = reeval_ladder::clear(forge, item, step) {
                    log::warn!("step_ledger: clearing the re-evaluation ladder on #{item}: {e}");
                }
            }
        }
        match rec.effect {
            Effect::Landed => {
                self.arm_landed_cooldown(item, LANDED_COOLDOWN);
                self.broadcast_step_cooldown(item, StepRole::Builder, now);
            }
            Effect::FastFailLadder => {
                let class = run
                    .failure_class
                    .clone()
                    .unwrap_or_else(|| run.outcome.reason.clone());
                self.charge_fast_failure(&run.sweep_id, item, &class, false);
                if let Some(until) = self.dispatch_backoff_until(item, now) {
                    self.step_ledger
                        .arm(item, step, until, CooldownCause::FastFail, now);
                }
            }
            Effect::ForgeLadder => {
                // On a failed write, the first rung still holds locally, so
                // the item is not redrawn on the next tick.
                let next = match reeval_ladder::record(forge, item, step, now) {
                    Ok(r) => {
                        log::info!(
                            "step_ledger: #{item} {step} re-evaluation ladder rung {} until {}",
                            r.rung,
                            r.next
                        );
                        r.next
                    }
                    Err(e) => {
                        log::warn!("step_ledger: writing the re-evaluation ladder on #{item}: {e}");
                        now + chrono::Duration::from_std(reeval_ladder::FIRST_RUNG)
                            .unwrap_or_else(|_| chrono::Duration::hours(6))
                    }
                };
                self.step_ledger
                    .arm(item, step, next, CooldownCause::ForgeLadder, now);
            }
            Effect::Redraw => {
                log::info!(
                    "step_ledger: #{item} {step} claim lost to a peer (count {})",
                    self.step_ledger.counters().claim_lost
                );
            }
            Effect::Advance
            | Effect::Cooldown(_)
            | Effect::FastFailWindow
            | Effect::Reclassify
            | Effect::Failed => {}
        }
        // A refusal is host-local (#11909): this host's exhausted pool or its
        // claim race says nothing about a peer's, so its cooldown is not
        // broadcast. Every other window is.
        if run.outcome.class != OutcomeClass::Refused {
            self.broadcast_step_cooldown(item, step, now);
        }
        let cooling_until = self.step_ledger.cooling_until(item, step, now);
        Recorded {
            cooling_until,
            ..rec
        }
    }

    /// Feed a Builder step's `noop` on issue `item` to the no-op hold
    /// (#12024): the fleet-wide streak, and the `loom:blocked` park at the
    /// threshold. A `noop` that waited on something outside the item
    /// (`ci_pending`) is not a loop. Counted once per dispatch: a step that
    /// also self-reported over IPC (`noop-cooldown record`) carries the same
    /// sweep id, and the hold counts a sweep id once.
    pub(crate) fn note_builder_step_noop(&mut self, item: u32, sweep_id: &str, reason: &str) {
        if crate::step_dispatch::outcome_ledger::EXTERNAL_WAIT_NOOPS.contains(&reason) {
            return;
        }
        let sweep = (!sweep_id.is_empty()).then_some(sweep_id);
        self.note_noop_for_hold(item, sweep, Some(reason));
    }

    /// The draw's query at `now`: this host's ledger, plus every live peer
    /// step cooldown for this workspace. The cutover adds the forge ladder's
    /// due times ([`reeval_ladder::due`]) from the listing's bodies.
    #[must_use]
    pub fn step_cooldowns(&self, now: DateTime<Utc>) -> StepCooldowns {
        let mut out = self.step_ledger.snapshot(now);
        if let Some(view) = &self.peer_claims {
            let repo = peer_claims::repo_slug(&self.config.workspace_root);
            let view = view
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (item, step, remaining) in view.step_cooldowns_at(&repo, Instant::now()) {
                if let Ok(d) = chrono::Duration::from_std(remaining) {
                    out.insert(item, step, now + d);
                }
            }
        }
        out
    }

    /// The ledger's counters.
    #[must_use]
    pub fn step_ledger_counters(&self) -> &LedgerCounters {
        self.step_ledger.counters()
    }

    /// Drop the ledger's lapsed entries (each reaper tick).
    pub fn prune_step_ledger(&mut self) {
        self.step_ledger.prune(Utc::now());
    }

    /// A verdict changed `item`'s state: drop every peer copy of its step
    /// cooldowns from this host's view, and tell every peer to drop theirs,
    /// for every step, whoever armed it (a zero-length ad lifts it).
    fn lift_item_step_cooldowns(&self, item: u32) {
        if let Some(view) = &self.peer_claims {
            let repo = peer_claims::repo_slug(&self.config.workspace_root);
            view.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear_step_cooldowns(&repo, item);
        }
        for step in StepRole::ALL {
            self.publish_step_cooldown(item, step, Duration::ZERO);
        }
    }

    /// Broadcast `(item, step)`'s local window in force at `now`, if any.
    fn broadcast_step_cooldown(&self, item: u32, step: StepRole, now: DateTime<Utc>) {
        if let Some(until) = self.step_ledger.cooling_until(item, step, now) {
            let remaining = (until - now).to_std().unwrap_or_default();
            self.publish_step_cooldown(item, step, remaining);
        }
    }

    /// Send one [`peer_claims::ClaimKind::StepCooldownArmed`] ad (zero lifts
    /// it), fail-open like every other brake publisher.
    fn publish_step_cooldown(&self, item: u32, step: StepRole, remaining: Duration) {
        let Some(tx) = &self.peer_claim_publisher else {
            return;
        };
        let remaining = remaining.min(peer_claims::MAX_PEER_STEP_COOLDOWN);
        let ad = ClaimAd::step_cooldown_armed(
            item,
            step,
            peer_claims::repo_slug(&self.config.workspace_root),
            host_identity(),
            std::process::id(),
            Utc::now().to_rfc3339(),
            remaining.as_secs(),
        );
        if let Err(e) = tx.try_send(ad) {
            log::debug!("step_ledger: step cooldown ad for #{item} {step} dropped ({e})");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "step_ledger_tests.rs"]
mod tests;
