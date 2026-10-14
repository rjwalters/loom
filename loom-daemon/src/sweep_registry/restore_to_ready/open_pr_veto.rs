//! An issue keeps `loom:building` while its PR is open. (Issue #11852)
//!
//! # The bug
//!
//! A sweep that opened its PR and then died holding a checkpoint had its
//! claim released by the reaper, and the release re-added `loom:issue`. The
//! issue went back into the Builder queue beside its own open PR. The #4123
//! open-PR dispatch guard refused it, and the work finder's pre-filter dropped
//! it, on every tick for as long as the PR stayed open. Most of the fleet's
//! `loom:issue` candidates were in this state at once.
//!
//! Older comments called `loom:issue` beside an open PR the "resting state",
//! safe because the #4123 guard refuses re-dispatch. That premise is retired.
//! Removing both labels is no better: the issue still carries its proposal
//! or curation label (`loom:curated`, `loom:architect`, …), so Champion,
//! Curator and intake would pick it up while the PR is open.
//!
//! # The rule
//!
//! **An issue carries `loom:building` while its PR is open**, which is Loom's
//! own state machine: `merge-pr.sh` moves issue N from `loom:building` to
//! `loom:issue` on a `Part of #N` merge, and every work queue already skips
//! `loom:building`. So a release that would re-add `loom:issue` first asks
//! whether an open PR is linked to the issue; on a verified `Open(pr)` it
//! writes **no label at all**, leaving `loom:building` in place (and adding
//! nothing if the label is somehow already gone). The in-memory and lock
//! release around it is unchanged.
//!
//! This is the last term of the shared release veto in
//! [`restore_label_to_ready_checked`](SweepRegistry::restore_label_to_ready_checked),
//! so every release path gets it, and it is asked only when the re-add would
//! otherwise happen: a veto label, a PR, a closed issue or a never-ready claim
//! already decides the outcome without a probe. It also needs the issue
//! verified OPEN by the release's own signals read: when that read failed
//! (a wedged or rate-limited `gh`), the probe is skipped and the release goes
//! ahead, so a dead forge costs no extra bounded calls (#3973).
//!
//! # When the PR closes
//!
//! Merged: the merge closes the issue (or `merge-pr.sh` re-queues a `Part of`
//! issue itself). Closed unmerged: claim reconciliation finds a
//! `loom:building` issue with no live claim behind it, its own open-PR hold
//! (`claim_reconciliation::open_pr_hold`) no longer fires, and it restores
//! `loom:issue`.
//!
//! # Memo use is safe here
//!
//! The probe is [`SweepRegistry::probe_open_linked_pr`], which serves a fresh
//! verified-open memo (#6788) without a forge call. The W9 rule ("read live
//! when an `Open` answer *permits* a dispatch", see `refusal_memo`) does not
//! apply: here `Open` *prevents* a label write. A memo that is wrong (the PR
//! closed inside the freshness window) leaves `loom:building` on for one more
//! reconciliation pass, not a duplicate build.
//!
//! # Off-switch
//!
//! On by default. `LOOM_RELEASE_OPEN_PR_VETO=0` (or `false`/`no`/`off`)
//! turns off both halves, this one and the reconciliation hold, and restores
//! the old release. It shipped dark at first (default off) because removing
//! `loom:issue` alone left the issue in the Champion/Curator queues; keeping
//! `loom:building` is what makes it safe to enable.
//!
//! # Fail-open contract
//!
//! Same family as the other carve-outs: only a verified `Open(pr)` holds.
//! `NoneOpen` and `ProbeFailed` release as before, because a stranded claim
//! is the more common failure. Skipped entirely under `skip_label_flip`
//! (test fixtures without `gh` credentials), like every other forge probe on
//! the release path.

use super::*;

impl SweepRegistry {
    /// The open PR linked to `issue`, when one is verified open (Issue #11852).
    /// `None` for no open PR, a failed probe, or `skip_label_flip`.
    pub(super) fn release_open_linked_pr(&self, issue: u32) -> Option<u32> {
        if self.config.skip_label_flip || !crate::release_veto::open_pr_veto_enabled() {
            return None;
        }
        match self.probe_open_linked_pr(issue) {
            OpenPrProbe::Open(pr) => Some(pr),
            OpenPrProbe::NoneOpen | OpenPrProbe::ProbeFailed => None,
        }
    }
}
