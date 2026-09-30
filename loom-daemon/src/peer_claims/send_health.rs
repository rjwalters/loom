//! Publish-side health for the peer-claim channel (Issue #9294).
//!
//! # The failure this exists to make visible
//!
//! Every counter on [`super::PeerClaimCounters`] before this module described
//! what this daemon *attempted* or what it *consumed*. None described whether
//! the attempts landed. `advertised` in particular is explicitly "did we queue
//! an advertisement" — [`super::PeerClaimView::record_advertised`] counts at
//! enqueue time, before the socket write — not "did the room accept it". The
//! outbound path is fire-and-forget by design.
//!
//! That left one whole failure class invisible. Measured on `loom-worker-1`,
//! 2026-09-29, against the fleet's real claims room
//! `!WyzxHvMPAPaCqaXmL6:safehouse.2amlogic.com`:
//!
//! - The room's most recent event of any kind is from **2026-09-23T17:54:52Z**
//!   (`GET /_matrix/client/v3/rooms/<claims>/messages?dir=b&limit=3`), while
//!   the narration room kept accepting events seconds apart. The homeserver
//!   had lost the claims room's forward extremities, so every send into it
//!   fails server-side:
//!
//!   ```text
//!   [2026-09-23T18:08:57.710] [WARN] safehouse: peer claim-ad rejected
//!     (safehoused rejected send: sending to room: the server returned an error:
//!     [500 / M_UNKNOWN] cannot create a non-create event in a room with no
//!     forward extremities in !WyzxHvMPAPaCqaXmL6:safehouse.2amlogic.com);
//!     peer-claim dedup disabled, dispatch unaffected
//!   ```
//!
//! - Six days later the daemon still reported the channel as ordinary:
//!
//!   ```text
//!   Safehouse:   connected (room: …)
//!   Peer claims: none live (… advertised=3765 received=0 expired=0 dispatch_skipped=0)
//!   ```
//!
//! `advertised` was counting writes into a channel that accepted none of them,
//! and `received=0` was the honest reading of a room every host was equally
//! unable to write to — so the outage was fleet-wide, not one host's receive
//! path.
//!
//! # Two shapes, not one
//!
//! The daemon must survive **both** ways a dead publish side presents, because
//! this outage produced both:
//!
//! 1. **An explicit refusal.** safehoused answers `ok:false` with the server's
//!    error. This is what the 2026-09-23/24 WARNs above are. It was already
//!    detectable in principle — but the only reaction was a `warn!` deduped to
//!    one line *per connection*, so a six-day outage left two log lines and no
//!    state change anywhere.
//! 2. **Silence.** Re-measured 2026-09-29: a `send` addressed at the claims
//!    room over safehoused's socket produced **no reply at all** within 60s
//!    (neither `ok:true` nor `ok:false`) — matrix-sdk retries the 500
//!    internally, so the op never returns — while the identical send to the
//!    narration room round-tripped `ok:true` in under a second. In this shape
//!    counting refusals finds nothing: there are none. Only the *absence of an
//!    acknowledgement* distinguishes it from a healthy channel.
//!
//! So this module tracks refusals **and** unacknowledged sends, and treats
//! either as proof the publish side is blocked.
//!
//! # The rule
//!
//! A blocked publish side is **direct proof** that this host is putting nothing
//! into the room — not an inference from silence. So, unlike
//! [`super::PeerClaimView::evaluate_coordination`]'s receive-quiet heuristic, a
//! refusal needs no grace window, and neither shape is subject to #8026's idle
//! gate (a host whose sends are refused or unanswered is, by definition, trying
//! to send). It clears the instant a send is acknowledged.
//!
//! Stickiness matches [`crate::safehouse::SafehouseState::SendRejected`]'s own
//! contract (#4464): a reconnect does not clear a block, only a reply does. A
//! reconnect is not evidence the channel publishes — and the replies owed to
//! everything written before it can never arrive, so forgetting them would let
//! a reconnect loop hide a permanently dead room.

use std::time::{Duration, Instant};

/// How long every outstanding claim-ad send must go unanswered before the
/// silence counts as a publish block.
///
/// Ads are written at least once per reaper tick (30s) while any claim is live,
/// so this is ~10 consecutive missed acknowledgements. Deliberately a constant
/// rather than an operator knob: it is a floor under a proof-grade signal, not
/// a policy choice.
pub const UNACKED_SEND_GRACE: Duration = Duration::from_secs(300);

/// How many sends must be outstanding before [`UNACKED_SEND_GRACE`] is even
/// consulted. One unanswered write is a race with a reply still in flight;
/// three spanning five minutes is a pattern.
pub const MIN_UNACKED_SENDS: u64 = 3;

/// Why the publish side is currently blocked — see
/// [`ClaimSendHealth::publish_blocked_at`].
///
/// The two variants are the two measured shapes of the same outage (see this
/// module's header), kept distinct because an operator acts on them
/// differently: a refusal hands over the homeserver's own error text, while
/// silence means the send never returned and the error is only in safehoused's
/// own log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishBlock<'a> {
    /// safehoused answered `ok:false`, and no send has been accepted since.
    Rejected {
        /// The transport's verbatim error text.
        reason: &'a str,
        /// Running total of refusals (not one per connection).
        rejected: u64,
    },
    /// safehoused answered nothing at all — [`MIN_UNACKED_SENDS`] or more
    /// sends have been outstanding for at least [`UNACKED_SEND_GRACE`].
    Unacknowledged {
        /// How many sends are outstanding.
        unacked: u64,
        /// How long the oldest of them has been waiting.
        quiet_for: Duration,
    },
}

impl PublishBlock<'_> {
    /// The operator-facing sentence for this block, quoted verbatim by
    /// `loom-daemon status`, the `peer_coordination` health component and the
    /// tests, so all three say the same thing.
    #[must_use]
    pub fn reason(&self) -> String {
        match *self {
            Self::Rejected { reason, rejected } => publish_blocked_reason(reason, rejected),
            Self::Unacknowledged { unacked, quiet_for } => {
                unacknowledged_reason(unacked, quiet_for)
            }
        }
    }
}

/// Publish-side bookkeeping for the peer-claim channel: whether safehoused is
/// currently accepting this host's claim ads, and the last reason it gave for
/// refusing one.
///
/// Held as a single field on [`super::PeerClaimView`] so the view keeps one
/// field rather than seven, and so the whole publish-health concern (counters,
/// stickiness rule, verdict text) stays in this module.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaimSendHealth {
    /// How many claim-ad sends safehoused has **refused** (`ok:false`) since
    /// this daemon started. Counts every rejection, not one per connection —
    /// the `warn!` in [`crate::safehouse`]'s coordination loop is deliberately
    /// deduped per connection, which is what made a six-day outage look like a
    /// pair of stale log lines.
    rejected: u64,
    /// How many claim-ad sends safehoused has **accepted** (`ok:true`).
    accepted: u64,
    /// How many claim-ad sends have been written to the socket, answered or
    /// not. Distinct from [`super::PeerClaimCounters::advertised`], which
    /// counts at enqueue time and so keeps climbing even while the socket is
    /// unwritable.
    attempted: u64,
    /// How many written sends are still awaiting a reply of any kind. Reset by
    /// any reply, because safehoused services one connection's ops in order:
    /// one reply proves the connection is moving.
    unacked: u64,
    /// When the oldest currently-outstanding send was written.
    oldest_unacked_at: Option<Instant>,
    /// When the most recent rejection landed, and the raw safehoused `error`
    /// string that came with it.
    last_rejected: Option<(Instant, String)>,
    /// When the most recent acceptance landed. Compared against
    /// `last_rejected` to decide whether the publish side is *currently*
    /// blocked, rather than treating one historical blip as permanent.
    last_accepted_at: Option<Instant>,
}

impl ClaimSendHealth {
    /// Record that one claim ad was written to the socket and a reply is now
    /// owed. Called by [`crate::safehouse::PeerClaimSink`] from the
    /// coordination loop's successful-write path.
    pub fn observe_attempt(&mut self, now: Instant) {
        self.attempted = self.attempted.saturating_add(1);
        self.unacked = self.unacked.saturating_add(1);
        if self.oldest_unacked_at.is_none() {
            self.oldest_unacked_at = Some(now);
        }
    }

    /// Fold in one send outcome: `Some(reason)` for a safehoused rejection,
    /// `None` for an accepted send. Either way the outstanding-reply backlog
    /// clears — a reply of any kind proves the socket is being serviced — and
    /// only the rejection stickiness below decides whether the channel is
    /// nevertheless blocked.
    pub fn observe(&mut self, rejection: Option<&str>, now: Instant) {
        self.unacked = 0;
        self.oldest_unacked_at = None;
        match rejection {
            Some(reason) => {
                self.rejected = self.rejected.saturating_add(1);
                self.last_rejected = Some((now, reason.to_owned()));
            }
            None => {
                self.accepted = self.accepted.saturating_add(1);
                self.last_accepted_at = Some(now);
            }
        }
    }

    /// Whether the publish side is **currently** blocked at local time `now`,
    /// and why.
    ///
    /// A refusal outranks silence: when safehoused has told us why, quote it
    /// rather than inferring from the backlog. Otherwise a backlog of at least
    /// [`MIN_UNACKED_SENDS`] outstanding for [`UNACKED_SEND_GRACE`] is the
    /// block — the shape measured on 2026-09-29, where the send never returns
    /// at all.
    #[must_use]
    pub fn publish_blocked_at(&self, now: Instant) -> Option<PublishBlock<'_>> {
        if let Some((rejected_at, reason)) = self.last_rejected.as_ref() {
            let cleared = matches!(self.last_accepted_at, Some(at) if at >= *rejected_at);
            if !cleared {
                return Some(PublishBlock::Rejected {
                    reason: reason.as_str(),
                    rejected: self.rejected,
                });
            }
        }
        let quiet_for = now.saturating_duration_since(self.oldest_unacked_at?);
        (self.unacked >= MIN_UNACKED_SENDS && quiet_for >= UNACKED_SEND_GRACE).then_some(
            PublishBlock::Unacknowledged {
                unacked: self.unacked,
                quiet_for,
            },
        )
    }

    /// Running rejection total — surfaced beside `advertised` so the pair
    /// `advertised=3765 rejected=3765` is unambiguous where `advertised=3765`
    /// alone was not.
    #[must_use]
    pub fn rejected(&self) -> u64 {
        self.rejected
    }

    /// Running acceptance total.
    #[must_use]
    pub fn accepted(&self) -> u64 {
        self.accepted
    }

    /// Running total of sends written to the socket.
    #[must_use]
    pub fn attempted(&self) -> u64 {
        self.attempted
    }

    /// How many written sends are still awaiting any reply.
    #[must_use]
    pub fn unacked(&self) -> u64 {
        self.unacked
    }
}

/// The verdict text for a channel whose sends are being refused. Factored out
/// so the status line, the health component and the test assertions all quote
/// one sentence.
#[must_use]
pub fn publish_blocked_reason(reason: &str, rejected: u64) -> String {
    format!(
        "every peer claim ad is being REJECTED by safehoused ({rejected} so far; last: {reason}) \
         — this host is publishing nothing, so `advertised` is counting attempts that never \
         landed and no peer can see this host's claims"
    )
}

/// The verdict text for a channel whose sends are never answered — the shape
/// where there is no error to quote because the send never returns.
#[must_use]
pub fn unacknowledged_reason(unacked: u64, quiet_for: Duration) -> String {
    format!(
        "safehoused has not acknowledged any peer claim ad for {}s ({unacked} send(s) \
         outstanding) — this host is publishing nothing, so `advertised` is counting attempts \
         that never landed; check safehoused's own log for the room it is stuck writing to",
        quiet_for.as_secs()
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A fresh recorder is not publish-blocked: no send has been written yet.
    #[test]
    fn fresh_health_is_not_publish_blocked() {
        let health = ClaimSendHealth::default();
        assert_eq!(health.publish_blocked_at(Instant::now()), None);
        assert_eq!(health.rejected(), 0);
        assert_eq!(health.accepted(), 0);
        assert_eq!(health.attempted(), 0);
    }

    /// One rejection blocks the publish side and carries the server's reason
    /// through verbatim — the operator needs the raw homeserver error to act.
    #[test]
    fn a_rejection_blocks_publishing_and_keeps_the_reason() {
        let mut health = ClaimSendHealth::default();
        let now = Instant::now();
        health.observe(Some("[500 / M_UNKNOWN] no forward extremities"), now);
        assert_eq!(
            health.publish_blocked_at(now),
            Some(PublishBlock::Rejected {
                reason: "[500 / M_UNKNOWN] no forward extremities",
                rejected: 1,
            })
        );
    }

    /// Rejections accumulate — the count is what distinguishes a six-day
    /// outage from a pair of stale log lines.
    #[test]
    fn rejections_accumulate_rather_than_dedup() {
        let mut health = ClaimSendHealth::default();
        let base = Instant::now();
        for i in 0..5 {
            health.observe(Some("room dead"), base + Duration::from_secs(i));
        }
        assert_eq!(health.rejected(), 5);
        assert!(matches!(
            health.publish_blocked_at(base + Duration::from_secs(5)),
            Some(PublishBlock::Rejected { rejected: 5, .. })
        ));
    }

    /// An accepted send clears the block (#4464's stickiness rule, mirrored):
    /// a transient blip must not latch the verdict.
    #[test]
    fn an_accepted_send_clears_the_block() {
        let mut health = ClaimSendHealth::default();
        let base = Instant::now();
        health.observe(Some("timed out"), base);
        assert!(health.publish_blocked_at(base).is_some());
        health.observe(None, base + Duration::from_secs(1));
        assert_eq!(health.publish_blocked_at(base + Duration::from_secs(1)), None);
        assert_eq!(health.accepted(), 1);
        // The historical rejection is still counted — it happened.
        assert_eq!(health.rejected(), 1);
    }

    /// A rejection AFTER an acceptance re-blocks: the channel went bad again.
    #[test]
    fn a_later_rejection_reblocks_after_an_acceptance() {
        let mut health = ClaimSendHealth::default();
        let base = Instant::now();
        health.observe(None, base);
        health.observe(Some("room dead again"), base + Duration::from_secs(1));
        assert!(matches!(
            health.publish_blocked_at(base + Duration::from_secs(1)),
            Some(PublishBlock::Rejected {
                reason: "room dead again",
                ..
            })
        ));
    }

    /// The 2026-09-29 shape: sends are written and simply never answered. No
    /// rejection exists to count, so only the unacknowledged backlog sees it.
    #[test]
    fn sustained_unanswered_sends_block_publishing() {
        let mut health = ClaimSendHealth::default();
        let base = Instant::now();
        for i in 0..12 {
            health.observe_attempt(base + Duration::from_secs(i * 30));
        }
        let now = base + UNACKED_SEND_GRACE;
        let block = health
            .publish_blocked_at(now)
            .expect("10+ unanswered sends over the grace window is a dead publish side");
        assert!(matches!(block, PublishBlock::Unacknowledged { unacked: 12, .. }));
        assert_eq!(health.rejected(), 0, "there is no rejection to count in this shape");
        assert!(block.reason().contains("has not acknowledged"), "{}", block.reason());
    }

    /// A handful of sends still in flight is a race with the reply, not an
    /// outage: neither the count nor the elapsed window alone may trip it.
    #[test]
    fn a_short_or_small_backlog_does_not_block() {
        let base = Instant::now();

        // Long enough, but only one send outstanding.
        let mut health = ClaimSendHealth::default();
        health.observe_attempt(base);
        assert_eq!(health.publish_blocked_at(base + UNACKED_SEND_GRACE * 2), None);

        // Enough sends, but nowhere near long enough.
        let mut health = ClaimSendHealth::default();
        for i in 0..20 {
            health.observe_attempt(base + Duration::from_secs(i));
        }
        assert_eq!(health.publish_blocked_at(base + Duration::from_secs(20)), None);
    }

    /// Any reply clears the backlog — safehoused services one connection's ops
    /// in order, so one reply proves the connection is moving.
    #[test]
    fn a_reply_clears_the_unacknowledged_backlog() {
        let mut health = ClaimSendHealth::default();
        let base = Instant::now();
        for i in 0..12 {
            health.observe_attempt(base + Duration::from_secs(i * 30));
        }
        assert!(health
            .publish_blocked_at(base + UNACKED_SEND_GRACE)
            .is_some());
        health.observe(None, base + UNACKED_SEND_GRACE);
        assert_eq!(health.unacked(), 0);
        assert_eq!(health.publish_blocked_at(base + UNACKED_SEND_GRACE), None);
        assert_eq!(health.attempted(), 12);
    }

    /// A refusal outranks silence: when safehoused has said why, quote it
    /// rather than reporting a backlog the refusal itself explains.
    #[test]
    fn a_rejection_outranks_an_unacknowledged_backlog() {
        let mut health = ClaimSendHealth::default();
        let base = Instant::now();
        health.observe(Some("room dead"), base);
        for i in 0..12 {
            health.observe_attempt(base + Duration::from_secs(i * 30));
        }
        assert!(matches!(
            health.publish_blocked_at(base + UNACKED_SEND_GRACE),
            Some(PublishBlock::Rejected { .. })
        ));
    }

    /// The verdict sentences name the numbers and the raw reason — both are
    /// what an operator needs to tell this apart from a quiet fleet.
    #[test]
    fn the_verdict_texts_name_the_counts_and_the_reason() {
        let text = publish_blocked_reason("[500 / M_UNKNOWN] no forward extremities", 3765);
        assert!(text.contains("3765"), "{text}");
        assert!(text.contains("no forward extremities"), "{text}");
        assert!(text.contains("REJECTED"), "{text}");

        let text = unacknowledged_reason(41, Duration::from_secs(1230));
        assert!(text.contains("1230s"), "{text}");
        assert!(text.contains("41 send(s) outstanding"), "{text}");
    }
}
