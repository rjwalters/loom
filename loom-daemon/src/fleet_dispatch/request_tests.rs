#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::super::proto;
use super::super::test_server::TestServer;
use super::*;
use crate::step_role::StepRole;
use serde_json::json;
use serial_test::serial;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;
use tempfile::tempdir;

fn enable(root: &Path, extra: &serde_json::Value) {
    let mut block = json!({"enabled": true, "url": "http://captain.invalid:1"});
    if let (Some(b), Some(e)) = (block.as_object_mut(), extra.as_object()) {
        b.extend(e.clone());
    }
    crate::sweep_registry::test_support::write_config(
        root,
        &json!({"fleetDispatch": block}).to_string(),
    );
}

struct Fixed(f64);

impl Rng for Fixed {
    fn unit(&mut self) -> f64 {
        self.0
    }
}

fn d(secs: u64) -> Duration {
    Duration::from_secs(secs)
}

fn t(secs: u64) -> Mono {
    Mono(d(secs))
}

fn candidate(item: u64) -> Candidate {
    let resp = proto::NextResponse {
        server_epoch: "e1".into(),
        queue_age_s: None,
        assignments: vec![proto::Assignment {
            assignment_id: format!("a-{item}"),
            repo: "acme/w".into(),
            item,
            step: "builder".into(),
            ttl_s: 1200,
        }],
    };
    let mut got = worker::candidates_for_test("http://c:1", &resp);
    got.remove(0)
}

type Script = Rc<RefCell<VecDeque<NextOutcome>>>;
type Sent = Rc<RefCell<Vec<(u32, Vec<String>)>>>;

/// Scripted answers; records `(free_slots, roles_ok)` per request.
struct Fake {
    answers: Script,
    sent: Sent,
}

impl Transport for Fake {
    fn request(&mut self, _: &Path, free_slots: u32, roles_ok: &[String]) -> NextOutcome {
        self.sent.borrow_mut().push((free_slots, roles_ok.to_vec()));
        self.answers
            .borrow_mut()
            .pop_front()
            .expect("an unscripted request")
    }
}

fn empty() -> NextOutcome {
    NextOutcome::Answered {
        candidates: vec![],
        epoch_changed: false,
        queue_age_s: Some(1),
    }
}

fn work(item: u64) -> NextOutcome {
    NextOutcome::Answered {
        candidates: vec![candidate(item)],
        epoch_changed: false,
        queue_age_s: None,
    }
}

struct Rig {
    lp: Loop<Fake, Fixed>,
    answers: Script,
    sent: Sent,
    root: tempfile::TempDir,
}

/// A rig with fleet mode on, jitter 0 and no startup wait: exact delays.
fn rig(answers: Vec<NextOutcome>) -> Rig {
    worker::reset_for_test();
    let root = tempdir().unwrap();
    enable(root.path(), &json!({"request": {"jitter": 0, "startupSecs": 0}}));
    let answers: Script = Rc::new(RefCell::new(answers.into()));
    let sent = Rc::new(RefCell::new(Vec::new()));
    let fake = Fake {
        answers: Rc::clone(&answers),
        sent: Rc::clone(&sent),
    };
    let lp = Loop::with(fake, Fixed(0.0), Instant::now(), root.path());
    Rig {
        lp,
        answers,
        sent,
        root,
    }
}

impl Rig {
    fn step(&mut self, at: u64) -> Step {
        self.lp.step_at(self.root.path(), t(at))
    }
}

#[test]
#[serial]
fn it_asks_when_capacity_is_available_and_backs_off_on_empty_answers() {
    let mut r = rig(vec![empty(), empty(), empty(), empty()]);
    let roles = ["builder".to_string()];
    // No capacity: nothing asked, wait for a notification.
    assert_eq!(r.step(0).next, Next::OnNotification);
    assert!(r.sent.borrow().is_empty());
    r.lp.note_capacity(3, &roles);
    let mut at = 0;
    for wait in [30, 60, 120, 120] {
        let s = r.step(at);
        assert!(s.candidates.is_empty());
        assert_eq!(s.next, Next::After(d(wait)), "after the empty answer at {at}");
        // Ordinary capacity re-checks in between ask nothing and reset nothing.
        for _ in 0..10 {
            r.lp.note_capacity(3, &roles);
            assert!(matches!(r.step(at + 1).next, Next::After(_)));
        }
        at += wait;
    }
    assert_eq!(r.sent.borrow().len(), 4, "no extra request from the re-checks");
    assert!(r
        .sent
        .borrow()
        .iter()
        .all(|(n, roles)| *n == 3 && roles == &["builder"]));
}

#[test]
#[serial]
fn work_resets_backoff_and_the_next_request_follows_at_once() {
    let mut r = rig(vec![empty(), empty(), work(7), empty()]);
    r.lp.note_capacity(2, &["builder".to_string()]);
    r.step(0);
    r.step(30);
    assert_eq!(r.lp.requester().streak(), 2);
    let s = r.step(90);
    assert_eq!(s.candidates.len(), 1);
    assert_eq!(s.candidates[0].item, 7);
    assert_eq!(r.lp.requester().streak(), 0);
    assert_eq!(s.next, Next::After(Duration::ZERO), "capacity remains: ask again");
    let s = r.step(90);
    assert_eq!(s.next, Next::After(d(30)), "backoff restarts from the initial delay");
}

#[test]
#[serial]
fn a_captain_error_neither_escalates_nor_resets_and_empty_work_is_not_an_error() {
    let mut r = rig(vec![
        empty(),
        empty(),
        NextOutcome::Failed,
        NextOutcome::Failed,
        empty(),
    ]);
    r.lp.note_capacity(1, &[]);
    r.step(0);
    r.step(30);
    assert_eq!(r.lp.requester().streak(), 2);
    for at in [90, 150] {
        assert_eq!(r.step(at).next, Next::After(d(60)), "errors retry at the current delay");
        assert_eq!(r.lp.requester().streak(), 2);
    }
    assert_eq!(r.step(210).next, Next::After(d(120)));
    assert_eq!(r.lp.requester().streak(), 3, "an empty answer after errors escalates");
}

#[test]
#[serial]
fn no_capacity_stops_requests_and_new_capacity_prompts_one() {
    let mut r = rig(vec![empty(), work(1)]);
    r.lp.note_capacity(1, &[]);
    r.step(0);
    r.lp.note_capacity(0, &[]);
    assert_eq!(r.step(500).next, Next::OnNotification, "due, but no capacity");
    assert_eq!(r.sent.borrow().len(), 1);
    r.lp.note_capacity(4, &["judge".to_string()]);
    let s = r.step(500);
    assert_eq!(s.candidates.len(), 1);
    assert_eq!(r.sent.borrow()[1], (4, vec!["judge".to_string()]));
    assert!(r.answers.borrow().is_empty());
}

#[test]
#[serial]
fn a_notification_storm_asks_once_per_floor() {
    let mut r = rig(vec![empty(), empty()]);
    r.lp.note_capacity(1, &[]);
    r.step(0);
    // 100 free/fill cycles in the next second: still one request in total.
    for i in 0..100 {
        r.lp.note_capacity(0, &[]);
        r.lp.note_capacity(1, &[]);
        r.step(i / 100);
    }
    assert_eq!(r.sent.borrow().len(), 1, "the wake floor holds the storm");
    // The floor is the config's smallest first delay (here 30 s: no jitter).
    r.step(30);
    assert_eq!(r.sent.borrow().len(), 2);
}

#[test]
#[serial]
fn startup_is_randomized_per_host() {
    worker::reset_for_test();
    let root = tempdir().unwrap();
    enable(root.path(), &json!({}));
    let first_wait = |u: f64| {
        let fake = Fake {
            answers: Rc::default(),
            sent: Rc::default(),
        };
        let mut lp = Loop::with(fake, Fixed(u), Instant::now(), root.path());
        lp.note_capacity(1, &[]);
        match lp.step_at(root.path(), t(0)).next {
            Next::After(w) => w,
            Next::OnNotification => panic!("expected a wait"),
        }
    };
    assert_eq!(first_wait(0.5), d(15));
    assert_eq!(first_wait(0.9), d(27));
    assert_ne!(first_wait(0.1), first_wait(0.7));
}

#[test]
#[serial]
fn a_config_reload_applies_live_and_an_invalid_block_keeps_the_last_good_one() {
    let mut r = rig(vec![empty(), empty(), empty()]);
    r.lp.note_capacity(1, &[]);
    assert_eq!(r.step(0).next, Next::After(d(30)));
    enable(
        r.root.path(),
        &json!({"request": {"jitter": 0, "startupSecs": 0,
        "initialBackoffSecs": 5, "maxBackoffSecs": 20}}),
    );
    // The pending 60 s wait (streak 1) is pulled in to the new 20 s cap.
    assert_eq!(r.step(1).next, Next::After(d(20)));
    // The next delay is drawn from the new schedule: 5 s * 2 = 10 s.
    assert_eq!(r.step(21).next, Next::After(d(10)));
    enable(r.root.path(), &json!({"request": {"jitter": 7}}));
    assert_eq!(r.step(22).next, Next::After(d(9)), "an invalid block changes nothing");
}

#[test]
#[serial]
fn with_fleet_mode_off_nothing_is_sent() {
    let mut r = rig(vec![]);
    crate::sweep_registry::test_support::write_config(
        r.root.path(),
        &json!({"fleetDispatch": {"enabled": false, "url": "http://c:1"}}).to_string(),
    );
    r.lp.note_capacity(5, &[]);
    assert_eq!(r.step(1000).next, Next::OnNotification);
    assert!(r.sent.borrow().is_empty());
}

/// Over the real wire: grant, then the stale epoch after a "restart", then a
/// failure, against the production transport.
#[test]
#[serial]
fn the_wire_transport_reads_grants_empties_epoch_changes_and_failures() {
    worker::reset_for_test();
    let epoch = Arc::new(std::sync::Mutex::new("e1".to_string()));
    let assign = Arc::new(std::sync::Mutex::new(true));
    let (ep, asg) = (Arc::clone(&epoch), Arc::clone(&assign));
    let server = TestServer::start(move |path, body| {
        assert_eq!(path, proto::NEXT_PATH);
        let epoch = ep.lock().unwrap().clone();
        let n = body["free_slots"].as_u64().unwrap();
        let list = if *asg.lock().unwrap() {
            json!([{"assignment_id": "a-9", "repo": "acme/w", "item": 9,
                    "step": "judge", "ttl_s": 1200}])
        } else {
            json!([])
        };
        (200, json!({"server_epoch": epoch, "queue_age_s": n, "assignments": list}))
    });
    let dir = tempdir().unwrap();
    crate::sweep_registry::test_support::write_config(
        dir.path(),
        &json!({"fleetDispatch": {"enabled": true, "url": server.url()}}).to_string(),
    );
    let roles = ["judge".to_string()];
    let NextOutcome::Answered {
        candidates,
        epoch_changed,
        queue_age_s,
    } = worker::request_next(dir.path(), 2, &roles)
    else {
        panic!("answered");
    };
    assert_eq!((candidates.len(), epoch_changed, queue_age_s), (1, false, Some(2)));
    assert_eq!(candidates[0].grant.assignment_id, "a-9");
    assert_eq!(candidates[0].step, StepRole::Judge);
    let sent = &server.requests_to(proto::NEXT_PATH)[0];
    assert_eq!(sent["free_slots"], 2);
    assert_eq!(sent["roles_ok"], json!(["judge"]));
    assert_eq!(sent["host_id"], crate::host_identity::host_identity());

    *epoch.lock().unwrap() = "e2".into();
    *assign.lock().unwrap() = false;
    let NextOutcome::Answered {
        candidates,
        epoch_changed,
        ..
    } = worker::request_next(dir.path(), 1, &roles)
    else {
        panic!("answered");
    };
    assert!(candidates.is_empty() && epoch_changed, "a restarted captain: stale epoch seen");

    crate::sweep_registry::test_support::write_config(
        dir.path(),
        &json!({"fleetDispatch": {"enabled": true, "url": TestServer::dead_url()}}).to_string(),
    );
    assert_eq!(worker::request_next(dir.path(), 1, &roles), NextOutcome::Failed);
}

#[test]
#[serial]
fn an_assignment_this_build_cannot_run_is_refused_back_not_offered() {
    worker::reset_for_test();
    let server = TestServer::start(|path, _| match path {
        proto::NEXT_PATH => (
            200,
            json!({"server_epoch": "e1", "assignments": [
                {"assignment_id": "a-1", "repo": "acme/w", "item": 1, "step": "teleport", "ttl_s": 9},
                {"assignment_id": "a-2", "repo": "acme/w", "item": 2, "step": "builder", "ttl_s": 9}
            ]}),
        ),
        _ => (200, json!({"server_epoch": "e1"})),
    });
    let dir = tempdir().unwrap();
    crate::sweep_registry::test_support::write_config(
        dir.path(),
        &json!({"fleetDispatch": {"enabled": true, "url": server.url()}}).to_string(),
    );
    let NextOutcome::Answered { candidates, .. } = worker::request_next(dir.path(), 2, &[]) else {
        panic!("answered");
    };
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].item, 2);
    let refused = server.wait_for(proto::REFUSE_PATH, 1);
    assert_eq!(refused[0]["assignment_id"], "a-1");
    assert_eq!(refused[0]["reason"], "unknown_step");
}
