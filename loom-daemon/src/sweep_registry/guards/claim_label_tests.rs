//! Unit coverage for Issue #9453 Phase 3.1 — the label leg of
//! [`SweepRegistry::resolve_lease_order`](super::super::SweepRegistry::resolve_lease_order).
//!
//! The four cases the phase's acceptance criterion names, driven through
//! `resolve_lease_order` itself rather than only through the predicate, so the
//! *wiring* (which branch consults the timeline, and which never does) is pinned
//! and not merely the arithmetic:
//!
//! | case | label event | lease comments in window | expected |
//! |---|---|---|---|
//! | young leaseless claim | 5 min before `episode_start` | own only | `YieldToLeaselessClaim` |
//! | aged-out claim | 11 min before `episode_start` | own only | `Proceed` |
//! | unreadable timeline | `gh` exits non-zero | own only | `Proceed` (fail-open) |
//! | a lease record exists | 5 min before `episode_start` | own + peer | `Proceed` (leg never fires) |
//!
//! Plus the two pure properties the wiring depends on: this dispatcher's own
//! flip can never trigger the leg, and "sole claimant" means *foreign* records
//! only.
//!
//! Sibling file rather than an inline `mod`, for the same reason
//! `repo_env_tests.rs` is one: `guards.rs` is over the file-size ratchet
//! threshold (`scripts/file-size-baseline.txt`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::sweep_registry::test_support::with_fleet_author;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;

/// Safely inside [`LEASELESS_CLAIM_LABEL_GRACE_SECS`] — the shape of the #9432
/// incident, where the hand-claim's label was minutes old when the fleet
/// dispatched.
const YOUNG_LABEL_SECS_AGO: i64 = 300;

/// Past the grace: the shape an abandoned or already-finished claim leaves
/// behind, which must never wedge redispatch.
const AGED_LABEL_SECS_AGO: i64 = LEASELESS_CLAIM_LABEL_GRACE_SECS + 60;

/// A registry whose fake `gh` answers `repo view` (so `resolve_owner_repo`
/// succeeds), the `.../comments` lease read with `comments_stdout`, and the
/// `.../timeline` claim-label read with `timeline_stdout` / `timeline_exit`.
///
/// Deliberately a local fixture rather than an extension of
/// `lease_order_unit_registry` in `guards.rs`: that file is frozen at its
/// current size, and the timeline arm is only needed here.
fn claim_label_registry(
    dir: &Path,
    comments_stdout: &str,
    timeline_stdout: &str,
    timeline_exit: i32,
) -> SweepRegistry {
    let fake_gh = dir.join("fake-gh-claim-label.sh");
    let script = format!(
        "#!/usr/bin/env bash\n\
         if [[ \"$1\" == \"repo\" && \"$2\" == \"view\" ]]; then\n\
         printf 'rjwalters/loom\\n'\n\
         exit 0\n\
         fi\n\
         if [[ \"$1\" == \"api\" && \"$*\" == *\"/comments\"* ]]; then\n\
         printf '%s' '{comments}'\n\
         exit 0\n\
         fi\n\
         if [[ \"$1\" == \"api\" && \"$*\" == *\"/timeline\"* ]]; then\n\
         printf '%s\\n' '{timeline}'\n\
         exit {timeline_exit}\n\
         fi\n\
         exit 1\n",
        comments = with_fleet_author(comments_stdout).replace('\'', "'\\''"),
        timeline = timeline_stdout.replace('\'', "'\\''"),
    );
    std::fs::write(&fake_gh, &script).unwrap();
    let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_gh, perms).unwrap();
    if let Ok(f) = std::fs::File::open(&fake_gh) {
        let _ = f.sync_all();
    }
    let mut config = SweepRegistryConfig::new(dir.to_path_buf());
    config.gh_bin = Some(fake_gh);
    SweepRegistry::new(config)
}

/// This dispatcher's own lease comment (`id` 1), as `read_lease_comments` would
/// read it back moments after `write_lease_comment` posted it.
fn own_lease_only(dir: &Path, now: DateTime<Utc>) -> String {
    let this_host =
        SweepRegistry::new(SweepRegistryConfig::new(dir.to_path_buf())).published_host_id();
    format!(
        r#"{{"id":1,"created_at":"{t}","updated_at":"{t}","body":"<!-- loom:lease host={this_host} sweep=sweep-mine -->"}}"#,
        t = now.to_rfc3339(),
    )
}

/// An RFC-3339 timestamp `secs_ago` before `now`, as the timeline `--jq`
/// `max` filter emits it (a single JSON-quoted line).
fn timeline_answer(now: DateTime<Utc>, secs_ago: i64) -> String {
    format!("\"{}\"", (now - chrono::Duration::seconds(secs_ago)).to_rfc3339())
}

/// **The Phase 3.1 regression.** A hand-claim applied `loom:building` five
/// minutes ago and published no lease. This dispatcher flipped the (already
/// present, so event-free) label, wrote its own lease, and finds itself the only
/// claimant in the comment read-back — the exact state in which the fleet
/// dispatched a duplicate builder onto #9432. The label event must now be read
/// and the dispatch must yield.
#[test]
#[serial]
fn resolve_lease_order_yields_to_a_young_leaseless_claim() {
    let dir = tempdir().unwrap();
    let now = Utc::now();
    let registry = claim_label_registry(
        dir.path(),
        &own_lease_only(dir.path(), now),
        &timeline_answer(now, YOUNG_LABEL_SECS_AGO),
        0,
    );

    let decision = registry.resolve_lease_order(9432, "sweep-mine", now);
    let identity = decision.clone().yield_identity();

    let LeaseOrderDecision::YieldToLeaselessClaim { labeled_at } = decision else {
        panic!("a young leaseless `loom:building` must refuse this dispatch");
    };
    assert_eq!(
        labeled_at.timestamp(),
        (now - chrono::Duration::seconds(YOUNG_LABEL_SECS_AGO)).timestamp(),
        "the yield must carry the label event's own forge-assigned timestamp"
    );
    assert_eq!(
        identity,
        Some((
            LEASELESS_CLAIM_HOST.to_string(),
            format!(
                "{LEASELESS_CLAIM_SWEEP_PREFIX}{}",
                (now - chrono::Duration::seconds(YOUNG_LABEL_SECS_AGO)).format("%Y%m%dT%H%M%SZ")
            ),
        )),
        "the dispatch path consumes one (host, sweep) shape; a label event names neither, so it \
         reports an explicit unknown rather than inventing an identity"
    );
}

/// The anti-wedge complement: a `loom:building` older than the label grace is
/// the shape a finished or abandoned claim leaves behind. Yielding to it would
/// make every redispatch of a previously-claimed issue lose to its own history.
#[test]
#[serial]
fn resolve_lease_order_proceeds_past_a_leaseless_claim_older_than_the_grace() {
    let dir = tempdir().unwrap();
    let now = Utc::now();
    let registry = claim_label_registry(
        dir.path(),
        &own_lease_only(dir.path(), now),
        &timeline_answer(now, AGED_LABEL_SECS_AGO),
        0,
    );

    assert_eq!(
        registry.resolve_lease_order(9433, "sweep-mine", now),
        LeaseOrderDecision::Proceed,
        "a label event past the grace is not evidence of a live claimant"
    );
}

/// FAIL-OPEN, the contract every leg of this tie-break shares: an unreadable
/// timeline (non-zero `gh` exit — a rate limit, a timeout, an outage) is not
/// evidence of anything and must never be turned into a refusal. The fixture is
/// otherwise identical to the yielding regression above, so the read failure is
/// provably the only reason this proceeds.
#[test]
#[serial]
fn resolve_lease_order_proceeds_when_the_claim_label_read_fails() {
    let dir = tempdir().unwrap();
    let now = Utc::now();
    let registry = claim_label_registry(
        dir.path(),
        &own_lease_only(dir.path(), now),
        &timeline_answer(now, YOUNG_LABEL_SECS_AGO),
        1,
    );

    assert_eq!(
        registry.resolve_lease_order(9434, "sweep-mine", now),
        LeaseOrderDecision::Proceed,
        "an unverifiable read must never manufacture a yield"
    );
}

/// The regression guard on the "additive" claim: when a foreign lease record IS
/// in the window, the comment-order verdict stands and the label leg never
/// fires — even with a young `loom:building` present. Two racing daemons both
/// see that same label (whichever flipped first created it; the second flip adds
/// no event), so firing it here would make both yield and neither build.
///
/// This dispatcher's own record is `id` 1, the peer's `id` 2, so #6287 decides
/// `Proceed` — and `Proceed` it must stay.
#[test]
#[serial]
fn a_peer_lease_record_keeps_the_label_leg_out_of_the_verdict() {
    let dir = tempdir().unwrap();
    let now = Utc::now();
    let comments = format!(
        "{own}\n{{\"id\":2,\"created_at\":\"{t}\",\"updated_at\":\"{t}\",\"body\":\"<!-- \
         loom:lease host=peer-host sweep=sweep-peer -->\"}}",
        own = own_lease_only(dir.path(), now),
        t = now.to_rfc3339(),
    );
    let registry =
        claim_label_registry(dir.path(), &comments, &timeline_answer(now, YOUNG_LABEL_SECS_AGO), 0);

    assert_eq!(
        registry.resolve_lease_order(9435, "sweep-mine", now),
        LeaseOrderDecision::Proceed,
        "with a peer lease record in-window the label leg must not fire — if it did, both \
         racing daemons would yield to the same label and nobody would build"
    );
}

/// The property that keeps the leg from refusing every uncontested dispatch:
/// `episode_start` is captured immediately BEFORE this dispatcher's own label
/// flip, so the event that flip creates is always newer and can never be
/// mistaken for a foreign claim.
#[test]
fn claim_label_is_live_foreign_ignores_this_dispatchers_own_flip() {
    let episode_start = Utc::now();
    assert!(
        !claim_label_is_live_foreign(episode_start + chrono::Duration::seconds(1), episode_start),
        "a label event created by this attempt's own flip is not a foreign claim"
    );
    assert!(
        !claim_label_is_live_foreign(episode_start, episode_start),
        "an event at the episode start is this attempt's own flip, not foreign (older-by-slack is the foreign threshold)"
    );
    assert!(
        claim_label_is_live_foreign(episode_start - chrono::Duration::seconds(2), episode_start),
        "an event predating the episode start, inside the grace, is a live foreign claim"
    );
    assert!(
        claim_label_is_live_foreign(
            episode_start - chrono::Duration::seconds(LEASELESS_CLAIM_LABEL_GRACE_SECS),
            episode_start
        ),
        "the grace boundary itself is still live (inclusive), matching orphan recovery's own \
         10-minute label grace"
    );
    assert!(
        !claim_label_is_live_foreign(
            episode_start - chrono::Duration::seconds(LEASELESS_CLAIM_LABEL_GRACE_SECS + 1),
            episode_start
        ),
        "one second past the grace is an aged claim, not a live one"
    );
}

/// [`is_sole_claimant`] is about FOREIGN records: several of this dispatcher's
/// own (e.g. a re-published lease) still leave it the sole claimant, and a
/// single foreign record — whatever its comment order — does not.
#[test]
fn is_sole_claimant_counts_foreign_records_only() {
    let lease = |host: &str, sweep: &str, id: u64| LeaseComment {
        id,
        created_at: None,
        updated_at: None,
        host: host.to_string(),
        sweep_id: sweep.to_string(),
    };
    let mine_a = lease("host-me", "sweep-mine", 1);
    let mine_b = lease("host-me", "sweep-mine", 3);
    let peer = lease("host-peer", "sweep-peer", 2);

    assert!(is_sole_claimant(&[], "host-me", "sweep-mine"));
    assert!(is_sole_claimant(&[&mine_a, &mine_b], "host-me", "sweep-mine"));
    assert!(!is_sole_claimant(&[&mine_a, &peer], "host-me", "sweep-mine"));
    assert!(
        !is_sole_claimant(&[&peer], "host-me", "sweep-mine"),
        "a record from another sweep on another host is foreign"
    );
    assert!(
        !is_sole_claimant(&[&lease("host-me", "sweep-other", 1)], "host-me", "sweep-mine"),
        "a different sweep on THIS host is still not this dispatcher's own record"
    );
}

/// `yield_identity` is the one shape the dispatch path consumes: `Proceed`
/// carries no claimant, and a comment-order yield passes the peer's real
/// host/sweep straight through.
#[test]
fn yield_identity_maps_each_variant_onto_the_dispatch_shape() {
    assert_eq!(LeaseOrderDecision::Proceed.yield_identity(), None);
    assert_eq!(
        LeaseOrderDecision::Yield {
            earliest_host: "peer-host".to_string(),
            earliest_sweep_id: "sweep-peer".to_string(),
        }
        .yield_identity(),
        Some(("peer-host".to_string(), "sweep-peer".to_string()))
    );
}

/// Issue #10337 regression: the forge reports `created_at` truncated to whole
/// seconds while `episode_start` is sub-second, so this dispatcher's own flip
/// in the same wall-second read as "older" and it yielded to itself.
#[test]
fn claim_label_is_live_foreign_ignores_same_second_truncated_own_flip() {
    use chrono::TimeZone;
    let episode_start =
        Utc.with_ymd_and_hms(2026, 10, 5, 0, 40, 26).unwrap() + chrono::Duration::milliseconds(800);
    let truncated = Utc.with_ymd_and_hms(2026, 10, 5, 0, 40, 26).unwrap();
    assert!(
        !claim_label_is_live_foreign(truncated, episode_start),
        "own flip truncated to the same second must not be foreign"
    );
    assert!(
        !claim_label_is_live_foreign(truncated - chrono::Duration::seconds(1), episode_start),
        "within clock slack is still own"
    );
    assert!(
        claim_label_is_live_foreign(truncated - chrono::Duration::seconds(5), episode_start),
        "a genuinely older young label is still foreign"
    );
}
