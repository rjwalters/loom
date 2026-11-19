//! The worker's filing-lock tier against a real loopback server (#13918).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::super::test_server::TestServer;
use super::super::worker::reset_for_test;
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
        proto::FILING_LOCK_PATH => (200, json!({"server_epoch":"e1","granted":true,"ttl_s":60})),
        _ => (200, json!({"server_epoch":"e1","known":true})),
    })
}

fn held() -> TestServer {
    TestServer::start(|_, _| {
        (
            200,
            json!({"server_epoch":"e1","granted":false,"held_by":"worker-b","retry_after_s":1}),
        )
    })
}

#[test]
#[serial]
fn with_fleet_mode_off_nothing_is_asked() {
    reset_for_test();
    let dir = tempdir().unwrap();
    let server = granting();
    enable(dir.path(), &server.url(), false);
    let before = fallbacks();
    let tier = acquire(dir.path(), "architect", "h-1", Duration::from_secs(1));
    assert_eq!(tier, Tier::Off);
    assert!(tier.may_file());
    assert!(server.requests().is_empty(), "{:?}", server.requests());
    assert_eq!(fallbacks(), before, "off is not a fallback");
}

#[test]
#[serial]
fn a_grant_is_held_under_the_callers_holder_id_and_released_by_it() {
    reset_for_test();
    let dir = tempdir().unwrap();
    let server = granting();
    enable(dir.path(), &server.url(), true);
    let Tier::Granted(hold) = acquire(dir.path(), "architect", "h-1", Duration::from_secs(1))
    else {
        panic!("granted");
    };
    assert_eq!(hold.holder, "h-1");
    let asked = server.requests_to(proto::FILING_LOCK_PATH);
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0]["holder"], "h-1");
    assert_eq!(asked[0]["label"], "architect");
    assert_eq!(asked[0]["host_id"], worker::host_id());

    release(&hold);
    let unlocked = server.requests_to(proto::FILING_UNLOCK_PATH);
    assert_eq!(unlocked.len(), 1);
    assert_eq!(unlocked[0]["holder"], "h-1");
}

#[test]
#[serial]
fn a_slot_held_past_the_wait_defers_and_never_falls_open() {
    reset_for_test();
    let dir = tempdir().unwrap();
    let server = held();
    enable(dir.path(), &server.url(), true);
    let before = fallbacks();
    let started = Instant::now();
    let tier = acquire(dir.path(), "auditor", "h-2", Duration::from_millis(300));
    let Tier::Deferred { reason } = &tier else {
        panic!("deferred, got {tier:?}");
    };
    assert!(reason.contains("worker-b"), "{reason}");
    assert!(!tier.may_file());
    assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
    assert!(server.requests_to(proto::FILING_LOCK_PATH).len() >= 2, "it polled");
    assert_eq!(fallbacks(), before, "a refusal is an answer, not a fallback");
    assert_eq!(super::super::worker::mode(), Mode::Normal);
}

#[test]
#[serial]
fn a_zero_wait_still_asks_once() {
    reset_for_test();
    let dir = tempdir().unwrap();
    let server = held();
    enable(dir.path(), &server.url(), true);
    let tier = acquire(dir.path(), "x", "h-3", Duration::ZERO);
    assert!(matches!(tier, Tier::Deferred { .. }));
    assert_eq!(server.requests_to(proto::FILING_LOCK_PATH).len(), 1);
}

#[test]
#[serial]
fn an_unreachable_captain_fails_open_is_counted_and_three_failures_enter_fallback() {
    reset_for_test();
    let dir = tempdir().unwrap();
    enable(dir.path(), &TestServer::dead_url(), true);
    let before = fallbacks();
    for i in 1..=3 {
        let tier = acquire(dir.path(), "architect", &format!("h-{i}"), Duration::from_secs(5));
        assert_eq!(
            tier,
            Tier::FailOpen {
                reason: "transport".into()
            },
            "call {i}"
        );
        assert!(tier.may_file());
    }
    assert_eq!(fallbacks(), before + 3);
    assert_eq!(super::super::worker::mode(), Mode::Fallback, "the claim hysteresis is reused");

    // In fallback the captain is not asked at all, and the fallback still counts.
    let server = granting();
    enable(dir.path(), &server.url(), true);
    let tier = acquire(dir.path(), "architect", "h-4", Duration::from_secs(5));
    assert_eq!(
        tier,
        Tier::FailOpen {
            reason: REASON_FALLBACK.into()
        }
    );
    assert!(server.requests().is_empty(), "{:?}", server.requests());
    assert_eq!(fallbacks(), before + 4);
    // …and a release is skipped for the same reason.
    release(&Hold::new(&server.url(), "h-4"));
    assert!(server.requests().is_empty());
}

#[test]
#[serial]
fn a_403_from_the_captain_is_a_fail_open_too() {
    reset_for_test();
    let dir = tempdir().unwrap();
    let server = TestServer::start(|_, _| (403, json!({"server_epoch":"e1","error":"no"})));
    enable(dir.path(), &server.url(), true);
    let tier = acquire(dir.path(), "x", "h-5", Duration::from_secs(1));
    assert_eq!(
        tier,
        Tier::FailOpen {
            reason: "status_4xx".into()
        }
    );
}

#[test]
#[serial]
fn the_filing_lock_layers_the_captain_over_the_host_lock_and_releases_both_in_reverse() {
    use crate::filing_lock::{acquire_in, holder_path, with_captain_tier, AcquireOutcome};
    reset_for_test();
    let dir = tempdir().unwrap();
    let store = dir.path().join("store");
    let server = granting();
    enable(dir.path(), &server.url(), true);
    let local = |label: &str| {
        acquire_in(
            &store,
            "host-a",
            std::process::id(),
            label,
            Duration::from_millis(100),
            Duration::from_millis(10),
            Duration::from_secs(300),
        )
    };
    let outcome =
        with_captain_tier(local("architect"), dir.path(), "architect", Duration::from_secs(1));
    assert!(matches!(outcome, AcquireOutcome::Acquired(_)), "{}", outcome.as_str());
    assert!(holder_path(&store).is_dir());
    assert_eq!(server.requests_to(proto::FILING_LOCK_PATH).len(), 1);
    drop(outcome);
    assert!(!holder_path(&store).exists(), "the host lock is released");
    assert_eq!(server.requests_to(proto::FILING_UNLOCK_PATH).len(), 1, "and the fleet slot");

    // A slot held elsewhere defers, and the host lock is handed back so a
    // local peer is not stuck behind a burst that is not filing.
    let busy = held();
    enable(dir.path(), &busy.url(), true);
    let deferred = with_captain_tier(local("auditor"), dir.path(), "auditor", Duration::ZERO);
    assert!(matches!(deferred, AcquireOutcome::Deferred { .. }), "{}", deferred.as_str());
    assert!(!deferred.may_file());
    assert!(!holder_path(&store).exists());

    // A deferred or degraded host lock never reaches the captain.
    let untouched = with_captain_tier(
        AcquireOutcome::DegradedOpen { reason: "x".into() },
        dir.path(),
        "x",
        Duration::ZERO,
    );
    assert!(matches!(untouched, AcquireOutcome::DegradedOpen { .. }));
    assert_eq!(busy.requests().len(), 1);
}

/// #14129, the Judge's blocking item: the live path is the one-shot
/// `filing-lock` subcommand, whose only emission machinery is the standalone
/// sink it registers at startup and flushes at exit. This is that sequence:
/// with the sink registered (no senders — the process lives a second), an
/// acquire's span lands in the tagged queue file with the filing attributes,
/// instead of being dropped the way it was before the CLI registered a sink.
#[test]
#[serial]
fn the_one_shot_cli_path_emits_the_acquire_span_to_the_queue() {
    reset_for_test();
    let dir = tempdir().unwrap();
    let server = granting();
    // One `.loom/config.json` carries both blocks the CLI resolves: fleet
    // dispatch at the loopback captain (as `enable` would write it) plus the
    // observability block. The exporter endpoint is a bound port nothing
    // listens on: nothing leaves the process, so the queue file is what the
    // test asserts.
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let key = dir.path().join("ingest.key");
    std::fs::write(&key, "k\n").unwrap();
    crate::sweep_registry::test_support::write_config(
        dir.path(),
        &json!({
            "fleetDispatch": {"enabled": true, "url": server.url()},
            "observability": {
                "enabled": true,
                "ingestKeyFile": key.to_string_lossy(),
                "exporters": [
                    {"kind": "otlp", "endpoint": format!("http://127.0.0.1:{}", dead.local_addr().unwrap().port())}
                ]
            }
        })
        .to_string(),
    );
    let config = crate::observability::read_config(dir.path());
    assert!(
        crate::observability::standalone_ops::register_sink(
            &config,
            dir.path(),
            "filing-lock-test",
        ),
        "the CLI's register_sink plans the exporter"
    );

    let Tier::Granted(_) = acquire(dir.path(), "architect", "h-9", Duration::from_secs(1)) else {
        panic!("granted");
    };

    let queue = crate::observability::queue::DurableQueue::open(
        dir.path()
            .join(".loom/logs/filing-lock-test-ops-queue.0.jsonl"),
        100,
    );
    let queued = queue.peek_batch(10);
    assert_eq!(queued.len(), 1, "the acquire's span was queued: {queued:?}");
    let crate::telemetry::TelemetryRecord::Span(span) = &queued[0].record else {
        panic!("a span record, got {:?}", queued[0].record);
    };
    assert_eq!(
        span.attributes
            .get("loom.fleet_dispatch.endpoint")
            .map(String::as_str),
        Some(proto::FILING_LOCK_PATH)
    );
    assert_eq!(
        span.attributes
            .get("loom.fleet_dispatch.result")
            .map(String::as_str),
        Some("granted")
    );
    assert_eq!(
        span.attributes
            .get("loom.fleet_dispatch.side")
            .map(String::as_str),
        Some("worker")
    );
    assert_eq!(
        span.attributes
            .get("loom.fleet_dispatch.reason")
            .map(String::as_str),
        Some("granted")
    );
}
