//! Per-role claim restore when a step dies (Issue #11898; design #11894 §5).
//!
//! | Released claim | Restored to |
//! |---|---|
//! | `loom:curating` | `loom:triage` |
//! | `loom:evaluating` | `loom:curated` |
//! | `loom:building` | `loom:issue` |
//! | `loom:reviewing` | `loom:review-requested` |
//! | `loom:treating` | `loom:changes-requested` |
//!
//! The table itself is [`StepRole::claim_label`] / [`StepRole::restore_label`].
//!
//! # Builder
//!
//! A Builder step's claim is the sweep's claim, so its release is the sweep's:
//! [`SweepRegistry::restore_label_to_ready_checked`], with every carve-out
//! (frozen, park veto, PR, closed, never-ready, the #11438 no-op release) and
//! the #11852 open-PR hold: an issue whose linked PR is open keeps
//! `loom:building` and nothing is written ([`ClaimRelease::HeldByOpenPr`]).
//! That hold has an operator off-switch (`LOOM_RELEASE_OPEN_PR_VETO=0`) for
//! the sweep path. A step does not honour it: the step engine draws by item
//! state, and an issue back in `loom:issue` beside its own open PR would be
//! drawn for a Builder step on every tick. So with the switch off, the step
//! asks the same probe ([`SweepRegistry::probe_open_linked_pr`]) itself
//! before the shared release.
//!
//! # The other claims
//!
//! One REST read of the item (an issue or a PR, through the issues endpoint)
//! decides, fail-open like every release path:
//!
//! - `loom:frozen`: no label write at all (#11743).
//! - closed: the claim goes, nothing is added.
//! - a release veto ([`crate::work_finder::restore_veto`], the claim itself
//!   excluded): the claim goes, the hold stays, nothing is added.
//! - the item already carries a label the role moves it *to*
//!   ([`advanced_labels`]): the step finished its write and died before
//!   removing its claim. The claim goes; restoring the ready label would move
//!   the item backwards.
//! - otherwise: the claim goes and the ready label is added, in one edit.
//!
//! The merge step has no claim label: its claim is the PR lease, which ages
//! out on its own, so nothing is written.

use super::*;
use crate::step_role::StepRole;

/// The labels `role` moves its item to on success. Seeing one at release means
/// the step got that far.
#[must_use]
pub(crate) fn advanced_labels(role: StepRole) -> &'static [&'static str] {
    match role {
        StepRole::Curator => &["loom:curated", "loom:issue"],
        StepRole::ChampionPromote => &["loom:issue"],
        StepRole::Builder => &["loom:review-requested"],
        StepRole::Judge => &["loom:pr", "loom:changes-requested"],
        StepRole::Doctor => &["loom:review-requested"],
        StepRole::ChampionMerge => &[],
    }
}

/// What a non-Builder release writes, decided from the item's labels and
/// state. Pure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StepReleasePlan {
    /// Write nothing (frozen item, or no claim label to release).
    Nothing,
    /// Remove the claim label only.
    RemoveClaim,
    /// Remove the claim label and add the role's ready label.
    Restore,
}

/// [`StepReleasePlan`] for `role` on an item with `labels` and `closed`
/// (`None`: the read failed, which releases as an open item would).
#[must_use]
pub(crate) fn plan_step_release(
    role: StepRole,
    labels: &[String],
    closed: Option<bool>,
) -> StepReleasePlan {
    let Some(claim) = role.claim_label() else {
        return StepReleasePlan::Nothing;
    };
    let veto = crate::work_finder::restore_veto(labels, &[claim]);
    if veto == Some(crate::work_finder::FROZEN_LABEL) {
        return StepReleasePlan::Nothing;
    }
    let advanced = labels
        .iter()
        .any(|l| advanced_labels(role).contains(&l.as_str()));
    if veto.is_some() || closed == Some(true) || advanced {
        return StepReleasePlan::RemoveClaim;
    }
    StepReleasePlan::Restore
}

impl SweepRegistry {
    /// Release a dead step's claim on `item` back to `role`'s ready stage
    /// (#11898). `sweep_id` is the step's own, for the Builder no-op release.
    pub(crate) fn restore_step_claim(
        &self,
        item: u32,
        role: StepRole,
        sweep_id: &str,
    ) -> Result<ClaimRelease> {
        if self.config.skip_label_flip {
            return Ok(ClaimRelease::Released);
        }
        match role {
            StepRole::Builder => self.restore_builder_step(item, sweep_id),
            StepRole::ChampionMerge => Ok(ClaimRelease::Released),
            StepRole::Curator | StepRole::ChampionPromote | StepRole::Judge | StepRole::Doctor => {
                self.restore_role_step(item, role)
            }
        }
    }

    fn restore_builder_step(&self, issue: u32, sweep_id: &str) -> Result<ClaimRelease> {
        if !crate::release_veto::open_pr_veto_enabled() {
            if let OpenPrProbe::Open(pr) = self.probe_open_linked_pr(issue) {
                log::info!(
                    "sweep_registry: Builder step on #{issue} died with open linked PR #{pr} — \
                     keeping `loom:building` (the step engine ignores the open-PR veto's \
                     off-switch, #11898)"
                );
                return Ok(ClaimRelease::HeldByOpenPr(pr));
            }
        }
        if self.noop_release_covers_dispatch(issue, sweep_id) {
            self.restore_label_after_reap(issue, sweep_id)?;
            return Ok(ClaimRelease::Released);
        }
        self.restore_label_to_ready_checked(issue)
    }

    fn restore_role_step(&self, item: u32, role: StepRole) -> Result<ClaimRelease> {
        let (Some(claim), Some(ready)) = (role.claim_label(), role.restore_label()) else {
            return Ok(ClaimRelease::Released);
        };
        let signals = self.release_issue_signals(item);
        let plan = plan_step_release(role, &signals.labels, signals.closed);
        log::info!(
            "sweep_registry: {role} step on #{item} died — releasing `{claim}` ({plan:?}, \
             restore target `{ready}`, #11898)"
        );
        let kind = if role.acts_on_pr() { "pr" } else { "issue" };
        let mut args = vec![
            kind.to_string(),
            "edit".to_string(),
            item.to_string(),
            "--remove-label".to_string(),
            claim.to_string(),
        ];
        match plan {
            StepReleasePlan::Nothing => return Ok(ClaimRelease::Frozen),
            StepReleasePlan::RemoveClaim => {}
            StepReleasePlan::Restore => {
                args.extend(["--add-label".to_string(), ready.to_string()]);
            }
        }
        self.run_claim_release_edit(item, args)
            .map(|()| ClaimRelease::Released)
    }
}

#[cfg(test)]
#[path = "step_restore_tests.rs"]
mod tests;
