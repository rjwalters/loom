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

use std::time::Instant;

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

impl PeerClaimView {
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

    #[test]
    fn this_hosts_own_merge_step_is_not_a_peers() {
        let mut view = PeerClaimView::new("me".into(), Duration::from_secs(60));
        let now = Instant::now();
        view.observe_at(&ad(ClaimKind::Advertise, 30, StepRole::ChampionMerge, "me"), now);
        assert_eq!(view.merge_in_flight_at("o/r", now), None);
    }
}
