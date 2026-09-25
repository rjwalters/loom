//! The fleet-visible **brake lanes** of the peer-claim room.
//!
//! A *claim* ad (`Advertise`/`Retract`, [`super`]'s own subject) answers "is a
//! sweep in flight on issue #N". The three kinds routed here answer a
//! different question — "may a dispatch happen at all right now" — and none of
//! them claims anything:
//!
//! | Lane | Kinds | Scope | Issue |
//! |------|-------|-------|-------|
//! | No-op cooldown | [`ClaimKind::NoopCooldownArmed`] | one issue | #7477 |
//! | Dispatch backoff | [`ClaimKind::DispatchBackoffArmed`] | one issue | #7477 |
//! | Pool-exhaustion hold | [`ClaimKind::PoolHoldArmed`]/[`ClaimKind::PoolHoldCleared`] | one **token pool**, every issue on it | #8001 |
//!
//! They share three properties, which is why they share a module:
//!
//! 1. **Each folds into its own single-purpose map**, never the `claims` map —
//!    folding a brake in there would manufacture a phantom in-flight sweep.
//! 2. **TTL is measured against LOCAL receipt**, never the advertiser's clock
//!    (see [`super`]'s module doc — wall clocks are not comparable across
//!    hosts).
//! 3. **None touches the `#6157` coordination-health counters.** That verdict
//!    answers "is *dispatch* coordination healthy"; brake traffic is not
//!    dispatch traffic, so it must never manufacture a false recovery.
//!
//! This module exists as a sibling of `peer_claims.rs` rather than inside it
//! for the reason `repo_slug_tests`/`coordination_tests` do: that module is
//! over the size ratchet (`scripts/file-size-baseline.txt`) and has no line
//! budget for a fourth lane.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use super::{ClaimAd, ClaimKind, PeerClaimView};

/// The `issue` value carried by [`ClaimKind::PoolHoldArmed`]/
/// [`ClaimKind::PoolHoldCleared`] ads (Issue #8001). A pool hold is
/// deliberately **not** about any one issue — attributing a pool-wide fault to
/// whichever issue happened to be dispatched into it is precisely what #7708
/// rejected — so the field is pinned to a sentinel exactly as
/// [`super::FILING_LOCK_SENTINEL_ISSUE`] is, keeping the wire shape uniform
/// and keeping a pool ad from ever reading as a claim on a real issue #0.
pub const POOL_HOLD_SENTINEL_ISSUE: u32 = 0;

/// Hard ceiling on the TTL a single [`ClaimKind::PoolHoldArmed`] ad may
/// install in a receiver's view (Issue #8001), regardless of the
/// `remaining_secs` it advertises.
///
/// Matches `tokens_pool::select`'s own `POOL_CLEAR_ESTIMATE_CAP_SECS` (900 s),
/// the cap the *arming* side's `pool_clear_estimate` already applies, so a
/// well-behaved ad is never clamped. The ceiling exists for the ill-behaved
/// one: the room is shared, this ad suppresses **all** dispatch for a pool
/// rather than one issue, and a single ad advertising a year of remaining hold
/// must not be able to wedge a peer's whole fleet lane. Bounded here so the
/// worst case degrades to "one extra hold window", never to a stall that
/// outlives the outage.
pub const MAX_PEER_POOL_HOLD_TTL: Duration = Duration::from_secs(900);

/// Route one inbound brake-lane ad into `view` and prune that lane's lapsed
/// entries (Issue #8001).
///
/// The single entry point [`crate::safehouse::PeerClaimSink`] calls for every
/// kind satisfying [`ClaimKind::is_cooldown_lane`] or
/// [`ClaimKind::is_pool_hold_lane`] — the router lives beside the lanes it
/// routes rather than being spelled out again in the socket layer, so adding a
/// lane is a change in exactly one place. A kind outside both lanes is a
/// no-op.
pub fn observe_brake_ad(view: &mut PeerClaimView, ad: &ClaimAd, now: Instant) {
    match ad.kind {
        ClaimKind::NoopCooldownArmed => {
            view.observe_noop_cooldown_at(ad, now);
            view.prune_expired_noop_cooldowns(now);
        }
        ClaimKind::DispatchBackoffArmed => {
            view.observe_dispatch_backoff_at(ad, now);
            view.prune_expired_dispatch_backoffs(now);
        }
        ClaimKind::PoolHoldArmed | ClaimKind::PoolHoldCleared => {
            view.observe_pool_hold_at(ad, now);
            view.prune_expired_pool_holds(now);
        }
        _ => {}
    }
}

impl ClaimAd {
    /// "The token pool `pool_key` is UNSPAWNABLE on my host, `remaining_secs`
    /// seconds left on my hold as of my send time" (Issue #8001). Broadcast
    /// by [`crate::sweep_registry::SweepRegistry::publish_peer_pool_hold_claim`]
    /// on the *arming edge* of
    /// [`crate::work_finder::pool_preflight::PoolHoldState`].
    ///
    /// `issue` is pinned to [`POOL_HOLD_SENTINEL_ISSUE`] — a pool hold covers
    /// every issue at once, so naming one would be a category error.
    #[must_use]
    pub fn pool_hold_armed(
        repo: String,
        host: String,
        pid: u32,
        ts: String,
        pool_key: String,
        remaining_secs: u64,
    ) -> Self {
        Self {
            kind: ClaimKind::PoolHoldArmed,
            issue: POOL_HOLD_SENTINEL_ISSUE,
            repo,
            host,
            pid,
            ts,
            pr: None,
            remaining_secs: Some(remaining_secs),
            pool_key: Some(pool_key),
        }
    }

    /// "The token pool `pool_key` RECOVERED on my host" (Issue #8001) — the
    /// early release of a [`Self::pool_hold_armed`] hold, before its TTL
    /// would lapse. See [`ClaimKind::PoolHoldCleared`] for why this lane has
    /// an explicit clear where the #7477 cooldown lane does not.
    #[must_use]
    pub fn pool_hold_cleared(
        repo: String,
        host: String,
        pid: u32,
        ts: String,
        pool_key: String,
    ) -> Self {
        Self {
            kind: ClaimKind::PoolHoldCleared,
            issue: POOL_HOLD_SENTINEL_ISSUE,
            repo,
            host,
            pid,
            ts,
            pr: None,
            remaining_secs: None,
            pool_key: Some(pool_key),
        }
    }
}

impl PeerClaimView {
    // ------------------------------------------------------------------
    // Fleet-wide no-op-cooldown / dispatch-backoff visibility (Issue #7477)
    // ------------------------------------------------------------------

    /// Observe an inbound [`ClaimKind::NoopCooldownArmed`] ad at local time
    /// `now`: "peer host H armed a no-op cooldown on issue #N in `repo`,
    /// `remaining_secs` seconds left as of H's send time". The local expiry
    /// is computed as `now + remaining_secs` — the received-at-based TTL
    /// discipline every other map in this module uses, never the
    /// advertiser's wall clock.
    ///
    /// Returns `true` when applied (a peer's), `false` when ignored as this
    /// host's own ad — the identical self-claim recognition
    /// [`Self::observe_at`] applies, including the `UNKNOWN_HOST` carve-out
    /// (see that method's doc comment): a host must back off on its own
    /// no-op-cooldown ad exactly as readily as on a peer's, so an
    /// unresolved-identity self-ad is still treated as a peer's here.
    ///
    /// A missing/zero `remaining_secs` (a malformed or already-expired ad)
    /// degrades to "already expired" — harmless, since a subsequent read
    /// simply finds nothing there rather than a bogus indefinite hold.
    ///
    /// Deliberately does **not** touch `counters`/`last_received_at`/
    /// `coordination_degraded` — same contract as
    /// [`Self::observe_completion_at`] and [`Self::observe_filing_lock_at`]:
    /// the `#6157` verdict answers "is *dispatch* coordination healthy", and
    /// a cooldown-lane ad is not dispatch traffic, so it must never
    /// manufacture a false recovery out of the cooldown lane alone.
    pub fn observe_noop_cooldown_at(&mut self, ad: &ClaimAd, now: Instant) -> bool {
        debug_assert_eq!(ad.kind, ClaimKind::NoopCooldownArmed);
        let is_unresolved_identity = ad.host == crate::sweep_registry::UNKNOWN_HOST;
        if ad.host == self.self_host && !is_unresolved_identity {
            return false; // never back off on our own cooldown
        }
        let remaining = ad.remaining_secs.unwrap_or(0);
        let expiry = now + Duration::from_secs(remaining);
        self.noop_cooldowns
            .insert((ad.repo.clone(), ad.issue), expiry);
        true
    }

    /// [`Self::observe_noop_cooldown_at`]'s sibling for
    /// [`ClaimKind::DispatchBackoffArmed`] (Issue #7477) — identical
    /// contract, including leaving the `#6157` coordination-health
    /// bookkeeping untouched.
    pub fn observe_dispatch_backoff_at(&mut self, ad: &ClaimAd, now: Instant) -> bool {
        debug_assert_eq!(ad.kind, ClaimKind::DispatchBackoffArmed);
        let is_unresolved_identity = ad.host == crate::sweep_registry::UNKNOWN_HOST;
        if ad.host == self.self_host && !is_unresolved_identity {
            return false; // never back off on our own backoff
        }
        let remaining = ad.remaining_secs.unwrap_or(0);
        let expiry = now + Duration::from_secs(remaining);
        self.dispatch_backoffs
            .insert((ad.repo.clone(), ad.issue), expiry);
        true
    }

    /// Every issue in `repo` with a live (non-expired) fleet-wide no-op
    /// cooldown at local time `now` (Issue #7477) — unioned into
    /// [`crate::sweep_registry::SweepRegistry::noop_cooldown_issues`] so a
    /// peer's self-reported "no actionable delta" suppresses re-dispatch
    /// fleet-wide, not just on the host that recorded it.
    #[must_use]
    pub fn noop_cooldown_issues_at(&self, repo: &str, now: Instant) -> HashSet<u32> {
        self.noop_cooldowns
            .iter()
            .filter(|((r, _), expiry)| r == repo && **expiry > now)
            .map(|((_, issue), _)| *issue)
            .collect()
    }

    /// [`Self::noop_cooldown_issues_at`]'s sibling for dispatch backoff
    /// (Issue #7477).
    #[must_use]
    pub fn dispatch_backoff_issues_at(&self, repo: &str, now: Instant) -> HashSet<u32> {
        self.dispatch_backoffs
            .iter()
            .filter(|((r, _), expiry)| r == repo && **expiry > now)
            .map(|((_, issue), _)| *issue)
            .collect()
    }

    /// Drop every expired `noop_cooldowns` entry at local time `now` (Issue
    /// #7477). Called opportunistically so the map does not grow without
    /// bound from stale peer ads.
    pub fn prune_expired_noop_cooldowns(&mut self, now: Instant) {
        self.noop_cooldowns.retain(|_, expiry| *expiry > now);
    }

    /// [`Self::prune_expired_noop_cooldowns`]'s sibling for dispatch backoff
    /// (Issue #7477).
    pub fn prune_expired_dispatch_backoffs(&mut self, now: Instant) {
        self.dispatch_backoffs.retain(|_, expiry| *expiry > now);
    }

    // ------------------------------------------------------------------
    // Fleet-wide token-pool exhaustion holds (Issue #8001)
    // ------------------------------------------------------------------

    /// Observe an inbound [`ClaimKind::PoolHoldArmed`]/
    /// [`ClaimKind::PoolHoldCleared`] ad at local time `now`: "peer host H
    /// armed (or released) a pool-exhaustion hold on the pool identified by
    /// `ad.pool_key`". The local expiry is computed as
    /// `now + min(remaining_secs, MAX_PEER_POOL_HOLD_TTL)` — the
    /// received-at-based TTL discipline every other map in this module uses,
    /// never the advertiser's wall clock.
    ///
    /// Returns `true` when applied, `false` when dropped. An ad is dropped
    /// when it is this host's own (the identical self-claim recognition
    /// [`Self::observe_at`] applies, `UNKNOWN_HOST` carve-out included) or
    /// when it names **no pool** (`pool_key: None` — a pre-#8001 peer or a
    /// malformed payload). Dropping a keyless ad is the safe direction: a
    /// hold that cannot say which pool it covers must never suppress an
    /// arbitrary one.
    ///
    /// A [`ClaimKind::PoolHoldCleared`] releases only the entry advertised by
    /// **that same host** — a peer may release its own hold early, never a
    /// third host's (the `filing_unlock` rule, for the same reason).
    ///
    /// Deliberately does **not** touch `counters`/`last_received_at`/
    /// `coordination_degraded` — same contract as
    /// [`Self::observe_noop_cooldown_at`]: the `#6157` verdict answers "is
    /// *dispatch* coordination healthy", and a pool-lane ad is not dispatch
    /// traffic.
    pub fn observe_pool_hold_at(&mut self, ad: &ClaimAd, now: Instant) -> bool {
        debug_assert!(ad.kind.is_pool_hold_lane());
        let is_unresolved_identity = ad.host == crate::sweep_registry::UNKNOWN_HOST;
        if ad.host == self.self_host && !is_unresolved_identity {
            return false; // never hold on our own pool advertisement
        }
        let Some(pool_key) = ad.pool_key.clone() else {
            return false; // names no pool — see the doc comment
        };
        let key = (pool_key, ad.host.clone());
        match ad.kind {
            ClaimKind::PoolHoldArmed => {
                let remaining =
                    Duration::from_secs(ad.remaining_secs.unwrap_or(0)).min(MAX_PEER_POOL_HOLD_TTL);
                self.pool_holds.insert(key, now + remaining);
                true
            }
            ClaimKind::PoolHoldCleared => self.pool_holds.remove(&key).is_some(),
            _ => false,
        }
    }

    /// Whether any **peer** currently holds the pool identified by `pool_key`
    /// at local time `now` (Issue #8001) — the read side
    /// `work_finder::pool_preflight::preflight_held_per_root` consults
    /// alongside its own local `observe_root` verdict.
    #[must_use]
    pub fn pool_hold_held_by_peer_at(&self, pool_key: &str, now: Instant) -> bool {
        self.pool_holds
            .iter()
            .any(|((key, _), expiry)| key == pool_key && *expiry > now)
    }

    /// Which peer hosts currently hold `pool_key` at local time `now` (Issue
    /// #8001), sorted for a stable log line. Diagnostic companion to
    /// [`Self::pool_hold_held_by_peer_at`] — the hold-edge WARN names them so
    /// an operator can tell "peer host X says this pool is dead" apart from
    /// "my own pre-flight says so".
    #[must_use]
    pub fn pool_hold_peers_at(&self, pool_key: &str, now: Instant) -> Vec<String> {
        let mut hosts: Vec<String> = self
            .pool_holds
            .iter()
            .filter(|((key, _), expiry)| key == pool_key && **expiry > now)
            .map(|((_, host), _)| host.clone())
            .collect();
        hosts.sort();
        hosts.dedup();
        hosts
    }

    /// Drop every expired `pool_holds` entry at local time `now` (Issue
    /// #8001). Called opportunistically so a crashed peer's holds do not
    /// accumulate — and, critically, so a crashed peer's hold **expires**:
    /// its `PoolHoldCleared` will never arrive.
    pub fn prune_expired_pool_holds(&mut self, now: Instant) {
        self.pool_holds.retain(|_, expiry| *expiry > now);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::peer_claims::tests::ad;
    use crate::peer_claims::PeerClaimCounters;

    /// A [`ClaimKind::NoopCooldownArmed`]/[`ClaimKind::DispatchBackoffArmed`]
    /// ad carrying a real `remaining_secs` (Issue #7477) — use this rather
    /// than `ad()` whenever a test cares about the cooldown-lane expiry,
    /// since `ad()` leaves `remaining_secs: None`.
    fn cooldown_ad(
        kind: ClaimKind,
        issue: u32,
        repo: &str,
        host: &str,
        remaining_secs: u64,
    ) -> ClaimAd {
        match kind {
            ClaimKind::NoopCooldownArmed => ClaimAd::noop_cooldown_armed(
                issue,
                repo.to_owned(),
                host.to_owned(),
                42,
                "2026-07-28T00:00:00Z".to_owned(),
                remaining_secs,
            ),
            ClaimKind::DispatchBackoffArmed => ClaimAd::dispatch_backoff_armed(
                issue,
                repo.to_owned(),
                host.to_owned(),
                42,
                "2026-07-28T00:00:00Z".to_owned(),
                remaining_secs,
            ),
            _ => panic!("cooldown_ad called with a non-cooldown-lane kind"),
        }
    }

    /// A [`ClaimKind::PoolHoldArmed`] ad for `pool_key` (Issue #8001).
    fn pool_ad(host: &str, pool_key: &str, remaining_secs: u64) -> ClaimAd {
        ClaimAd::pool_hold_armed(
            "rjwalters/loom".to_owned(),
            host.to_owned(),
            42,
            "2026-07-28T00:00:00Z".to_owned(),
            pool_key.to_owned(),
            remaining_secs,
        )
    }

    /// A [`ClaimKind::PoolHoldCleared`] ad for `pool_key` (Issue #8001).
    fn pool_clear_ad(host: &str, pool_key: &str) -> ClaimAd {
        ClaimAd::pool_hold_cleared(
            "rjwalters/loom".to_owned(),
            host.to_owned(),
            42,
            "2026-07-28T00:00:00Z".to_owned(),
            pool_key.to_owned(),
        )
    }

    // ==================================================================
    // Fleet-wide no-op-cooldown / dispatch-backoff visibility (Issue #7477)
    // ==================================================================

    #[test]
    fn cooldown_lane_ads_round_trip_over_the_wire() {
        for kind in [
            ClaimKind::NoopCooldownArmed,
            ClaimKind::DispatchBackoffArmed,
        ] {
            let a = cooldown_ad(kind, 7466, "rjwalters/loom", "host-a", 3600);
            let parsed = ClaimAd::from_body_str(&a.to_body_json()).unwrap();
            assert_eq!(parsed, a);
            assert_eq!(parsed.remaining_secs, Some(3600));
            assert!(parsed.kind.is_cooldown_lane());
        }
    }

    /// A cooldown-lane ad must never be folded into the dispatch-claims map —
    /// it answers a different question ("should a peer re-dispatch this
    /// issue right now") than "is a sweep in flight".
    #[test]
    fn observe_at_ignores_cooldown_lane_ads() {
        let mut view = PeerClaimView::new("A".into(), Duration::from_secs(120));
        let t = Instant::now();
        let ad = cooldown_ad(ClaimKind::NoopCooldownArmed, 7466, "loom", "B", 3600);
        assert!(!view.observe_at(&ad, t));
        assert!(view.is_empty());
        assert_eq!(view.counters().received, 0, "cooldown ads are not dispatch traffic");
    }

    /// The exact #7477 shape: a peer's self-reported "no actionable delta"
    /// on issue #7466 must suppress THIS host's fleet-wide skip set for that
    /// issue, in the repo the ad named, until the broadcast `remaining_secs`
    /// elapses (measured against local receipt, never the advertiser's
    /// clock).
    #[test]
    fn a_peer_noop_cooldown_is_visible_and_expires_on_schedule() {
        let mut view = PeerClaimView::new("A".into(), Duration::from_secs(120));
        let t0 = Instant::now();
        let ad = cooldown_ad(ClaimKind::NoopCooldownArmed, 7466, "rjwalters/loom", "host-b", 3600);
        assert!(view.observe_noop_cooldown_at(&ad, t0));

        assert!(view
            .noop_cooldown_issues_at("rjwalters/loom", t0 + Duration::from_secs(3599))
            .contains(&7466));
        assert!(
            !view
                .noop_cooldown_issues_at("rjwalters/loom", t0 + Duration::from_secs(3601))
                .contains(&7466),
            "an elapsed cooldown must no longer suppress dispatch"
        );
    }

    /// [`a_peer_noop_cooldown_is_visible_and_expires_on_schedule`]'s sibling
    /// for the dispatch-backoff lane.
    #[test]
    fn a_peer_dispatch_backoff_is_visible_and_expires_on_schedule() {
        let mut view = PeerClaimView::new("A".into(), Duration::from_secs(120));
        let t0 = Instant::now();
        let ad = cooldown_ad(ClaimKind::DispatchBackoffArmed, 7468, "rjwalters/loom", "host-c", 60);
        assert!(view.observe_dispatch_backoff_at(&ad, t0));

        assert!(view
            .dispatch_backoff_issues_at("rjwalters/loom", t0 + Duration::from_secs(59))
            .contains(&7468));
        assert!(!view
            .dispatch_backoff_issues_at("rjwalters/loom", t0 + Duration::from_secs(61))
            .contains(&7468));
    }

    /// Cooldown/backoff visibility is scoped per-`(repo, issue)`, unlike the
    /// fleet-wide filing lock — two managed repos' issue #N must never
    /// cross-suppress each other.
    #[test]
    fn cooldown_visibility_is_scoped_to_its_own_repo() {
        let mut view = PeerClaimView::new("A".into(), Duration::from_secs(120));
        let t = Instant::now();
        let ad = cooldown_ad(ClaimKind::NoopCooldownArmed, 42, "repo-one", "host-b", 3600);
        view.observe_noop_cooldown_at(&ad, t);
        assert!(view.noop_cooldown_issues_at("repo-one", t).contains(&42));
        assert!(
            !view.noop_cooldown_issues_at("repo-two", t).contains(&42),
            "a different repo's identical issue number must not be suppressed"
        );
    }

    /// Never back off on this host's own cooldown/backoff ad, but two
    /// identity-unresolved hosts must still back off from each other — the
    /// #5063 carve-out, applied to this lane too.
    #[test]
    fn own_cooldown_ad_is_ignored_but_unknown_host_is_not() {
        let mut view = PeerClaimView::new("A".into(), Duration::from_secs(120));
        let t = Instant::now();
        let own = cooldown_ad(ClaimKind::NoopCooldownArmed, 7466, "loom", "A", 3600);
        assert!(!view.observe_noop_cooldown_at(&own, t));
        assert!(!view.noop_cooldown_issues_at("loom", t).contains(&7466));

        let mut unresolved = PeerClaimView::new(
            crate::sweep_registry::UNKNOWN_HOST.to_string(),
            Duration::from_secs(120),
        );
        let ad = cooldown_ad(
            ClaimKind::NoopCooldownArmed,
            7466,
            "loom",
            crate::sweep_registry::UNKNOWN_HOST,
            3600,
        );
        assert!(unresolved.observe_noop_cooldown_at(&ad, t));
        assert!(unresolved
            .noop_cooldown_issues_at("loom", t)
            .contains(&7466));
    }

    /// A repeat ad (a peer re-arming/re-affirming the same window) refreshes
    /// the local expiry clock, exactly like a repeat `Advertise` does for a
    /// dispatch claim.
    #[test]
    fn re_observing_a_noop_cooldown_refreshes_its_expiry() {
        let mut view = PeerClaimView::new("A".into(), Duration::from_secs(120));
        let t0 = Instant::now();
        view.observe_noop_cooldown_at(
            &cooldown_ad(ClaimKind::NoopCooldownArmed, 7466, "loom", "B", 100),
            t0,
        );
        let t1 = t0 + Duration::from_secs(90);
        view.observe_noop_cooldown_at(
            &cooldown_ad(ClaimKind::NoopCooldownArmed, 7466, "loom", "B", 100),
            t1,
        );
        assert!(
            view.noop_cooldown_issues_at("loom", t0 + Duration::from_secs(150))
                .contains(&7466),
            "a refreshed window must survive past the original receipt's expiry"
        );
    }

    /// A missing/zero `remaining_secs` (a malformed or already-expired ad)
    /// degrades to "already expired" — harmless, since a subsequent read
    /// simply finds nothing there rather than a bogus indefinite hold.
    #[test]
    fn malformed_zero_remaining_secs_reads_as_already_expired() {
        let mut view = PeerClaimView::new("A".into(), Duration::from_secs(120));
        let t = Instant::now();
        view.observe_noop_cooldown_at(
            &cooldown_ad(ClaimKind::NoopCooldownArmed, 7466, "loom", "B", 0),
            t,
        );
        assert!(!view.noop_cooldown_issues_at("loom", t).contains(&7466));
    }

    /// Pruning removes only the lapsed entries, mirroring
    /// `prune_expired_filing_locks`'s contract.
    #[test]
    fn prune_expired_noop_cooldowns_drops_only_lapsed_entries() {
        let mut view = PeerClaimView::new("A".into(), Duration::from_secs(120));
        let t0 = Instant::now();
        view.observe_noop_cooldown_at(
            &cooldown_ad(ClaimKind::NoopCooldownArmed, 1, "loom", "B", 10),
            t0,
        );
        view.observe_noop_cooldown_at(
            &cooldown_ad(ClaimKind::NoopCooldownArmed, 2, "loom", "B", 1000),
            t0,
        );
        let after = t0 + Duration::from_secs(20);
        view.prune_expired_noop_cooldowns(after);
        assert!(!view.noop_cooldown_issues_at("loom", after).contains(&1));
        assert!(view.noop_cooldown_issues_at("loom", after).contains(&2));
    }

    /// A cooldown-lane ad must likewise not perturb the #6157
    /// dispatch-coordination-health bookkeeping — mirrors
    /// `observing_a_filing_lock_does_not_touch_coordination_health` and
    /// `observing_a_completion_never_touches_coordination_health_counters`.
    /// Critically, a stream of peer cooldown ads must NOT be able to clear a
    /// DEGRADED verdict: the verdict answers "is *dispatch* coordination
    /// healthy", and the cooldown lane is not dispatch traffic.
    #[test]
    fn observing_a_cooldown_does_not_touch_coordination_health() {
        let mut view = PeerClaimView::new("A".into(), Duration::from_secs(120));
        let t = Instant::now();
        view.observe_noop_cooldown_at(
            &cooldown_ad(ClaimKind::NoopCooldownArmed, 7466, "loom", "B", 3600),
            t,
        );
        view.observe_dispatch_backoff_at(
            &cooldown_ad(ClaimKind::DispatchBackoffArmed, 7468, "loom", "B", 60),
            t,
        );
        assert_eq!(view.counters(), PeerClaimCounters::default());
        assert!(!view.coordination_degraded());
        assert_eq!(view.coordination_receives_toward_recovery(), 0);
    }

    /// The #7477 fix must not make a genuinely orphaned claim (a crashed
    /// sweep whose host never got to arm — or broadcast — a cooldown/backoff
    /// window) un-reclaimable: with no cooldown ad ever observed, the
    /// fleet-wide skip sets are empty and the candidate stays immediately
    /// offerable. This is the "must not regress the lease-reclaim path"
    /// acceptance criterion expressed at the layer that actually holds the
    /// new state — `claim_reconciliation` itself never reads these maps.
    #[test]
    fn no_cooldown_ad_means_no_fleet_suppression() {
        let mut view = PeerClaimView::new("A".into(), Duration::from_secs(120));
        let t = Instant::now();
        // A live *dispatch* claim from a crashed peer says nothing about
        // cooldown — the two lanes are independent maps.
        view.observe_at(&ad(ClaimKind::Advertise, 7466, "loom", "B"), t);
        assert!(view.noop_cooldown_issues_at("loom", t).is_empty());
        assert!(view.dispatch_backoff_issues_at("loom", t).is_empty());
    }

    /// The two cooldown lanes are independent of each other: a no-op
    /// cooldown on an issue must not read back as a dispatch backoff (or
    /// vice versa), since they carry different durations and are consumed by
    /// different work-finder skip sets.
    #[test]
    fn the_two_cooldown_lanes_do_not_cross_contaminate() {
        let mut view = PeerClaimView::new("A".into(), Duration::from_secs(120));
        let t = Instant::now();
        view.observe_noop_cooldown_at(
            &cooldown_ad(ClaimKind::NoopCooldownArmed, 7466, "loom", "B", 3600),
            t,
        );
        assert!(view.noop_cooldown_issues_at("loom", t).contains(&7466));
        assert!(
            !view.dispatch_backoff_issues_at("loom", t).contains(&7466),
            "a no-op cooldown must not read back as a dispatch backoff"
        );
    }

    // ==================================================================
    // Fleet-wide token-pool exhaustion holds (Issue #8001)
    // ==================================================================

    #[test]
    fn pool_hold_ads_round_trip_over_the_wire() {
        let armed = pool_ad("host-a", "deadbeefcafe0001", 900);
        let parsed = ClaimAd::from_body_str(&armed.to_body_json()).unwrap();
        assert_eq!(parsed, armed);
        assert_eq!(parsed.pool_key.as_deref(), Some("deadbeefcafe0001"));
        assert_eq!(parsed.issue, POOL_HOLD_SENTINEL_ISSUE);
        assert!(parsed.kind.is_pool_hold_lane());

        let cleared = pool_clear_ad("host-a", "deadbeefcafe0001");
        let parsed = ClaimAd::from_body_str(&cleared.to_body_json()).unwrap();
        assert_eq!(parsed, cleared);
        assert!(parsed.remaining_secs.is_none());
    }

    /// A pool-hold ad must never be folded into the dispatch-claims map: it
    /// carries [`POOL_HOLD_SENTINEL_ISSUE`], so folding it in would
    /// manufacture a phantom peer claim on issue #0.
    #[test]
    fn observe_at_ignores_pool_hold_ads() {
        let mut view = PeerClaimView::new("A".into(), Duration::from_secs(120));
        let t = Instant::now();
        assert!(!view.observe_at(&pool_ad("B", "pool-1", 900), t));
        assert!(view.is_empty());
        assert_eq!(view.counters().received, 0, "pool ads are not dispatch traffic");
    }

    /// The core #8001 read path: a peer's hold on a pool is visible, scoped to
    /// that pool's key, and expires on the advertised window measured from
    /// LOCAL receipt.
    #[test]
    fn a_peer_pool_hold_is_visible_scoped_and_expires_on_schedule() {
        let mut view = PeerClaimView::new("A".into(), Duration::from_secs(120));
        let t0 = Instant::now();
        assert!(view.observe_pool_hold_at(&pool_ad("B", "pool-1", 600), t0));

        assert!(view.pool_hold_held_by_peer_at("pool-1", t0 + Duration::from_secs(599)));
        assert!(
            !view.pool_hold_held_by_peer_at("pool-2", t0),
            "a hold must never leak onto a DIFFERENT pool's key"
        );
        assert!(
            !view.pool_hold_held_by_peer_at("pool-1", t0 + Duration::from_secs(601)),
            "a crashed peer's hold must expire — its clear ad will never arrive"
        );
    }

    /// An ill-behaved ad cannot wedge the lane past the cap.
    #[test]
    fn an_overlong_advertised_window_is_clamped() {
        let mut view = PeerClaimView::new("A".into(), Duration::from_secs(120));
        let t0 = Instant::now();
        view.observe_pool_hold_at(&pool_ad("B", "pool-1", 86_400), t0);
        assert!(view.pool_hold_held_by_peer_at(
            "pool-1",
            t0 + MAX_PEER_POOL_HOLD_TTL - Duration::from_secs(1)
        ));
        assert!(
            !view.pool_hold_held_by_peer_at("pool-1", t0 + MAX_PEER_POOL_HOLD_TTL),
            "no ad may install a window longer than the cap"
        );
    }

    /// A clear releases only its own sender's hold — one peer's recovery must
    /// not speak for another's.
    #[test]
    fn a_clear_releases_only_its_own_senders_hold() {
        let mut view = PeerClaimView::new("A".into(), Duration::from_secs(120));
        let t = Instant::now();
        view.observe_pool_hold_at(&pool_ad("B", "pool-1", 900), t);
        view.observe_pool_hold_at(&pool_ad("C", "pool-1", 900), t);
        assert_eq!(view.pool_hold_peers_at("pool-1", t), vec!["B", "C"]);

        assert!(view.observe_pool_hold_at(&pool_clear_ad("B", "pool-1"), t));
        assert_eq!(view.pool_hold_peers_at("pool-1", t), vec!["C"]);
        assert!(view.pool_hold_held_by_peer_at("pool-1", t));

        assert!(view.observe_pool_hold_at(&pool_clear_ad("C", "pool-1"), t));
        assert!(!view.pool_hold_held_by_peer_at("pool-1", t));
    }

    /// This host never holds on its own advertisement — the same self-claim
    /// recognition every other lane applies.
    #[test]
    fn a_hosts_own_pool_ad_is_ignored() {
        let mut view = PeerClaimView::new("loom-host".into(), Duration::from_secs(120));
        let t = Instant::now();
        assert!(!view.observe_pool_hold_at(&pool_ad("loom-host", "pool-1", 900), t));
        assert!(!view.pool_hold_held_by_peer_at("pool-1", t));
    }

    /// An ad naming no pool is dropped rather than applied to an arbitrary
    /// one — the safe direction for a pre-#8001 peer or a malformed payload.
    #[test]
    fn a_pool_ad_with_no_pool_key_is_dropped() {
        let mut view = PeerClaimView::new("A".into(), Duration::from_secs(120));
        let t = Instant::now();
        let mut keyless = pool_ad("B", "pool-1", 900);
        keyless.pool_key = None;
        assert!(!view.observe_pool_hold_at(&keyless, t));
        assert!(!view.pool_hold_held_by_peer_at("pool-1", t));
    }

    /// A body whose `pool_key` is absent or empty parses as `None` rather
    /// than rejecting the whole ad — the same forward/backward-compatibility
    /// degradation `pr` (#6062) and `remaining_secs` (#7477) use.
    #[test]
    fn an_absent_pool_key_degrades_to_none() {
        let body = serde_json::json!({
            super::super::PEER_CLAIM_MARKER: super::super::CLAIM_SCHEMA_VERSION,
            "kind": "pool_hold_armed",
            "issue": 0,
            "repo": "rjwalters/loom",
            "host": "host-a",
            "remaining_secs": 900,
        });
        let parsed = ClaimAd::from_body_value(&body).unwrap();
        assert_eq!(parsed.kind, ClaimKind::PoolHoldArmed);
        assert!(parsed.pool_key.is_none());
    }

    /// `observe_brake_ad` routes every brake lane, and each lands in its own
    /// map — no lane may read back as another.
    #[test]
    fn observe_brake_ad_routes_each_lane_to_its_own_map() {
        let mut view = PeerClaimView::new("A".into(), Duration::from_secs(120));
        let t = Instant::now();

        observe_brake_ad(
            &mut view,
            &cooldown_ad(ClaimKind::NoopCooldownArmed, 42, "loom", "B", 600),
            t,
        );
        observe_brake_ad(
            &mut view,
            &cooldown_ad(ClaimKind::DispatchBackoffArmed, 43, "loom", "B", 600),
            t,
        );
        observe_brake_ad(&mut view, &pool_ad("B", "pool-1", 600), t);

        assert!(view.noop_cooldown_issues_at("loom", t).contains(&42));
        assert!(view.dispatch_backoff_issues_at("loom", t).contains(&43));
        assert!(view.pool_hold_held_by_peer_at("pool-1", t));
        assert!(view.is_empty(), "no brake lane may fold into the claims map");
    }

    /// Pruning drops only lapsed pool holds.
    #[test]
    fn prune_expired_pool_holds_drops_only_lapsed_entries() {
        let mut view = PeerClaimView::new("A".into(), Duration::from_secs(120));
        let t0 = Instant::now();
        view.observe_pool_hold_at(&pool_ad("B", "short", 60), t0);
        view.observe_pool_hold_at(&pool_ad("C", "long", 600), t0);

        view.prune_expired_pool_holds(t0 + Duration::from_secs(120));
        assert!(!view.pool_hold_held_by_peer_at("short", t0 + Duration::from_secs(120)));
        assert!(view.pool_hold_held_by_peer_at("long", t0 + Duration::from_secs(120)));
    }
}
