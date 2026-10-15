//! The registry half of the fleet-wide no-op streak (#12024).
//!
//! The no-op hold's streak used to be per host, so a loop spread across six
//! dispatchers never reached the threshold on any one of them. Every counted
//! no-op now publishes this host's own count against the fingerprint digest
//! it was counted on ([`ClaimAd::noop_streak`]), and the hold compares the
//! local count **plus every peer's on the same fingerprint** with the
//! threshold. Parking or releasing the hold publishes a break that resets the
//! streak on every host. The receive side and its rules are in
//! `peer_claims::noop_streak_tally`; this is #11930's fast-fail shape
//! (`sweep_registry::fast_fail_fleet`), reused.
//!
//! Fail-open in the old direction: with no peer view or publisher attached
//! (`safehouse.enabled` false) the streak is this host's alone, as before.

use super::*;

impl SweepRegistry {
    fn noop_streak_ad(&self, issue: u32, count: u32, fingerprint: &str) -> ClaimAd {
        ClaimAd::noop_streak(
            issue,
            peer_claims::repo_slug(&self.config.workspace_root),
            host_identity(),
            std::process::id(),
            Utc::now().to_rfc3339(),
            count,
            fingerprint.to_string(),
        )
    }

    fn send_noop_streak_ad(&self, issue: u32, ad: ClaimAd) {
        let Some(tx) = &self.peer_claim_publisher else {
            return;
        };
        if let Err(e) = tx.try_send(ad) {
            log::debug!(
                "sweep_registry: no-op streak advertisement for issue #{issue} dropped ({e}); \
                 the local streak is unaffected (#12024)"
            );
        }
    }

    /// Tell the peers this host's own no-op count on `issue`, taken against
    /// the fingerprint digest `fingerprint`.
    pub(super) fn publish_noop_streak(&self, issue: u32, count: u32, fingerprint: &str) {
        let ad = self.noop_streak_ad(issue, count, fingerprint);
        self.send_noop_streak_ad(issue, ad);
    }

    /// Reset the fleet's no-op streak on `issue`: it was just parked or
    /// released. Forgets the peers' counts here and tells them to do the same.
    pub(super) fn break_fleet_noop_streak(&self, issue: u32) {
        if let Some(view) = &self.peer_claims {
            let repo = peer_claims::repo_slug(&self.config.workspace_root);
            view.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear_noop_streaks(&repo, issue);
        }
        let ad = self.noop_streak_ad(issue, 0, "").as_noop_streak_break();
        self.send_noop_streak_ad(issue, ad);
    }

    /// The sum of every peer's no-op count on `issue` taken against
    /// `fingerprint`. `0` without a peer view.
    #[must_use]
    pub(super) fn peer_noop_streak(&self, issue: u32, fingerprint: &str) -> u32 {
        let Some(view) = &self.peer_claims else {
            return 0;
        };
        let repo = peer_claims::repo_slug(&self.config.workspace_root);
        view.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .noop_peer_streak_at(&repo, issue, fingerprint, Instant::now())
    }

    /// Whether a peer's break for `issue` arrived at or after `counted_at`,
    /// so this host's own streak counted then is broken too. `false` without
    /// a peer view.
    #[must_use]
    pub(super) fn peer_broke_noop_streak(&self, issue: u32, counted_at: DateTime<Utc>) -> bool {
        let Some(view) = &self.peer_claims else {
            return false;
        };
        let repo = peer_claims::repo_slug(&self.config.workspace_root);
        view.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .noop_streak_break_at(&repo, issue)
            .is_some_and(|at| counted_at <= at)
    }
}
