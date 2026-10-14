//! Is the released issue already being worked through an open PR? (Issue #11852)
//!
//! # Status: opt-in, default OFF
//!
//! The veto is gated by [`OPEN_PR_VETO_ENV`] (`LOOM_RELEASE_OPEN_PR_VETO`,
//! truthy = `1`/`true`/`yes`/`on`). Unset, release behaves exactly as before
//! this change. It ships dark because removing `loom:issue` is only half of a
//! lifecycle: the issue's remaining labels (`loom:curated`, `loom:architect`,
//! `loom:important`, ...) are still queue labels for the Champion and Curator
//! roles, which know nothing about an open linked PR, and nothing
//! deterministic re-queues the issue if that PR closes unmerged. Turning the
//! flag on is the follow-up's job, once those roles skip issues with a
//! verified open PR and a close handler restores `loom:issue`.
//!
//! # The bug
//!
//! A sweep that opened its PR and then died holding a checkpoint had its
//! claim released by the reaper, and the release re-added `loom:issue`. The
//! issue went back into the Builder queue beside its own open PR. The #4123
//! open-PR dispatch guard refused it, and the work finder's pre-filter dropped
//! it, on every tick for as long as the PR stayed open.
//!
//! # The rule (when enabled)
//!
//! **A release that would re-add `loom:issue` first asks whether an open PR is
//! linked to the issue; on a verified `Open(pr)` it removes `loom:building`
//! only.** It is the last term of the shared release veto in
//! [`restore_label_to_ready_checked`](SweepRegistry::restore_label_to_ready_checked),
//! so every release path gets it, and it is asked only when the re-add would
//! otherwise happen: a veto label, a PR, a closed issue or a never-ready claim
//! already decides the outcome without a probe. The flag is read last of all,
//! so a disabled veto costs no forge call.
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

/// Opt-in switch for the release veto. Default off (see the module doc).
pub const OPEN_PR_VETO_ENV: &str = "LOOM_RELEASE_OPEN_PR_VETO";

fn veto_enabled() -> bool {
    std::env::var(OPEN_PR_VETO_ENV).is_ok_and(|v| {
        matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    })
}

/// Test helper: enable the veto until the returned guard drops. Callers are
/// `#[serial]`, so the process-wide env write cannot race.
#[cfg(test)]
pub(crate) struct VetoEnabled;

#[cfg(test)]
impl VetoEnabled {
    pub(crate) fn new() -> Self {
        std::env::set_var(OPEN_PR_VETO_ENV, "1");
        Self
    }
}

#[cfg(test)]
impl Drop for VetoEnabled {
    fn drop(&mut self) {
        std::env::remove_var(OPEN_PR_VETO_ENV);
    }
}

impl SweepRegistry {
    /// The open PR linked to `issue`, when one is verified open (Issue #11852).
    /// `None` for no open PR, a failed probe, `skip_label_flip`, or the veto
    /// being disabled (the default).
    pub(super) fn release_open_linked_pr(&self, issue: u32) -> Option<u32> {
        if self.config.skip_label_flip || !veto_enabled() {
            return None;
        }
        match self.probe_open_linked_pr(issue) {
            OpenPrProbe::Open(pr) => Some(pr),
            OpenPrProbe::NoneOpen | OpenPrProbe::ProbeFailed => None,
        }
    }
}
