//! The fast-fail park counted fleet-wide, and a park that survives a restart
//! (Issue #11930).
//!
//! Two gaps kept the #11854 park from holding on a multi-host fleet:
//!
//! 1. **The streak was per host.** Only the ladder's window was broadcast, so
//!    an issue failing fast once on each of three hosts never reached the
//!    threshold anywhere. The ladder's `DispatchBackoffArmed` ad now carries
//!    the sender's own fast-failure streak, and the park compares the local
//!    streak **plus every peer's** against the threshold
//!    ([`SweepRegistry::fleet_fast_failure_streak`]; the receiving half and
//!    its rules are in `peer_claims::fast_fail_tally`).
//! 2. **The park was memory-only.** The startup reconciliation (#4110)
//!    released every marked `loom:blocked` on daemon start, so each worker
//!    restart in a roll undid every park. It now keeps a park younger than
//!    its TTL on the forge and hands it here, where the reaper rebuilds the
//!    in-memory quarantine from the marker's timestamp
//!    ([`SweepRegistry::adopt_restored_parks`]); the ordinary TTL expiry then
//!    releases it on schedule.
//!
//! Both are fail-open in the old direction: without a peer view the streak is
//! the local one, and a park the startup pass could not date is released as
//! before.

use super::*;

impl SweepRegistry {
    /// Publish the #4485 ladder's window for `issue`, carrying this host's own
    /// fast-failure streak (`0` under any other cause). Fail-open, like
    /// [`Self::publish_peer_cooldown_claim`].
    pub(crate) fn publish_dispatch_backoff_claim(
        &self,
        issue: u32,
        remaining: Duration,
        fast_fail_streak: u32,
    ) {
        let Some(tx) = &self.peer_claim_publisher else {
            return;
        };
        let ad = ClaimAd::dispatch_backoff_armed(
            issue,
            peer_claims::repo_slug(&self.config.workspace_root),
            host_identity(),
            std::process::id(),
            Utc::now().to_rfc3339(),
            remaining.as_secs(),
        )
        .with_fast_fail_streak(fast_fail_streak);
        if let Err(e) = tx.try_send(ad) {
            log::debug!(
                "sweep_registry: backoff advertisement for issue #{issue} dropped ({e}); local \
                 backoff and streak unaffected (#7477, #11930)"
            );
        }
    }

    /// The sum of every peer host's live fast-failure streak on `issue`
    /// (Issue #11930), read against this registry's ladder `max` (the local
    /// streak-cold rule). `0` without a peer view.
    #[must_use]
    pub(crate) fn peer_fast_failure_streak(&self, issue: u32) -> u32 {
        let Some(view) = &self.peer_claims else {
            return 0;
        };
        let repo = peer_claims::repo_slug(&self.config.workspace_root);
        let max_age = self.dispatch_backoff_config.max;
        let now = Instant::now();
        match view.lock() {
            Ok(v) => v.fast_fail_peer_streak_at(&repo, issue, now, max_age),
            Err(poisoned) => poisoned
                .into_inner()
                .fast_fail_peer_streak_at(&repo, issue, now, max_age),
        }
    }

    /// The fast-failure streak the park counts (Issue #11930): this host's own
    /// plus every peer's.
    #[must_use]
    pub(crate) fn fleet_fast_failure_streak(&self, issue: u32) -> u32 {
        self.fast_failure_streak(issue)
            .saturating_add(self.peer_fast_failure_streak(issue))
    }

    /// Break the fleet's fast-failure streak on `issue` (Issue #11930): it
    /// just made real progress, or an operator cleared its park. Forgets every
    /// peer's count here and, when there was anything to break, tells the
    /// peers to do the same by publishing a zero window with a zero streak
    /// (`peer_claims::fast_fail_tally` reads that as a break of **every**
    /// host's entry). Call it *before* the local backoff record is cleared,
    /// since that record is where this host's own streak lives.
    pub(crate) fn break_fleet_fast_fail_streak(&self, issue: u32) {
        let had_local = self.fast_failure_streak(issue) > 0;
        let had_peers = self.clear_peer_fast_fail_streaks(issue);
        if had_local || had_peers {
            self.publish_dispatch_backoff_claim(issue, Duration::ZERO, 0);
        }
    }

    /// Forget the peers' fast-failure streaks on `issue`. Returns whether any
    /// existed.
    fn clear_peer_fast_fail_streaks(&self, issue: u32) -> bool {
        let Some(view) = &self.peer_claims else {
            return false;
        };
        let repo = peer_claims::repo_slug(&self.config.workspace_root);
        match view.lock() {
            Ok(mut v) => v.clear_fast_fail_streaks(&repo, issue),
            Err(poisoned) => poisoned.into_inner().clear_fast_fail_streaks(&repo, issue),
        }
    }

    /// Rebuild the in-memory quarantine for every park the startup
    /// reconciliation kept on the forge for this workspace (Issue #11930).
    /// Called at the top of each reaper tick, before the TTL expiry, so a park
    /// whose TTL has since run out is released by the ordinary path on the
    /// same tick. An issue already quarantined here keeps its own entry.
    /// Returns how many parks were adopted.
    pub fn adopt_restored_parks(&mut self) -> usize {
        let restored = quarantine_reconciliation::restored::take(&self.config.workspace_root);
        let mut adopted = 0;
        for (issue, quarantined_at) in restored {
            if self.quarantined.contains_key(&issue) {
                continue;
            }
            // Served as a first-generation park: the forge does not record
            // the generation, and the startup pass dated it against the base
            // TTL.
            self.quarantine_generations.entry(issue).or_insert(1);
            self.quarantined.insert(issue, quarantined_at);
            adopted += 1;
            log::info!(
                "sweep_registry: issue #{issue} park restored from the forge (marked {}), held \
                 until its TTL instead of released on restart (#11930)",
                quarantined_at.to_rfc3339()
            );
        }
        adopted
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "fast_fail_fleet_tests.rs"]
mod tests;
