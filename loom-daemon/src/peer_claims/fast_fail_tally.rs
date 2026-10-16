//! The fleet-wide **fast-failure streak** (Issue #11930).
//!
//! # The gap
//!
//! The fast-fail park (#11854) parks an issue on its third consecutive
//! issue-attributable fast failure, but the streak it counts lived in one
//! process. Only the backoff *window* was broadcast
//! ([`ClaimKind::DispatchBackoffArmed`], #7477), so an issue that failed fast
//! once on each of three hosts never reached three on any of them, and kept
//! looping across the fleet.
//!
//! # The fix
//!
//! The ladder's `DispatchBackoffArmed` ad now also carries the sender's own
//! fast-failure streak in [`ClaimAd::consecutive`]: the count when the window
//! was armed by an issue-attributable fast failure, `0` when it was armed
//! under any other cause (a lost lease race, an environmental class, an
//! open-PR yield), which breaks that host's streak exactly as it breaks the
//! local one. The receiver keeps one entry **per host**, replacing rather than
//! accumulating (an ad reports a running total), and the parking host adds the
//! peers' sum to its own count. This is the PR-less tally's shape (#9292),
//! reused rather than reinvented:
//!
//! - our own ad is ignored, so the sum is disjoint from the local count;
//! - an `UNKNOWN_HOST` ad contributes `0` (#9518): it cannot prove its count
//!   is not our own echo;
//! - an ad without a count (an older peer, or the landed cooldown's window)
//!   leaves the tally alone.
//!
//! # Breaking the streak
//!
//! The streak is *consecutive across the fleet*, so anything that breaks it on
//! one host must break it on all of them. Real progress on an issue, and an
//! operator's `quarantine clear`, therefore publish a **break**
//! (`SweepRegistry::break_fleet_fast_fail_streak`): a `DispatchBackoffArmed`
//! ad marked [`ClaimAd::streak_break`] `= Some(true)`, with a zero window and
//! a zero streak. On a break a receiver:
//!
//! - drops **every** host's entry for the issue, not only the sender's, so a
//!   peer holding `A:2, C:1` is left with nothing after A's progress;
//! - records when the break arrived ([`PeerClaimView::fast_fail_break_at`]).
//!   Its **own** streak lives in the sweep registry, out of this view's
//!   reach, so the registry reads that instant and counts a local
//!   fast-failure record from before it as broken (#11952): its next ad then
//!   reports `1`, not a stale count the break was meant to erase;
//! - leaves every **window** alone. A break is about the fast-fail tally
//!   only; a peer's landed cooldown or ladder window for the issue keeps
//!   running, as it did before breaks existed (#11952).
//!
//! # Telling a break from an ordinary ad
//!
//! Before #11952 a break was encoded only as "zero window and zero streak".
//! An ordinary failure could forge that: with a zero backoff `base` the
//! ladder arms a zero window, and a transient cause reports a zero streak.
//! The explicit marker settles it, read as:
//!
//! | `streak_break` | meaning |
//! |---|---|
//! | `Some(true)` | a break, whatever the window says |
//! | `Some(false)` | never a break; a zero streak breaks only the sender's own entry |
//! | `None` | a pre-#11952 sender: zero window and zero streak is a break, as it was |
//!
//! Compatibility runs both ways. A new sender's break keeps the zero window
//! and zero streak, so an older receiver still reads it as a break (and, as
//! before, also as a window clear). A new sender also never puts a zero window
//! on an ordinary ladder ad (it advertises at least one second), so an older
//! receiver cannot mistake one for a break either. And the backoff config
//! refuses a zero `base` with a warning.
//!
//! # The streak's clock
//!
//! A peer's count is read against the reader's own ladder `max`, measured
//! from local receipt: the same "older than `max` has gone cold" rule the
//! local streak uses (`backoff_cause::continues`). Entries older than
//! [`MAX_PEER_FAST_FAIL_STREAK_AGE`] are pruned whatever `max` says.
//!
//! The environmental exclusions are unchanged: an environmental class is
//! charged under the transient cause, so it is advertised as `0` and can never
//! add to anyone's park count.

use std::time::{Duration, Instant};

use super::{ClaimAd, ClaimKind, PeerClaimView};

/// Hard ceiling on how long a peer's fast-failure streak is kept at all
/// (Issue #11930): one day, the escalated quarantine ceiling. The reader's
/// ladder `max` (6 h by default) decides what still counts; this only bounds
/// the map.
pub const MAX_PEER_FAST_FAIL_STREAK_AGE: Duration = Duration::from_secs(24 * 3600);

impl ClaimAd {
    /// Attach the sender's own fast-failure streak to a
    /// [`ClaimKind::DispatchBackoffArmed`] ad (Issue #11930). See the module
    /// docs for what `0` means.
    /// Since #11952 this also marks the ad as **not** a break
    /// ([`ClaimAd::streak_break`] `= Some(false)`), so a zero streak on it
    /// can only ever break the sender's own entry.
    #[must_use]
    pub fn with_fast_fail_streak(mut self, streak: u32) -> Self {
        debug_assert_eq!(self.kind, ClaimKind::DispatchBackoffArmed);
        self.consecutive = Some(streak);
        self.streak_break = Some(false);
        self
    }

    /// Turn a [`ClaimKind::DispatchBackoffArmed`] ad into a fast-fail streak
    /// **break** (#11930, marked explicitly since #11952): a zero window, a
    /// zero streak, and [`ClaimAd::streak_break`] `= Some(true)`. The zero
    /// window and streak are what a pre-#11952 receiver reads as a break.
    #[must_use]
    pub fn as_fast_fail_break(mut self) -> Self {
        debug_assert_eq!(self.kind, ClaimKind::DispatchBackoffArmed);
        self.remaining_secs = Some(0);
        self.consecutive = Some(0);
        self.streak_break = Some(true);
        self
    }
}

/// Whether `ad` is a fast-fail streak break (#11952): explicitly marked, or a
/// pre-#11952 sender's zero window with a zero streak. See the module docs'
/// table.
#[must_use]
pub fn is_fast_fail_break(ad: &ClaimAd) -> bool {
    match ad.streak_break {
        Some(marked) => marked,
        None => ad.consecutive == Some(0) && ad.remaining_secs.unwrap_or(0) == 0,
    }
}

impl PeerClaimView {
    /// Fold the fast-failure streak an inbound `DispatchBackoffArmed` ad
    /// carries into the per-host tally (Issue #11930), and prune lapsed
    /// entries. Called by [`Self::observe_dispatch_backoff_at`] after it has
    /// discarded this host's own ad. Returns whether the ad was a **break**
    /// (see the module docs), which the caller needs: an explicit break
    /// carries no window and must not touch the window map (#11952).
    pub(super) fn observe_fast_fail_streak_at(&mut self, ad: &ClaimAd, now: Instant) -> bool {
        self.fast_fail_streaks.retain(|_, (_, at)| {
            now.saturating_duration_since(*at) < MAX_PEER_FAST_FAIL_STREAK_AGE
        });
        let wall_now = chrono::Utc::now();
        let max_age = chrono::Duration::from_std(MAX_PEER_FAST_FAIL_STREAK_AGE)
            .unwrap_or_else(|_| chrono::Duration::days(1));
        self.fast_fail_breaks
            .retain(|_, at| wall_now.signed_duration_since(*at) < max_age);
        let unresolved = ad.host == crate::sweep_registry::UNKNOWN_HOST;
        if is_fast_fail_break(ad) {
            // An unresolved sender may be our own echo, so it never breaks
            // anything.
            if !unresolved {
                self.clear_fast_fail_streaks(&ad.repo, ad.issue);
                self.fast_fail_breaks
                    .insert((ad.repo.clone(), ad.issue), wall_now);
            }
            return true;
        }
        let Some(streak) = ad.consecutive else {
            return false;
        };
        // An unresolved sender may be our own echo: count it as nothing (#9518).
        let streak = if unresolved { 0 } else { streak };
        let key = (ad.repo.clone(), ad.issue, ad.host.clone());
        if streak == 0 {
            self.fast_fail_streaks.remove(&key);
            return false;
        }
        self.fast_fail_streaks.insert(key, (streak, now));
        false
    }

    /// When this view last received a fast-fail streak **break** for
    /// `(repo, issue)` from a peer, on this host's wall clock (#11952), or
    /// `None`. The sweep registry treats its own fast-failure record from
    /// before this instant as broken. Kept for
    /// [`MAX_PEER_FAST_FAIL_STREAK_AGE`]; by then a record the registry has
    /// not rewritten is past any default ladder `max` anyway.
    #[must_use]
    pub fn fast_fail_break_at(
        &self,
        repo: &str,
        issue: u32,
    ) -> Option<chrono::DateTime<chrono::Utc>> {
        self.fast_fail_breaks
            .get(&(repo.to_string(), issue))
            .copied()
    }

    /// The sum of every **peer** host's fast-failure streak on `(repo, issue)`
    /// received less than `max_age` before `now` (Issue #11930): the term the
    /// fast-fail park adds to its own local streak. Saturating; `0` when no
    /// peer has reported one.
    #[must_use]
    pub fn fast_fail_peer_streak_at(
        &self,
        repo: &str,
        issue: u32,
        now: Instant,
        max_age: Duration,
    ) -> u32 {
        let max_age = max_age.min(MAX_PEER_FAST_FAIL_STREAK_AGE);
        self.fast_fail_streaks
            .iter()
            .filter(|((r, i, _), (_, at))| {
                r == repo && *i == issue && now.saturating_duration_since(*at) < max_age
            })
            .fold(0u32, |acc, (_, (n, _))| acc.saturating_add(*n))
    }

    /// Drop every peer fast-failure streak on `(repo, issue)` (Issue #11930):
    /// this host saw the issue make real progress, so the fleet streak is
    /// broken here too. Returns whether any entry existed.
    pub fn clear_fast_fail_streaks(&mut self, repo: &str, issue: u32) -> bool {
        let before = self.fast_fail_streaks.len();
        self.fast_fail_streaks
            .retain(|(r, i, _), _| !(r == repo && *i == issue));
        self.fast_fail_streaks.len() != before
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const SIX_HOURS: Duration = Duration::from_secs(6 * 3600);

    fn ad(host: &str, issue: u32, streak: Option<u32>, remaining: u64) -> ClaimAd {
        let ad = ClaimAd::dispatch_backoff_armed(
            issue,
            "rjwalters/loom".into(),
            host.into(),
            1,
            "ts".into(),
            remaining,
        );
        match streak {
            Some(n) => ad.with_fast_fail_streak(n),
            None => ad,
        }
    }

    fn view() -> PeerClaimView {
        PeerClaimView::new("self".into(), Duration::from_secs(120))
    }

    #[test]
    fn the_streak_rides_the_wire() {
        let sent = ad("host-a", 7, Some(2), 120);
        let back = ClaimAd::from_body_str(&sent.to_body_json()).unwrap();
        assert_eq!(back.consecutive, Some(2));
        assert_eq!(back.kind, ClaimKind::DispatchBackoffArmed);
    }

    #[test]
    fn peer_streaks_sum_across_hosts_and_ignore_our_own() {
        let mut v = view();
        let t = Instant::now();
        v.observe_dispatch_backoff_at(&ad("host-a", 7, Some(1), 60), t);
        v.observe_dispatch_backoff_at(&ad("host-b", 7, Some(1), 60), t);
        v.observe_dispatch_backoff_at(&ad("self", 7, Some(5), 60), t);
        assert_eq!(v.fast_fail_peer_streak_at("rjwalters/loom", 7, t, SIX_HOURS), 2);
        assert_eq!(v.fast_fail_peer_streak_at("rjwalters/loom", 8, t, SIX_HOURS), 0);
        assert_eq!(v.fast_fail_peer_streak_at("other/repo", 7, t, SIX_HOURS), 0);
    }

    #[test]
    fn a_repeat_ad_replaces_and_a_zero_breaks_the_hosts_streak() {
        let mut v = view();
        let t = Instant::now();
        v.observe_dispatch_backoff_at(&ad("host-a", 7, Some(1), 60), t);
        v.observe_dispatch_backoff_at(&ad("host-a", 7, Some(2), 120), t);
        assert_eq!(v.fast_fail_peer_streak_at("rjwalters/loom", 7, t, SIX_HOURS), 2);
        // host-a's next window was a transient cause: its streak is broken.
        v.observe_dispatch_backoff_at(&ad("host-a", 7, Some(0), 60), t);
        assert_eq!(v.fast_fail_peer_streak_at("rjwalters/loom", 7, t, SIX_HOURS), 0);
    }

    #[test]
    fn a_countless_or_unidentified_ad_adds_nothing() {
        let mut v = view();
        let t = Instant::now();
        v.observe_dispatch_backoff_at(&ad("host-a", 7, None, 60), t);
        let unknown = ad(crate::sweep_registry::UNKNOWN_HOST, 7, Some(3), 60);
        v.observe_dispatch_backoff_at(&unknown, t);
        assert_eq!(v.fast_fail_peer_streak_at("rjwalters/loom", 7, t, SIX_HOURS), 0);
        // ...but both still arm the window, as before.
        assert!(v
            .dispatch_backoff_issues_at("rjwalters/loom", t)
            .contains(&7));
    }

    #[test]
    fn a_zero_remaining_clear_still_records_the_streak() {
        let mut v = view();
        let t = Instant::now();
        v.observe_dispatch_backoff_at(&ad("host-a", 7, Some(2), 0), t);
        assert_eq!(v.fast_fail_peer_streak_at("rjwalters/loom", 7, t, SIX_HOURS), 2);
    }

    #[test]
    fn a_streak_older_than_max_goes_cold() {
        let mut v = view();
        let t = Instant::now();
        v.observe_dispatch_backoff_at(&ad("host-a", 7, Some(2), 60), t);
        let later = t + Duration::from_secs(901);
        assert_eq!(
            v.fast_fail_peer_streak_at("rjwalters/loom", 7, later, Duration::from_secs(900)),
            0
        );
        assert_eq!(v.fast_fail_peer_streak_at("rjwalters/loom", 7, later, SIX_HOURS), 2);
    }

    #[test]
    fn the_break_marker_rides_the_wire() {
        let brk = ad("host-a", 7, None, 60).as_fast_fail_break();
        let back = ClaimAd::from_body_str(&brk.to_body_json()).unwrap();
        assert_eq!(
            (back.streak_break, back.consecutive, back.remaining_secs),
            (Some(true), Some(0), Some(0))
        );
        let plain = ClaimAd::from_body_str(&ad("host-a", 7, Some(1), 60).to_body_json()).unwrap();
        assert_eq!(plain.streak_break, Some(false));
    }

    #[test]
    fn the_break_table() {
        let mut legacy = ad("host-a", 7, None, 0);
        legacy.consecutive = Some(0);
        assert!(is_fast_fail_break(&legacy), "unmarked 0/0 is the pre-#11952 break");
        let marked_false = ad("host-a", 7, Some(0), 0);
        assert!(!is_fast_fail_break(&marked_false), "a marked ordinary ad never is");
        assert!(is_fast_fail_break(&ad("host-a", 7, None, 60).as_fast_fail_break()));
        assert!(!is_fast_fail_break(&ad("host-a", 7, None, 0)), "a countless clear is not");
    }

    #[test]
    fn a_marked_zero_zero_ad_breaks_only_the_senders_entry() {
        let mut v = view();
        let t = Instant::now();
        v.observe_dispatch_backoff_at(&ad("host-a", 7, Some(2), 60), t);
        v.observe_dispatch_backoff_at(&ad("host-c", 7, Some(1), 60), t);
        v.observe_dispatch_backoff_at(&ad("host-a", 7, Some(0), 0), t);
        assert_eq!(v.fast_fail_peer_streak_at("rjwalters/loom", 7, t, SIX_HOURS), 1);
        assert!(v.fast_fail_break_at("rjwalters/loom", 7).is_none());
    }

    #[test]
    fn a_break_is_recorded_and_an_unresolved_one_ignored() {
        let mut v = view();
        let t = Instant::now();
        v.observe_dispatch_backoff_at(&ad("host-a", 7, Some(2), 60), t);
        let unknown = ad(crate::sweep_registry::UNKNOWN_HOST, 7, None, 0).as_fast_fail_break();
        v.observe_dispatch_backoff_at(&unknown, t);
        assert_eq!(v.fast_fail_peer_streak_at("rjwalters/loom", 7, t, SIX_HOURS), 2);
        assert!(v.fast_fail_break_at("rjwalters/loom", 7).is_none());
        v.observe_dispatch_backoff_at(&ad("host-b", 7, None, 0).as_fast_fail_break(), t);
        assert_eq!(v.fast_fail_peer_streak_at("rjwalters/loom", 7, t, SIX_HOURS), 0);
        assert!(v.fast_fail_break_at("rjwalters/loom", 7).is_some());
        assert!(v.fast_fail_break_at("rjwalters/loom", 8).is_none());
    }

    #[test]
    fn progress_clears_the_peer_streaks_for_that_issue_only() {
        let mut v = view();
        let t = Instant::now();
        v.observe_dispatch_backoff_at(&ad("host-a", 7, Some(2), 60), t);
        v.observe_dispatch_backoff_at(&ad("host-a", 8, Some(1), 60), t);
        assert!(v.clear_fast_fail_streaks("rjwalters/loom", 7));
        assert_eq!(v.fast_fail_peer_streak_at("rjwalters/loom", 7, t, SIX_HOURS), 0);
        assert_eq!(v.fast_fail_peer_streak_at("rjwalters/loom", 8, t, SIX_HOURS), 1);
    }
}
