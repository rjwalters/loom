//! The worker client against a real loopback server (#12450).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::super::test_server::TestServer;
use super::*;
use serde_json::json;
use serial_test::serial;
use tempfile::tempdir;

fn enable(root: &Path, url: &str, enabled: bool) {
    crate::sweep_registry::test_support::write_config(
        root,
        &json!({"fleetDispatch": {"enabled": enabled, "url": url}}).to_string(),
    );
}

fn granting() -> TestServer {
    TestServer::start(|path, _| match path {
        proto::CLAIM_PATH => (
            200,
            json!({"server_epoch":"e1","granted":true,"assignment_id":"a-1","ttl_s":1200}),
        ),
        proto::HEARTBEAT_PATH => (200, json!({"server_epoch":"e1","grace":false})),
        _ => (200, json!({"server_epoch":"e1"})),
    })
}

fn refusing(reason: &'static str) -> TestServer {
    TestServer::start(move |_, _| {
        (
            200,
            json!({"server_epoch":"e1","granted":false,"reason":reason,"retry_after_s":30}),
        )
    })
}

#[test]
fn the_flag_reads_enabled_and_url_together() {
    assert_eq!(endpoint_in(&json!({})), None);
    assert_eq!(endpoint_in(&json!({"fleetDispatch":{"url":"http://c:1"}})), None);
    assert_eq!(endpoint_in(&json!({"fleetDispatch":{"enabled":true}})), None);
    assert_eq!(endpoint_in(&json!({"fleetDispatch":{"enabled":true,"url":"  "}})), None);
    assert_eq!(
        endpoint_in(&json!({"fleetDispatch":{"enabled":true,"url":"http://c:1/"}})).as_deref(),
        Some("http://c:1")
    );
}

/// Flag off: no call, no budget change, no heartbeat, whatever the server.
#[test]
#[serial]
fn with_the_flag_off_nothing_is_asked_or_sent() {
    reset_for_test();
    let dir = tempdir().unwrap();
    let server = granting();
    enable(dir.path(), &server.url(), false);
    assert_eq!(claim(dir.path(), 7, StepRole::Judge), Ask::Off);
    assert_eq!(tick_budget(dir.path(), 9, 2), (9, false));
    note_capacity(9, &BTreeSet::new());
    on_reaper_tick(dir.path(), &[("step-judge-7-1".into(), 7, StepRole::Judge)]);
    release("step-judge-7-1", "verdict");
    std::thread::sleep(Duration::from_millis(100));
    assert!(server.requests().is_empty(), "{:?}", server.requests());
}

#[test]
#[serial]
fn a_grant_carries_its_assignment_and_a_refusal_its_reason() {
    reset_for_test();
    let dir = tempdir().unwrap();
    let server = granting();
    enable(dir.path(), &server.url(), true);
    let Ask::Granted(grant) = claim(dir.path(), 7, StepRole::ChampionMerge) else {
        panic!("granted");
    };
    assert_eq!(grant.assignment_id, "a-1");
    let sent = &server.requests_to(proto::CLAIM_PATH)[0];
    assert_eq!(sent["item"], 7);
    assert_eq!(sent["step"], "champion_merge");
    assert_eq!(sent["host_id"], crate::host_identity::host_identity());

    for (wire, reason) in [
        ("held", RefusalReason::Held),
        ("not_eligible", RefusalReason::NotEligible),
        ("cap", RefusalReason::Cap),
    ] {
        let server = refusing(wire);
        enable(dir.path(), &server.url(), true);
        assert_eq!(claim(dir.path(), 7, StepRole::Judge), Ask::Refused(reason));
    }
    assert_eq!(refusal_token(RefusalReason::Held), "captain_held");
}

/// `grace` claims as today and leaves the failure run alone: two failures,
/// any number of graces, then one more failure enters fallback.
#[test]
#[serial]
fn grace_fails_open_without_touching_the_hysteresis() {
    reset_for_test();
    let dir = tempdir().unwrap();
    let dead = TestServer::dead_url();
    enable(dir.path(), &dead, true);
    assert_eq!(claim(dir.path(), 1, StepRole::Judge), Ask::FailOpen);
    assert_eq!(claim(dir.path(), 1, StepRole::Judge), Ask::FailOpen);
    let grace = refusing("grace");
    enable(dir.path(), &grace.url(), true);
    for _ in 0..5 {
        assert_eq!(claim(dir.path(), 1, StepRole::Judge), Ask::FailOpen);
    }
    assert_eq!(mode(), Mode::Normal);
    enable(dir.path(), &dead, true);
    assert_eq!(claim(dir.path(), 1, StepRole::Judge), Ask::FailOpen);
    assert_eq!(mode(), Mode::Fallback, "the graces neither failed nor reset the run");
}

/// Three failures (a 5xx and a 403 included) enter fallback; in fallback
/// a claim does not call the server and the tick draws one round.
#[test]
#[serial]
fn three_failures_enter_fallback_and_fallback_stops_asking() {
    reset_for_test();
    let dir = tempdir().unwrap();
    let five = TestServer::start(|_, _| (503, json!({})));
    let forbidden = TestServer::start(|_, _| (403, json!({})));
    enable(dir.path(), &five.url(), true);
    assert_eq!(claim(dir.path(), 1, StepRole::Judge), Ask::FailOpen);
    enable(dir.path(), &forbidden.url(), true);
    assert_eq!(claim(dir.path(), 1, StepRole::Judge), Ask::FailOpen);
    assert_eq!(mode(), Mode::Normal);
    assert_eq!(tick_budget(dir.path(), 9, 2), (9, false));
    assert_eq!(claim(dir.path(), 1, StepRole::Judge), Ask::FailOpen);
    assert_eq!(mode(), Mode::Fallback);

    let server = granting();
    enable(dir.path(), &server.url(), true);
    assert_eq!(claim(dir.path(), 1, StepRole::Judge), Ask::FailOpen);
    assert!(server.requests_to(proto::CLAIM_PATH).is_empty(), "fallback asks nobody");
    let (budget, signals) =
        crate::observability::ops::capture::capture(|| tick_budget(dir.path(), 9, 2));
    assert_eq!(budget, (2, true));
    let ticks: Vec<_> = signals
        .metrics
        .iter()
        .filter(|p| p.name == crate::telemetry::ops::MetricName::FleetDispatchFallbackTicks)
        .collect();
    assert_eq!(ticks.len(), 1);
    assert_eq!(ticks[0].labels["reason"], "status_4xx", "the failure that entered it");
}

/// The heartbeat carries every repo's live steps, the granted one with its
/// assignment, at most once per gap; a release names the outcome.
#[test]
#[serial]
fn the_heartbeat_and_the_release_reach_the_server() {
    reset_for_test();
    let (a, b) = (tempdir().unwrap(), tempdir().unwrap());
    let server = granting();
    enable(a.path(), &server.url(), true);
    enable(b.path(), &server.url(), true);
    on_reaper_tick(a.path(), &[]);
    assert!(server.requests().is_empty(), "no heartbeat before the tick noted capacity");

    let Ask::Granted(grant) = claim(a.path(), 7, StepRole::Judge) else {
        panic!("granted");
    };
    note_spawned(a.path(), "step-judge-7-1", grant);
    note_capacity(5, &BTreeSet::from(["builder".to_string()]));
    on_reaper_tick(b.path(), &[("step-doctor-9-1".into(), 9, StepRole::Doctor)]);
    on_reaper_tick(a.path(), &[("step-judge-7-1".into(), 7, StepRole::Judge)]);
    let beats = server.wait_for(proto::HEARTBEAT_PATH, 1);
    assert_eq!(beats.len(), 1, "one heartbeat per gap for the whole host");
    let beat = &beats[0];
    assert_eq!(beat["free_slots"], 5);
    assert!(!beat["roles_ok"]
        .as_array()
        .unwrap()
        .contains(&json!("builder")));
    assert!(beat["roles_ok"]
        .as_array()
        .unwrap()
        .contains(&json!("judge")));
    assert_eq!(beat["live"].as_array().unwrap().len(), 1, "a's steps were not yet recorded");
    assert_eq!(beat["live"][0]["item"], 9);
    assert!(beat["live"][0].get("assignment_id").is_none());

    // The next due heartbeat lists both repos, with the assignment.
    state().last_heartbeat = None;
    on_reaper_tick(a.path(), &[("step-judge-7-1".into(), 7, StepRole::Judge)]);
    let beats = server.wait_for(proto::HEARTBEAT_PATH, 2);
    let live = beats[1]["live"].as_array().unwrap();
    assert_eq!(live.len(), 2);
    assert!(live
        .iter()
        .any(|s| s["assignment_id"] == "a-1" && s["step"] == "judge"));

    release("step-judge-7-1", "verdict");
    let released = server.wait_for(proto::RELEASE_PATH, 1);
    assert_eq!(released[0]["assignment_id"], "a-1");
    assert_eq!(released[0]["outcome"], "verdict");
    release("step-judge-7-1", "verdict");
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(server.requests_to(proto::RELEASE_PATH).len(), 1, "released once");
}

/// A granted step that vanished from its registry without an end is
/// released; a refuse names its reason.
#[test]
#[serial]
fn a_vanished_step_is_released_and_a_refuse_names_its_reason() {
    reset_for_test();
    let dir = tempdir().unwrap();
    let server = granting();
    enable(dir.path(), &server.url(), true);
    let Ask::Granted(grant) = claim(dir.path(), 7, StepRole::Judge) else {
        panic!("granted");
    };
    refuse(&grant, proto::REFUSE_CLAIM_LOST);
    let refused = server.wait_for(proto::REFUSE_PATH, 1);
    assert_eq!(refused[0]["reason"], "claim_lost");
    assert_eq!(refused[0]["assignment_id"], "a-1");

    note_spawned(dir.path(), "step-judge-7-1", grant);
    on_reaper_tick(dir.path(), &[]);
    let released = server.wait_for(proto::RELEASE_PATH, 1);
    assert_eq!(released[0]["outcome"], "failed");
    assert!(state().assignments.is_empty());
}
