#![allow(clippy::unwrap_used)]

use super::*;
use crate::fleet_dispatch::proto::{HeartbeatRequest, LiveStep, RefusalReason};
use std::collections::BTreeSet;
use std::time::Duration;

const A: &str = "worker-a";
const B: &str = "worker-b";
const ALL_ROLES: [&str; 5] = ["builder", "judge", "doctor", "curator", "champion"];

fn at(secs: u64) -> Mono {
    Mono(Duration::from_secs(secs))
}

fn expected(hosts: &[&str]) -> BTreeSet<String> {
    hosts.iter().map(|h| (*h).to_string()).collect()
}

/// A table past its grace window, so claims are decided by policy.
fn open_table() -> Table {
    let mut t = Table::new("epoch-0001", Policy::default(), at(0), BTreeSet::new());
    assert!(!t.in_grace(at(90)));
    t
}

fn hb(host: &str, live: Vec<LiveStep>, free: u32) -> HeartbeatRequest {
    HeartbeatRequest {
        host_id: host.to_string(),
        live,
        free_slots: free,
        roles_ok: ALL_ROLES.iter().map(|r| (*r).to_string()).collect(),
    }
}

fn live(repo: &str, item: u64, step: &str, id: Option<&str>) -> LiveStep {
    LiveStep {
        repo: repo.into(),
        item,
        step: step.into(),
        assignment_id: id.map(str::to_string),
    }
}

fn granted_id(d: &ClaimDecision) -> String {
    match d {
        ClaimDecision::Granted { assignment_id, .. } => assignment_id.clone(),
        other => panic!("expected a grant, got {other:?}"),
    }
}

fn refusal(d: &ClaimDecision) -> (RefusalReason, Option<String>) {
    match d {
        ClaimDecision::Refused {
            reason, held_by, ..
        } => (*reason, held_by.clone()),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_free_item_is_granted_with_the_assignment_ttl() {
    let mut t = open_table();
    t.heartbeat(at(100), A, &hb(A, vec![], 4));
    let d = t.claim(at(100), A, "acme/w", 7, "builder", Some(40));
    let ClaimDecision::Granted {
        assignment_id,
        ttl_s,
        renewed,
    } = d
    else {
        panic!("{d:?}")
    };
    assert!(assignment_id.starts_with("a-epoch-00-"));
    assert_eq!(ttl_s, 20 * 60, "forge lease 15 min + 5 min margin");
    assert!(!renewed);
    let row = t.row("acme/w", 7).unwrap();
    assert_eq!(row.state, RowState::Granted);
    assert_eq!(row.host.as_deref(), Some(A));
}

#[test]
fn the_policy_follows_the_resolved_lease_ttl() {
    let p = Policy::for_lease_ttl_minutes(30.0, Duration::from_secs(10));
    assert_eq!(p.assignment_ttl, Duration::from_secs(35 * 60));
    assert_eq!(p.hold_ttl, Duration::from_secs(30 * 60));
    assert_eq!(p.grace, Duration::from_secs(10));
    for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert_eq!(Policy::for_lease_ttl_minutes(bad, DEFAULT_GRACE), Policy::default(), "{bad}");
    }
}

#[test]
fn a_second_host_is_refused_held_naming_the_holder() {
    let mut t = open_table();
    t.heartbeat(at(100), A, &hb(A, vec![], 4));
    t.heartbeat(at(100), B, &hb(B, vec![], 4));
    granted_id(&t.claim(at(100), A, "acme/w", 7, "builder", Some(40)));
    let d = t.claim(at(101), B, "acme/w", 7, "builder", Some(40));
    assert_eq!(refusal(&d), (RefusalReason::Held, Some(A.to_string())));
    let ClaimDecision::Refused { retry_after_s, .. } = d else {
        unreachable!()
    };
    assert!((1..=60).contains(&retry_after_s));
    // Another step on the same item is held too: one step per item.
    let d = t.claim(at(101), B, "acme/w", 7, "curator", Some(40));
    assert_eq!(refusal(&d).0, RefusalReason::Held);
}

#[test]
fn a_duplicate_claim_from_the_holder_is_an_idempotent_regrant() {
    let mut t = open_table();
    t.heartbeat(at(100), A, &hb(A, vec![], 1));
    let first = granted_id(&t.claim(at(100), A, "acme/w", 7, "judge", Some(40)));
    // At its cap of 1, the same claim is still re-granted, same id.
    let again = t.claim(at(200), A, "acme/w", 7, "judge", Some(40));
    assert_eq!(granted_id(&again), first);
    assert!(matches!(again, ClaimDecision::Granted { renewed: true, .. }));
    assert_eq!(t.row("acme/w", 7).unwrap().deadline, at(200 + 1200));
    assert_eq!(t.held_by(A), 1);
}

#[test]
fn a_same_step_regrant_returns_the_same_id_and_renews_the_row() {
    let mut t = open_table();
    t.heartbeat(at(100), A, &hb(A, vec![], 4));
    let first = granted_id(&t.claim(at(100), A, "acme/w", 7, "doctor", Some(40)));
    for when in [150, 300] {
        let d = t.claim(at(when), A, "acme/w", 7, "doctor", Some(40));
        assert!(matches!(d, ClaimDecision::Granted { renewed: true, .. }), "{d:?}");
        assert_eq!(granted_id(&d), first);
        let row = t.row("acme/w", 7).unwrap();
        assert_eq!(row.step, "doctor");
        assert_eq!(row.deadline, at(when + 1200));
    }
    assert_eq!(t.held_by(A), 1);
}

/// The Judge's reproduction on #12465: a host's own stale row must not let a
/// different step skip the merge lock and `roles_ok`.
#[test]
fn a_different_step_on_an_own_item_cannot_bypass_the_merge_lock_or_roles_ok() {
    let mut t = open_table();
    t.heartbeat(at(100), B, &hb(B, vec![], 8));
    let mut beat = hb(A, vec![], 8);
    beat.roles_ok = vec!["judge".into()];
    t.heartbeat(at(100), A, &beat);
    granted_id(&t.claim(at(100), B, "acme/w", 10, "champion_merge", Some(40)));
    // A's `judge` on PR 11, whose `release` was lost: the row lives on.
    let judge = granted_id(&t.claim(at(100), A, "acme/w", 11, "judge", Some(40)));

    let d = t.claim(at(110), A, "acme/w", 11, "champion_merge", Some(40));
    assert_eq!(refusal(&d), (RefusalReason::Held, Some(B.to_string())));
    let merges = t
        .rows()
        .iter()
        .filter(|((repo, _), row)| repo == "acme/w" && row.step == "champion_merge")
        .count();
    assert_eq!(merges, 1, "exactly one champion_merge row in the repo");
    // The refused claim left A's row exactly as it was.
    let row = t.row("acme/w", 11).unwrap();
    assert_eq!(row.step, "judge");
    assert_eq!(row.assignment_id, judge);

    // With the merge lock gone, `roles_ok` still refuses it.
    let merge = t.row("acme/w", 10).unwrap().assignment_id.clone();
    assert!(t.release(B, &merge));
    let d = t.claim(at(120), A, "acme/w", 11, "champion_merge", Some(40));
    assert_eq!(refusal(&d).0, RefusalReason::NotEligible);
    assert_eq!(t.row("acme/w", 11).unwrap().step, "judge");
}

#[test]
fn a_different_step_on_an_own_item_is_a_new_grant_through_the_cap() {
    let mut t = open_table();
    t.heartbeat(at(100), A, &hb(A, vec![], 1));
    let judge = granted_id(&t.claim(at(100), A, "acme/w", 7, "judge", Some(40)));
    // At its cap of 1: the new step replaces the row, so it fits.
    let d = t.claim(at(110), A, "acme/w", 7, "doctor", Some(40));
    assert!(matches!(d, ClaimDecision::Granted { renewed: false, .. }), "{d:?}");
    let doctor = granted_id(&d);
    assert_ne!(doctor, judge, "a different step is a new assignment");
    let row = t.row("acme/w", 7).unwrap();
    assert_eq!((row.step.as_str(), row.state), ("doctor", RowState::Granted));
    assert_eq!(t.held_by(A), 1);
    assert!(!t.release(A, &judge), "the replaced assignment is gone");

    // Holding two rows against a limit of one, a step change on either is
    // over the cap and changes nothing.
    t.heartbeat(
        at(120),
        A,
        &hb(
            A,
            vec![
                live("acme/w", 7, "doctor", Some(&doctor)),
                live("acme/w", 8, "builder", None),
            ],
            0,
        ),
    );
    let d = t.claim(at(130), A, "acme/w", 7, "judge", Some(1));
    assert_eq!(refusal(&d).0, RefusalReason::Cap);
    let row = t.row("acme/w", 7).unwrap();
    assert_eq!((row.step.as_str(), row.assignment_id.as_str()), ("doctor", doctor.as_str()));
}

#[test]
fn one_champion_merge_per_repo_fleet_wide() {
    let mut t = open_table();
    t.heartbeat(at(100), A, &hb(A, vec![], 8));
    t.heartbeat(at(100), B, &hb(B, vec![], 8));
    granted_id(&t.claim(at(100), A, "acme/w", 10, "champion_merge", Some(40)));
    // Another PR in the same repo, any host, including the holder.
    for host in [B, A] {
        let d = t.claim(at(101), host, "acme/w", 11, "champion_merge", Some(40));
        assert_eq!(refusal(&d), (RefusalReason::Held, Some(A.to_string())), "{host}");
    }
    // Other steps in the repo, and merges in other repos, are unaffected.
    granted_id(&t.claim(at(101), B, "acme/w", 11, "judge", Some(40)));
    granted_id(&t.claim(at(101), B, "acme/other", 3, "champion_merge", Some(40)));
    // A merge learnt from a live set blocks just the same.
    t.heartbeat(at(102), B, &hb(B, vec![live("acme/x", 5, "champion_merge", None)], 8));
    let d = t.claim(at(103), A, "acme/x", 6, "champion_merge", Some(40));
    assert_eq!(refusal(&d), (RefusalReason::Held, Some(B.to_string())));
    // Once released, the next merge in the repo is granted.
    let id = t.row("acme/w", 10).unwrap().assignment_id.clone();
    assert!(t.release(A, &id));
    granted_id(&t.claim(at(104), B, "acme/w", 12, "champion_merge", Some(40)));
}

#[test]
fn eligibility_is_roles_ok_only() {
    let mut t = open_table();
    let mut beat = hb(A, vec![], 8);
    beat.roles_ok = vec!["judge".into()];
    t.heartbeat(at(100), A, &beat);
    assert_eq!(
        refusal(&t.claim(at(100), A, "acme/w", 1, "builder", Some(40))).0,
        RefusalReason::NotEligible
    );
    // Both Champion steps are the `champion` role.
    assert_eq!(
        refusal(&t.claim(at(100), A, "acme/w", 2, "champion_merge", Some(40))).0,
        RefusalReason::NotEligible
    );
    assert_eq!(
        refusal(&t.claim(at(100), A, "acme/w", 3, "sweep", Some(40))).0,
        RefusalReason::NotEligible,
        "a step this build does not know has no role"
    );
    granted_id(&t.claim(at(100), A, "acme/w", 4, "judge", Some(40)));
    // A host that has not heartbeated this epoch has no roles_ok to check.
    granted_id(&t.claim(at(100), B, "acme/w", 5, "builder", Some(40)));
}

#[test]
fn the_cap_is_min_of_reported_capacity_and_max_concurrent() {
    let mut t = open_table();
    // Reports 2 live + 1 free = capacity 3.
    t.heartbeat(
        at(100),
        A,
        &hb(
            A,
            vec![
                live("acme/w", 1, "builder", None),
                live("acme/w", 2, "judge", None),
            ],
            1,
        ),
    );
    granted_id(&t.claim(at(100), A, "acme/w", 3, "builder", Some(40)));
    assert_eq!(t.held_by(A), 3);
    assert_eq!(
        refusal(&t.claim(at(100), A, "acme/w", 4, "builder", Some(40))).0,
        RefusalReason::Cap
    );
    // maxConcurrent clamps an inflated report, so a host cannot hoard.
    t.heartbeat(at(110), B, &hb(B, vec![], 1000));
    granted_id(&t.claim(at(110), B, "acme/v", 1, "builder", Some(2)));
    granted_id(&t.claim(at(110), B, "acme/v", 2, "builder", Some(2)));
    assert_eq!(
        refusal(&t.claim(at(110), B, "acme/v", 3, "builder", Some(2))).0,
        RefusalReason::Cap
    );
    // A host with no heartbeat yet is bounded by maxConcurrent alone, and
    // refused when that is unknown too.
    granted_id(&t.claim(at(110), "worker-c", "acme/u", 1, "builder", Some(1)));
    assert_eq!(
        refusal(&t.claim(at(110), "worker-c", "acme/u", 2, "builder", Some(1))).0,
        RefusalReason::Cap
    );
    assert_eq!(
        refusal(&t.claim(at(110), "worker-d", "acme/u", 3, "builder", None)).0,
        RefusalReason::Cap
    );
    // Held rows do not count against anyone's cap.
    let id = t.row("acme/v", 1).unwrap().assignment_id.clone();
    t.refuse(at(111), B, &id, "claim_lost");
    granted_id(&t.claim(at(111), B, "acme/v", 3, "builder", Some(2)));
}

#[test]
fn expiry_runs_on_the_monotonic_clock() {
    let mut t = open_table();
    t.heartbeat(at(100), A, &hb(A, vec![], 4));
    let id = granted_id(&t.claim(at(100), A, "acme/w", 7, "builder", Some(40)));
    // One second short of the TTL, the row stands.
    assert!(t.expire(at(100 + 1199)).is_empty());
    assert!(t.row("acme/w", 7).is_some());
    let e = t.expire(at(100 + 1200));
    assert_eq!(
        e,
        Expired {
            assignments: 1,
            holds: 0
        }
    );
    assert!(t.row("acme/w", 7).is_none());
    // Its id is gone with it.
    assert!(!t.release(A, &id));
    // The item is free again.
    granted_id(&t.claim(at(1400), B, "acme/w", 7, "builder", Some(40)));
}

#[test]
fn a_heartbeat_renews_listed_rows_and_never_releases_omitted_ones() {
    let mut t = open_table();
    t.heartbeat(at(100), A, &hb(A, vec![], 4));
    let id1 = granted_id(&t.claim(at(100), A, "acme/w", 1, "builder", Some(40)));
    let _id2 = granted_id(&t.claim(at(100), A, "acme/w", 2, "builder", Some(40)));
    // Row 1 goes live; row 2 is not listed (a grant between heartbeat and spawn).
    t.heartbeat(at(1000), A, &hb(A, vec![live("acme/w", 1, "builder", Some(&id1))], 3));
    let r1 = t.row("acme/w", 1).unwrap();
    assert_eq!(r1.state, RowState::Live);
    assert_eq!(r1.deadline, at(1000 + 1200));
    let r2 = t.row("acme/w", 2).unwrap();
    assert_eq!(r2.state, RowState::Granted, "omitted, not released");
    assert_eq!(r2.deadline, at(100 + 1200));
    // At the original TTL only the unrenewed row goes.
    let e = t.expire(at(1300));
    assert_eq!(e.assignments, 1);
    assert!(t.row("acme/w", 1).is_some());
    assert!(t.row("acme/w", 2).is_none());
}

#[test]
fn claim_lost_holds_for_the_forge_ttl_and_a_stale_reason_drops() {
    let mut t = open_table();
    t.heartbeat(at(100), A, &hb(A, vec![], 4));
    t.heartbeat(at(100), B, &hb(B, vec![], 4));
    let lost = granted_id(&t.claim(at(100), A, "acme/w", 1, "builder", Some(40)));
    let stale = granted_id(&t.claim(at(100), A, "acme/w", 2, "builder", Some(40)));
    // Another host cannot refuse A's assignment.
    assert_eq!(t.refuse(at(101), B, &lost, "claim_lost"), RefuseOutcome::Unknown);
    assert_eq!(t.refuse(at(101), A, &lost, "claim_lost"), RefuseOutcome::Held { granted: true });
    let row = t.row("acme/w", 1).unwrap();
    assert_eq!((row.state, row.host.clone()), (RowState::Held, None));
    assert_eq!(row.deadline, at(101 + 900));
    // Nobody, including the loser, is assigned it while held.
    for host in [A, B] {
        assert_eq!(
            refusal(&t.claim(at(500), host, "acme/w", 1, "builder", Some(40))),
            (RefusalReason::Held, None)
        );
    }
    // A held row is nobody's to release or refuse again.
    assert!(!t.release(A, &lost));
    assert_eq!(t.refuse(at(502), A, &lost, "claim_lost"), RefuseOutcome::Unknown);
    // The hold ends at the forge TTL.
    let e = t.expire(at(101 + 900));
    assert_eq!(
        e,
        Expired {
            assignments: 0,
            holds: 1
        }
    );
    granted_id(&t.claim(at(1001), B, "acme/w", 1, "builder", Some(40)));
    // A stale reason drops the row at once.
    assert_eq!(t.refuse(at(102), A, &stale, "already_closed"), RefuseOutcome::Dropped);
    assert!(t.row("acme/w", 2).is_none());
}

#[test]
fn a_held_item_listed_live_becomes_its_runners_again() {
    let mut t = open_table();
    t.heartbeat(at(100), A, &hb(A, vec![], 4));
    let id = granted_id(&t.claim(at(100), A, "acme/w", 1, "builder", Some(40)));
    t.refuse(at(101), A, &id, "claim_lost");
    // B, outside fleet mode a moment ago, won the forge and now reports it.
    t.heartbeat(at(130), B, &hb(B, vec![live("acme/w", 1, "builder", None)], 4));
    let row = t.row("acme/w", 1).unwrap();
    assert_eq!((row.state, row.host.as_deref()), (RowState::Live, Some(B)));
}

#[test]
fn release_frees_the_item_and_only_for_its_holder() {
    let mut t = open_table();
    t.heartbeat(at(100), A, &hb(A, vec![], 4));
    let id = granted_id(&t.claim(at(100), A, "acme/w", 1, "doctor", Some(40)));
    assert!(!t.release(B, &id));
    assert!(!t.release(A, "a-nope"));
    assert!(t.release(A, &id));
    assert!(t.row("acme/w", 1).is_none());
    granted_id(&t.claim(at(101), B, "acme/w", 1, "doctor", Some(40)));
}

#[test]
fn claims_are_refused_with_grace_until_the_window_ends() {
    let mut t = Table::new("e", Policy::default(), at(1000), BTreeSet::new());
    let d = t.claim(at(1000), A, "acme/w", 1, "builder", Some(40));
    assert_eq!(
        d,
        ClaimDecision::Refused {
            reason: RefusalReason::Grace,
            held_by: None,
            retry_after_s: 90
        }
    );
    // Heartbeats are accepted (and say grace) during the window.
    assert!(t.heartbeat(at(1030), A, &hb(A, vec![], 4)));
    assert_eq!(
        refusal(&t.claim(at(1089), A, "acme/w", 1, "builder", Some(40))).0,
        RefusalReason::Grace
    );
    granted_id(&t.claim(at(1090), A, "acme/w", 1, "builder", Some(40)));
    // Once closed it stays closed.
    assert!(!t.in_grace(at(1091)));
}

#[test]
fn grace_ends_early_once_every_fleet_mode_host_has_reported() {
    let mut t = Table::new("e", Policy::default(), at(0), expected(&[A, B]));
    assert!(t.heartbeat(at(5), A, &hb(A, vec![], 4)), "B has not reported");
    // A host that is not expected does not end it either.
    assert!(t.heartbeat(at(6), "worker-c", &hb("worker-c", vec![], 4)));
    assert!(!t.heartbeat(at(7), B, &hb(B, vec![], 4)), "every expected host reported");
    granted_id(&t.claim(at(8), A, "acme/w", 1, "builder", Some(40)));
    // With nobody expected, only the time bound ends it.
    let mut t = Table::new("e", Policy::default(), at(0), BTreeSet::new());
    assert!(t.heartbeat(at(5), A, &hb(A, vec![], 4)));
    // A reload can set the expected hosts.
    t.set_expected(expected(&[A]));
    assert!(!t.in_grace(at(6)));
}

#[test]
fn a_restart_rebuilds_the_table_from_heartbeats_under_a_new_epoch() {
    // Before the restart: A holds a grant and is live on it; B runs a step
    // the captain never granted.
    let mut old = open_table();
    old.heartbeat(at(100), A, &hb(A, vec![], 4));
    let id = granted_id(&old.claim(at(100), A, "acme/w", 1, "builder", Some(40)));

    // The new process: a new epoch, an empty table, a grace window.
    let mut t = Table::new("epoch-0002", Policy::default(), at(0), expected(&[A, B]));
    assert_ne!(t.epoch(), old.epoch());
    assert!(t.rows().is_empty());
    assert!(t.heartbeat(at(10), A, &hb(A, vec![live("acme/w", 1, "builder", Some(&id))], 3)));
    assert!(!t.heartbeat(at(12), B, &hb(B, vec![live("acme/w", 2, "judge", None)], 3)));
    // A's grant is back under its old id; B's untracked step occupies its item.
    let r1 = t.row("acme/w", 1).unwrap();
    assert_eq!(
        (r1.host.as_deref(), r1.assignment_id.as_str(), r1.state),
        (Some(A), id.as_str(), RowState::Live)
    );
    assert!(r1.granted);
    let r2 = t.row("acme/w", 2).unwrap();
    assert!(r2.assignment_id.starts_with("l-"));
    assert!(!r2.granted);
    // The rebuilt rows refuse other hosts, and the old id still releases.
    assert_eq!(
        refusal(&t.claim(at(20), B, "acme/w", 1, "builder", Some(40))),
        (RefusalReason::Held, Some(A.to_string()))
    );
    assert_eq!(
        refusal(&t.claim(at(20), A, "acme/w", 2, "judge", Some(40))),
        (RefusalReason::Held, Some(B.to_string()))
    );
    assert!(t.release(A, &id));
}

#[test]
fn a_live_entry_cannot_steal_another_hosts_row_or_reuse_its_id() {
    let mut t = open_table();
    t.heartbeat(at(100), A, &hb(A, vec![], 4));
    let id = granted_id(&t.claim(at(100), A, "acme/w", 1, "builder", Some(40)));
    // B lists the same item (a duplicate) and another item under A's id.
    t.heartbeat(
        at(110),
        B,
        &hb(
            B,
            vec![
                live("acme/w", 1, "builder", None),
                live("acme/w", 9, "judge", Some(&id)),
            ],
            4,
        ),
    );
    assert_eq!(t.row("acme/w", 1).unwrap().host.as_deref(), Some(A));
    let r9 = t.row("acme/w", 9).unwrap();
    assert_ne!(r9.assignment_id, id, "a colliding id is replaced");
    assert!(t.release(A, &id), "A's id still names A's row");
}

#[test]
fn a_live_entry_without_an_id_adopts_the_hosts_own_grant() {
    // The grant's response timed out at the worker, which claimed anyway and
    // now lists the item with no assignment id.
    let mut t = open_table();
    t.heartbeat(at(100), A, &hb(A, vec![], 4));
    let id = granted_id(&t.claim(at(100), A, "acme/w", 3, "builder", Some(40)));
    t.heartbeat(at(400), A, &hb(A, vec![live("acme/w", 3, "builder", None)], 3));
    assert_eq!(t.rows().len(), 1, "no duplicate row");
    let row = t.row("acme/w", 3).unwrap();
    assert_eq!(row.assignment_id, id, "the grant's id is kept");
    assert_eq!(row.state, RowState::Live);
    assert!(row.granted);
    assert_eq!(row.deadline, at(400 + 1200), "renewed");
    assert_eq!(t.held_by(A), 1);
    // The id still names the adopted row.
    assert!(t.release(A, &id));
}
