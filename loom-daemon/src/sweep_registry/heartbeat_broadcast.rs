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
//! Published on **every** reaper tick (default 30s), regardless of
//! live-sweep count — unlike [`SweepRegistry::readvertise_peer_claims`],
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
}
