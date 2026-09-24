//! Fleet broadcast of this host's token-pool exhaustion hold (Issue #8001).
//!
//! The publishing/consulting half of [`crate::work_finder::pool_preflight`]'s
//! hold: the pre-flight decides *whether* this host holds a pool, this module
//! tells the fleet and asks the fleet back.
//!
//! # Why a sibling module rather than more of `dispatch.rs`
//!
//! `sweep_registry::dispatch` is over the size ratchet
//! (`scripts/file-size-baseline.txt`), and this is a self-contained lane —
//! three methods over one field pair — so it lives beside it, matching the
//! `noop_cooldown` / `quarantine` / `decline_cooldown` split the same module
//! already uses.
//!
//! # What is broadcast, and what is deliberately not
//!
//! Only **edges** ([`crate::work_finder::pool_preflight::PoolHoldEdge`]), and
//! only keyed by
//! [`crate::tokens_pool::select::pool_account_fingerprint`] — an account-set
//! hash, never a directory path. The full argument for that key is on the
//! fingerprint function; the one-line version is that a path neither matches
//! across hosts that share a pool nor distinguishes hosts that merely resolve
//! the same path to different pools, and the second failure would silently
//! suppress a healthy peer.
//!
//! Fail-open everywhere: no publisher (`safehouse.enabled` false), a full or
//! closed channel, or simply no ad received yet all degrade byte-for-byte to
//! the pre-#8001 local-only pre-flight, which still stops this host on its own
//! next tick.

use super::*;

impl SweepRegistry {
    /// Publish a fleet-wide token-pool exhaustion hold arm/clear edge (Issue
    /// #8001) — the [`Self::publish_peer_cooldown_claim`] sibling for
    /// [`peer_claims::ClaimKind::PoolHoldArmed`]/
    /// [`peer_claims::ClaimKind::PoolHoldCleared`], reusing the same outbound
    /// channel and fail-open contract: a no-op without a publisher
    /// (`safehouse.enabled` false), and a full/closed channel drops the ad
    /// without blocking the work finder or the reaper.
    ///
    /// **Edge-triggered, not per tick.** `work_finder::pool_preflight` already
    /// computes exactly one arm edge and one clear edge per outage (that is
    /// what makes its "one hold log line" property hold), so this publishes
    /// once per edge rather than once per tick — a multi-hour pool outage
    /// costs two ads, not one per tick per root. A peer whose arm ad is
    /// dropped still converges via its own pre-flight on the very next tick:
    /// the broadcast removes doomed dispatches, it is never the only thing
    /// that can stop them.
    ///
    /// `pool_key` is a
    /// [`crate::tokens_pool::select::pool_account_fingerprint`], never a
    /// directory path — see that function and [`peer_claims::ClaimAd::pool_key`]
    /// for why a path would suppress healthy peers.
    pub(crate) fn publish_peer_pool_hold_claim(
        &self,
        kind: peer_claims::ClaimKind,
        pool_key: &str,
        remaining: Duration,
    ) {
        let Some(tx) = &self.peer_claim_publisher else {
            return;
        };
        let repo = peer_claims::repo_slug(&self.config.workspace_root);
        let host = host_identity();
        let pid = std::process::id();
        let ts = Utc::now().to_rfc3339();
        let ad = match kind {
            peer_claims::ClaimKind::PoolHoldArmed => ClaimAd::pool_hold_armed(
                repo,
                host,
                pid,
                ts,
                pool_key.to_owned(),
                remaining.as_secs(),
            ),
            peer_claims::ClaimKind::PoolHoldCleared => {
                ClaimAd::pool_hold_cleared(repo, host, pid, ts, pool_key.to_owned())
            }
            // Unreachable: every call site passes one of the two kinds above.
            _ => {
                log::warn!(
                    "sweep_registry: publish_peer_pool_hold_claim called with a non-pool-lane \
                     kind for pool {pool_key} — this is a pool-hold-only path (#8001); dropping"
                );
                return;
            }
        };
        if let Err(e) = tx.try_send(ad) {
            // Fail-open, mirroring `publish_peer_cooldown_claim`: the
            // fleet-wide broadcast is an optimization on top of the
            // still-correct per-host pre-flight hold, never a liveness
            // dependency.
            log::debug!(
                "sweep_registry: pool-hold advertisement for pool {pool_key} dropped ({e}); \
                 this host's own pool hold unaffected (#8001)"
            );
        }
    }

    /// Broadcast the arm/clear edge carried by a
    /// [`crate::work_finder::pool_preflight::PoolObservation`], if it crossed
    /// one (Issue #8001) — the reaper's one-liner over
    /// [`Self::publish_peer_pool_hold_claim`], so the post-mortem
    /// `note_pool_dead` path reaches the room on exactly the same terms as
    /// the work finder's pre-flight path.
    pub(crate) fn broadcast_pool_hold(
        &self,
        observation: crate::work_finder::pool_preflight::PoolObservation,
    ) {
        use crate::work_finder::pool_preflight::PoolHoldEdge;
        let Some(pool_key) = observation.pool_key.as_deref() else {
            return; // pool has no accounts — no identity to advertise
        };
        match observation.edge {
            Some(PoolHoldEdge::Armed { remaining }) => self.publish_peer_pool_hold_claim(
                peer_claims::ClaimKind::PoolHoldArmed,
                pool_key,
                remaining,
            ),
            Some(PoolHoldEdge::Cleared) => self.publish_peer_pool_hold_claim(
                peer_claims::ClaimKind::PoolHoldCleared,
                pool_key,
                Duration::from_secs(0),
            ),
            None => {}
        }
    }

    /// Whether a **peer** host currently advertises a live pool-exhaustion
    /// hold for `pool_key` (Issue #8001). `false` whenever peer-claim
    /// coordination is not wired up (`safehouse.enabled` false), which is
    /// what degrades this host byte-for-byte to the pre-#8001 local-only
    /// pre-flight.
    ///
    /// Returns `(held, peers)` so the caller's hold-edge log line can name
    /// which peers reported the pool dead without a second lock acquisition.
    #[must_use]
    pub(crate) fn peer_pool_hold(&self, pool_key: &str) -> (bool, Vec<String>) {
        let Some(view) = &self.peer_claims else {
            return (false, Vec::new());
        };
        let now = Instant::now();
        let read = |v: &peer_claims::PeerClaimView| {
            (v.pool_hold_held_by_peer_at(pool_key, now), v.pool_hold_peers_at(pool_key, now))
        };
        match view.lock() {
            Ok(v) => read(&v),
            Err(poisoned) => read(&poisoned.into_inner()),
        }
    }
}
