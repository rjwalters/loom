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
        started_at: None,
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
    let d = t.claim(at(100), A, "acme/w", 7, "builder");
    let ClaimDecision::Granted {
        assignment_id,
        ttl_s,
        renewed,
        lineage,
    } = d
    else {
        panic!("{d:?}")
    };
    assert!(assignment_id.starts_with("a-epoch-00-"));
    assert_eq!(ttl_s, 20 * 60, "forge lease 15 min + 5 min margin");
    assert!(!renewed);
    assert_eq!(lineage, None, "a first grant carries no lineage");
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
    granted_id(&t.claim(at(100), A, "acme/w", 7, "builder"));
    let d = t.claim(at(101), B, "acme/w", 7, "builder");
    assert_eq!(refusal(&d), (RefusalReason::Held, Some(A.to_string())));
    let ClaimDecision::Refused { retry_after_s, .. } = d else {
        unreachable!()
    };
    assert!((1..=60).contains(&retry_after_s));
    // Another step on the same item is held too: one step per item.
    let d = t.claim(at(101), B, "acme/w", 7, "curator");
    assert_eq!(refusal(&d).0, RefusalReason::Held);
}

#[test]
fn a_duplicate_claim_from_the_holder_is_an_idempotent_regrant() {
    let mut t = open_table();
    t.heartbeat(at(100), A, &hb(A, vec![], 1));
    let first = granted_id(&t.claim(at(100), A, "acme/w", 7, "judge"));
    // At its cap of 1, the same claim is still re-granted, same id.
    let again = t.claim(at(200), A, "acme/w", 7, "judge");
    assert_eq!(granted_id(&again), first);
    assert!(matches!(again, ClaimDecision::Granted { renewed: true, .. }));
    assert_eq!(t.row("acme/w", 7).unwrap().deadline, at(200 + 1200));
    assert_eq!(t.held_by(A), 1);
}

#[test]
fn a_same_step_regrant_returns_the_same_id_and_renews_the_row() {
    let mut t = open_table();
    t.heartbeat(at(100), A, &hb(A, vec![], 4));
    let first = granted_id(&t.claim(at(100), A, "acme/w", 7, "doctor"));
    for when in [150, 300] {
        let d = t.claim(at(when), A, "acme/w", 7, "doctor");
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
    granted_id(&t.claim(at(100), B, "acme/w", 10, "champion_merge"));
    // A's `judge` on PR 11, whose `release` was lost: the row lives on.
    let judge = granted_id(&t.claim(at(100), A, "acme/w", 11, "judge"));

    let d = t.claim(at(110), A, "acme/w", 11, "champion_merge");
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
    let d = t.claim(at(120), A, "acme/w", 11, "champion_merge");
    assert_eq!(refusal(&d).0, RefusalReason::NotEligible);
    assert_eq!(t.row("acme/w", 11).unwrap().step, "judge");
}

#[test]
fn a_different_step_on_an_own_item_is_a_new_grant() {
    let mut t = open_table();
    t.heartbeat(at(100), A, &hb(A, vec![], 1));
    let judge = granted_id(&t.claim(at(100), A, "acme/w", 7, "judge"));
    // The new step replaces the row.
    let d = t.claim(at(110), A, "acme/w", 7, "doctor");
    assert!(matches!(d, ClaimDecision::Granted { renewed: false, .. }), "{d:?}");
    let doctor = granted_id(&d);
    assert_ne!(doctor, judge, "a different step is a new assignment");
    let row = t.row("acme/w", 7).unwrap();
    assert_eq!((row.step.as_str(), row.state), ("doctor", RowState::Granted));
    assert_eq!(t.held_by(A), 1);
    assert!(!t.release(A, &judge), "the replaced assignment is gone");

    // Holding two rows while reporting no free slots, a step change on
    // either is still granted: the captain applies no capacity limit
    // (#12864), the worker asked because it has room by its own limits.
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
    let d = t.claim(at(130), A, "acme/w", 7, "judge");
    assert!(matches!(d, ClaimDecision::Granted { renewed: false, .. }), "{d:?}");
    let row = t.row("acme/w", 7).unwrap();
    assert_eq!(row.step, "judge");
    assert_eq!(t.held_by(A), 2);
}

#[test]
fn one_champion_merge_per_repo_fleet_wide() {
    let mut t = open_table();
    t.heartbeat(at(100), A, &hb(A, vec![], 8));
    t.heartbeat(at(100), B, &hb(B, vec![], 8));
    granted_id(&t.claim(at(100), A, "acme/w", 10, "champion_merge"));
    // Another PR in the same repo, any host, including the holder.
    for host in [B, A] {
        let d = t.claim(at(101), host, "acme/w", 11, "champion_merge");
        assert_eq!(refusal(&d), (RefusalReason::Held, Some(A.to_string())), "{host}");
    }
    // Other steps in the repo, and merges in other repos, are unaffected.
    granted_id(&t.claim(at(101), B, "acme/w", 11, "judge"));
    granted_id(&t.claim(at(101), B, "acme/other", 3, "champion_merge"));
    // A merge learnt from a live set blocks just the same.
    t.heartbeat(at(102), B, &hb(B, vec![live("acme/x", 5, "champion_merge", None)], 8));
    let d = t.claim(at(103), A, "acme/x", 6, "champion_merge");
    assert_eq!(refusal(&d), (RefusalReason::Held, Some(B.to_string())));
    // Once released, the next merge in the repo is granted.
    let id = t.row("acme/w", 10).unwrap().assignment_id.clone();
    assert!(t.release(A, &id));
    granted_id(&t.claim(at(104), B, "acme/w", 12, "champion_merge"));
}

#[test]
fn eligibility_is_roles_ok_only() {
    let mut t = open_table();
    let mut beat = hb(A, vec![], 8);
    beat.roles_ok = vec!["judge".into()];
    t.heartbeat(at(100), A, &beat);
    assert_eq!(
        refusal(&t.claim(at(100), A, "acme/w", 1, "builder")).0,
        RefusalReason::NotEligible
    );
    // Both Champion steps are the `champion` role.
    assert_eq!(
        refusal(&t.claim(at(100), A, "acme/w", 2, "champion_merge")).0,
        RefusalReason::NotEligible
    );
    assert_eq!(
        refusal(&t.claim(at(100), A, "acme/w", 3, "sweep")).0,
        RefusalReason::NotEligible,
        "a step this build does not know has no role"
    );
    granted_id(&t.claim(at(100), A, "acme/w", 4, "judge"));
    // A host that has not heartbeated this epoch has no roles_ok to check.
    granted_id(&t.claim(at(100), B, "acme/w", 5, "builder"));
}

#[test]
fn a_claim_is_granted_regardless_of_the_captains_row_count() {
    // #12864: the captain never refuses `cap`. Reports 2 live + 1 free, and
    // the captain already holds 3 rows for A: more claims are still granted.
    let mut t = open_table();
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
    granted_id(&t.claim(at(100), A, "acme/w", 3, "builder"));
    assert_eq!(t.held_by(A), 3);
    for item in 4..=8 {
        granted_id(&t.claim(at(100), A, "acme/w", item, "builder"));
    }
    assert_eq!(t.held_by(A), 8);
    // A host reporting zero free slots is granted too.
    t.heartbeat(at(110), B, &hb(B, vec![], 0));
    granted_id(&t.claim(at(110), B, "acme/v", 1, "builder"));
    // So is a host that has never heartbeated this epoch.
    granted_id(&t.claim(at(110), "worker-c", "acme/u", 1, "builder"));
    granted_id(&t.claim(at(110), "worker-c", "acme/u", 2, "builder"));
    // The other refusals are unchanged: another host's row still holds.
    assert_eq!(
        refusal(&t.claim(at(111), B, "acme/w", 3, "builder")),
        (RefusalReason::Held, Some(A.to_string()))
    );
}

#[test]
fn a_later_stage_goes_back_to_the_host_that_ran_the_earlier_one() {
    // Session affinity (#11913) is the worker's: the host that ran the
    // earlier stage asks for the next one. The captain grants it to that
    // host however many rows it already holds, and holds it against others.
    let mut t = open_table();
    t.heartbeat(at(100), A, &hb(A, vec![], 0));
    t.heartbeat(at(100), B, &hb(B, vec![], 8));
    for item in 1..=4 {
        granted_id(&t.claim(at(100), A, "acme/w", item, "builder"));
    }
    let judge = granted_id(&t.claim(at(100), A, "acme/w", 7, "judge"));
    // B, idle, cannot take the item while A holds it.
    assert_eq!(
        refusal(&t.claim(at(105), B, "acme/w", 7, "doctor")),
        (RefusalReason::Held, Some(A.to_string()))
    );
    // A asks for the later stage and gets it, replacing its own row.
    let doctor = granted_id(&t.claim(at(110), A, "acme/w", 7, "doctor"));
    assert_ne!(doctor, judge);
    let row = t.row("acme/w", 7).unwrap();
    assert_eq!((row.host.as_deref(), row.step.as_str()), (Some(A), "doctor"));
    assert_eq!(t.held_by(A), 5);
}

#[test]
fn expiry_runs_on_the_monotonic_clock() {
    let mut t = open_table();
    t.heartbeat(at(100), A, &hb(A, vec![], 4));
    let id = granted_id(&t.claim(at(100), A, "acme/w", 7, "builder"));
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
    granted_id(&t.claim(at(1400), B, "acme/w", 7, "builder"));
}

#[test]
fn a_heartbeat_renews_listed_rows_and_never_releases_omitted_ones() {
    let mut t = open_table();
    t.heartbeat(at(100), A, &hb(A, vec![], 4));
    let id1 = granted_id(&t.claim(at(100), A, "acme/w", 1, "builder"));
    let _id2 = granted_id(&t.claim(at(100), A, "acme/w", 2, "builder"));
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
    let lost = granted_id(&t.claim(at(100), A, "acme/w", 1, "builder"));
    let stale = granted_id(&t.claim(at(100), A, "acme/w", 2, "builder"));
    // Another host cannot refuse A's assignment.
    assert_eq!(t.refuse(at(101), B, &lost, "claim_lost"), RefuseOutcome::Unknown);
    assert_eq!(t.refuse(at(101), A, &lost, "claim_lost"), RefuseOutcome::Held { granted: true });
    let row = t.row("acme/w", 1).unwrap();
    assert_eq!((row.state, row.host.clone()), (RowState::Held, None));
    assert_eq!(row.deadline, at(101 + 900));
    // Nobody, including the loser, is assigned it while held.
    for host in [A, B] {
        assert_eq!(
            refusal(&t.claim(at(500), host, "acme/w", 1, "builder")),
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
    granted_id(&t.claim(at(1001), B, "acme/w", 1, "builder"));
    // A stale reason drops the row at once.
    assert_eq!(t.refuse(at(102), A, &stale, "already_closed"), RefuseOutcome::Dropped);
    assert!(t.row("acme/w", 2).is_none());
}

#[test]
fn a_held_item_listed_live_becomes_its_runners_again() {
    let mut t = open_table();
    t.heartbeat(at(100), A, &hb(A, vec![], 4));
    let id = granted_id(&t.claim(at(100), A, "acme/w", 1, "builder"));
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
    let id = granted_id(&t.claim(at(100), A, "acme/w", 1, "doctor"));
    assert!(!t.release(B, &id));
    assert!(!t.release(A, "a-nope"));
    assert!(t.release(A, &id));
    assert!(t.row("acme/w", 1).is_none());
    granted_id(&t.claim(at(101), B, "acme/w", 1, "doctor"));
}

#[test]
fn claims_are_refused_with_grace_until_the_window_ends() {
    let mut t = Table::new("e", Policy::default(), at(1000), BTreeSet::new());
    let d = t.claim(at(1000), A, "acme/w", 1, "builder");
    assert_eq!(
        d,
        ClaimDecision::Refused {
            reason: RefusalReason::Grace,
            held_by: None,
            holder_assignment_id: None,
            retry_after_s: 90
        }
    );
    // Heartbeats are accepted (and say grace) during the window.
    assert!(t.heartbeat(at(1030), A, &hb(A, vec![], 4)));
    assert_eq!(refusal(&t.claim(at(1089), A, "acme/w", 1, "builder")).0, RefusalReason::Grace);
    granted_id(&t.claim(at(1090), A, "acme/w", 1, "builder"));
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
    granted_id(&t.claim(at(8), A, "acme/w", 1, "builder"));
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
    let id = granted_id(&old.claim(at(100), A, "acme/w", 1, "builder"));

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
        refusal(&t.claim(at(20), B, "acme/w", 1, "builder")),
        (RefusalReason::Held, Some(A.to_string()))
    );
    assert_eq!(
        refusal(&t.claim(at(20), A, "acme/w", 2, "judge")),
        (RefusalReason::Held, Some(B.to_string()))
    );
    assert!(t.release(A, &id));
}

#[test]
fn a_live_entry_cannot_steal_another_hosts_row_or_reuse_its_id() {
    let mut t = open_table();
    t.heartbeat(at(100), A, &hb(A, vec![], 4));
    let id = granted_id(&t.claim(at(100), A, "acme/w", 1, "builder"));
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
    let id = granted_id(&t.claim(at(100), A, "acme/w", 3, "builder"));
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

#[test]
fn an_in_flight_heartbeat_cannot_revert_a_just_replaced_row() {
    let mut t = open_table();
    t.heartbeat(at(100), A, &hb(A, vec![], 4));
    let judge = granted_id(&t.claim(at(100), A, "acme/w", 7, "judge"));
    t.heartbeat(at(105), A, &hb(A, vec![live("acme/w", 7, "judge", Some(&judge))], 3));
    let doctor = granted_id(&t.claim(at(110), A, "acme/w", 7, "doctor"));
    // The heartbeat was read before the replacement and lands after it.
    t.heartbeat(at(111), A, &hb(A, vec![live("acme/w", 7, "judge", Some(&judge))], 3));
    let row = t.row("acme/w", 7).unwrap();
    assert_eq!((row.step.as_str(), row.assignment_id.as_str()), ("doctor", doctor.as_str()));
    assert_eq!(row.state, RowState::Granted);
    // An id-less entry of the old step is just as stale.
    t.heartbeat(at(112), A, &hb(A, vec![live("acme/w", 7, "judge", None)], 3));
    assert_eq!(t.row("acme/w", 7).unwrap().step, "doctor");
    // The next heartbeat carries the new id and adopts it.
    t.heartbeat(at(130), A, &hb(A, vec![live("acme/w", 7, "doctor", Some(&doctor))], 3));
    let row = t.row("acme/w", 7).unwrap();
    assert_eq!((row.step.as_str(), row.state), ("doctor", RowState::Live));
}

// ---------------------------------------------- no-progress cooldown (#12541)

const C: &str = "worker-c";

/// A table with three hosts heartbeated at `now`.
fn three_hosts(now: Mono) -> Table {
    let mut t = open_table();
    for h in [A, B, C] {
        t.heartbeat(now, h, &hb(h, vec![], 8));
    }
    t
}

fn retry(d: &ClaimDecision) -> u64 {
    match d {
        ClaimDecision::Refused { retry_after_s, .. } => *retry_after_s,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_no_progress_release_holds_the_triple_for_every_host_until_it_ends() {
    for outcome in ["noop", "refused", "failed", "", "something_new"] {
        let mut t = three_hosts(at(100));
        let id = granted_id(&t.claim(at(100), A, "acme/w", 9, "champion_merge"));
        assert!(t.release_outcome(at(200), A, &id, outcome));
        assert!(t.row("acme/w", 9).is_none(), "the row itself is gone");
        // Every host, the releaser included, is refused `held`, with no
        // holder and the hold's remaining time as the retry.
        for host in [A, B, C] {
            let d = t.claim(at(300), host, "acme/w", 9, "champion_merge");
            assert_eq!(refusal(&d), (RefusalReason::Held, None), "{outcome:?} {host}");
            assert_eq!(retry(&d), 30 * 60 - 100, "{outcome:?}");
        }
        assert!(t.cooldown(at(300), "acme/w", 9, "champion_merge").is_some());
        // Another step on the item, and the same step elsewhere, are free.
        granted_id(&t.claim(at(300), B, "acme/w", 9, "judge"));
        granted_id(&t.claim(at(300), C, "acme/w", 10, "champion_merge"));
        // At the hold's end the triple is claimable again.
        let id = t.row("acme/w", 9).unwrap().assignment_id.clone();
        assert!(t.release(B, &id));
        let id = t.row("acme/w", 10).unwrap().assignment_id.clone();
        assert!(t.release(C, &id));
        granted_id(&t.claim(at(200 + 30 * 60), C, "acme/w", 9, "champion_merge"));
    }
}

#[test]
fn consecutive_no_progress_releases_escalate_and_a_progressing_one_resets() {
    let mut t = three_hosts(at(100));
    let mut now = 100;
    let mut expect = Vec::new();
    for (host, rung) in [(A, 1800), (B, 7200), (C, 21_600), (A, 86_400), (B, 86_400)] {
        t.heartbeat(at(now), host, &hb(host, vec![], 8));
        let id = granted_id(&t.claim(at(now), host, "acme/w", 9, "champion_merge"));
        assert!(t.release_outcome(at(now), host, &id, "noop"));
        let c = t.cooldown(at(now), "acme/w", 9, "champion_merge").unwrap();
        expect.push(rung);
        assert_eq!(c.until, at(now + rung), "rung {}", expect.len());
        // Still held one second before the end, free at it.
        assert!(matches!(
            t.claim(at(now + rung - 1), C, "acme/w", 9, "champion_merge"),
            ClaimDecision::Refused { .. }
        ));
        now += rung;
    }
    assert_eq!(
        t.cooldown(at(now), "acme/w", 9, "champion_merge")
            .map_or(0, |c| c.streak),
        0,
        "the hold has ended"
    );

    // A progressing release of the triple clears the streak: the next
    // no-progress one starts again at the first rung.
    t.heartbeat(at(now), A, &hb(A, vec![], 8));
    let id = granted_id(&t.claim(at(now), A, "acme/w", 9, "champion_merge"));
    assert!(t.release_outcome(at(now), A, &id, "verdict"));
    assert!(t.cooldown(at(now), "acme/w", 9, "champion_merge").is_none());
    let id = granted_id(&t.claim(at(now), B, "acme/w", 9, "champion_merge"));
    assert!(t.release_outcome(at(now), B, &id, "noop"));
    let c = t.cooldown(at(now), "acme/w", 9, "champion_merge").unwrap();
    assert_eq!((c.until, c.streak), (at(now + 1800), 1));
}

#[test]
fn a_progressing_release_holds_nothing_and_clears_every_hold_on_the_item() {
    let mut t = three_hosts(at(100));
    for outcome in PROGRESS_OUTCOMES {
        let id = granted_id(&t.claim(at(100), A, "acme/w", 9, "judge"));
        assert!(t.release_outcome(at(100), A, &id, outcome));
        granted_id(&t.claim(at(101), B, "acme/w", 9, "judge"));
        let id = t.row("acme/w", 9).unwrap().assignment_id.clone();
        assert!(t.release(B, &id));
    }
    // A Champion step held the PR; a Judge verdict on it later (the item
    // moved) frees the Champion step at once.
    let id = granted_id(&t.claim(at(200), A, "acme/w", 9, "champion_merge"));
    assert!(t.release_outcome(at(200), A, &id, "refused"));
    assert!(t.cooldown(at(201), "acme/w", 9, "champion_merge").is_some());
    let id = granted_id(&t.claim(at(201), B, "acme/w", 9, "judge"));
    assert!(t.release_outcome(at(202), B, &id, "verdict"));
    assert!(t.cooldown(at(203), "acme/w", 9, "champion_merge").is_none());
    granted_id(&t.claim(at(203), C, "acme/w", 9, "champion_merge"));
}

#[test]
fn a_host_lost_release_ends_the_row_and_holds_nothing() {
    let mut t = three_hosts(at(100));
    let id = granted_id(&t.claim(at(100), A, "acme/w", 9, "builder"));
    assert!(t.release_outcome(at(150), A, &id, HOST_LOST_OUTCOME));
    assert!(t.row("acme/w", 9).is_none(), "the row ends at once");
    assert!(t.cooldown(at(150), "acme/w", 9, "builder").is_none(), "no hold armed");
    let next = t.claim(at(151), B, "acme/w", 9, "builder");
    assert_eq!(
        lineage_of(&next),
        Some(Lineage {
            attempt: 1,
            prev_assignment_id: id,
            prev_end: PrevEnd::HostLost,
        }),
        "#12875: the next grant names the lost assignment"
    );
}

#[test]
fn a_release_the_table_does_not_know_arms_nothing() {
    let mut t = three_hosts(at(100));
    let id = granted_id(&t.claim(at(100), A, "acme/w", 9, "builder"));
    assert!(!t.release_outcome(at(100), B, &id, "noop"), "another host's id");
    assert!(!t.release_outcome(at(100), A, "a-nope", "noop"));
    assert!(t.cooldown(at(100), "acme/w", 9, "builder").is_none());
}

#[test]
fn the_ladder_is_configurable_and_a_zero_rung_holds_nothing() {
    let policy = Policy::default().with_no_progress([Duration::ZERO; 4]);
    let mut t = Table::new("epoch-0001", policy, at(0), BTreeSet::new());
    assert!(!t.in_grace(at(90)));
    t.heartbeat(at(100), A, &hb(A, vec![], 8));
    let id = granted_id(&t.claim(at(100), A, "acme/w", 9, "judge"));
    assert!(t.release_outcome(at(100), A, &id, "noop"));
    granted_id(&t.claim(at(101), A, "acme/w", 9, "judge"));

    let p = Policy::default();
    assert_eq!(p.no_progress, DEFAULT_NO_PROGRESS_LADDER);
    assert_eq!(p.no_progress_hold(1), Duration::from_secs(1800));
    assert_eq!(p.no_progress_hold(4), Duration::from_secs(86_400));
    assert_eq!(p.no_progress_hold(99), Duration::from_secs(86_400));
    assert_eq!(
        Policy::for_lease_ttl_minutes(30.0, DEFAULT_GRACE).no_progress,
        DEFAULT_NO_PROGRESS_LADDER
    );
}

#[test]
fn a_streak_is_forgotten_a_last_rung_after_its_hold_ends() {
    let mut t = three_hosts(at(100));
    let id = granted_id(&t.claim(at(100), A, "acme/w", 9, "judge"));
    assert!(t.release_outcome(at(100), A, &id, "noop"));
    // Long after: the streak is gone, so the next hold is the first rung.
    let later = 100 + 1800 + 86_400;
    t.expire(at(later));
    t.heartbeat(at(later), A, &hb(A, vec![], 8));
    let id = granted_id(&t.claim(at(later), A, "acme/w", 9, "judge"));
    assert!(t.release_outcome(at(later), A, &id, "noop"));
    assert_eq!(t.cooldown(at(later), "acme/w", 9, "judge").unwrap().streak, 1);
}

#[test]
fn a_ci_wait_release_holds_flat_and_never_climbs_the_ladder() {
    let mut t = three_hosts(at(100));
    // Repeated CI-wait releases stay at the flat hold, not 30 min / 2 h / ...
    let mut now = 100;
    for host in [A, B, C, A] {
        t.heartbeat(at(now), host, &hb(host, vec![], 8));
        let id = granted_id(&t.claim(at(now), host, "acme/w", 9, "judge"));
        assert!(t.release_outcome(at(now), host, &id, CI_WAIT_OUTCOME));
        let c = t.cooldown(at(now), "acme/w", 9, "judge").unwrap();
        assert_eq!((c.until, c.streak), (at(now + 300), 0));
        assert!(matches!(
            t.claim(at(now + 299), B, "acme/w", 9, "judge"),
            ClaimDecision::Refused { .. }
        ));
        now += 300;
    }
    granted_id(&t.claim(at(now), B, "acme/w", 9, "judge"));
}

#[test]
fn a_ci_wait_release_keeps_the_streak_and_a_longer_hold() {
    let mut t = three_hosts(at(100));
    let id = granted_id(&t.claim(at(100), A, "acme/w", 9, "champion_merge"));
    assert!(t.release_outcome(at(100), A, &id, "noop"));
    // The 30 min hold ends at 1900; claim just after and wait on CI.
    t.heartbeat(at(1900), B, &hb(B, vec![], 8));
    let id = granted_id(&t.claim(at(1900), B, "acme/w", 9, "champion_merge"));
    assert!(t.release_outcome(at(1900), B, &id, CI_WAIT_OUTCOME));
    let c = t.cooldown(at(1900), "acme/w", 9, "champion_merge").unwrap();
    assert_eq!((c.until, c.streak), (at(1900 + 300), 1), "streak kept");
}

#[test]
fn only_a_hold_refusal_is_traced_as_cooled() {
    let mut t = three_hosts(at(100));
    let id = granted_id(&t.claim(at(100), A, "acme/w", 9, "judge"));
    // Another host's live claim: held, but not by the hold.
    let (d, cooled) = t.claim_traced(at(110), B, "acme/w", 9, "judge");
    assert_eq!(refusal(&d).0, RefusalReason::Held);
    assert!(cooled.is_none());
    assert!(t.release_outcome(at(120), A, &id, "noop"));
    let (d, cooled) = t.claim_traced(at(130), B, "acme/w", 9, "judge");
    assert_eq!(refusal(&d).0, RefusalReason::Held);
    let hold = cooled.unwrap();
    assert_eq!(hold.streak, 1);
    assert_eq!(hold.set_by_outcome, "noop");
    assert_eq!(hold.set_by_assignment_id, id);
}

fn lineage_of(d: &ClaimDecision) -> Option<Lineage> {
    match d {
        ClaimDecision::Granted { lineage, .. } => lineage.clone(),
        other => panic!("expected a grant, got {other:?}"),
    }
}

/// #12875: an expired assignment is reported with its identity, a held row
/// is not, and the next grant names the expired one.
#[test]
fn an_expired_assignment_is_reported_and_the_next_grant_names_it() {
    let mut t = open_table();
    t.heartbeat(at(100), A, &hb(A, vec![], 4));
    t.heartbeat(at(100), B, &hb(B, vec![], 4));
    let first = t.claim(at(100), A, "acme/w", 7, "builder");
    assert_eq!(lineage_of(&first), None, "a first grant carries no lineage");
    let id = granted_id(&first);
    // A held row (an item someone outside fleet mode owns) is not an assignment.
    let lost = granted_id(&t.claim(at(100), A, "acme/w", 8, "builder"));
    t.refuse(at(100), A, &lost, REFUSE_CLAIM_LOST);
    let ttl = Policy::default().assignment_ttl.as_secs();
    t.expire(at(100 + ttl));
    let rows = t.take_expired();
    assert_eq!(
        rows,
        vec![ExpiredAssignment {
            host: Some(A.into()),
            repo: "acme/w".into(),
            item: 7,
            step: "builder".into(),
            assignment_id: id.clone(),
        }]
    );
    assert!(t.take_expired().is_empty(), "drained once");
    let again = t.claim(at(100 + ttl), B, "acme/w", 7, "builder");
    assert_eq!(
        lineage_of(&again),
        Some(Lineage {
            attempt: 1,
            prev_assignment_id: id,
            prev_end: PrevEnd::Expired,
        })
    );
}

/// #12875: an expiry found inside a claim is still reported.
#[test]
fn an_expiry_inside_a_claim_is_not_lost() {
    let mut t = open_table();
    t.heartbeat(at(100), A, &hb(A, vec![], 4));
    let id = granted_id(&t.claim(at(100), A, "acme/w", 7, "builder"));
    let ttl = Policy::default().assignment_ttl.as_secs();
    let d = t.claim(at(100 + ttl), A, "acme/w", 7, "builder");
    assert_eq!(lineage_of(&d).map(|l| l.prev_assignment_id), Some(id.clone()));
    assert_eq!(t.take_expired().len(), 1);
}

/// #12875: a release is `released`, the attempt counts every earlier end,
/// and a renewal is not a new attempt.
#[test]
fn a_release_is_recorded_and_a_renewal_is_not_an_attempt() {
    let mut t = open_table();
    t.heartbeat(at(100), A, &hb(A, vec![], 4));
    let one = granted_id(&t.claim(at(100), A, "acme/w", 7, "builder"));
    let renewed = t.claim(at(110), A, "acme/w", 7, "builder");
    assert!(matches!(renewed, ClaimDecision::Granted { renewed: true, .. }));
    assert_eq!(lineage_of(&renewed), None);
    assert!(t.release_outcome(at(120), A, &one, "verdict"));
    let two_d = t.claim(at(130), A, "acme/w", 7, "builder");
    let two = granted_id(&two_d);
    assert_eq!(
        lineage_of(&two_d),
        Some(Lineage {
            attempt: 1,
            prev_assignment_id: one,
            prev_end: PrevEnd::Released,
        })
    );
    assert!(t.release_outcome(at(140), A, &two, "verdict"));
    let three = t.claim(at(150), A, "acme/w", 7, "builder");
    assert_eq!(lineage_of(&three).map(|l| l.attempt), Some(2));
    // Another step on the same item is its own triple.
    assert_eq!(lineage_of(&t.claim(at(150), B, "acme/w", 9, "judge")), None);
}

/// #12875: the record is bounded by size and by age.
#[test]
fn the_lineage_record_is_bounded() {
    let mut t = open_table();
    t.heartbeat(at(100), A, &hb(A, vec![], 4));
    for item in 0..(LINEAGE_MAX as u64 + 10) {
        let id = granted_id(&t.claim(at(100), A, "acme/w", item, "builder"));
        assert!(t.release_outcome(at(101 + item), A, &id, "verdict"));
    }
    assert_eq!(t.lineage.len(), LINEAGE_MAX);
    assert!(
        !t.lineage
            .contains_key(&("acme/w".to_string(), 0, "builder".to_string())),
        "the oldest end is evicted first"
    );
    t.expire(at(100 + LINEAGE_MAX as u64 + 20 + LINEAGE_MAX_AGE.as_secs()));
    assert!(t.lineage.is_empty(), "records age out");
}

#[test]
fn a_live_row_refusal_names_the_holders_assignment_and_a_cooldown_names_none() {
    let mut t = open_table();
    t.heartbeat(at(100), A, &hb(A, vec![], 4));
    t.heartbeat(at(100), B, &hb(B, vec![], 4));
    let id = granted_id(&t.claim(at(100), A, "acme/w", 1, "builder"));
    let holder = |d: ClaimDecision| match d {
        ClaimDecision::Refused {
            reason: RefusalReason::Held,
            holder_assignment_id,
            ..
        } => holder_assignment_id,
        other => panic!("expected a held refusal, got {other:?}"),
    };
    assert_eq!(holder(t.claim(at(101), B, "acme/w", 1, "builder")), Some(id));
    // A refusal that is not a live-row hold carries none.
    let d = t.claim(at(101), B, "acme/w", 2, "nonsense");
    assert!(
        matches!(
            d,
            ClaimDecision::Refused {
                holder_assignment_id: None,
                ..
            }
        ),
        "{d:?}"
    );
}

/// #12873: the gauge snapshot. A fresh host reports its slots, live count
/// and oldest start; a host whose heartbeat went stale keeps only its row
/// counts; a host never heard from has nothing; a held row is fleet-wide.
#[test]
fn the_gauge_snapshot_separates_rows_from_what_hosts_report() {
    let mut t = open_table();
    let start = |s: &str| Some(s.parse::<chrono::DateTime<chrono::Utc>>().unwrap());
    let mut early = live("acme/w", 1, "judge", None);
    early.started_at = start("2026-10-20T10:00:00Z");
    let mut late = live("acme/w", 2, "builder", None);
    late.started_at = start("2026-10-20T11:00:00Z");
    t.heartbeat(at(100), A, &hb(A, vec![early, late], 3));
    granted_id(&t.claim(at(100), A, "acme/w", 3, "doctor"));
    // B heartbeats an old-worker step (no start), then goes silent.
    t.heartbeat(at(100), B, &hb(B, vec![live("acme/w", 4, "judge", None)], 2));
    // A held row: a granted item lost the forge order.
    let id = granted_id(&t.claim(at(100), A, "acme/w", 5, "judge"));
    t.refuse(at(100), A, &id, crate::fleet_dispatch::proto::REFUSE_CLAIM_LOST);

    let g = t.host_gauges(at(150), HEARTBEAT_FRESH);
    assert_eq!(g.held, 1);
    let a = &g.hosts[A];
    assert_eq!((a.granted, a.live), (1, 2));
    let reported = a.reported.as_ref().unwrap();
    assert_eq!((reported.free_slots, reported.live_reported), (3, 2));
    assert_eq!(reported.oldest_live_started_at, start("2026-10-20T10:00:00Z"));
    let b = &g.hosts[B];
    assert_eq!((b.granted, b.live), (0, 1));
    assert_eq!(b.reported.as_ref().unwrap().oldest_live_started_at, None, "no start sent");

    // A heartbeats again; B's last heartbeat is now stale.
    t.heartbeat(at(250), A, &hb(A, vec![], 4));
    let g = t.host_gauges(at(250), HEARTBEAT_FRESH);
    let b = &g.hosts[B];
    assert_eq!((b.granted, b.live), (0, 1), "a silent host keeps its stale rows");
    assert_eq!(b.reported, None, "a stale host reports nothing, never 0");
    let a = g.hosts[A].reported.as_ref().unwrap();
    assert_eq!((a.free_slots, a.live_reported), (4, 0));
    assert_eq!(a.oldest_live_started_at, None, "nothing live, no start");
    assert!(!g.hosts.contains_key("worker-never"), "an unknown host has no entry");

    // Once B's rows expire, B drops out entirely.
    let g = t.host_gauges(at(2000), HEARTBEAT_FRESH);
    t.expire(at(2000));
    let g_after = t.host_gauges(at(2000), HEARTBEAT_FRESH);
    assert!(g.hosts.contains_key(B));
    assert!(!g_after.hosts.contains_key(B));
}
