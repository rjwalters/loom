//! The fleet-visible **brake lanes** of the peer-claim room.
//!
//! A *claim* ad (`Advertise`/`Retract`, [`super`]'s own subject) answers "is a
//! sweep in flight on issue #N". The kinds routed here answer a
//! different question — "may a dispatch happen at all right now" — and none of
//! them claims anything:
//!
//! | Lane | Kinds | Scope | Issue |
//! |------|-------|-------|-------|
//! | No-op cooldown | [`ClaimKind::NoopCooldownArmed`] | one issue | #7477 |
//! | Dispatch backoff | [`ClaimKind::DispatchBackoffArmed`] | one issue | #7477 |
//! | PR-less retry tally | [`ClaimKind::PrlessReleaseArmed`] | one issue | #9292 |
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
//! # The PR-less lane carries a COUNT, not just a window (Issue #9292)
//!
//! The other three lanes answer a yes/no question — is this issue (or pool)
//! braked right now — so a receiver only has to remember an expiry. The
//! PR-less lane is different: #7972's hold fires on a **tally**, and a tally
//! that lives in one process is a tally each host accumulates privately. Four
//! dispatch hosts therefore spent up to `4 × threshold` claim/release cycles
//! before any one of them reached `threshold` — the `rjwalters/loom#8812`
//! trace behind #9292 shows nine cycles and four near-duplicate "Attempt 2 of
//! 3" notes before the first hold. So this lane's entries carry the
//! advertiser's own `consecutive` count as well as its window, keyed per
//! **host** so the receiver can sum them (see
//! [`PeerClaimView::prless_peer_release_count_at`]).
//!
//! The count also needs its **own clock**, longer than the window's: an
//! advertised window is the interval after which the next host may claim, so a
//! tally expiring with it would lapse at exactly the moment the fleet's next
//! release is about to be recorded, and nothing would ever accumulate. See
//! [`PEER_PRLESS_STREAK_TTL`].
//!
//! This module exists as a sibling of `peer_claims.rs` rather than inside it
//! for the reason `repo_slug_tests`/`coordination_tests` do: that module is
//! over the size ratchet (`scripts/file-size-baseline.txt`) and has no line
//! budget for the brake lanes.

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

/// Hard ceiling on the TTL a single [`ClaimKind::PrlessReleaseArmed`] ad may
/// install in a receiver's view (Issue #9292), regardless of the
/// `remaining_secs` it advertises: **six hours**.
///
/// Comfortably above `DEFAULT_PRLESS_RETRY_MAX_BACKOFF_SECS` (one hour), the
/// ceiling a well-behaved advertiser's window is already clamped to on the
/// arming side, so a healthy ad is never truncated — a repo that deliberately
/// configures a longer `maxBackoffSecs` still gets up to six hours of
/// fleet-visible streak. The ceiling exists for the ill-behaved ad: unlike the
/// #7477 lanes this one contributes to a **hold** decision, so a single ad
/// advertising a year of remaining streak must not be able to keep an issue
/// one release away from `loom:blocked` indefinitely. Bounded here so the
/// worst case degrades to "one over-long streak window", never to a permanent
/// phantom tally.
pub const MAX_PEER_PRLESS_RELEASE_TTL: Duration = Duration::from_secs(6 * 3600);

/// How long a peer's PR-less release keeps counting toward the **fleet tally**
/// (Issue #9292): one hour, matching `DEFAULT_PRLESS_RETRY_MAX_BACKOFF_SECS` —
/// the window the arming side's own streak-cold rule uses, where a release
/// older than `max_backoff` restarts the tally at one.
///
/// # Why this is NOT the advertised window
///
/// The two clocks answer different questions and, crucially, have different
/// lengths. The advertised `remaining_secs` is the **backoff** — 300 s after a
/// first release on shipped config — and it is precisely the interval after
/// which the next host is *allowed to claim*. Reusing it as the tally's TTL
/// makes the fleet-wide count self-defeating: host A's contribution lapses at
/// the exact moment host B becomes free to claim, so B's release (a minute or
/// two later still) sees an empty peer map and reads `1` — and so on around the
/// fleet, forever, which is the pre-#9292 per-host tally with extra steps.
///
/// A streak is a slower thing than a backoff, so it gets the slower clock: a
/// peer's release counts for as long as the local rule would have counted the
/// same release on the host that recorded it.
pub const PEER_PRLESS_STREAK_TTL: Duration = Duration::from_secs(3600);

/// One peer host's PR-less-release tally for one issue (Issue #9292) — the
/// value type of [`PeerClaimView::prless_releases`].
///
/// Three fields rather than the bare [`Instant`] the other per-issue lanes
/// store, because this lane answers "how many" as well as "until when" (see the
/// module doc's "The PR-less lane carries a COUNT" section) — and because its
/// two "until when"s are different clocks, see [`PEER_PRLESS_STREAK_TTL`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerPrlessRelease {
    /// The advertiser's **own** consecutive PR-less releases at send time —
    /// never a fleet total, so summing one entry per host is exact.
    pub consecutive: u32,
    /// Local [`Instant`] at which the advertised **dispatch-suppression
    /// window** lapses, computed once at receipt as
    /// `now + min(remaining_secs, MAX_PEER_PRLESS_RELEASE_TTL)`. Read by
    /// [`PeerClaimView::prless_release_issues_at`].
    pub window_expiry: Instant,
    /// Local [`Instant`] at which this release stops counting toward the
    /// **fleet tally**, computed once at receipt as
    /// `now + max(window TTL, PEER_PRLESS_STREAK_TTL)` — never shorter than
    /// the window, and by default an hour longer. Read by
    /// [`PeerClaimView::prless_peer_release_count_at`], and the field the
    /// whole entry is pruned on.
    pub streak_expiry: Instant,
}

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
        ClaimKind::PrlessReleaseArmed => {
            view.observe_prless_release_at(ad, now);
            view.prune_expired_prless_releases(now);
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
            consecutive: None,
        }
    }

    /// "I recorded my `consecutive`-th consecutive PR-less claim/release cycle
    /// on issue #N, `remaining_secs` seconds left on the window it armed"
    /// (Issue #9292). Broadcast by
    /// [`crate::sweep_registry::SweepRegistry::publish_peer_prless_release_claim`]
    /// from every [`crate::sweep_registry::SweepRegistry::record_prless_release`].
    ///
    /// `consecutive` is this host's own tally only — see
    /// [`ClaimAd::consecutive`] for why a fleet total on the wire would
    /// double-count.
    #[must_use]
    pub fn prless_release_armed(
        issue: u32,
        repo: String,
        host: String,
        pid: u32,
        ts: String,
        consecutive: u32,
        remaining_secs: u64,
    ) -> Self {
        Self {
            kind: ClaimKind::PrlessReleaseArmed,
            issue,
            repo,
            host,
            pid,
            ts,
            pr: None,
            remaining_secs: Some(remaining_secs),
            pool_key: None,
            consecutive: Some(consecutive),
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
            consecutive: None,
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

    /// Remaining time on the live fleet-wide no-op cooldown a **peer** host
    /// armed for `repo`/`issue` at local time `now`, or `None` when no live
    /// peer window covers it (Issue #9928).
    ///
    /// [`Self::noop_cooldown_issues_at`]'s per-issue sibling: the work-finder
    /// pre-filter wants the whole set, but the dispatch-path guard
    /// ([`crate::sweep_registry::SweepRegistry::noop_cooldown_dispatch_block`],
    /// step 2.75) needs the remaining duration for the one issue it was asked
    /// to dispatch, so it can report `retry_after_secs` on its refusal.
    /// `Some(Duration::ZERO)` is never returned — an elapsed window reads as
    /// `None`, mirroring the `expiry > now` filter above.
    #[must_use]
    pub fn noop_cooldown_remaining_at(
        &self,
        repo: &str,
        issue: u32,
        now: Instant,
    ) -> Option<Duration> {
        let expiry = self.noop_cooldowns.get(&(repo.to_string(), issue))?;
        (*expiry > now).then(|| *expiry - now)
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
    // Fleet-wide PR-less-retry tally (Issue #9292)
    // ------------------------------------------------------------------

    /// Observe an inbound [`ClaimKind::PrlessReleaseArmed`] ad at local time
    /// `now`: "peer host H has now recorded `consecutive` consecutive PR-less
    /// releases on issue #N in `repo`, with `remaining_secs` left on the
    /// window it armed as of H's send time".
    ///
    /// Stores the peer's count **verbatim, per host**, replacing that host's
    /// previous entry rather than accumulating — an ad reports a running
    /// total, not an increment, so a redelivered or duplicated ad is
    /// idempotent and can never inflate a threshold.
    ///
    /// Two local expiries, both measured from receipt (the TTL discipline every
    /// other map in this module uses, never the advertiser's clock):
    /// `window_expiry = now + min(remaining_secs, MAX_PEER_PRLESS_RELEASE_TTL)`
    /// for the dispatch-suppression half, and `streak_expiry = now +
    /// max(that, PEER_PRLESS_STREAK_TTL)` for the tally half. See
    /// [`PEER_PRLESS_STREAK_TTL`] for why collapsing them into one clock would
    /// hand the tally straight back to the per-host behaviour #9292 removes.
    ///
    /// Returns `true` when applied, `false` when ignored as this host's own ad.
    /// Ignoring our own ad is what keeps the sum in
    /// [`Self::prless_peer_release_count_at`] disjoint from the caller's local
    /// tally: counting it in both places would double every release this host
    /// recorded and trip the hold at half the threshold.
    ///
    /// **Unresolved identity (Issue #9518).** The #5063 `UNKNOWN_HOST`
    /// carve-out ("treat it as a peer, never as self") is correct for the
    /// window lanes, whose worst case is a harmless extra backoff. This lane
    /// also stores a *count that gets summed*, and an unresolved sender cannot
    /// prove its count is disjoint from ours (when both sides are
    /// `UNKNOWN_HOST` it is very likely our own echoed ad). So an
    /// `UNKNOWN_HOST` ad keeps its conservative backoff window (and streak
    /// clock) but contributes `0` to the additive peer tally, whatever the
    /// receiver's identity. Unidentified senders thus degrade the tally to the
    /// exact local count plus any identifiable peers.
    ///
    /// A missing/zero `consecutive` (a pre-#9292 peer or a malformed payload)
    /// contributes `0` and a missing/zero `remaining_secs` degrades to
    /// "already expired" — both the safe direction, since the worst case is
    /// that the fleet falls back to this host's own exact local tally.
    pub fn observe_prless_release_at(&mut self, ad: &ClaimAd, now: Instant) -> bool {
        debug_assert_eq!(ad.kind, ClaimKind::PrlessReleaseArmed);
        let is_unresolved_identity = ad.host == crate::sweep_registry::UNKNOWN_HOST;
        if ad.host == self.self_host && !is_unresolved_identity {
            return false; // our own releases are already in our local tally
        }
        // Unresolved senders keep the window but add nothing to the sum (#9518).
        let consecutive = if is_unresolved_identity {
            0
        } else {
            ad.consecutive.unwrap_or(0)
        };
        let window =
            Duration::from_secs(ad.remaining_secs.unwrap_or(0)).min(MAX_PEER_PRLESS_RELEASE_TTL);
        self.prless_releases.insert(
            (ad.repo.clone(), ad.issue, ad.host.clone()),
            PeerPrlessRelease {
                consecutive,
                window_expiry: now + window,
                streak_expiry: now + window.max(PEER_PRLESS_STREAK_TTL),
            },
        );
        true
    }

    /// The sum of every **peer** host's live PR-less-release tally for
    /// `(repo, issue)` at local time `now` (Issue #9292) — the term
    /// [`crate::sweep_registry::SweepRegistry::record_prless_release`] adds to
    /// its own local count before comparing against
    /// `PrlessRetryConfig::threshold`, which is what makes the `loom:blocked`
    /// hold trip at `threshold` claims fleet-wide rather than per host.
    ///
    /// Counted against `streak_expiry` — the slower of the entry's two clocks,
    /// for the reason [`PEER_PRLESS_STREAK_TTL`] spells out: a peer's release
    /// must keep counting past the moment its backoff window frees the next
    /// host to claim, or the fleet tally can never accumulate at all.
    ///
    /// Saturating, and `0` when nothing is on record — a fleet with no peer
    /// coordination degrades byte-for-byte to the pre-#9292 per-host tally.
    #[must_use]
    pub fn prless_peer_release_count_at(&self, repo: &str, issue: u32, now: Instant) -> u32 {
        self.prless_releases
            .iter()
            .filter(|((r, i, _), entry)| r == repo && *i == issue && entry.streak_expiry > now)
            .fold(0u32, |acc, (_, entry)| acc.saturating_add(entry.consecutive))
    }

    /// Every issue in `repo` with a live peer-advertised PR-less-release
    /// window at local time `now` (Issue #9292) — unioned into
    /// [`crate::sweep_registry::SweepRegistry::prless_retry_issues`] so a
    /// peer's backoff window also spaces out *this* host's next claim, the
    /// half of the fix `prless_retry`'s own doc comment deferred as "fleet-wide
    /// broadcast of the sub-threshold window".
    ///
    /// Read against `window_expiry`, the faster of the entry's two clocks: this
    /// is the advertised backoff, and when it lapses the next host SHOULD be
    /// free to claim. The tally keeps counting after that
    /// ([`Self::prless_peer_release_count_at`]) — a claim's runway and a
    /// streak's memory are deliberately different lengths.
    #[must_use]
    pub fn prless_release_issues_at(&self, repo: &str, now: Instant) -> HashSet<u32> {
        self.prless_releases
            .iter()
            .filter(|((r, _, _), entry)| r == repo && entry.window_expiry > now)
            .map(|((_, issue, _), _)| *issue)
            .collect()
    }

    /// Drop every `prless_releases` entry whose **streak** clock has lapsed at
    /// local time `now` (Issue #9292) — the
    /// [`Self::prune_expired_noop_cooldowns`] sibling, pruning on the later of
    /// the entry's two expiries so an elapsed window never discards a tally
    /// that is still counting. Also the fleet-wide half of the streak-cold
    /// rule: a peer that stops re-advertising stops contributing, exactly as a
    /// local streak older than `max_backoff` restarts at one.
    pub fn prune_expired_prless_releases(&mut self, now: Instant) {
        self.prless_releases
            .retain(|_, entry| entry.streak_expiry > now);
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
    // Fleet-wide PR-less-retry tally (Issue #9292)
    // ==================================================================

    /// A [`ClaimKind::PrlessReleaseArmed`] ad from `host` (Issue #9292).
    fn prless_ad(host: &str, issue: u32, consecutive: u32, remaining_secs: u64) -> ClaimAd {
        ClaimAd::prless_release_armed(
            issue,
            "rjwalters/loom".to_owned(),
            host.to_owned(),
            42,
            "2026-09-29T00:00:00Z".to_owned(),
            consecutive,
            remaining_secs,
        )
    }

    #[test]
    fn prless_release_ads_round_trip_over_the_wire() {
        let a = prless_ad("host-a", 8812, 2, 600);
        let parsed = ClaimAd::from_body_str(&a.to_body_json()).unwrap();
        assert_eq!(parsed, a);
        assert_eq!(parsed.consecutive, Some(2));
        assert_eq!(parsed.remaining_secs, Some(600));
        assert!(
            parsed.kind.is_cooldown_lane(),
            "the PR-less lane must route through the brake-lane predicate the \
             socket layer already gates on, so `safehouse.rs` needs no change"
        );
    }

    /// A pre-#9292 peer's ad (or a malformed one) carries no `consecutive`.
    /// It must parse rather than be rejected, and contribute `0` — never an
    /// unreadable number that could inflate someone's hold threshold.
    #[test]
    fn an_absent_consecutive_degrades_to_none_and_counts_as_zero() {
        let body = serde_json::json!({
            super::super::PEER_CLAIM_MARKER: super::super::CLAIM_SCHEMA_VERSION,
            "kind": "prless_release_armed",
            "issue": 8812,
            "repo": "rjwalters/loom",
            "host": "host-a",
            "remaining_secs": 600,
        });
        let parsed = ClaimAd::from_body_value(&body).unwrap();
        assert_eq!(parsed.kind, ClaimKind::PrlessReleaseArmed);
        assert!(parsed.consecutive.is_none());

        let mut view = PeerClaimView::new("A".into(), Duration::from_secs(120));
        let t = Instant::now();
        assert!(view.observe_prless_release_at(&parsed, t));
        assert_eq!(view.prless_peer_release_count_at("rjwalters/loom", 8812, t), 0);
    }

    /// The core #9292 read path: peers' tallies SUM, so three hosts at one
    /// release each read back as three — the fleet total the threshold is
    /// compared against.
    #[test]
    fn peer_tallies_sum_across_hosts() {
        let mut view = PeerClaimView::new("A".into(), Duration::from_secs(120));
        let t = Instant::now();
        for host in ["B", "C", "D"] {
            assert!(view.observe_prless_release_at(&prless_ad(host, 8812, 1, 600), t));
        }
        assert_eq!(view.prless_peer_release_count_at("rjwalters/loom", 8812, t), 3);
        assert!(view
            .prless_release_issues_at("rjwalters/loom", t)
            .contains(&8812));
    }

    /// An ad reports a RUNNING TOTAL, not an increment, so a host's repeat ad
    /// replaces its own entry rather than accumulating — a redelivered or
    /// duplicated ad can never manufacture a hold.
    #[test]
    fn a_repeat_ad_from_one_host_replaces_rather_than_accumulates() {
        let mut view = PeerClaimView::new("A".into(), Duration::from_secs(120));
        let t = Instant::now();
        view.observe_prless_release_at(&prless_ad("B", 8812, 1, 600), t);
        view.observe_prless_release_at(&prless_ad("B", 8812, 1, 600), t);
        assert_eq!(
            view.prless_peer_release_count_at("rjwalters/loom", 8812, t),
            1,
            "a duplicate delivery must be idempotent"
        );
        view.observe_prless_release_at(&prless_ad("B", 8812, 2, 600), t);
        assert_eq!(
            view.prless_peer_release_count_at("rjwalters/loom", 8812, t),
            2,
            "a genuine second release raises the same host's entry"
        );
    }

    /// This host's own ad is ignored — which is what keeps the peer sum
    /// disjoint from the caller's local tally. Counting it in both places
    /// would double every release and trip the hold at half the threshold.
    /// An `UNKNOWN_HOST` ad keeps its window but adds zero to the tally
    /// (Issue #9518), whatever the receiver's identity.
    #[test]
    fn a_hosts_own_prless_ad_is_ignored_and_unknown_host_counts_zero() {
        let mut view = PeerClaimView::new("A".into(), Duration::from_secs(120));
        let t = Instant::now();
        assert!(!view.observe_prless_release_at(&prless_ad("A", 8812, 3, 600), t));
        assert_eq!(view.prless_peer_release_count_at("rjwalters/loom", 8812, t), 0);

        let unknown = crate::sweep_registry::UNKNOWN_HOST;
        for self_host in [unknown, "A"] {
            let mut v = PeerClaimView::new(self_host.to_string(), Duration::from_secs(120));
            let ad = prless_ad(unknown, 8812, 2, 600);
            assert!(v.observe_prless_release_at(&ad, t));
            assert_eq!(v.prless_peer_release_count_at("rjwalters/loom", 8812, t), 0);
            // Window stays conservative.
            assert!(v
                .prless_release_issues_at("rjwalters/loom", t)
                .contains(&8812));
            // A known peer still adds its own count alongside.
            v.observe_prless_release_at(&prless_ad("B", 8812, 2, 600), t);
            assert_eq!(v.prless_peer_release_count_at("rjwalters/loom", 8812, t), 2);
        }
    }

    /// A lapsed entry stops counting — the fleet-wide form of the local
    /// streak-cold rule. A crashed peer must not pin an issue one release
    /// short of `loom:blocked` forever.
    #[test]
    fn a_lapsed_peer_tally_stops_counting_and_prunes() {
        let mut view = PeerClaimView::new("A".into(), Duration::from_secs(120));
        let t0 = Instant::now();
        view.observe_prless_release_at(&prless_ad("B", 8812, 2, 600), t0);
        assert_eq!(
            view.prless_peer_release_count_at(
                "rjwalters/loom",
                8812,
                t0 + Duration::from_secs(599)
            ),
            2
        );
        let after = t0 + PEER_PRLESS_STREAK_TTL + Duration::from_secs(1);
        assert_eq!(view.prless_peer_release_count_at("rjwalters/loom", 8812, after), 0);
        assert!(view
            .prless_release_issues_at("rjwalters/loom", after)
            .is_empty());

        view.observe_prless_release_at(&prless_ad("C", 9999, 1, 4 * 3600), t0);
        view.prune_expired_prless_releases(after);
        assert!(
            view.prless_release_issues_at("rjwalters/loom", after)
                .contains(&9999),
            "pruning on the streak clock must not discard a still-live window"
        );
    }

    /// The #9292 correctness trap, pinned: the tally's clock is **not** the
    /// advertised window's. A first release advertises a 300 s backoff — which
    /// is exactly the interval after which the next host is free to claim — so
    /// a tally that lapsed with the window would be empty at the precise moment
    /// the fleet's second release lands, and the fleet-wide count could never
    /// reach the threshold at all. The window stops suppressing dispatch on
    /// schedule; the count keeps counting.
    #[test]
    fn the_tally_outlives_the_advertised_window_it_rode_in_on() {
        let mut view = PeerClaimView::new("A".into(), Duration::from_secs(120));
        let t0 = Instant::now();
        // Shipped config's first-release window: 300s.
        view.observe_prless_release_at(&prless_ad("B", 8812, 1, 300), t0);

        // A minute past the window: B's *claim* is unblocked...
        let unblocked = t0 + Duration::from_secs(360);
        assert!(
            !view
                .prless_release_issues_at("rjwalters/loom", unblocked)
                .contains(&8812),
            "the advertised window must free the next claim on schedule"
        );
        // ...but B's release still counts toward the fleet streak, which is the
        // whole mechanism.
        assert_eq!(
            view.prless_peer_release_count_at("rjwalters/loom", 8812, unblocked),
            1,
            "a lapsed backoff window must not erase the release that armed it"
        );
        // And it is not immortal either: the streak clock is the local
        // cold-streak window.
        assert_eq!(
            view.prless_peer_release_count_at(
                "rjwalters/loom",
                8812,
                t0 + PEER_PRLESS_STREAK_TTL + Duration::from_secs(1)
            ),
            0
        );
    }

    /// An advertiser whose configured window is LONGER than the default streak
    /// window (a repo with a raised `maxBackoffSecs`, or any held issue riding
    /// the ceiling) must not have its tally lapse while its own suppression
    /// window is still live — the streak clock is never the shorter of the two.
    #[test]
    fn a_window_longer_than_the_streak_ttl_extends_the_tally_with_it() {
        let mut view = PeerClaimView::new("A".into(), Duration::from_secs(120));
        let t0 = Instant::now();
        let long = PEER_PRLESS_STREAK_TTL + Duration::from_secs(3600);
        view.observe_prless_release_at(&prless_ad("B", 8812, 2, long.as_secs()), t0);

        let late = t0 + long - Duration::from_secs(1);
        assert!(view
            .prless_release_issues_at("rjwalters/loom", late)
            .contains(&8812));
        assert_eq!(view.prless_peer_release_count_at("rjwalters/loom", 8812, late), 2);
    }

    /// Scoped per `(repo, issue)`: neither another repo's identical issue
    /// number nor another issue in the same repo may contribute.
    #[test]
    fn peer_tallies_are_scoped_to_their_own_repo_and_issue() {
        let mut view = PeerClaimView::new("A".into(), Duration::from_secs(120));
        let t = Instant::now();
        view.observe_prless_release_at(&prless_ad("B", 8812, 2, 600), t);
        let mut other_repo = prless_ad("B", 8812, 5, 600);
        other_repo.repo = "someone/else".to_owned();
        view.observe_prless_release_at(&other_repo, t);
        view.observe_prless_release_at(&prless_ad("B", 7893, 4, 600), t);

        assert_eq!(view.prless_peer_release_count_at("rjwalters/loom", 8812, t), 2);
        assert_eq!(view.prless_peer_release_count_at("someone/else", 8812, t), 5);
        assert_eq!(view.prless_peer_release_count_at("rjwalters/loom", 7893, t), 4);
    }

    /// An ill-behaved ad cannot keep an issue one release from a hold
    /// indefinitely: the advertised window is clamped at the cap.
    #[test]
    fn an_overlong_prless_window_is_clamped() {
        let mut view = PeerClaimView::new("A".into(), Duration::from_secs(120));
        let t0 = Instant::now();
        view.observe_prless_release_at(&prless_ad("B", 8812, 2, 365 * 86_400), t0);
        assert_eq!(
            view.prless_peer_release_count_at(
                "rjwalters/loom",
                8812,
                t0 + MAX_PEER_PRLESS_RELEASE_TTL - Duration::from_secs(1)
            ),
            2
        );
        assert_eq!(
            view.prless_peer_release_count_at(
                "rjwalters/loom",
                8812,
                t0 + MAX_PEER_PRLESS_RELEASE_TTL
            ),
            0,
            "no ad may install a window longer than the cap"
        );
    }

    /// A PR-less ad must not be folded into the dispatch-claims map, and must
    /// not perturb the #6157 coordination-health bookkeeping — the same
    /// contract every other brake lane carries.
    #[test]
    fn a_prless_ad_is_neither_a_claim_nor_dispatch_traffic() {
        let mut view = PeerClaimView::new("A".into(), Duration::from_secs(120));
        let t = Instant::now();
        assert!(!view.observe_at(&prless_ad("B", 8812, 1, 600), t));
        assert!(view.is_empty());
        view.observe_prless_release_at(&prless_ad("B", 8812, 1, 600), t);
        assert_eq!(view.counters(), PeerClaimCounters::default());
        assert!(!view.coordination_degraded());
        assert_eq!(view.coordination_receives_toward_recovery(), 0);
    }

    /// `observe_brake_ad` routes the PR-less lane into its own map — no lane
    /// may read back as another.
    #[test]
    fn observe_brake_ad_routes_the_prless_lane_to_its_own_map() {
        let mut view = PeerClaimView::new("A".into(), Duration::from_secs(120));
        let t = Instant::now();
        observe_brake_ad(&mut view, &prless_ad("B", 8812, 2, 600), t);
        assert_eq!(view.prless_peer_release_count_at("rjwalters/loom", 8812, t), 2);
        assert!(
            !view
                .noop_cooldown_issues_at("rjwalters/loom", t)
                .contains(&8812),
            "a PR-less release must not read back as a no-op cooldown"
        );
        assert!(!view
            .dispatch_backoff_issues_at("rjwalters/loom", t)
            .contains(&8812));
        assert!(view.is_empty(), "no brake lane may fold into the claims map");
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
