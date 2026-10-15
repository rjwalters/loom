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
//! First, as for every role: an unreadable issue, or one that no longer
//! carries `loom:building`, gets no write ([`ClaimRelease::Untouched`]). For a
//! Builder even the claim-only removal is skipped on a failed read, because
//! dropping `loom:building` blind could drop the open-PR hold below.
//! Otherwise a Builder step's claim is the sweep's claim, so its release is
//! the sweep's:
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
//! decides. The reaper runs this on every step exit, a clean one included,
//! so the claim must still be there before anything is restored:
//!
//! - the read failed: never a restore; at most the step's own claim label is
//!   removed (`stale_after_minutes` is the backstop).
//! - the claim label is gone: nothing is written. The step finished, or a
//!   later role holds the item.
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
/// the step got that far. Champion promotion moves an issue to `loom:issue`
/// (`promoted`) or back to `loom:triage` (`revise`, #11969): a revise that
/// died before removing `loom:evaluating` must not get `loom:curated` back
/// beside its `loom:triage`.
#[must_use]
pub(crate) fn advanced_labels(role: StepRole) -> &'static [&'static str] {
    match role {
        StepRole::Curator => &["loom:curated", "loom:issue"],
        StepRole::ChampionPromote => &["loom:issue", "loom:triage"],
        StepRole::Builder => &["loom:review-requested"],
        StepRole::Judge => &["loom:pr", "loom:changes-requested"],
        StepRole::Doctor => &["loom:review-requested"],
        StepRole::ChampionMerge => &[],
    }
}

/// What a step release writes, decided from the item's labels and state.
/// Pure.
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
/// (`None`: the read failed).
///
/// The reaper calls this on **every** step exit, a clean exit 0 included: a
/// step has no completion signal like a sweep's checkpoint. So the plan first
/// asks whether the claim is still there:
///
/// - read failed: never restore. At most the step's own claim label goes
///   (a no-op when the step already removed it); `stale_after_minutes` is the
///   backstop for anything left behind.
/// - claim label absent: write nothing. The step released it (and moved the
///   item on), or another role holds the item now: a Doctor that finished and
///   a Judge that claimed since must not get `loom:changes-requested` back.
#[must_use]
pub(crate) fn plan_step_release(
    role: StepRole,
    labels: &[String],
    closed: Option<bool>,
) -> StepReleasePlan {
    let Some(claim) = role.claim_label() else {
        return StepReleasePlan::Nothing;
    };
    if closed.is_none() {
        return StepReleasePlan::RemoveClaim;
    }
    if !labels.iter().any(|l| l == claim) {
        return StepReleasePlan::Nothing;
    }
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
                self.restore_role_step(item, role, sweep_id)
            }
        }
    }

    fn restore_builder_step(&self, issue: u32, sweep_id: &str) -> Result<ClaimRelease> {
        let signals = self.release_issue_signals(issue);
        let release = self.release_builder_step(issue, sweep_id, &signals);
        // A Builder step moved its issue when its PR is open: the release's
        // own verdict, so the ledger and the release agree (#11909).
        let pr_open = matches!(release, Ok(ClaimRelease::HeldByOpenPr(_)));
        let post = signals.closed.map(|closed| {
            Self::step_post_state(StepRole::Builder, &signals.labels, closed, pr_open)
        });
        crate::sweep_registry::step_spawn::ended::note_post(
            &self.config.workspace_root,
            sweep_id,
            post,
        );
        release
    }

    fn release_builder_step(
        &self,
        issue: u32,
        sweep_id: &str,
        signals: &outcome_journal::complexity_signal::IssueSignals,
    ) -> Result<ClaimRelease> {
        // The same rule as `plan_step_release`, before the shared release
        // (which fails open to a restore on a failed read, as a sweep's crash
        // path wants): a Builder step that exited with `loom:building` gone
        // is finished, and an unreadable issue is not touched at all, since
        // removing `loom:building` blind could drop the open-PR hold (#11852).
        if signals.closed.is_none() {
            log::info!(
                "sweep_registry: Builder step on #{issue} ended; the issue could not be read — \
                 writing nothing, claim staleness is the backstop (#11898)"
            );
            return Ok(ClaimRelease::Untouched);
        }
        if !signals.labels.iter().any(|l| l == "loom:building") {
            return Ok(ClaimRelease::Untouched);
        }
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

    /// The outcome ledger's post-state of a `role` step on an item with
    /// `labels` and `closed`, read before the release writes anything
    /// (#11909). `pr_open`: a Builder's linked PR is open (the release's
    /// open-PR hold decided it). Pure.
    ///
    /// Each claim label *replaces* its role's ready label, so "the ready
    /// label is gone" proves nothing: a crashed step leaves exactly that.
    /// The item moved only when it carries a label the role moves it *to*
    /// ([`advanced_labels`], the release's own rule), a Builder's PR is
    /// open, or a merge step's PR left `loom:pr`. A claim still on an item
    /// that did not move is held: the step died mid-work.
    #[must_use]
    pub fn step_post_state(
        role: StepRole,
        labels: &[String],
        closed: bool,
        pr_open: bool,
    ) -> crate::step_dispatch::outcome_ledger::PostState {
        let has = |l: &str| labels.iter().any(|x| x == l);
        let claim = role.claim_label().is_some_and(has);
        let moved = !closed
            && match role {
                StepRole::Builder => {
                    pr_open
                        || advanced_labels(role).iter().any(|l| has(l))
                        || !claim && !has("loom:issue")
                }
                StepRole::ChampionMerge => !has("loom:pr"),
                _ => advanced_labels(role).iter().any(|l| has(l)),
            };
        crate::step_dispatch::outcome_ledger::PostState {
            moved,
            closed,
            claim_held: claim && !moved && !closed,
        }
    }

    /// The ledger's post-state from the release's own read, before any
    /// restore write (`step_spawn::ended`, #11909).
    fn note_step_post(
        &self,
        role: StepRole,
        sweep_id: &str,
        labels: &[String],
        closed: Option<bool>,
    ) {
        let post = closed.map(|closed| Self::step_post_state(role, labels, closed, false));
        crate::sweep_registry::step_spawn::ended::note_post(
            &self.config.workspace_root,
            sweep_id,
            post,
        );
    }

    fn restore_role_step(&self, item: u32, role: StepRole, sweep_id: &str) -> Result<ClaimRelease> {
        let (Some(claim), Some(ready)) = (role.claim_label(), role.restore_label()) else {
            return Ok(ClaimRelease::Released);
        };
        let signals = self.release_issue_signals(item);
        self.note_step_post(role, sweep_id, &signals.labels, signals.closed);
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
            StepReleasePlan::Nothing if crate::work_finder::is_frozen(&signals.labels) => {
                return Ok(ClaimRelease::Frozen)
            }
            StepReleasePlan::Nothing => return Ok(ClaimRelease::Untouched),
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
