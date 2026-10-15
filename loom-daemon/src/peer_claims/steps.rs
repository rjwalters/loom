//! Step claims on the peer channel (Issue #11898; design #11894 §5–§6).
//!
//! A step advertises `(item, step)`: the ordinary `Advertise`/`Retract` ad,
//! with `issue` carrying the step's item and the additive `step` field naming
//! the step. An older peer ignores the field and holds the item claimed, which
//! is correct: the step holds it.
//!
//! The Champion **merge** step's advertisement is also the best-effort,
//! repo-keyed merge serialisation the design calls for: a host about to merge
//! in a repo asks [`PeerClaimView::merge_in_flight_at`] whether a peer is
//! already merging there. It is a soft signal like every peer claim. The
//! merge's correctness guards stay Champion's own (the merge-queue handoff,
//! else `merge-pr.sh --expected-head`).
//!
//! # Step cooldowns (#11900)
//!
//! [`ClaimKind::StepCooldownArmed`] carries the step-outcome ledger's short
//! window for one `(item, step)`: the brake-lane sibling of
//! `DispatchBackoffArmed`, keyed by the ad's `step` as well as its item. The
//! receiver measures the window against local receipt, caps it at
//! [`MAX_PEER_STEP_COOLDOWN`] (the ledger's own 24 h ceiling), never lets a
//! shorter window shorten a longer one, and reads zero as a clear. An older
//! peer does not know the kind and drops the ad, which is the safe
//! direction: it never reads a step cooldown as an issue-wide backoff.

use std::time::{Duration, Instant};

use super::{ClaimAd, ClaimKind, PeerClaimView};
use crate::step_role::StepRole;

impl ClaimAd {
    /// A step's claim (`Advertise`) or its early release (`Retract`) on
    /// `item`. Any other kind is not a step lane and is returned as-is
    /// without a step.
    #[must_use]
    pub fn step(
        kind: ClaimKind,
        item: u32,
        step: StepRole,
        repo: String,
        host: String,
        pid: u32,
        ts: String,
    ) -> Self {
        let mut ad = match kind {
            ClaimKind::Retract => Self::retract(item, repo, host, pid, ts),
            _ => Self::advertise(item, repo, host, pid, ts),
        };
        ad.step = Some(step);
        ad
    }
}

/// Ceiling on the window one [`ClaimKind::StepCooldownArmed`] ad installs.
pub const MAX_PEER_STEP_COOLDOWN: Duration =
    crate::step_dispatch::outcome_ledger::MAX_SHORT_COOLDOWN;

impl ClaimAd {
    /// "`(item, step)` is cooling down for `remaining_secs` more seconds"
    /// (#11900). Zero lifts it.
    #[must_use]
    pub fn step_cooldown_armed(
        item: u32,
        step: StepRole,
        repo: String,
        host: String,
        pid: u32,
        ts: String,
        remaining_secs: u64,
    ) -> Self {
        let mut ad = Self::advertise(item, repo, host, pid, ts);
        ad.kind = ClaimKind::StepCooldownArmed;
        ad.remaining_secs = Some(remaining_secs);
        ad.step = Some(step);
        ad
    }
}

impl PeerClaimView {
    /// Observe a [`ClaimKind::StepCooldownArmed`] ad at local time `now`.
    /// Returns `false` for this host's own ad and for an ad naming no step;
    /// otherwise applies it (extend-only, capped, zero clears).
    pub fn observe_step_cooldown_at(&mut self, ad: &ClaimAd, now: Instant) -> bool {
        debug_assert_eq!(ad.kind, ClaimKind::StepCooldownArmed);
        let is_unresolved_identity = ad.host == crate::sweep_registry::UNKNOWN_HOST;
        if ad.host == self.self_host && !is_unresolved_identity {
            return false;
        }
        let Some(step) = ad.step else {
            return false;
        };
        let key = (ad.repo.clone(), ad.issue, step);
        let remaining = Duration::from_secs(ad.remaining_secs.unwrap_or(0));
        if remaining.is_zero() {
            self.step_cooldowns.remove(&key);
            return true;
        }
        let expiry = now + remaining.min(MAX_PEER_STEP_COOLDOWN);
        let entry = self.step_cooldowns.entry(key).or_insert(expiry);
        *entry = (*entry).max(expiry);
        true
    }

    /// Drop every peer step cooldown on `item` in `repo` (#11900): a verdict
    /// on this host changed the item's state. Returns how many were dropped.
    pub fn clear_step_cooldowns(&mut self, repo: &str, item: u32) -> usize {
        let before = self.step_cooldowns.len();
        self.step_cooldowns
            .retain(|(r, i, _), _| !(r == repo && *i == item));
        before - self.step_cooldowns.len()
    }

    /// Drop lapsed peer step cooldowns.
    pub fn prune_expired_step_cooldowns(&mut self, now: Instant) {
        self.step_cooldowns.retain(|_, expiry| *expiry > now);
    }

    /// Every live peer step cooldown in `repo` at local time `now`, as
    /// `(item, step, remaining)`.
    #[must_use]
    pub fn step_cooldowns_at(&self, repo: &str, now: Instant) -> Vec<(u32, StepRole, Duration)> {
        self.step_cooldowns
            .iter()
            .filter(|((r, _, _), expiry)| r == repo && **expiry > now)
            .map(|((_, item, step), expiry)| (*item, *step, expiry.saturating_duration_since(now)))
            .collect()
    }

    /// The step a live peer claim on `item` in `repo` holds, at local time
    /// `now`. `None` when the item is unclaimed, or claimed by a sweep.
    #[must_use]
    pub fn step_claim_at(&self, repo: &str, item: u32, now: Instant) -> Option<StepRole> {
        self.claims
            .get(&(repo.to_owned(), item))
            .filter(|e| now.saturating_duration_since(e.received_at) < self.ttl)
            .and_then(|e| e.step)
    }

    /// A live peer Champion merge step in `repo` at local time `now`: the PR
    /// and the host merging it. The lowest PR number when several are live.
    #[must_use]
    pub fn merge_in_flight_at(&self, repo: &str, now: Instant) -> Option<(u32, String)> {
        self.claims
            .iter()
            .filter(|((r, _), e)| {
                r == repo
                    && e.step == Some(StepRole::ChampionMerge)
                    && now.saturating_duration_since(e.received_at) < self.ttl
            })
            .map(|((_, pr), e)| (*pr, e.host.clone()))
            .min()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ad(kind: ClaimKind, item: u32, step: StepRole, host: &str) -> ClaimAd {
        ClaimAd::step(kind, item, step, "o/r".into(), host.into(), 1, "t".into())
    }

    #[test]
    fn the_step_field_round_trips_and_is_absent_for_a_sweep() {
        let wire = ad(ClaimKind::Advertise, 21, StepRole::Judge, "peer").to_body_json();
        let v: serde_json::Value = serde_json::from_str(&wire).unwrap();
        assert_eq!(v["step"], "judge");
        assert_eq!(v["issue"], 21);
        let back = ClaimAd::from_body_value(&v).unwrap();
        assert_eq!(back.step, Some(StepRole::Judge));

        let sweep = ClaimAd::advertise(5, "o/r".into(), "peer".into(), 1, "t".into());
        let v: serde_json::Value = serde_json::from_str(&sweep.to_body_json()).unwrap();
        assert!(v["step"].is_null());
        assert_eq!(ClaimAd::from_body_value(&v).unwrap().step, None);
    }

    /// An older peer's ad (no `step` key) and an unknown role name both read as
    /// a plain claim on the item, never a rejected ad.
    #[test]
    fn an_absent_or_unknown_step_degrades_to_a_plain_claim() {
        let mut v: serde_json::Value = serde_json::from_str(
            &ad(ClaimKind::Advertise, 9, StepRole::Curator, "peer").to_body_json(),
        )
        .unwrap();
        v["step"] = serde_json::Value::String("some_future_step".into());
        let back = ClaimAd::from_body_value(&v).unwrap();
        assert_eq!((back.issue, back.step), (9, None));
        v.as_object_mut().unwrap().remove("step");
        assert_eq!(ClaimAd::from_body_value(&v).unwrap().step, None);
    }

    #[test]
    fn a_peer_merge_step_is_visible_per_repo_until_retracted_or_expired() {
        let mut view = PeerClaimView::new("me".into(), Duration::from_secs(60));
        let now = Instant::now();
        view.observe_at(&ad(ClaimKind::Advertise, 30, StepRole::ChampionMerge, "peer"), now);
        view.observe_at(&ad(ClaimKind::Advertise, 31, StepRole::Judge, "peer"), now);

        assert_eq!(view.merge_in_flight_at("o/r", now), Some((30, "peer".to_string())));
        assert_eq!(view.merge_in_flight_at("other/repo", now), None);
        assert_eq!(view.step_claim_at("o/r", 31, now), Some(StepRole::Judge));
        assert!(view.is_claimed_at("o/r", 31, now), "a step claim is a claim on its item");

        let later = now + Duration::from_secs(61);
        assert_eq!(view.merge_in_flight_at("o/r", later), None, "expired");

        view.observe_at(&ad(ClaimKind::Retract, 30, StepRole::ChampionMerge, "peer"), now);
        assert_eq!(view.merge_in_flight_at("o/r", now), None, "retracted");
    }

    fn cooldown(item: u32, step: StepRole, host: &str, secs: u64) -> ClaimAd {
        ClaimAd::step_cooldown_armed(item, step, "o/r".into(), host.into(), 1, "t".into(), secs)
    }

    /// The wire form carries kind, step and window; an older peer cannot
    /// parse the kind, so it never mistakes the ad for an issue backoff.
    #[test]
    fn a_step_cooldown_round_trips_and_routes_to_the_brake_lane() {
        let ad = cooldown(40, StepRole::Judge, "peer", 600);
        let v: serde_json::Value = serde_json::from_str(&ad.to_body_json()).unwrap();
        assert_eq!(v["kind"], "step_cooldown_armed");
        assert_eq!(v["step"], "judge");
        assert_eq!(v["remaining_secs"], 600);
        assert_eq!(ClaimAd::from_body_value(&v).unwrap(), ad);
        assert!(ClaimKind::StepCooldownArmed.is_cooldown_lane());

        let mut view = PeerClaimView::new("me".into(), Duration::from_secs(60));
        let now = Instant::now();
        super::super::brakes::observe_brake_ad(&mut view, &ad, now);
        let live = view.step_cooldowns_at("o/r", now);
        assert_eq!(live, vec![(40, StepRole::Judge, Duration::from_secs(600))]);
        assert!(!view.is_claimed_at("o/r", 40, now), "a cooldown is not a claim");
        assert!(view.step_cooldowns_at("other/repo", now).is_empty());
    }

    /// The 24 h bound holds on receipt: a peer advertising a week installs a
    /// day. Extend-only; zero clears; own ads and step-less ads are ignored.
    #[test]
    fn a_peer_step_cooldown_is_capped_at_24h_extend_only_and_zero_clears() {
        let mut view = PeerClaimView::new("me".into(), Duration::from_secs(60));
        let now = Instant::now();
        assert!(
            view.observe_step_cooldown_at(&cooldown(7, StepRole::Builder, "peer", 7 * 86_400), now)
        );
        let day = Duration::from_secs(86_400);
        assert_eq!(view.step_cooldowns_at("o/r", now), vec![(7, StepRole::Builder, day)]);

        view.observe_step_cooldown_at(&cooldown(7, StepRole::Builder, "peer", 60), now);
        assert_eq!(view.step_cooldowns_at("o/r", now)[0].2, day, "a shorter window never shortens");

        view.observe_step_cooldown_at(&cooldown(7, StepRole::Curator, "peer", 60), now);
        assert_eq!(view.step_cooldowns_at("o/r", now).len(), 2, "keyed by step too");

        view.observe_step_cooldown_at(&cooldown(7, StepRole::Builder, "peer", 0), now);
        assert_eq!(
            view.step_cooldowns_at("o/r", now),
            vec![(7, StepRole::Curator, Duration::from_secs(60))]
        );

        assert!(!view.observe_step_cooldown_at(&cooldown(8, StepRole::Judge, "me", 60), now));
        let mut stepless = cooldown(9, StepRole::Judge, "peer", 60);
        stepless.step = None;
        assert!(!view.observe_step_cooldown_at(&stepless, now));

        let later = now + Duration::from_secs(61);
        view.prune_expired_step_cooldowns(later);
        assert!(view.step_cooldowns_at("o/r", later).is_empty());
    }

    #[test]
    fn this_hosts_own_merge_step_is_not_a_peers() {
        let mut view = PeerClaimView::new("me".into(), Duration::from_secs(60));
        let now = Instant::now();
        view.observe_at(&ad(ClaimKind::Advertise, 30, StepRole::ChampionMerge, "me"), now);
        assert_eq!(view.merge_in_flight_at("o/r", now), None);
    }
}
