//! Fleet broadcast of this host's periodic liveness heartbeat (Issue #8736).
//!
//! # Why a sibling module rather than more of `dispatch.rs`
//!
//! `sweep_registry::dispatch` is at the size ratchet
//! (`scripts/file-size-baseline.txt`) — one more method here would grow it —
//! and this is a self-contained, one-method lane, so it lives beside it,
//! matching the `pool_hold_broadcast` / `noop_cooldown` split the same module
//! already uses.
//!
//! # What this closes
//!
//! [`crate::peer_claims::coordination_idle`] (Issue #8026) fixed the case
//! where THIS host is idle and therefore has no standing to judge its own
//! silence. It deliberately left the converse open: this host busy, its
//! *peers* idle. Advertising ([`SweepRegistry::publish_peer_claim`],
//! [`ClaimKind::Advertise`]) is entirely dispatch-gated, so an idle peer and
//! an unreachable one are indistinguishable from `received`/`quiet_for`
//! alone — no *local* signal can separate them. A heartbeat is the one thing
//! an idle host still emits, closing that gap.
//!
//! # Cost/cadence, and why it is acceptable here
//!
//! Published **once per reaper interval per host** (default 30s), regardless
//! of live-sweep count. Every managed workspace has its own reaper, so the
//! call runs once per workspace per tick; the shared [`PeerClaimView`] gates
//! it ([`PeerClaimView::take_heartbeat_slot_at`]) so only the first reaper
//! past the interval sends. Before that gate a host managing ten
//! workspaces sent ten identical heartbeats every 30s, and peers cannot use
//! the extra nine: they keep one "last heartbeat from anyone" timestamp and
//! read only the sender's host from the ad (to drop their own echo). The
//! heartbeat's `repo` field is now whichever workspace's reaper won the slot;
//! no receiver reads it. Before this gate it was published on every reaper
//! tick, regardless of live-sweep count — unlike [`SweepRegistry::readvertise_peer_claims`],
//! which only republishes claims for `Running`/`Pending` entries. This is a
//! standing, permanent addition to fleet-wide traffic on the shared
//! safehouse channel, not an edge-triggered one like
//! [`SweepRegistry::publish_peer_pool_hold_claim`]'s arm/clear ads — the
//! whole point is that it must arrive even when nothing else would. That
//! channel already carries several other always-or-often lanes (dispatch
//! advertise/retract, cooldown/backoff arms, pool holds), so one more small,
//! fixed-size ad per tick is a marginal addition, not a new category of
//! load.
//!
//! Fail-open, mirroring every other lane on this channel: a no-op without a
//! publisher (`safehouse.enabled` false), and a full/closed channel drops
//! the ad without blocking the reaper — a dropped heartbeat costs one tick of
//! staleness on peers' anchors, never a stall.
use super::*;

impl SweepRegistry {
    /// Publish this host's liveness heartbeat (Issue #8736) — called once per
    /// reaper tick, unconditionally, right beside
    /// [`Self::readvertise_peer_claims`]. Deliberately does **not** call
    /// [`crate::peer_claims::PeerClaimView::record_advertised`]: that field
    /// drives the #8026 idle gate's "is THIS host itself dispatching"
    /// question, which a heartbeat — sent whether or not this host has any
    /// live sweep — must never answer on this host's behalf. Folding it in
    /// would make every host permanently read as "actively advertising" and
    /// silently undo #8026's fix.
    pub(crate) fn publish_peer_heartbeat(&self) {
        let Some(tx) = &self.peer_claim_publisher else {
            return;
        };
        // One heartbeat per host per interval, not per workspace: every
        // registry in this process shares the same view, so the first reaper
        // past the interval takes the slot and the rest skip. Without a view
        // (tests, a registry built outside the pool) there is no fan-out to
        // collapse, so it sends every call as before.
        if let Some(view) = &self.peer_claims {
            let min_interval = heartbeat_min_interval(super::reaper::resolve_reaper_interval());
            let mut view = view
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !view.take_heartbeat_slot_at(Instant::now(), min_interval) {
                return;
            }
        }
        let repo = peer_claims::repo_slug(&self.config.workspace_root);
        let host = host_identity();
        let pid = std::process::id();
        let ts = Utc::now().to_rfc3339();
        let ad = ClaimAd::heartbeat(repo, host, pid, ts);
        if let Err(e) = tx.try_send(ad) {
            // Fail-open: see the module doc comment. Debug, not warn, so a
            // persistent safehoused outage does not spam the log once per
            // reaper tick forever.
            log::debug!(
                "sweep_registry: heartbeat ad dropped ({e}); coordination-health evaluation \
                 unaffected for this tick (#8736)"
            );
        }
    }
}

/// The shortest gap this host allows between two of its own heartbeats: four
/// fifths of the reaper interval (24s at the 30s default). Below a full
/// interval so ordinary tick jitter never makes the one reaper that sent last
/// time skip and leave a two-interval gap; well above zero so N reapers
/// ticking at N different phases still collapse to one send per interval.
fn heartbeat_min_interval(reaper_interval: Duration) -> Duration {
    reaper_interval.saturating_sub(reaper_interval / 5)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::sweep_registry::{SweepRegistry, SweepRegistryConfig};
    use tempfile::tempdir;

    fn test_registry() -> SweepRegistry {
        let dir = tempdir().unwrap();
        SweepRegistry::new(SweepRegistryConfig::new(dir.path().to_path_buf()))
    }

    #[test]
    fn publish_peer_heartbeat_sends_a_heartbeat_ad() {
        let mut reg = test_registry();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        reg.set_peer_claim_publisher(tx);

        reg.publish_peer_heartbeat();

        let ad = rx.try_recv().expect("a heartbeat ad was sent");
        assert_eq!(ad.kind, peer_claims::ClaimKind::Heartbeat);
        assert_eq!(ad.issue, peer_claims::HEARTBEAT_SENTINEL_ISSUE);
    }

    #[test]
    fn publish_peer_heartbeat_is_a_silent_no_op_without_a_publisher() {
        let reg = test_registry();
        // No publisher attached — must not panic, and there is nothing else
        // to assert on beyond "this returns".
        reg.publish_peer_heartbeat();
    }

    /// Issue #8026: a heartbeat must never make this host read as "actively
    /// advertising" on its own behalf — that must stay driven solely by
    /// `record_advertised` (via `publish_peer_claim`/`readvertise_peer_claims`),
    /// or every host would permanently read as advertising and silently undo
    /// #8026's idle gate. Verified indirectly through the public
    /// `evaluate_coordination` surface: a heartbeat alone must never advance
    /// this host past its "nothing to judge yet" (never-advertised) state.
    #[test]
    fn publish_peer_heartbeat_never_marks_this_host_as_advertising() {
        let mut reg = test_registry();
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        reg.set_peer_claim_publisher(tx);
        let view = std::sync::Arc::new(std::sync::Mutex::new(peer_claims::PeerClaimView::new(
            "me".into(),
            peer_claims::DEFAULT_PEER_CLAIM_TTL,
        )));
        reg.set_peer_claims(view.clone());

        reg.publish_peer_heartbeat();

        let eval =
            view.lock()
                .unwrap()
                .evaluate_coordination(Instant::now(), Duration::from_secs(1), 1);
        assert!(
            eval.reason.contains("nothing to judge"),
            "a heartbeat alone must never make this host read as having advertised: {eval:?}"
        );
    }

    /// One heartbeat per host, not one per managed workspace: two registries
    /// (two workspaces, two reapers) sharing this process's view and
    /// publisher send exactly one heartbeat between them per interval.
    #[test]
    fn registries_sharing_a_view_send_one_heartbeat_per_interval() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let view = std::sync::Arc::new(std::sync::Mutex::new(peer_claims::PeerClaimView::new(
            "me".into(),
            peer_claims::DEFAULT_PEER_CLAIM_TTL,
        )));
        let mut a = test_registry();
        let mut b = test_registry();
        for reg in [&mut a, &mut b] {
            reg.set_peer_claim_publisher(tx.clone());
            reg.set_peer_claims(view.clone());
        }

        a.publish_peer_heartbeat();
        b.publish_peer_heartbeat();
        a.publish_peer_heartbeat();

        let first = rx.try_recv().expect("the first reaper to tick sends");
        assert_eq!(first.kind, peer_claims::ClaimKind::Heartbeat);
        assert!(
            rx.try_recv().is_err(),
            "the other workspace's reaper (and a re-tick) inside the interval must not send"
        );
    }

    /// The slot reopens once the interval has passed, so the gate never
    /// silences a host — it only collapses the per-workspace fan-out.
    #[test]
    fn heartbeat_slot_reopens_after_the_interval() {
        let mut view =
            peer_claims::PeerClaimView::new("me".into(), peer_claims::DEFAULT_PEER_CLAIM_TTL);
        let min = heartbeat_min_interval(Duration::from_secs(30));
        let t0 = Instant::now();
        assert!(view.take_heartbeat_slot_at(t0, min), "first heartbeat always sends");
        assert!(!view.take_heartbeat_slot_at(t0 + Duration::from_secs(5), min));
        assert!(!view.take_heartbeat_slot_at(t0 + min - Duration::from_millis(1), min));
        // A full reaper interval later, the same reaper sends again.
        assert!(view.take_heartbeat_slot_at(t0 + Duration::from_secs(30), min));
        assert!(!view.take_heartbeat_slot_at(t0 + Duration::from_secs(31), min));
    }

    /// The gate sits below one reaper interval, so ordinary tick jitter never
    /// makes the reaper that sent last skip its next tick.
    #[test]
    fn heartbeat_min_interval_is_under_one_reaper_interval() {
        let interval = Duration::from_secs(DEFAULT_REAPER_INTERVAL_SECS);
        let min = heartbeat_min_interval(interval);
        assert!(min < interval && min > interval / 2, "{min:?} vs {interval:?}");
        assert_eq!(heartbeat_min_interval(Duration::ZERO), Duration::ZERO);
    }
}
