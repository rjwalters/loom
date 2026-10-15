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

/// The shortest window an ordinary ladder ad advertises (#11952). One second
/// costs a peer nothing, and it keeps the ad from reading as a streak break to
/// a peer that predates the explicit marker.
pub(crate) const MIN_LADDER_AD_WINDOW: Duration = Duration::from_secs(1);

/// Whether a configured dispatch-backoff base is usable (#11952). A zero base
/// arms zero-length windows, which a peer older than the explicit break marker
/// reads as a fleet-wide fast-fail streak break, so it is refused, loudly.
pub(super) fn positive_base_secs(secs: u64, source: &str) -> bool {
    if secs == 0 {
        log::warn!(
            "sweep_registry: {source}=0 ignored — the dispatch backoff base must be positive \
             (#11952)"
        );
    }
    secs > 0
}

impl SweepRegistry {
    /// Publish the #4485 ladder's window for `issue`, carrying this host's own
    /// fast-failure streak (`0` under any other cause). Fail-open, like
    /// [`Self::publish_peer_cooldown_claim`].
    ///
    /// The window advertised is never shorter than
    /// [`MIN_LADDER_AD_WINDOW`] (#11952): a zero window with a zero streak is
    /// what a pre-#11952 peer reads as a fleet-wide streak break, so an
    /// ordinary failure must never send one, even with a zero backoff `base`.
    pub(crate) fn publish_dispatch_backoff_claim(
        &self,
        issue: u32,
        remaining: Duration,
        fast_fail_streak: u32,
    ) {
        let ad = self
            .dispatch_backoff_ad(issue, remaining.max(MIN_LADDER_AD_WINDOW))
            .with_fast_fail_streak(fast_fail_streak);
        self.send_dispatch_backoff_ad(issue, ad);
    }

    /// Publish a fast-fail streak **break** for `issue` (#11930, marked as
    /// one since #11952). See `peer_claims::fast_fail_tally`.
    fn publish_fast_fail_break(&self, issue: u32) {
        let ad = self
            .dispatch_backoff_ad(issue, Duration::ZERO)
            .as_fast_fail_break();
        self.send_dispatch_backoff_ad(issue, ad);
    }

    fn dispatch_backoff_ad(&self, issue: u32, remaining: Duration) -> ClaimAd {
        ClaimAd::dispatch_backoff_armed(
            issue,
            peer_claims::repo_slug(&self.config.workspace_root),
            host_identity(),
            std::process::id(),
            Utc::now().to_rfc3339(),
            remaining.as_secs(),
        )
    }

    fn send_dispatch_backoff_ad(&self, issue: u32, ad: ClaimAd) {
        let Some(tx) = &self.peer_claim_publisher else {
            return;
        };
        if let Err(e) = tx.try_send(ad) {
            log::debug!(
                "sweep_registry: backoff advertisement for issue #{issue} dropped ({e}); local \
                 backoff and streak unaffected (#7477, #11930)"
            );
        }
    }

    /// Whether an event of `cause` at `now` extends `prev`'s backoff streak
    /// for `issue`: [`backoff_cause::continues`], unless a peer's fast-fail
    /// break has reset it since (#11952).
    pub(super) fn backoff_streak_continues(
        &self,
        issue: u32,
        prev: &DispatchBackoffState,
        cause: DispatchBackoffCause,
        now: DateTime<Utc>,
        max_secs: i64,
    ) -> bool {
        dispatch::backoff_cause::continues(prev, cause, now, max_secs)
            && !self.peer_broke_fast_fail_streak(issue, prev)
    }

    /// Whether a peer's fast-fail streak break for `issue` arrived at or after
    /// `state` was recorded (#11952), so this host's own fast-failure streak
    /// in `state` is broken too. Only a [`DispatchBackoffCause::FastFailure`]
    /// record is affected: the break is about that streak alone, and every
    /// other cause's window and count are left as they are. `false` without a
    /// peer view.
    pub(super) fn peer_broke_fast_fail_streak(
        &self,
        issue: u32,
        state: &DispatchBackoffState,
    ) -> bool {
        if state.cause != DispatchBackoffCause::FastFailure {
            return false;
        }
        let Some(view) = &self.peer_claims else {
            return false;
        };
        let repo = peer_claims::repo_slug(&self.config.workspace_root);
        let broke_at = view
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .fast_fail_break_at(&repo, issue);
        broke_at.is_some_and(|at| state.last_failure_at <= at)
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
    /// peers to do the same by publishing a break
    /// ([`ClaimAd::as_fast_fail_break`]). A receiver drops **every** host's
    /// entry and resets its own local streak, and leaves its windows alone
    /// (`peer_claims::fast_fail_tally`). Call it *before* the local backoff
    /// record is cleared, since that record is where this host's own streak
    /// lives.
    pub(crate) fn break_fleet_fast_fail_streak(&self, issue: u32) {
        let had_local = self.fast_failure_streak(issue) > 0;
        let had_peers = self.clear_peer_fast_fail_streaks(issue);
        if had_local || had_peers {
            self.publish_fast_fail_break(issue);
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
