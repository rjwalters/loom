//! The poller's fleet-captain gate made visible (Issue #9014): a refused tick
//! is recorded in `status.json` and `ci-telemetry status` reads `refused`
//! with the reason, instead of `stale`/`never-polled` with none.

use chrono::{Duration, Utc};
use tempfile::TempDir;

use crate::ci_telemetry::state::{self, Health, PollStatus};
use crate::ci_telemetry::{collect_health, gate_tick, state_dir, SINGLETON_JOB_NAME};

fn root_with_config(config: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join(crate::config_resolver::LEGACY_CONFIG_REL);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, config).unwrap();
    dir
}

#[test]
fn a_no_captain_tick_is_recorded_and_reads_refused() {
    let root = root_with_config(r#"{"autonomous": {"ciTelemetry": {"enabled": true}}}"#);
    let now = Utc::now();
    assert!(!gate_tick(root.path(), "host-a", now), "no captain ⇒ the cycle must not run");

    let status = state::load_status(&state_dir(root.path()));
    let refusal = status
        .captain_refusal
        .clone()
        .expect("the refusal is recorded");
    assert!(refusal.no_captain_declared);
    assert!(refusal.reason.contains("no fleet.captain declared"), "{}", refusal.reason);
    match state::classify(&status, now, 120) {
        Health::Refused {
            no_captain_declared,
            ..
        } => assert!(no_captain_declared),
        other => panic!("expected refused, got {other:?}"),
    }

    let health = collect_health(root.path());
    assert!(health.enabled);
    assert_eq!(health.state, "refused");
    assert!(health.captain_refusal.is_some());

    // A later refused tick keeps the original `since`.
    let later = now + Duration::seconds(120);
    gate_tick(root.path(), "host-a", later);
    let again = state::load_status(&state_dir(root.path()))
        .captain_refusal
        .unwrap();
    assert_eq!(again.since, refusal.since);
    assert_eq!(again.last_at, later);
    crate::fleet_captain::disarm_singleton_job(SINGLETON_JOB_NAME);
}

#[test]
fn arming_clears_the_recorded_refusal() {
    let root = root_with_config(r#"{"fleet": {"captain": "host-b"}}"#);
    let now = Utc::now();
    assert!(!gate_tick(root.path(), "host-a", now));
    let refusal = state::load_status(&state_dir(root.path()))
        .captain_refusal
        .unwrap();
    assert!(!refusal.no_captain_declared, "a declared captain elsewhere is routine");
    assert!(refusal.reason.contains("host-b"), "{}", refusal.reason);

    assert!(gate_tick(root.path(), "host-b", now + Duration::seconds(1)));
    assert!(state::load_status(&state_dir(root.path()))
        .captain_refusal
        .is_none());
    crate::fleet_captain::disarm_singleton_job(SINGLETON_JOB_NAME);
}

#[test]
fn a_cycle_attempted_after_the_refusal_outranks_it() {
    // e.g. an operator's `ci-telemetry --once` on a refused host: the
    // attempted cycle is the newer fact, so the normal classification wins.
    let now = Utc::now();
    let status = PollStatus {
        last_attempt_at: Some(now),
        last_ok_at: Some(now),
        captain_refusal: Some(state::CaptainRefusal {
            reason: "refused".to_string(),
            no_captain_declared: true,
            since: now - Duration::seconds(600),
            last_at: now - Duration::seconds(60),
        }),
        ..PollStatus::default()
    };
    assert_eq!(state::classify(&status, now, 120).label(), "ok");
}

#[test]
fn a_pre_9014_status_file_still_parses() {
    let parsed: PollStatus =
        serde_json::from_str(r#"{"org": "2amlogic", "consecutive_failures": 0}"#).unwrap();
    assert!(parsed.captain_refusal.is_none());
}
