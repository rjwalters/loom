//! Claim-episode membership for the claim-then-verify-order tie-break
//! (Issue #8840).
//!
//! # The question this module answers
//!
//! [`SweepRegistry::resolve_lease_order`](super::SweepRegistry::resolve_lease_order)
//! decides whether a dispatcher that just flipped `loom:building` and wrote
//! its own lease record is the *earliest* claimant, by forge-assigned
//! comment `id`. Before it can order anything it must decide **which**
//! records to order: an issue accumulates one lease comment per dispatch
//! over its entire lifetime (comments are never deleted), so comparing
//! against the full history would make every normal, uncontested dispatch
//! lose to some long-finished claim from months ago. That membership
//! predicate lives here.
//!
//! Ordering stays entirely in `guards.rs` and stays entirely `id`-based.
//! Nothing here breaks an order tie; this module only decides what is
//! compared at all.
//!
//! # The 2026-09-24 collision (loom#8787) this module exists for
//!
//! Issue #6287 defined membership as "`created_at` within
//! [`LEASE_ORDER_LOOKBACK_SECS`] of this dispatch attempt". That is correct
//! for the race it was written for — two dispatchers arriving within
//! seconds of each other — and wrong for every claim held longer than the
//! window, because **a lease record is renewed in place**. The renewal loop
//! (`defaults/scripts/sweep-lease-renew.sh`) PATCHes the *same* comment, so
//! a live owner's `created_at` stays frozen at the instant its claim episode
//! began while only `updated_at` advances.
//!
//! The forge evidence from the incident, on loom#8787:
//!
//! | record | host | `created_at` | `updated_at` |
//! |---|---|---|---|
//! | comment 5822662982 | `host-d9142cf3` (in-session sweep) | 21:37:54Z | 21:42:08Z (renewed) |
//! | comment 5822727954 | `host-f4fe7748` (peer daemon) | 21:42:16Z | — |
//!
//! The peer dispatched eight seconds after a *successful* renewal of a
//! lease that was, by every liveness definition the fleet uses, alive. But
//! by `created_at` that lease was 262 seconds old — 2.9× the 90-second
//! lookback — so it never entered the comparison set. The peer saw only its
//! own record, concluded "sole claimant", and spawned a second owner of the
//! same issue.
//!
//! Every other mechanism behaved correctly, which is why this was not
//! caught earlier: the pre-flip label read (#4085) saw no claim label,
//! because the in-session sweep held its lease *before* promoting the issue
//! and flipping `loom:building`; the in-session pre-push fence
//! (`sweep-lease-fence.sh`) did fire, fail-closed, and stopped the *earlier*
//! owner from pushing. The record that should have produced a `Yield` was
//! filtered out before it could be compared.
//!
//! # What changed
//!
//! Membership gains a second, renewal-anchored leg. See
//! [`in_claim_episode`] for the exact predicate, the `locally_terminal`
//! subtraction, and why the change can only ever ADD a refusal.

use chrono::{DateTime, Utc};

use super::guards::{LeaseComment, LEASE_ORDER_LOOKBACK_SECS};
use super::SweepRegistry;

/// The instant before which a lease record's forge-assigned `created_at` is
/// too old to make it a participant in this near-simultaneous race — leg 1
/// of [`in_claim_episode`], byte-for-byte the bound Issue #6287 shipped.
///
/// Derived here rather than at the call site so both cutoffs are computed
/// from the one `episode_start` clock reading the dispatch attempt captured
/// before its own label flip, and so neither leg's window can be widened or
/// narrowed in one of `resolve_lease_order`'s two read phases without the
/// other.
#[must_use]
pub(crate) fn created_cutoff(episode_start: DateTime<Utc>) -> DateTime<Utc> {
    episode_start - chrono::Duration::seconds(LEASE_ORDER_LOOKBACK_SECS)
}

/// The instant before which a lease record's forge-assigned `updated_at` is
/// too old to count as a live renewal, anchored — like
/// [`created_cutoff`] — to the dispatch attempt's `episode_start` rather
/// than a fresh `Utc::now()`, so both legs of [`in_claim_episode`] measure
/// from one clock reading.
///
/// Deliberately the SAME window the reclamation gate already uses
/// ([`crate::claim_reconciliation::resolve_lease_ttl_minutes`]: default 15
/// minutes, env `LOOM_LEASE_TTL_MINUTES`, matched by
/// `defaults/scripts/sweep-lease-fence.sh`'s own `DEFAULT_TTL_MINUTES`).
/// That shared definition is the point. A lease the reclaimer considers
/// fresh enough to REFUSE reclaiming the claim for must not simultaneously
/// be invisible to the dispatcher — that combination is precisely what lets
/// a host duplicate a build against an owner the rest of the fleet has
/// already agreed is alive. The renewal loop's own cadence (300s) sits
/// comfortably inside the window, so three consecutive missed renewals
/// still do not misclassify a live owner.
#[must_use]
pub(crate) fn renewal_cutoff(episode_start: DateTime<Utc>) -> DateTime<Utc> {
    let ttl_minutes = crate::claim_reconciliation::resolve_lease_ttl_minutes();
    // Clamp before casting: a garbage env override must not produce a NaN
    // or an overflowing `i64`, either of which would poison the comparison
    // rather than merely mis-size the window.
    let secs = (ttl_minutes * 60.0).clamp(0.0, 365.0 * 24.0 * 3600.0) as i64;
    episode_start - chrono::Duration::seconds(secs)
}

/// Whether `c` belongs to the claim episode
/// [`SweepRegistry::resolve_lease_order`](super::SweepRegistry::resolve_lease_order)
/// is adjudicating.
///
/// # Leg 1 — created within the lookback (Issue #6287, unchanged)
///
/// A record whose forge-assigned `created_at` is at or after
/// `created_cutoff` (`episode_start - `[`LEASE_ORDER_LOOKBACK_SECS`]) is a
/// participant in this near-simultaneous race. This leg is byte-for-byte
/// the behavior #6287 shipped, including its treatment of an absent
/// `created_at` as non-membership.
///
/// # Leg 2 — created earlier but RENEWED since (Issue #8840)
///
/// A record that fell out of leg 1 is re-admitted when it carries positive,
/// forge-assigned evidence of being alive: both timestamps present,
/// `updated_at > created_at` (genuinely renewed at least once, not merely
/// created), and `updated_at` at or after `renewal_cutoff` (see
/// [`renewal_cutoff`]).
///
/// This reads the forge's own `updated_at` field — never a timestamp
/// embedded in the comment's prose, which `defaults/docs/lease-record.md`
/// explicitly forbids treating as liveness, and never the advertising
/// host's own clock as carried in the body text.
///
/// A record that simply aged out — a finished sweep whose renewal loop
/// stopped — fails leg 2 exactly as it failed leg 1, so the anti-wedge
/// property [`LEASE_ORDER_LOOKBACK_SECS`] exists for is preserved: only an
/// *actively renewed* old record is admitted, never merely an old one.
///
/// # `locally_terminal` — the one subtraction, and why
///
/// `locally_terminal` is "this daemon's own registry holds an entry for
/// `c.sweep_id` and knows it is no longer running"
/// ([`crate::types::SweepState::is_terminal`]). Such a record's renewal
/// loop is dead, so its `updated_at` is frozen and merely has not yet aged
/// out of the TTL — it describes a finished sweep, not a live owner.
/// Admitting it would block this host's own legitimate re-dispatch (a
/// PR-less retry, a crash resume) for up to a full TTL after its previous
/// attempt ended. Local registry state is strictly better evidence than a
/// forge read for a sweep this daemon itself ran, so it wins.
///
/// A sweep this daemon has never heard of is not terminal-known and so is
/// fully covered by leg 2. That is every peer host's sweep, and every
/// in-session sweep — which is precisely the #8840 case.
///
/// The subtraction applies to leg 2 only. Leg 1's membership is left
/// exactly as #6287 defined it.
///
/// # Direction of the change
///
/// Strictly additive: leg 2 can only move a record from "not compared" to
/// "compared", and being compared can only ever produce a `Yield` (a
/// refusal on positive evidence of an earlier live claim), never a
/// `Proceed` that would not have happened anyway. The surrounding
/// fail-open behavior on unreadable or ambiguous reads is untouched.
#[must_use]
pub(crate) fn in_claim_episode(
    c: &LeaseComment,
    created_cutoff: DateTime<Utc>,
    renewal_cutoff: DateTime<Utc>,
    locally_terminal: bool,
) -> bool {
    if c.created_at.is_some_and(|ts| ts >= created_cutoff) {
        return true;
    }
    if locally_terminal {
        return false;
    }
    let (Some(created), Some(updated)) = (c.created_at, c.updated_at) else {
        return false;
    };
    updated > created && updated >= renewal_cutoff
}

/// Apply [`in_claim_episode`] across a whole read-back, supplying the one
/// piece of context the pure predicate cannot derive on its own: whether
/// THIS daemon's registry already knows `sweep_id` to be finished.
///
/// Both the initial read in
/// [`SweepRegistry::resolve_lease_order`](super::SweepRegistry::resolve_lease_order)
/// and every confirmation re-read in `confirm_sole_claim` route through
/// here, so the two phases can never drift apart on what "in this claim
/// episode" means.
pub(crate) fn in_episode<'a>(
    registry: &SweepRegistry,
    comments: &'a [LeaseComment],
    created_cutoff: DateTime<Utc>,
    renewal_cutoff: DateTime<Utc>,
) -> Vec<&'a LeaseComment> {
    comments
        .iter()
        .filter(|c| {
            let locally_terminal = registry
                .get(&c.sweep_id)
                .is_some_and(|entry| entry.state.is_terminal());
            in_claim_episode(c, created_cutoff, renewal_cutoff, locally_terminal)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    /// Build a lease comment with `created_at`/`updated_at` expressed as
    /// seconds before `now`.
    fn lease(id: u64, now: DateTime<Utc>, created_ago: i64, updated_ago: i64) -> LeaseComment {
        LeaseComment {
            id,
            created_at: Some(now - Duration::seconds(created_ago)),
            updated_at: Some(now - Duration::seconds(updated_ago)),
            host: "peer-host".into(),
            sweep_id: format!("sweep-{id}"),
        }
    }

    fn cutoffs(now: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
        (created_cutoff(now), renewal_cutoff(now))
    }

    /// Leg 1, unchanged from #6287: a record created inside the lookback is
    /// a member regardless of renewal state.
    #[test]
    fn a_record_created_inside_the_lookback_is_a_member() {
        let now = Utc::now();
        let (created_cutoff, renewal_cutoff) = cutoffs(now);
        assert!(in_claim_episode(&lease(1, now, 10, 10), created_cutoff, renewal_cutoff, false));
    }

    /// The #8840 regression, at the predicate level, using the exact
    /// timings recovered from the loom#8787 forge evidence: created 262s
    /// before the racing dispatch (2.9× the 90s lookback), renewed 8s
    /// before it. Pre-#8840 this record was filtered out and the peer
    /// proceeded; it must now be a member so the `id` comparison can see
    /// it.
    #[test]
    fn a_renewed_record_created_before_the_lookback_is_still_a_member() {
        let now = Utc::now();
        let (created_cutoff, renewal_cutoff) = cutoffs(now);
        let incident = lease(1, now, 262, 8);
        assert!(
            incident.created_at.unwrap() < created_cutoff,
            "the fixture must genuinely fall outside leg 1, or it proves nothing"
        );
        assert!(in_claim_episode(&incident, created_cutoff, renewal_cutoff, false));
    }

    /// The anti-wedge property `LEASE_ORDER_LOOKBACK_SECS` exists for: an
    /// OLD record that was never renewed (a real forge sets
    /// `updated_at == created_at` at creation) stays out. Otherwise every
    /// uncontested dispatch would lose to some long-finished claim.
    #[test]
    fn an_old_never_renewed_record_is_not_a_member() {
        let now = Utc::now();
        let (created_cutoff, renewal_cutoff) = cutoffs(now);
        assert!(!in_claim_episode(
            &lease(1, now, 3600, 3600),
            created_cutoff,
            renewal_cutoff,
            false
        ));
    }

    /// A record whose renewal loop stopped — the shape a finished or
    /// crashed sweep leaves behind — ages out of the TTL and stops being a
    /// member, so an abandoned claim can never wedge redispatch forever.
    #[test]
    fn a_record_whose_last_renewal_aged_past_the_ttl_is_not_a_member() {
        let now = Utc::now();
        let (created_cutoff, renewal_cutoff) = cutoffs(now);
        // Renewed 30 minutes ago; the default TTL is 15.
        assert!(!in_claim_episode(
            &lease(1, now, 7200, 1800),
            created_cutoff,
            renewal_cutoff,
            false
        ));
    }

    /// The `locally_terminal` subtraction: a still-in-TTL renewed record
    /// belonging to a sweep THIS daemon knows has finished must not block
    /// this host's own re-dispatch. Local registry state beats a forge read
    /// for a sweep this daemon itself ran.
    #[test]
    fn a_renewed_record_for_a_locally_finished_sweep_is_not_a_member() {
        let now = Utc::now();
        let (created_cutoff, renewal_cutoff) = cutoffs(now);
        let c = lease(1, now, 262, 8);
        assert!(
            in_claim_episode(&c, created_cutoff, renewal_cutoff, false),
            "precondition: this record IS a member while the sweep is unknown/live"
        );
        assert!(!in_claim_episode(&c, created_cutoff, renewal_cutoff, true));
    }

    /// `locally_terminal` never subtracts from leg 1 — a record created
    /// inside the lookback stays a member exactly as #6287 defined it, so
    /// the near-simultaneous race this tie-break was built for cannot be
    /// weakened by the new subtraction.
    #[test]
    fn locally_terminal_never_subtracts_from_the_lookback_leg() {
        let now = Utc::now();
        let (created_cutoff, renewal_cutoff) = cutoffs(now);
        assert!(in_claim_episode(&lease(1, now, 10, 10), created_cutoff, renewal_cutoff, true));
    }

    /// Missing forge timestamps are never invented into liveness: a record
    /// with no `created_at`, or an old one with no `updated_at`, is not a
    /// member. The tie-break only ever refuses on positive evidence.
    #[test]
    fn absent_forge_timestamps_are_not_evidence_of_liveness() {
        let now = Utc::now();
        let (created_cutoff, renewal_cutoff) = cutoffs(now);
        let mut no_created = lease(1, now, 10, 10);
        no_created.created_at = None;
        assert!(!in_claim_episode(&no_created, created_cutoff, renewal_cutoff, false));
        let mut no_updated = lease(2, now, 3600, 3600);
        no_updated.updated_at = None;
        assert!(!in_claim_episode(&no_updated, created_cutoff, renewal_cutoff, false));
    }

    /// An `updated_at` that merely EQUALS `created_at` is creation, not
    /// renewal (that is exactly what a real forge writes at creation
    /// time), so it must not resurrect an out-of-lookback record.
    #[test]
    fn an_updated_at_equal_to_created_at_is_not_a_renewal() {
        let now = Utc::now();
        // A renewal window wide enough that only the `updated > created`
        // clause can be what rejects this record.
        let wide_renewal_cutoff = now - Duration::seconds(86_400);
        assert!(!in_claim_episode(
            &lease(1, now, 3600, 3600),
            created_cutoff(now),
            wide_renewal_cutoff,
            false
        ));
    }

    /// The renewal window is the reclamation gate's own TTL, not the
    /// dispatch lookback — the two must not drift, or a lease could be
    /// simultaneously "too fresh to reclaim" and "too old to compare",
    /// which is the exact combination that produced the #8840 collision.
    #[test]
    fn the_renewal_window_matches_the_reclamation_ttl() {
        let now = Utc::now();
        let expected_secs =
            (crate::claim_reconciliation::resolve_lease_ttl_minutes() * 60.0) as i64;
        assert_eq!((now - renewal_cutoff(now)).num_seconds(), expected_secs);
        assert!(
            expected_secs > LEASE_ORDER_LOOKBACK_SECS,
            "a renewal window no wider than the lookback would make leg 2 dead code"
        );
    }
}
