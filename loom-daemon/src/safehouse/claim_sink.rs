//! The peer-claim inbound sink and its on-disk filing-lock mirror, moved out of
//! `safehouse.rs` in Issue #9294 so the publish-outcome handling this issue adds
//! (`on_send_outcome`) lands in a sibling module rather than growing an
//! already-over-threshold file (`scripts/file-size-baseline.txt`).
//!
//! Behaviour of the moved code is unchanged; `safehouse::PeerClaimSink` keeps
//! working via the `pub use` re-export, so no call site moves.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::Value;

use super::InboundEventSink;
use crate::peer_claims::{ClaimAd, PeerClaimView};

/// The peer-claim consumer: parses claim ads out of inbound room events and
/// folds them into a shared [`PeerClaimView`] (self-claim recognition + TTL live
/// in the view). A non-claim event (a human chat message, a narration line) is
/// silently ignored.
pub struct PeerClaimSink {
    view: Arc<Mutex<PeerClaimView>>,
}

impl PeerClaimSink {
    #[must_use]
    pub fn new(view: Arc<Mutex<PeerClaimView>>) -> Self {
        Self { view }
    }
}

impl InboundEventSink for PeerClaimSink {
    fn on_event(&self, event: &Value) {
        // #6249: safehoused's live push (`main.rs` `on_message`) carries the
        // message text at `envelope.body` — there is no top-level `body`.
        // Reading only the top level dropped 100% of inbound claims fleet-wide
        // (`received=0` while peers visibly advertised). Read the writer's real
        // shape first, keeping the top-level `body` as a fallback for any
        // legacy emitter of the flat shape.
        let Some(body) = event
            .pointer("/envelope/body")
            .and_then(Value::as_str)
            .or_else(|| event.get("body").and_then(Value::as_str))
        else {
            return;
        };
        let Some(ad) = ClaimAd::from_body_str(body) else {
            return; // not a claim (human chat, narration, malformed) — ignore
        };
        match self.view.lock() {
            Ok(mut view) => {
                let now = Instant::now();
                // Issue #6352: a `Completed` ad routes to the dedicated
                // completion-dedup map (its own TTL, no #6157
                // coordination-health side effects) rather than
                // `observe_at`'s dispatch-claims map — see
                // `PeerClaimView::observe_completion_at`'s doc comment.
                if ad.kind.is_filing_lock_lane() {
                    // Issue #6714: the issue-filing lane. Fold into the view's
                    // own single-purpose bookkeeping AND mirror to the
                    // machine-wide on-disk store, which is the only thing the
                    // shell filer (`create-issue.sh` -> `lib/filing-lock.sh`)
                    // can see — the daemon is the bridge between the
                    // cross-host transport and the cross-process lock.
                    if view.observe_filing_lock_at(&ad, now) {
                        mirror_filing_lock_to_disk(&ad);
                    }
                    for expired in view.prune_expired_filing_locks(now) {
                        clear_filing_lock_mirror(&expired);
                    }
                } else if ad.kind.is_cooldown_lane() || ad.kind.is_pool_hold_lane() {
                    // The BRAKE lanes: the #7477 per-issue no-op-cooldown /
                    // dispatch-backoff windows and the #8001 per-pool
                    // exhaustion hold. Each folds into its own single-purpose
                    // map (mirroring the filing-lock lane above) rather than
                    // `observe_at`'s dispatch-claims map — a brake answers
                    // "may a dispatch happen at all right now", not "is a
                    // sweep in flight". The per-lane routing lives beside the
                    // lanes themselves in `peer_claims::brakes`, so adding a
                    // lane never touches this socket layer.
                    crate::peer_claims::observe_brake_ad(&mut view, &ad, now);
                } else if ad.kind == crate::peer_claims::ClaimKind::Completed {
                    view.observe_completion_at(&ad, now);
                    view.prune_expired_completions(now);
                } else if ad.kind == crate::peer_claims::ClaimKind::Heartbeat {
                    // Issue #8736: a liveness ping, folded into its own
                    // single-purpose field rather than `observe_at`'s
                    // dispatch-claims map — see `observe_heartbeat_at`'s doc
                    // comment for why it must never inflate the `#6157`
                    // transport counters.
                    view.observe_heartbeat_at(&ad, now);
                } else {
                    view.observe_at(&ad, now);
                    // Opportunistically prune so a crashed peer's entries do
                    // not accumulate between work-finder queries.
                    view.prune_expired(now);
                }
            }
            Err(poisoned) => {
                log::error!("safehouse: peer-claim view mutex poisoned ({poisoned:?})");
            }
        }
    }

    /// Issue #9294: fold the outcome of one of THIS host's outbound claim-ad
    /// sends into the view, so a channel that accepts nothing stops looking
    /// identical to a quiet fleet.
    ///
    /// Before this, a rejected send produced exactly one `warn!` per connection
    /// (deduped by `run_coordination`'s `ad_rejected` flag) and nothing else:
    /// `advertised` kept climbing, `Safehouse:` kept reading `connected`, and a
    /// six-day fleet-wide outage — the homeserver 500ing every send into the
    /// claims room with `no forward extremities` — left no trace in
    /// `loom-daemon status` at all.
    fn on_send_outcome(&self, rejection: Option<&str>) {
        let now = Instant::now();
        match self.view.lock() {
            Ok(mut view) => view.observe_send_outcome_at(rejection, now),
            Err(poisoned) => poisoned
                .into_inner()
                .observe_send_outcome_at(rejection, now),
        }
    }

    /// Issue #9294: record that an ad reached the socket, so a send that is
    /// never answered at all is distinguishable from one that was accepted.
    /// The outage this issue was filed for took exactly that shape — see
    /// `peer_claims::send_health`.
    fn on_send_attempt(&self) {
        let now = Instant::now();
        match self.view.lock() {
            Ok(mut view) => view.observe_send_attempt_at(now),
            Err(poisoned) => poisoned.into_inner().observe_send_attempt_at(now),
        }
    }
}

/// Mirror an observed peer [`crate::peer_claims::ClaimKind::FilingLock`] /
/// `FilingUnlock` into the machine-wide filing-lock store (Issue #6714).
///
/// The daemon is the bridge: only it is connected to the safehouse room, and
/// only the on-disk store is visible to the shell filers
/// (`create-issue.sh` → `lib/filing-lock.sh`) that actually run `gh issue
/// create`. Best-effort — an unresolvable store degrades the fleet tier to
/// host-only serialization, never to a failed filing.
fn mirror_filing_lock_to_disk(ad: &ClaimAd) {
    let Some(store) = crate::filing_lock::store_dir() else {
        return;
    };
    match ad.kind {
        crate::peer_claims::ClaimKind::FilingLock => {
            crate::filing_lock::record_peer_hold(&store, &ad.host);
        }
        crate::peer_claims::ClaimKind::FilingUnlock => {
            crate::filing_lock::clear_peer_hold(&store, &ad.host);
        }
        _ => {}
    }
}

/// Clear a TTL-expired peer's filing-hold mirror (Issue #6714) — the
/// crash-release path: a peer that dies mid-burst never sends `FilingUnlock`,
/// so its marker must be removed when the view expires it.
fn clear_filing_lock_mirror(host: &str) {
    if let Some(store) = crate::filing_lock::store_dir() {
        crate::filing_lock::clear_peer_hold(&store, host);
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn view() -> Arc<Mutex<PeerClaimView>> {
        Arc::new(Mutex::new(PeerClaimView::new("loom-worker-1".into(), Duration::from_secs(120))))
    }

    /// Issue #9294: a rejected send must reach the view, so the counters an
    /// operator reads stop describing only attempts. This is the plumbing the
    /// six-day 2026-09-23 claims-room outage went missing — `run_coordination`
    /// had the homeserver's error in hand and dropped it after one `warn!`.
    #[test]
    fn a_rejected_send_reaches_the_view() {
        let view = view();
        let sink = PeerClaimSink::new(view.clone());

        sink.on_send_outcome(Some("[500 / M_UNKNOWN] no forward extremities"));

        let v = view.lock().unwrap();
        assert_eq!(v.counters().advertise_rejected, 1);
        assert!(matches!(
            v.send_health().publish_blocked_at(Instant::now()),
            Some(crate::peer_claims::PublishBlock::Rejected {
                reason: "[500 / M_UNKNOWN] no forward extremities",
                ..
            })
        ));
    }

    /// An accepted send clears the block through the same path — the sink is
    /// wired to BOTH reply shapes, not just the failure one.
    #[test]
    fn an_accepted_send_clears_the_block_through_the_sink() {
        let view = view();
        let sink = PeerClaimSink::new(view.clone());

        sink.on_send_outcome(Some("room dead"));
        sink.on_send_outcome(None);

        let v = view.lock().unwrap();
        assert_eq!(v.send_health().publish_blocked_at(Instant::now()), None);
        assert_eq!(v.send_health().accepted(), 1);
        // The rejection is still counted — it happened.
        assert_eq!(v.counters().advertise_rejected, 1);
    }

    /// Every rejection counts. The `warn!` in `run_coordination` is deduped per
    /// connection on purpose; the counter must not be, or a sustained outage
    /// again reads as a single stale log line.
    #[test]
    fn repeated_rejections_are_all_counted() {
        let view = view();
        let sink = PeerClaimSink::new(view.clone());
        for _ in 0..40 {
            sink.on_send_outcome(Some("room dead"));
        }
        assert_eq!(view.lock().unwrap().counters().advertise_rejected, 40);
    }

    /// A send outcome must never be mistaken for inbound peer traffic: the
    /// dispatch-claims map and `received` are untouched.
    #[test]
    fn a_send_outcome_is_not_inbound_traffic() {
        let view = view();
        let sink = PeerClaimSink::new(view.clone());
        sink.on_send_outcome(Some("room dead"));
        sink.on_send_outcome(None);
        let v = view.lock().unwrap();
        assert!(v.is_empty());
        assert_eq!(v.counters().received, 0);
        assert_eq!(v.counters().advertised, 0);
    }

    /// Issue #9294: the write half. A send reaching the socket is recorded as
    /// awaiting a reply, which is the only thing that can distinguish the
    /// measured failure — safehoused answering *nothing* — from a healthy
    /// channel, since that shape produces no rejection to count.
    #[test]
    fn a_written_send_is_recorded_as_awaiting_a_reply() {
        let view = view();
        let sink = PeerClaimSink::new(view.clone());

        for _ in 0..12 {
            sink.on_send_attempt();
        }
        {
            let v = view.lock().unwrap();
            assert_eq!(v.send_health().attempted(), 12);
            assert_eq!(v.send_health().unacked(), 12);
            assert_eq!(v.counters().advertise_rejected, 0, "an unanswered send is not a rejection");
        }

        // One reply of any kind clears the backlog.
        sink.on_send_outcome(None);
        let v = view.lock().unwrap();
        assert_eq!(v.send_health().unacked(), 0);
        assert_eq!(v.send_health().attempted(), 12);
    }
}
