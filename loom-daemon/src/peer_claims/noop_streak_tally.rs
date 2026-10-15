//! The fleet-wide **no-op streak** (#12024).
//!
//! # The gap
//!
//! The no-op hold (#10156) parks an issue after `holdThreshold` consecutive
//! no-op sweeps with an unchanged forge fingerprint, but the streak lived in
//! one process. On a six-dispatcher fleet one issue took ten no-op sweeps in
//! 97 minutes and no host saw more than two, so the hold never tripped. This
//! is #11930's fast-fail gap again, and it is closed the same way.
//!
//! # The shape
//!
//! Each counted no-op publishes a [`ClaimKind::NoopStreak`] ad carrying the
//! sender's **own** count ([`ClaimAd::consecutive`]) and the digest of the
//! fingerprint it was counted against ([`ClaimAd::fingerprint`]). The
//! receiver keeps one entry per host, replacing rather than accumulating, and
//! the counting host adds the peers' sum to its own. The PR-less (#9292) and
//! fast-fail (#11930) rules apply unchanged:
//!
//! - our own ad is ignored, so the sum is disjoint from the local count;
//! - an `UNKNOWN_HOST` ad contributes nothing and breaks nothing (#9518): it
//!   cannot prove it is not our own echo;
//! - a zero count drops that host's entry.
//!
//! One rule is new: a peer count is summed only when its fingerprint matches
//! the one the reader has just read for itself. A count taken against an
//! older state of the issue is not part of this streak, so any change to the
//! issue restarts the fleet streak without anyone having to announce it.
//!
//! # Breaking the streak
//!
//! When a host parks or releases the issue it publishes a **break**
//! (`streak_break: Some(true)`). A receiver drops every host's entry for the
//! issue and records when the break arrived
//! ([`PeerClaimView::noop_streak_break_at`]); the sweep registry treats its
//! own local streak counted before that instant as broken. That is what stops
//! a park lifted by hand from being re-applied at once by the peers' stale
//! counts, whose fingerprint (labels aside) may well match again.
//!
//! # Compatibility
//!
//! `NoopStreak` is a new kind, so a peer older than #12024 drops the ad as
//! unknown (`ClaimKind::from_str`), and its own no-ops are simply not counted
//! by the new hosts. Nothing else on the wire changes.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use super::{ClaimAd, ClaimKind, PeerClaimView};

/// How long a peer's no-op count is kept (#12024): one day. A no-op is
/// followed by an hour's fleet-wide cooldown (#7477), so a streak of three
/// takes hours, not minutes; the fingerprint, not this clock, is what
/// normally restarts it.
pub const MAX_PEER_NOOP_STREAK_AGE: Duration = Duration::from_secs(24 * 3600);

/// One peer host's no-op count for one issue.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PeerNoopStreak {
    count: u32,
    fingerprint: String,
    at: Instant,
}

/// The receive side's state, one field on [`PeerClaimView`].
#[derive(Debug, Default)]
pub(super) struct NoopStreakTally {
    /// Keyed by `(repo_slug, issue, advertising_host)`.
    streaks: HashMap<(String, u32, String), PeerNoopStreak>,
    /// When a break for `(repo_slug, issue)` last arrived, on this host's
    /// wall clock.
    breaks: HashMap<(String, u32), DateTime<Utc>>,
}

impl ClaimAd {
    /// "I have seen `count` no-ops in a row on issue #N against the
    /// fingerprint digest `fingerprint`" (#12024).
    #[must_use]
    pub fn noop_streak(
        issue: u32,
        repo: String,
        host: String,
        pid: u32,
        ts: String,
        count: u32,
        fingerprint: String,
    ) -> Self {
        let mut ad = Self::advertise(issue, repo, host, pid, ts);
        ad.kind = ClaimKind::NoopStreak;
        ad.consecutive = Some(count);
        ad.fingerprint = Some(fingerprint);
        ad.streak_break = Some(false);
        ad
    }

    /// Turn a [`ClaimKind::NoopStreak`] ad into a break: every host's count
    /// for the issue is dropped (#12024).
    #[must_use]
    pub fn as_noop_streak_break(mut self) -> Self {
        debug_assert_eq!(self.kind, ClaimKind::NoopStreak);
        self.consecutive = Some(0);
        self.streak_break = Some(true);
        self
    }
}

impl PeerClaimView {
    /// Fold an inbound [`ClaimKind::NoopStreak`] ad into the per-host tally
    /// and prune lapsed entries. Returns whether the ad was applied (`false`
    /// for this host's own).
    pub fn observe_noop_streak_at(&mut self, ad: &ClaimAd, now: Instant) -> bool {
        debug_assert_eq!(ad.kind, ClaimKind::NoopStreak);
        let unresolved = ad.host == crate::sweep_registry::UNKNOWN_HOST;
        if ad.host == self.self_host && !unresolved {
            return false;
        }
        let tally = &mut self.noop_streaks;
        tally
            .streaks
            .retain(|_, s| now.saturating_duration_since(s.at) < MAX_PEER_NOOP_STREAK_AGE);
        let wall_now = Utc::now();
        let max_age = chrono::Duration::from_std(MAX_PEER_NOOP_STREAK_AGE)
            .unwrap_or_else(|_| chrono::Duration::days(1));
        tally
            .breaks
            .retain(|_, at| wall_now.signed_duration_since(*at) < max_age);
        // An unresolved sender may be our own echo: it counts as nothing and
        // breaks nothing.
        if unresolved {
            return true;
        }
        if ad.streak_break == Some(true) {
            tally
                .streaks
                .retain(|(r, i, _), _| !(r == &ad.repo && *i == ad.issue));
            tally.breaks.insert((ad.repo.clone(), ad.issue), wall_now);
            return true;
        }
        let key = (ad.repo.clone(), ad.issue, ad.host.clone());
        match (ad.consecutive.unwrap_or(0), ad.fingerprint.as_deref()) {
            (0, _) | (_, None) => {
                tally.streaks.remove(&key);
            }
            (count, Some(fp)) => {
                tally.streaks.insert(
                    key,
                    PeerNoopStreak {
                        count,
                        fingerprint: fp.to_string(),
                        at: now,
                    },
                );
            }
        }
        true
    }

    /// The sum of every peer host's no-op count on `(repo, issue)` taken
    /// against `fingerprint` (#12024). Saturating; `0` when none.
    #[must_use]
    pub fn noop_peer_streak_at(
        &self,
        repo: &str,
        issue: u32,
        fingerprint: &str,
        now: Instant,
    ) -> u32 {
        self.noop_streaks
            .streaks
            .iter()
            .filter(|((r, i, _), s)| {
                r == repo
                    && *i == issue
                    && s.fingerprint == fingerprint
                    && now.saturating_duration_since(s.at) < MAX_PEER_NOOP_STREAK_AGE
            })
            .fold(0u32, |acc, (_, s)| acc.saturating_add(s.count))
    }

    /// When a peer's no-op streak break for `(repo, issue)` last arrived, on
    /// this host's wall clock (#12024).
    #[must_use]
    pub fn noop_streak_break_at(&self, repo: &str, issue: u32) -> Option<DateTime<Utc>> {
        self.noop_streaks
            .breaks
            .get(&(repo.to_string(), issue))
            .copied()
    }

    /// Drop every peer no-op count on `(repo, issue)`: this host parked or
    /// released it. Returns whether any existed.
    pub fn clear_noop_streaks(&mut self, repo: &str, issue: u32) -> bool {
        let before = self.noop_streaks.streaks.len();
        self.noop_streaks
            .streaks
            .retain(|(r, i, _), _| !(r == repo && *i == issue));
        self.noop_streaks.streaks.len() != before
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const REPO: &str = "rjwalters/loom";

    fn ad(host: &str, issue: u32, count: u32, fp: &str) -> ClaimAd {
        ClaimAd::noop_streak(issue, REPO.into(), host.into(), 1, "ts".into(), count, fp.into())
    }

    fn view() -> PeerClaimView {
        PeerClaimView::new("self".into(), Duration::from_secs(120))
    }

    #[test]
    fn the_count_and_fingerprint_ride_the_wire() {
        let sent = ad("host-a", 7, 2, "abc");
        let back = ClaimAd::from_body_str(&sent.to_body_json()).unwrap();
        assert_eq!(back.kind, ClaimKind::NoopStreak);
        assert_eq!(back.consecutive, Some(2));
        assert_eq!(back.fingerprint.as_deref(), Some("abc"));
        assert_eq!(back.streak_break, Some(false));
        assert!(back.kind.is_cooldown_lane(), "routed by the brake-lane predicate");
    }

    /// An older peer's parser drops an unknown kind; the new kind cannot be
    /// misread as any existing lane.
    #[test]
    fn an_unknown_kind_is_not_a_claim() {
        let body = ad("host-a", 7, 2, "abc")
            .to_body_json()
            .replace("noop_streak", "some_future_kind");
        assert!(ClaimAd::from_body_str(&body).is_none());
    }

    #[test]
    fn peer_counts_sum_per_host_on_a_matching_fingerprint_only() {
        let mut v = view();
        let t = Instant::now();
        v.observe_noop_streak_at(&ad("host-a", 7, 1, "fp1"), t);
        v.observe_noop_streak_at(&ad("host-b", 7, 1, "fp1"), t);
        v.observe_noop_streak_at(&ad("host-c", 7, 4, "fp0"), t);
        v.observe_noop_streak_at(&ad("self", 7, 9, "fp1"), t);
        assert_eq!(v.noop_peer_streak_at(REPO, 7, "fp1", t), 2);
        assert_eq!(v.noop_peer_streak_at(REPO, 7, "fp0", t), 4);
        assert_eq!(v.noop_peer_streak_at(REPO, 8, "fp1", t), 0);
        // A repeat replaces; a zero drops the host.
        v.observe_noop_streak_at(&ad("host-a", 7, 2, "fp1"), t);
        assert_eq!(v.noop_peer_streak_at(REPO, 7, "fp1", t), 3);
        v.observe_noop_streak_at(&ad("host-a", 7, 0, "fp1"), t);
        assert_eq!(v.noop_peer_streak_at(REPO, 7, "fp1", t), 1);
    }

    #[test]
    fn a_break_drops_every_host_and_is_dated_but_an_unknown_host_cannot_break() {
        let mut v = view();
        let t = Instant::now();
        v.observe_noop_streak_at(&ad("host-a", 7, 2, "fp"), t);
        let unknown = ad(crate::sweep_registry::UNKNOWN_HOST, 7, 0, "fp").as_noop_streak_break();
        v.observe_noop_streak_at(&unknown, t);
        assert_eq!(v.noop_peer_streak_at(REPO, 7, "fp", t), 2);
        assert!(v.noop_streak_break_at(REPO, 7).is_none());
        v.observe_noop_streak_at(&ad("host-b", 7, 0, "fp").as_noop_streak_break(), t);
        assert_eq!(v.noop_peer_streak_at(REPO, 7, "fp", t), 0);
        assert!(v.noop_streak_break_at(REPO, 7).is_some());
        assert!(v.noop_streak_break_at(REPO, 8).is_none());
    }

    #[test]
    fn a_count_older_than_a_day_goes_cold() {
        let mut v = view();
        let t = Instant::now();
        v.observe_noop_streak_at(&ad("host-a", 7, 2, "fp"), t);
        let later = t + MAX_PEER_NOOP_STREAK_AGE + Duration::from_secs(1);
        assert_eq!(v.noop_peer_streak_at(REPO, 7, "fp", later), 0);
    }

    #[test]
    fn the_router_reaches_the_tally() {
        let mut v = view();
        let t = Instant::now();
        super::super::observe_brake_ad(&mut v, &ad("host-a", 7, 2, "fp"), t);
        assert_eq!(v.noop_peer_streak_at(REPO, 7, "fp", t), 2);
        assert!(v.clear_noop_streaks(REPO, 7));
        assert_eq!(v.noop_peer_streak_at(REPO, 7, "fp", t), 0);
    }
}
