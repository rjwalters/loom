//! The reaper's no-progress backstop predicate (#4366) and its progress
//! carve-out (#8439).
//!
//! Extracted from `reaper.rs` (which is at the file-size ratchet's threshold,
//! see `.loom/docs/file-size-policy.md`) so the predicate, the full rationale
//! for each of its arms, and its own regression tests sit together rather than
//! being spread across the 300-line `reap_once` match arm that consumes it.
//!
//! The consumer is the checkpoint-less clean-exit branch of `reap_once`. The
//! verdict feeds three things there, all off this one computation:
//!
//! 1. the insta-crash **quarantine tally** (`counted_failure = insta_crash ||
//!    no_progress`),
//! 2. the per-issue **dispatch backoff** arm (`insta_crash || no_progress ||
//!    yielded_open_pr`), and
//! 3. the `SweepExited` event's `no_progress` field and the terminal
//!    `sweep.outcome` record's telemetry `result`.

use super::OpenPrProbe;
use crate::sweep_registry::SweepRegistry;

impl SweepRegistry {
    /// Whether a checkpoint-less sweep exit made **no forward lifecycle
    /// progress at all** — the #4366 backstop.
    ///
    /// # The shape this targets
    ///
    /// A headless child that ends its turn parked on a monitored background
    /// task (e.g. "cache download is running... I'll pick this back up") exits
    /// 0 with NO checkpoint and NO forward lifecycle progress whatsoever. That
    /// shape is indistinguishable from the legitimate #3823b self-skip /
    /// no-work exit by exit code alone, so the predicate is conjunctive: clean
    /// exit AND no observed progress AND no open linked PR (excludes the #4123
    /// open-PR self-skip) AND the issue is still open (excludes a legitimate
    /// curator close-as-not-planned / already-done self-skip).
    ///
    /// # Progress exemption (#8439)
    ///
    /// Observed lifecycle progress exempts the exit from the backstop
    /// entirely, and is checked FIRST.
    ///
    /// The two forge arms ask "is there an open PR / is the issue still open
    /// RIGHT NOW", which is the wrong question for a productive sweep that
    /// merged a **partial-increment** PR: the PR it created is merged (so the
    /// probe correctly answers `NoneOpen`) and the issue stays open on purpose
    /// (a `Part of #N` slice does not close its parent), so both arms were
    /// satisfied and a genuinely productive exit-0 sweep was charged to the
    /// insta-crash quarantine tally — repeatedly, until the issue quarantined
    /// itself out of the queue despite every dispatch landing real work.
    ///
    /// The honest discriminator is what the daemon itself watched happen, not
    /// the forge's end state — see [`observed_lifecycle_progress`](Self::observed_lifecycle_progress).
    /// This can only ever REMOVE false positives: a sweep with no sampled
    /// phase history and no known PR — the actual zero-progress shape — still
    /// flags exactly as before. Ordering it first also spares a productive
    /// exit the `issue_is_closed_or_pr` forge round trip.
    ///
    /// # Fail-open contract
    ///
    /// Both forge probes are FAIL-OPEN, so each arm demands a POSITIVE verdict
    /// rather than accepting the "probe failed" state:
    ///
    /// - The issue-state arm demands `== Some(false)` ("the issue is verifiably
    ///   OPEN") rather than the weaker `!= Some(true)`: a timed-out /
    ///   rate-limited `gh` probe returns `None`, and `None != Some(true)` would
    ///   have been *satisfied*, turning a benign self-skip into a counted
    ///   failed attempt and wrongly quarantining an issue during a forge
    ///   outage.
    /// - The open-PR arm (#4452) demands a VERIFIED [`OpenPrProbe::NoneOpen`]
    ///   rather than the old `Option::is_none()`, which conflated "no open
    ///   linked PR" with "the PR probe itself failed". That conflation meant a
    ///   PARTIAL outage (PR probe fails while the issue probe answers OPEN)
    ///   could still false-positive; matching `NoneOpen` closes that gap — a
    ///   `ProbeFailed` yields `false`.
    ///
    /// Consequently a probe failure on EITHER arm — and a fortiori a full forge
    /// outage — yields `false` (the pre-#4366 behavior), so an outage can never
    /// manufacture quarantine pressure. `open_pr_probe` is `None` when the
    /// caller did not run the probe at all (`skip_label_flip`, or a non-zero
    /// exit), which likewise yields `false`.
    pub(crate) fn is_no_progress(
        &self,
        issue: u32,
        sweep_id: &str,
        open_pr_probe: Option<OpenPrProbe>,
    ) -> bool {
        !self.observed_lifecycle_progress(sweep_id)
            && open_pr_probe == Some(OpenPrProbe::NoneOpen)
            && self.issue_is_closed_or_pr(issue) == Some(false)
    }

    /// Whether the daemon itself observed this sweep make forward lifecycle
    /// progress (#8439) — a free, in-memory check, never a forge call.
    ///
    /// A sweep the reaper sampled through even one phase boundary
    /// (`curator-done`, `builder-done`, …) demonstrably did forward lifecycle
    /// work, which is the exact opposite of the shape the #4366 backstop
    /// targets ("ended its turn parked on a monitored background task", zero
    /// phases ever observed). `phase_history` is filled by
    /// [`sample_phase_transition`](SweepRegistry::sample_phase_transition) at
    /// the top of every reap tick from the sweep's own checkpoint, so this
    /// costs zero extra forge calls — it is state already in memory.
    ///
    /// The check is on a NON-EMPTY history, not on the mere presence of a map
    /// entry: an entry with nothing recorded in it is not evidence of
    /// anything, and keying on `contains_key` would silently disable the
    /// backstop for any sweep that ever touched the map.
    ///
    /// [`sampled_pr_number`](SweepRegistry::sampled_pr_number) is ORed in
    /// because a known PR is independently sufficient evidence of progress. It
    /// is today a strict refinement of the history check (it reads the same
    /// `phase_history`) and is further shadowed by the #8355 memo seed in the
    /// caller, which turns a sampled PR into an `Open(_)` probe verdict — but
    /// it is named explicitly so the "we knew this sweep's PR" exemption
    /// survives any future change to either of those two mechanisms.
    fn observed_lifecycle_progress(&self, sweep_id: &str) -> bool {
        self.phase_history
            .get(sweep_id)
            .is_some_and(|history| !history.is_empty())
            || self.sampled_pr_number(sweep_id).is_some()
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
