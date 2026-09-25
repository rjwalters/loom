//! PR-side park check for the reaper's #4256 resume decision (Issue #8689).
//!
//! Extracted from `reaper.rs` (which sits just under the file-size ratchet's
//! 1000-code-line threshold, see `.loom/docs/file-size-policy.md`) so the
//! predicate and its rationale live together instead of adding another arm to
//! the already-long `reap_once` match.
//!
//! # The shape this targets
//!
//! When `/loom:sweep` exhausts its Doctor-cycle cap it blocks the PR
//! (`loom:blocked` + `loom:changes-requested` on the **PR**) and deliberately
//! leaves the issue's checkpoint at `doctor-done`
//! (`sweep-wave-lifecycle.md` step 6, "leave the last checkpoint as-is") so a
//! later Champion-lifted PR can resume at Judge without re-burning a Doctor
//! cycle. That checkpoint write counts as `checkpoint_progress`, so the #5614
//! deterministic-no-op guard (which keys on a clean exit that changed
//! *nothing*) does not fire, and the #4256 resume path spawns one more child —
//! which re-loads the full sweep prompt prefix, re-verifies byte-identical
//! forge state, and exits 0 with nothing to do. One guaranteed no-op dispatch
//! (a full agent spawn plus a rotated token) per cap-exhausted block.
//!
//! The park is a fact already on the forge; this predicate reads it.
//!
//! # Why the issue-side guard does not already cover this
//!
//! `dispatch_inner` step 2.7 (#4444) refuses a dispatch whose **issue** wears
//! a park label. A cap-exhausted block parks the **PR**, which that guard
//! never looks at — so the resume dispatch clears every existing guard.
//!
//! # Why [`PARK_LABELS`](crate::work_finder::PARK_LABELS), and why not
//! `loom:operator`
//!
//! Reusing the same constant step 2.7 consults keeps the two sides of the same
//! decision from drifting, and it excludes `loom:operator` for exactly the
//! reason documented on
//! [`OPERATOR_HOLD_LABEL`](crate::work_finder::OPERATOR_HOLD_LABEL): the
//! merge-risk hold is re-evaluable by design and the routes that re-evaluate
//! held work — the watchdogs and this resume path — must keep reaching it. A
//! `loom:operator` PR whose sweep genuinely crashed still resumes.

use crate::sweep_registry::SweepRegistry;

impl SweepRegistry {
    /// Whether linked PR `pr` is parked (#8689) — logging the NOT-resuming
    /// decision when it is. See the module docs for why a parked PR must not
    /// be resumed and why this does not consume a resume attempt.
    pub(crate) fn linked_pr_parked(&self, issue: u32, pr: u32, phase: &Option<String>) -> bool {
        let Some(label) = self.linked_pr_park_label(pr) else {
            return false;
        };
        log::info!(
            "issue #{issue}: linked PR #{pr} carries `{label}` — a deliberate park (e.g. a \
             Doctor-cycle-cap block) that the #4444 issue-side guard cannot see; NOT resuming \
             at checkpoint phase {phase:?} (#8689). The issue is back at loom:issue with the \
             #4123 open-PR guard in force; the parked PR is the periodic Judge/Champion \
             roles' remit."
        );
        true
    }

    /// The first [`PARK_LABELS`](crate::work_finder::PARK_LABELS) entry
    /// carried by pull request `pr`, or `None` when it carries none — or when
    /// the labels could not be read.
    ///
    /// **Fails open**, mirroring the dispatch-side park guard (#4444) and the
    /// surrounding reaper probes: a `gh` error, timeout or unresolvable repo
    /// yields `None` and the resume proceeds, so a forge outage can never wedge
    /// the #4256 recovery path.
    ///
    /// Rides the REST issues endpoint (`repos/{owner}/{repo}/issues/{pr}`),
    /// which serves a pull request's labels too — the same transport, and the
    /// same separate-from-GraphQL rate-limit bucket, as
    /// [`first_park_label`](SweepRegistry::first_park_label).
    pub(crate) fn linked_pr_park_label(&self, pr: u32) -> Option<String> {
        let labels = self.current_labels_via_rest(pr)?;
        // `PARK_LABELS` order, not forge order, so a PR carrying both yields a
        // deterministic answer (matching `first_park_label`).
        crate::work_finder::PARK_LABELS
            .iter()
            .find(|park| labels.iter().any(|l| l == *park))
            .map(|park| (*park).to_string())
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    unused_imports
)]
mod tests;
