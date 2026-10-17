//! Contracts for `loom-daemon lease release` (#12103).

use super::*;
use std::cell::RefCell;

const TTL: i64 = 15 * 60;

fn now() -> DateTime<Utc> {
    "2026-10-15T12:00:00Z".parse().unwrap()
}

fn lease(host: &str, sweep: &str, age_secs: i64) -> LeaseRow {
    LeaseRow {
        updated_at: now() - chrono::Duration::seconds(age_secs),
        body: format!("<!-- loom:lease host={host} sweep={sweep} -->\nprose"),
    }
}

fn yielded(host: &str, sweep: &str) -> LeaseRow {
    LeaseRow {
        updated_at: now(),
        body: yield_body(host, sweep),
    }
}

fn plan_of(rows: &[LeaseRow]) -> Plan {
    plan(rows, now(), chrono::Duration::seconds(TTL), &["h-me".to_string()])
}

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let s = Store {
        dir: dir.path().join("lease-renew"),
    };
    (dir, s)
}

fn all_live(_: u32, _: &str) -> bool {
    true
}

#[test]
fn plan_splits_live_leases_by_host_and_ignores_stale_ones() {
    let p = plan_of(&[
        lease("h-me", "s1", 60),
        lease("h-other", "s2", 60),
        lease("h-other", "s3", TTL + 60),
    ]);
    assert_eq!(p.own, vec![("h-me".into(), "s1".into())]);
    assert_eq!(p.foreign, vec![("h-other".into(), "s2".into())]);
}

#[test]
fn a_yield_record_parses_back_as_the_pair_it_excuses() {
    let p = plan_of(&[lease("h-me", "s1", 60), yielded("h-me", "s1")]);
    assert_eq!(
        p,
        Plan {
            own: vec![],
            foreign: vec![]
        }
    );
}

#[test]
fn foreign_lease_is_refused_and_nothing_is_stopped_or_posted() {
    let (_d, s) = store();
    lease_renewer::claim(
        &s,
        &lease_renewer::Key {
            repo: "acme/w".into(),
            host: "h-me".into(),
            sweep: "s1".into(),
            issue: 7,
        },
        100,
        "t",
        &|_| String::new(),
        &all_live,
    )
    .unwrap();
    let p = plan_of(&[lease("h-me", "s1", 60), lease("h-other", "s2", 60)]);
    let signalled = RefCell::new(Vec::new());
    let posted = RefCell::new(Vec::new());
    let out = release_lease(
        &s,
        "acme/w",
        7,
        &p,
        &["h-me".to_string()],
        &all_live,
        &|pid| signalled.borrow_mut().push(pid),
        &|b| {
            posted.borrow_mut().push(b.to_string());
            true
        },
    )
    .unwrap();
    assert_eq!(out, Outcome::Refused(vec![("h-other".into(), "s2".into())]));
    assert!(signalled.borrow().is_empty() && posted.borrow().is_empty());
    assert!(!s.all()[0].1.released, "own renewer must be left running");
}

#[test]
fn release_stops_a_real_renewer_under_a_long_lived_parent_and_yields_idempotently() {
    // The renewer watches a long-lived process (standing in for the parent
    // session) and uses an attended sweep id the caller never knows.
    let mut child = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .unwrap();
    let pid = child.id();
    let (_d, s) = store();
    let key = lease_renewer::Key {
        repo: "acme/w".into(),
        host: "h-me".into(),
        sweep: "sweep-insession-s1-abc".into(),
        issue: 7,
    };
    lease_renewer::claim(
        &s,
        &key,
        pid,
        "t",
        &|p| lease_renewer::start_identity(p).unwrap_or_default(),
        &lease_renewer::owner_is_live,
    )
    .unwrap();

    let mut rows = vec![lease("h-me", "sweep-insession-s1-abc", 60)];
    let posted = RefCell::new(Vec::new());
    let mine = ["h-me".to_string()];
    let signal = |p: u32| unsafe {
        libc::kill(p as i32, libc::SIGTERM);
    };
    let post = |b: &str| {
        posted.borrow_mut().push(b.to_string());
        true
    };

    let out = release_lease(
        &s,
        "acme/w",
        7,
        &plan_of(&rows),
        &mine,
        &lease_renewer::owner_is_live,
        &signal,
        &post,
    )
    .unwrap();
    assert_eq!(
        out,
        Outcome::Released {
            renewers: 1,
            yielded: 1
        }
    );
    assert!(!child.wait().unwrap().success(), "renewer must be terminated");
    assert!(s.all()[0].1.released);

    // The posted yield is what check-claim reads: feed it back and re-run.
    rows.push(LeaseRow {
        updated_at: now(),
        body: posted.borrow()[0].clone(),
    });
    assert_eq!(
        plan_of(&rows),
        Plan {
            own: vec![],
            foreign: vec![]
        }
    );
    let out = release_lease(
        &s,
        "acme/w",
        7,
        &plan_of(&rows),
        &mine,
        &lease_renewer::owner_is_live,
        &signal,
        &post,
    )
    .unwrap();
    assert_eq!(
        out,
        Outcome::Released {
            renewers: 1,
            yielded: 0
        }
    );
    assert_eq!(posted.borrow().len(), 1, "no duplicate yield");
}

#[test]
fn no_lease_and_no_renewer_is_a_quiet_success() {
    let (_d, s) = store();
    let out = release_lease(
        &s,
        "acme/w",
        7,
        &plan_of(&[]),
        &["h-me".to_string()],
        &all_live,
        &|_| {},
        &|_| true,
    )
    .unwrap();
    assert_eq!(
        out,
        Outcome::Released {
            renewers: 0,
            yielded: 0
        }
    );
}

#[test]
fn a_failed_post_is_reported_so_the_caller_can_retry() {
    let (_d, s) = store();
    let p = plan_of(&[lease("h-me", "s1", 60)]);
    let out =
        release_lease(&s, "acme/w", 7, &p, &["h-me".to_string()], &all_live, &|_| {}, &|_| false)
            .unwrap();
    assert_eq!(
        out,
        Outcome::PostFailed {
            renewers: 0,
            sweep: "s1".into()
        }
    );
}
