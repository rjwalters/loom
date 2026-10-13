//! The quarantine-**release** retry path (Issue #4110) and its two #8953
//! backstops: a shared-rate-limit-breaker check that skips (without counting)
//! while the shared GitHub API quota is exhausted, and a bounded per-issue
//! attempt ceiling so one permanently-failing `loom:blocked` -> `loom:issue`
//! label edit cannot re-fire on every 30s reaper tick forever (the `#127`
//! case: 68 retries over ~34 minutes).
//!
//! A sibling file rather than more `impl SweepRegistry` in `quarantine.rs`:
//! that file is already over the file-size ratchet's threshold and therefore
//! frozen at its current size (`.loom/docs/file-size-policy.md`), the same
//! reason `provider_health_feedback.rs` was split out of it.
//!
//! The `gh` call these wrap — `SweepRegistry::release_quarantine_label` —
//! stays in `quarantine.rs` alongside the other label-flip glue; only the
//! retry policy lives here.

use super::*;

impl SweepRegistry {
    /// Whether the quarantine release may re-add `loom:issue` (#11480): only
    /// when nothing but its own `loom:blocked` holds the item, per the shared
    /// [`crate::work_finder::restore_veto`]. loom-ui#1950 is the case: a
    /// `loom:needs-capability` pre-flight skip counted as an insta-crash, the
    /// quarantine's TTL release re-armed `loom:issue` beside that veto, and the
    /// item was re-dispatched 31 times in 72 h. One REST read, fail-open (a
    /// failed read sees no labels and the release proceeds as before).
    ///
    /// #11517: nor onto a CLOSED issue, nor onto one that is claimed right now
    /// (`loom:building`): loom-ui#2980 gained `loom:issue` 40 s after a fresh
    /// claim, beside the live `loom:building`, from this release.
    pub(crate) fn quarantine_release_restores_ready(&self, issue: u32) -> bool {
        let signals = self.release_issue_signals(issue);
        let labels = signals.labels;
        let claimed = labels
            .iter()
            .any(|l| l == crate::work_finder::BUILDING_LABEL);
        if signals.closed == Some(true) || claimed {
            log::info!(
                "sweep_registry: quarantine release for #{issue}: the issue is {} — removing \
                 the quarantine's `loom:blocked` only, NOT re-adding `loom:issue` (#11517)",
                if claimed {
                    "claimed (`loom:building`)"
                } else {
                    "closed"
                }
            );
            return false;
        }
        let Some(veto) = crate::work_finder::restore_veto(&labels, &["loom:blocked"]) else {
            return true;
        };
        log::info!(
            "sweep_registry: quarantine release for #{issue} found `{veto}` — removing the \
             quarantine's `loom:blocked` only, NOT re-adding `loom:issue` (#11480)"
        );
        false
    }

    /// Whether the quarantine-release retry path should treat the shared
    /// GitHub API rate limit as currently exhausted (Issue #8953). Production
    /// consults the real process-global breaker
    /// ([`crate::rate_limit_breaker::global_is_suppressed`]); the `#[cfg(test)]`
    /// build additionally honors `test_force_rate_limited` so tests can
    /// exercise the suppression path without registering the real breaker's
    /// `GLOBAL` handle (a `OnceLock` shared by the whole test binary — see
    /// that field's doc comment for why that would leak into other tests).
    #[cfg(test)]
    fn quarantine_release_rate_limited(&self) -> bool {
        self.test_force_rate_limited
            || crate::rate_limit_breaker::global_skip_pass("quarantine_release")
    }

    /// Non-test build of [`Self::quarantine_release_rate_limited`] above —
    /// same contract, minus the test-only override.
    #[cfg(not(test))]
    fn quarantine_release_rate_limited(&self) -> bool {
        crate::rate_limit_breaker::global_skip_pass("quarantine_release")
    }

    /// One reaper-tick pass over every label-restoring hold: retry failed
    /// quarantine restores, then release no-op holds whose inputs changed
    /// (Issue #10156). Lives here so `reaper.rs`, frozen by the file-size
    /// ratchet, gains no line for it.
    pub(crate) fn release_pending_holds(&mut self) {
        self.retry_pending_quarantine_releases();
        self.reconcile_noop_holds(std::time::Instant::now());
    }

    /// Retry every issue in [`pending_quarantine_release`](Self::pending_quarantine_release_issues)
    /// (Issue #4110). Called every [`reap_once`](Self::reap_once) tick, right
    /// after [`expire_quarantine`](Self::expire_quarantine): a previously
    /// failed `loom:blocked` -> `loom:issue` restore (transient `gh` failure or
    /// timeout, #3973) is retried here until it succeeds, instead of leaving
    /// the issue permanently stranded. Cheap early-return when nothing is
    /// pending. Idempotent — re-running the flip on an issue that a human
    /// already restored by hand is a harmless no-op `gh` call.
    ///
    /// Consults the shared rate-limit breaker BEFORE looping (Issue #8953):
    /// while suppressed, every entry here would be a doomed `gh` call against
    /// an already-exhausted quota — and, worse, this path retrying every 30s
    /// reaper tick regardless is exactly what let one permanently-failing
    /// entry (the `#127` case) keep re-burning that same quota tick after
    /// tick, all the way through what should have been a cooldown window.
    /// [`Self::attempt_quarantine_release`] carries the identical check as a
    /// backstop for its other caller ([`Self::clear_quarantine`]'s immediate,
    /// operator-driven release attempt), so this outer check is purely a
    /// per-tick fast path, not the only enforcement point.
    pub(crate) fn retry_pending_quarantine_releases(&mut self) {
        if self.pending_quarantine_release.is_empty() {
            return;
        }
        if self.quarantine_release_rate_limited() {
            log::debug!(
                "sweep_registry: quarantine-release retry pass skipped — shared GitHub API rate \
                 limit exhausted (#8953); {} issue(s) remain pending",
                self.pending_quarantine_release.len()
            );
            return;
        }
        let pending: Vec<u32> = self.pending_quarantine_release.iter().copied().collect();
        for issue in pending {
            self.attempt_quarantine_release(issue);
        }
    }

    /// Attempt the `loom:blocked` -> `loom:issue` label restore for `issue`
    /// (Issue #4110). On success, clears any pending-retry record and its
    /// attempt tally. On failure, records `issue` in
    /// [`pending_quarantine_release`](Self::pending_quarantine_release_issues)
    /// (if not already there), increments its consecutive-failure tally, and
    /// logs at `warn` — a silent strand is the defect this exists to prevent,
    /// so the failure must be visible above the default log level.
    ///
    /// Two backstops added by Issue #8953, both ahead of the `gh` call:
    /// - **Rate-limit awareness**: while
    ///   [`Self::quarantine_release_rate_limited`] reports suppressed, this
    ///   skips the attempt entirely — no `gh` call, no tally increment, no
    ///   ceiling consumed — leaving `issue` pending exactly as it was, for a
    ///   clean retry once the shared quota's cooldown window clears.
    /// - **Retry ceiling**: once the tally reaches
    ///   [`QuarantineConfig::max_release_attempts`], this stops retrying the
    ///   issue (removed from both the pending set and the tally) and logs a
    ///   single `error` instead — a permanently-failing label edit (the
    ///   `#127` case) no longer retries forever at full reaper-tick cadence.
    pub(crate) fn attempt_quarantine_release(&mut self, issue: u32) {
        if self.quarantine_release_rate_limited() {
            log::debug!(
                "sweep_registry: quarantine release for #{issue} skipped — shared GitHub API \
                 rate limit exhausted (#8953); remains pending"
            );
            self.pending_quarantine_release.insert(issue);
            return;
        }
        if self.release_quarantine_label(issue) {
            self.pending_quarantine_release.remove(&issue);
            self.quarantine_release_attempts.remove(&issue);
        } else {
            let first_attempt = self.pending_quarantine_release.insert(issue);
            let attempts = {
                let counter = self.quarantine_release_attempts.entry(issue).or_insert(0);
                *counter += 1;
                *counter
            };
            let ceiling = self.quarantine_config.max_release_attempts;
            if attempts >= ceiling {
                self.pending_quarantine_release.remove(&issue);
                self.quarantine_release_attempts.remove(&issue);
                log::error!(
                    "sweep_registry: quarantine release for #{issue} failed {attempts} \
                     consecutive time(s) (ceiling {ceiling}) — giving up automatic retry; \
                     `loom:blocked` may remain stranded on the forge until a human intervenes \
                     (#8953)"
                );
            } else {
                log::warn!(
                    "sweep_registry: quarantine release for #{issue} failed (attempt \
                     {attempts}/{ceiling}) — `loom:blocked` may remain stranded on the forge; \
                     retrying on the next reaper tick (#4110){}",
                    if first_attempt {
                        ""
                    } else {
                        " (repeated failure)"
                    }
                );
            }
        }
    }
}

#[cfg(test)]
#[path = "quarantine_release_retry_tests.rs"]
mod release_retry_tests;
