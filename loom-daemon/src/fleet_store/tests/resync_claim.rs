//! Tests for the per-repo resync claim (#10718), against the in-memory forge.

use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, TimeZone, Utc};

use super::test_support::{FakeRefForge, Fault};
use super::*;

const REPO: &str = "acme/app";
const TREE: &str = "tree0";

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap()
}

fn claimant<'a>(forge: &'a FakeRefForge, host: &'a str) -> Claimant<'a> {
    Claimant {
        read: forge,
        write: forge,
        repo: REPO,
        host,
        version: "0.19.880",
        stale_after: stale_after(Duration::from_secs(60)),
    }
}

fn won(outcome: Acquire) -> Held {
    match outcome {
        Acquire::Won(held) => held,
        other => panic!("expected the claim, got {other:?}"),
    }
}

/// A claim another host took `age` ago, written straight into the forge.
fn seeded_claim(forge: &FakeRefForge, message: &str) -> String {
    let sha = forge.add_commit(message, &[]);
    forge.set_ref(CLAIM_REF, &sha);
    sha
}

#[test]
fn stale_window_is_ten_intervals_but_never_under_fifteen_minutes() {
    assert_eq!(stale_after(Duration::from_secs(60)), Duration::from_secs(900));
    assert_eq!(stale_after(Duration::from_secs(300)), Duration::from_secs(3000));
}

#[test]
fn the_message_round_trips_and_anything_else_is_not_a_claim() {
    let msg = claim_message("build 1", "0.19.880", t0());
    assert_eq!(msg, "loom-resync-claim host=build-1 version=0.19.880 at=2026-10-08T12:00:00Z");
    assert_eq!(
        parse_claim(&msg),
        Some(ClaimRecord {
            host: "build-1".into(),
            version: "0.19.880".into(),
            at: t0(),
        })
    );
    for bad in [
        "",
        "chore: something else",
        "loom-resync-claim host=a version=1",
        "loom-resync-claim host=a version=1 at=yesterday",
        "loom-resync-claim host= version=1 at=2026-10-08T12:00:00Z",
    ] {
        assert_eq!(parse_claim(bad), None, "{bad:?}");
    }
}

#[test]
fn two_hosts_racing_a_fresh_claim_exactly_one_wins() {
    let forge = FakeRefForge::new();
    let a = won(claimant(&forge, "host-a").acquire(TREE, t0()).unwrap());
    assert!(!a.took_over);
    assert_eq!(forge.ref_sha(CLAIM_REF).as_deref(), Some(a.sha.as_str()));

    // The loser is told who holds it. That is not an error.
    let b = claimant(&forge, "host-b").acquire(TREE, t0()).unwrap();
    assert_eq!(
        b,
        Acquire::HeldBy {
            host: "host-a".into(),
            since: t0(),
        }
    );
    assert_eq!(forge.ref_sha(CLAIM_REF).as_deref(), Some(a.sha.as_str()));
}

#[test]
fn meeting_a_held_claim_costs_two_reads_and_creates_nothing() {
    let forge = FakeRefForge::new();
    seeded_claim(&forge, &claim_message("host-a", "0.19.880", t0()));
    let commits = forge.commits.borrow().len();
    let b = claimant(&forge, "host-b").acquire(TREE, t0()).unwrap();
    assert!(matches!(b, Acquire::HeldBy { .. }), "{b:?}");
    // The ref is read before anything is created: no dangling claim commit.
    assert_eq!(forge.calls.borrow().len(), 2, "{:?}", forge.calls.borrow());
    assert!(forge.writes().is_empty(), "{:?}", forge.writes());
    assert_eq!(forge.commits.borrow().len(), commits);
}

#[test]
fn a_claim_created_between_the_read_and_the_create_is_held_not_an_error() {
    let forge = FakeRefForge::new();
    let theirs = forge.add_commit(&claim_message("host-a", "0.19.880", t0()), &[]);
    forge.before(move |forge, method, path| {
        // host-a's create lands just before host-b's.
        if method == "POST" && path.ends_with("/git/refs") {
            forge.set_ref(CLAIM_REF, &theirs);
        }
    });
    let b = claimant(&forge, "host-b").acquire(TREE, t0()).unwrap();
    assert_eq!(
        b,
        Acquire::HeldBy {
            host: "host-a".into(),
            since: t0(),
        }
    );
}

#[test]
fn a_fresh_claim_is_not_taken_over_and_a_stale_one_is() {
    let forge = FakeRefForge::new();
    let old = seeded_claim(&forge, &claim_message("host-a", "0.19.880", t0()));
    let b = claimant(&forge, "host-b");

    // 14 minutes: inside the window.
    let fresh = b.acquire(TREE, t0() + ChronoDuration::minutes(14)).unwrap();
    assert!(matches!(fresh, Acquire::HeldBy { .. }), "{fresh:?}");
    assert_eq!(forge.ref_sha(CLAIM_REF), Some(old.clone()));

    // 16 minutes: stale.
    let held = won(b.acquire(TREE, t0() + ChronoDuration::minutes(16)).unwrap());
    assert!(held.took_over);
    assert_eq!(forge.ref_sha(CLAIM_REF), Some(held.sha.clone()));
    // The new claim descends from the one it replaced, and the ticket is gone.
    assert_eq!(forge.commits.borrow()[&held.sha].1, vec![old.clone()]);
    assert_eq!(forge.ref_sha(&format!("{TAKEOVER_PREFIX}{old}")), None);
}

#[test]
fn an_unparseable_claim_is_stale() {
    let forge = FakeRefForge::new();
    seeded_claim(&forge, "not a claim at all");
    let held = won(claimant(&forge, "host-b").acquire(TREE, t0()).unwrap());
    assert!(held.took_over);
}

#[test]
fn this_hosts_own_stale_claim_is_taken_over_the_same_way() {
    let forge = FakeRefForge::new();
    seeded_claim(&forge, &claim_message("host-a", "0.19.880", t0()));
    let a = claimant(&forge, "host-a");
    assert!(matches!(
        a.acquire(TREE, t0() + ChronoDuration::minutes(1)).unwrap(),
        Acquire::HeldBy { .. }
    ));
    assert!(won(a.acquire(TREE, t0() + ChronoDuration::hours(1)).unwrap()).took_over);
}

#[test]
fn two_takers_of_one_stale_claim_exactly_one_wins() {
    // GitHub does not reject a non-fast-forward PATCH outside refs/heads (the
    // fake accepts it too), so the PATCH cannot pick a winner. The ticket,
    // named after the stale sha, can: it is created once.
    let forge = FakeRefForge::new();
    let old = seeded_claim(&forge, &claim_message("host-a", "0.19.880", t0()));
    let later = t0() + ChronoDuration::hours(1);

    // host-c reaches the ticket first, while host-b is about to ask for it.
    let ticket = format!("{TAKEOVER_PREFIX}{old}");
    let c_commit = forge.add_commit(&claim_message("host-c", "0.19.880", later), &[&old]);
    forge.set_ref(&ticket, &c_commit);

    let b = claimant(&forge, "host-b").acquire(TREE, later).unwrap();
    assert!(matches!(b, Acquire::Lost(_)), "{b:?}");
    assert_eq!(forge.ref_sha(CLAIM_REF), Some(old), "the loser must not move the claim");
    assert_eq!(forge.ref_sha(&ticket), Some(c_commit), "nor touch a live ticket");
    assert!(
        !forge.writes().iter().any(|c| c.starts_with("PATCH ")),
        "the loser never reaches the PATCH: {:?}",
        forge.writes()
    );
    // It read the ticket first, so it created no commit for it either.
    assert!(forge.writes().is_empty(), "{:?}", forge.writes());
}

#[test]
fn a_ticket_left_by_a_dead_taker_is_cleared_and_the_next_tick_wins() {
    let forge = FakeRefForge::new();
    let old = seeded_claim(&forge, &claim_message("host-a", "0.19.880", t0()));
    let ticket = format!("{TAKEOVER_PREFIX}{old}");
    let dead = forge.add_commit(
        &claim_message("host-c", "0.19.880", t0() + ChronoDuration::minutes(20)),
        &[&old],
    );
    forge.set_ref(&ticket, &dead);
    let b = claimant(&forge, "host-b");

    // An hour on, host-c's ticket is stale as well: clear it, and pass.
    let tick1 = b.acquire(TREE, t0() + ChronoDuration::hours(1)).unwrap();
    assert!(matches!(tick1, Acquire::Lost(_)), "{tick1:?}");
    assert_eq!(forge.ref_sha(&ticket), None);
    assert_eq!(forge.ref_sha(CLAIM_REF), Some(old));

    let tick2 = won(b.acquire(TREE, t0() + ChronoDuration::minutes(61)).unwrap());
    assert!(tick2.took_over);
}

#[test]
fn a_claim_that_changes_under_the_ticket_is_not_overwritten() {
    // host-b reads a stale claim and wins the ticket. Before its PATCH the
    // old holder releases and host-c takes a fresh claim. host-b must not
    // replace host-c's claim.
    let forge = FakeRefForge::new();
    let old = seeded_claim(&forge, &claim_message("host-a", "0.19.880", t0()));
    let later = t0() + ChronoDuration::hours(1);
    let fresh = forge.add_commit(&claim_message("host-c", "0.19.880", later), &[]);
    let ticket = format!("{TAKEOVER_PREFIX}{old}");
    let (swap_to, ticket_name) = (fresh.clone(), ticket.clone());
    forge.before(move |forge, method, _path| {
        // Right after host-b's ticket lands, the claim moves.
        if method == "GET" && forge.ref_sha(&ticket_name).is_some() {
            forge.set_ref(CLAIM_REF, &swap_to);
        }
    });

    let b = claimant(&forge, "host-b").acquire(TREE, later).unwrap();
    assert!(matches!(b, Acquire::Lost(_)), "{b:?}");
    assert_eq!(forge.ref_sha(CLAIM_REF), Some(fresh));
    assert_eq!(forge.ref_sha(&ticket), None, "the ticket is cleaned up either way");
}

#[test]
fn a_lost_create_reply_is_resolved_by_reading_the_ref() {
    let forge = FakeRefForge::new();
    forge.fault("POST repos/acme/app/git/refs", Fault::LostReply);
    let held = won(claimant(&forge, "host-a").acquire(TREE, t0()).unwrap());
    assert_eq!(forge.ref_sha(CLAIM_REF), Some(held.sha));

    // Same lost reply, but the create did not land: never assume the claim.
    let forge = FakeRefForge::new();
    forge.fault("POST repos/acme/app/git/refs", Fault::Unreachable);
    assert!(claimant(&forge, "host-a").acquire(TREE, t0()).is_err());
    assert_eq!(forge.ref_sha(CLAIM_REF), None);
}

#[test]
fn a_lost_takeover_reply_is_resolved_by_reading_the_ref() {
    let later = t0() + ChronoDuration::hours(1);
    // The PATCH landed and its reply was lost: the claim is ours, and saying
    // otherwise would leave it held by nobody for the stale window.
    let forge = FakeRefForge::new();
    let old = seeded_claim(&forge, &claim_message("host-a", "0.19.880", t0()));
    forge.fault("PATCH repos/acme/app/git/refs/loom/resync-claim", Fault::LostReply);
    let held = won(claimant(&forge, "host-b").acquire(TREE, later).unwrap());
    assert!(held.took_over);
    assert_eq!(forge.ref_sha(CLAIM_REF), Some(held.sha));
    assert_eq!(forge.ref_sha(&format!("{TAKEOVER_PREFIX}{old}")), None);

    // The PATCH never landed: an error, and never an assumed claim.
    let forge = FakeRefForge::new();
    let old = seeded_claim(&forge, &claim_message("host-a", "0.19.880", t0()));
    forge.fault("PATCH repos/acme/app/git/refs/loom/resync-claim", Fault::Unreachable);
    assert!(claimant(&forge, "host-b").acquire(TREE, later).is_err());
    assert_eq!(forge.ref_sha(CLAIM_REF), Some(old));
}

#[test]
fn an_unreachable_forge_is_told_apart_from_an_answer_it_cannot_use() {
    // No HTTP answer at all: an outage, reported once for the host.
    let forge = FakeRefForge::new();
    forge.fault("GET repos/acme/app/git/ref/loom/resync-claim", Fault::Unreachable);
    let err = claimant(&forge, "host-a").acquire(TREE, t0()).unwrap_err();
    assert!(err.downcast_ref::<ForgeUnreachable>().is_some(), "{err:#}");

    // An answer (here: no write scope on this repo) is this repo's failure.
    let forge = FakeRefForge::new();
    forge.fault("POST repos/acme/app/git/commits", Fault::Status(403));
    let err = claimant(&forge, "host-a").acquire(TREE, t0()).unwrap_err();
    assert!(err.downcast_ref::<ForgeUnreachable>().is_none(), "{err:#}");
}

#[test]
fn a_forge_error_is_an_error_not_a_claim() {
    for (needle, status) in [
        ("POST repos/acme/app/git/commits", 403),
        ("POST repos/acme/app/git/refs", 500),
        ("GET repos/acme/app/git/ref/loom/resync-claim", 502),
    ] {
        let forge = FakeRefForge::new();
        if needle.starts_with("GET") {
            seeded_claim(&forge, &claim_message("host-a", "0.19.880", t0()));
        }
        forge.fault(needle, Fault::Status(status));
        let err = claimant(&forge, "host-b")
            .acquire(TREE, t0())
            .expect_err(needle);
        assert!(format!("{err:#}").contains(&status.to_string()), "{err:#}");
    }
}

#[test]
fn the_fence_holds_only_while_the_ref_is_ours_and_young() {
    let forge = FakeRefForge::new();
    let a = claimant(&forge, "host-a");
    let held = won(a.acquire(TREE, t0()).unwrap());
    assert_eq!(a.fence(&held, t0() + ChronoDuration::minutes(7)).unwrap(), Ok(()));
    // Half of the 15 minute window.
    assert_eq!(
        a.fence(&held, t0() + ChronoDuration::seconds(450)).unwrap(),
        Err(FenceFailure::HeldTooLong)
    );
    // Someone else's commit under the ref.
    let other = forge.add_commit("x", &[]);
    forge.set_ref(CLAIM_REF, &other);
    assert_eq!(a.fence(&held, t0()).unwrap(), Err(FenceFailure::NotOurs));
    // Gone entirely.
    forge.refs.borrow_mut().clear();
    assert_eq!(a.fence(&held, t0()).unwrap(), Err(FenceFailure::NotOurs));
}

#[test]
fn release_deletes_only_our_own_claim() {
    let forge = FakeRefForge::new();
    let a = claimant(&forge, "host-a");
    let held = won(a.acquire(TREE, t0()).unwrap());
    assert!(a.release(&held, t0()).unwrap());
    assert_eq!(forge.ref_sha(CLAIM_REF), None);
    // Releasing again: nothing there, nothing deleted, no error.
    assert!(!a.release(&held, t0()).unwrap());

    // A claim that was taken over is the taker's to release.
    let held = won(a.acquire(TREE, t0()).unwrap());
    let theirs = forge.add_commit(&claim_message("host-b", "0.19.880", t0()), &[&held.sha]);
    forge.set_ref(CLAIM_REF, &theirs);
    assert!(!a.release(&held, t0()).unwrap());
    assert_eq!(forge.ref_sha(CLAIM_REF), Some(theirs));
}

#[test]
fn release_sends_no_delete_for_a_ticket_that_does_not_exist() {
    let forge = FakeRefForge::new();
    let a = claimant(&forge, "host-a");
    let held = won(a.acquire(TREE, t0()).unwrap());
    let before = forge.calls.borrow().len();
    assert!(a.release(&held, t0()).unwrap());
    let calls: Vec<String> = forge.calls.borrow()[before..].to_vec();
    assert_eq!(
        calls,
        vec![
            "GET repos/acme/app/git/ref/loom/resync-claim",
            "DELETE repos/acme/app/git/refs/loom/resync-claim",
            "GET repos/acme/app/git/matching-refs/loom/resync-takeover/",
        ],
        "one DELETE, for the claim; the ticket listing is empty"
    );
}

#[test]
fn release_collects_tickets_orphaned_by_a_dead_taker_and_leaves_a_live_one() {
    let forge = FakeRefForge::new();
    let a = claimant(&forge, "host-a");
    // A taker died after its PATCH: its ticket is named after a claim that is
    // long gone, so no later takeover will ever meet it.
    let gone = "f".repeat(40);
    let dead = forge.add_commit(&claim_message("host-x", "0.19.870", t0()), &[&gone]);
    let orphan = format!("{TAKEOVER_PREFIX}{gone}");
    forge.set_ref(&orphan, &dead);
    // One that is not a claim commit at all.
    let junk = forge.add_commit("not a claim", &[]);
    let odd = format!("{TAKEOVER_PREFIX}{}", "e".repeat(40));
    forge.set_ref(&odd, &junk);

    let later = t0() + ChronoDuration::hours(2);
    let held = won(a.acquire(TREE, later).unwrap());
    // Another host is, right now, starting to take OUR claim over.
    let live = format!("{TAKEOVER_PREFIX}{}", held.sha);
    let theirs = forge.add_commit(&claim_message("host-b", "0.19.880", later), &[&held.sha]);
    forge.set_ref(&live, &theirs);

    assert!(a.release(&held, later).unwrap());
    assert_eq!(forge.ref_sha(&orphan), None, "the orphan is collected");
    assert_eq!(forge.ref_sha(&odd), None, "so is one nobody can be shown to hold");
    assert_eq!(forge.ref_sha(&live), Some(theirs), "a fresh ticket is its taker's");
}
