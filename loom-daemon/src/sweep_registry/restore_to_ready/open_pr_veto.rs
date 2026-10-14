//! Is the released issue already being worked through an open PR? (Issue #11852)
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
//! safe because the #4123 guard refuses re-dispatch. That premise is retired:
//! a queue label the dispatcher must refuse forever is not a resting state, it
//! is a candidate every tick pays to reject. The open PR belongs to the
//! review lane (Judge / Doctor / Champion), and the issue carries no queue
//! label while it does.
//!
//! # The rule
//!
//! **A release that would re-add `loom:issue` first asks whether an open PR is
//! linked to the issue; on a verified `Open(pr)` it removes `loom:building`
//! only.** It is the last term of the shared release veto in
//! [`restore_label_to_ready_checked`](SweepRegistry::restore_label_to_ready_checked),
//! so every release path gets it, and it is asked only when the re-add would
//! otherwise happen: a veto label, a PR, a closed issue or a never-ready claim
//! already decides the outcome without a probe.
//!
//! # Memo use is safe here
//!
//! The probe is [`SweepRegistry::probe_open_linked_pr`], which serves a fresh
//! verified-open memo (#6788) without a forge call. The W9 rule ("read live
//! when an `Open` answer *permits* a dispatch", see `refusal_memo`) does not
//! apply: here `Open` *prevents* a label write. A memo that is wrong (the PR
//! merged or closed inside the freshness window) costs one missing
//! `loom:issue` on an issue whose PR just left, not a duplicate build.
//!
//! # Fail-open contract
//!
//! Same family as the other carve-outs: only a verified `Open(pr)` skips the
//! re-add. `NoneOpen` and `ProbeFailed` restore as before, because a stranded
//! claim is the more common failure. Skipped entirely under `skip_label_flip`
//! (test fixtures without `gh` credentials), like every other forge probe on
//! the release path.

use super::*;

impl SweepRegistry {
    /// The open PR linked to `issue`, when one is verified open (Issue #11852).
    /// `None` for no open PR, a failed probe, or `skip_label_flip`.
    pub(super) fn release_open_linked_pr(&self, issue: u32) -> Option<u32> {
        if self.config.skip_label_flip {
            return None;
        }
        match self.probe_open_linked_pr(issue) {
            OpenPrProbe::Open(pr) => Some(pr),
            OpenPrProbe::NoneOpen | OpenPrProbe::ProbeFailed => None,
        }
    }
}
