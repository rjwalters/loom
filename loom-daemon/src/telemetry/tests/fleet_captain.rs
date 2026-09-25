//! `host.health` → `is_captain` / `armed_singleton_jobs` wire-schema tests
//! (Issue #8848).
//!
//! A sibling module rather than more lines in `telemetry/tests.rs`, which is
//! at the `scripts/check-file-size-budget.sh` threshold — the same shape
//! `admission_brake.rs` next door already uses.
//!
//! What is asserted here is the *contract* a fleet consumer depends on, not
//! the sampling (that lives in `observability/collector.rs`) and not the gate
//! itself (that lives in `fleet_captain.rs`): `is_captain` is three-valued on
//! the wire, and a record emitted by a pre-#8848 daemon must still decode.

use super::super::*;
use super::ts;

/// A `HostHealthRecord` with only the captain fields varied — every other
/// field pinned at the "nothing measured" baseline so a captain assertion
/// below can never be satisfied by some unrelated field.
fn host_health_with_captain(
    is_captain: Option<bool>,
    armed_singleton_jobs: Vec<String>,
) -> TelemetryRecord {
    TelemetryRecord::HostHealth(HostHealthRecord {
        captured_at: ts(),
        daemon_version: "0.17.0".to_string(),
        build_commit: "unknown".to_string(),
        built_at: None,
        uptime_sec: 1,
        logical_cpus: 4,
        cpu_idle_fraction: None,
        load_per_core: None,
        worktree_root_free_gb: None,
        worktree_root_total_gb: None,
        active_sweep_ids: Vec::new(),
        dispatch_halted: false,
        halt_reason: None,
        managed_repos: Vec::new(),
        roles: RoleTickHealth::default(),
        protection: None,
        admission_brake: None,
        is_captain,
        armed_singleton_jobs,
    })
}

#[test]
fn host_health_round_trips_the_captain_fields() {
    // #8848: `is_captain` is THREE-valued on the wire, so all three states
    // must survive a round trip distinctly — collapsing `None` into `false`
    // is exactly what would make the dashboard's "no host reports
    // `is_captain: true`" check fire on every repo that never opted in.
    for (is_captain, jobs) in [
        (Some(true), vec!["edge-queue-pull".to_string()]),
        (Some(false), Vec::new()),
        (Some(false), vec!["edge-queue-pull".to_string(), "feed-staleness".to_string()]),
        (None, Vec::new()),
    ] {
        let record = host_health_with_captain(is_captain, jobs);
        let value = serde_json::to_value(&record).unwrap();
        let decoded: TelemetryRecord = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, record, "captain fields must survive a round trip");
    }
}

#[test]
fn host_health_omits_the_captain_fields_when_the_mechanism_does_not_apply() {
    // The default, behavior-unchanged case: no `fleet.captain` declared, so
    // `is_captain` is absent from the wire (NOT `false`) and
    // `armed_singleton_jobs` is absent (NOT `[]`) — the same
    // omit-when-unmeasurable contract every other optional field on this
    // struct follows.
    let record = host_health_with_captain(None, Vec::new());
    let value = serde_json::to_value(&record).unwrap();
    assert!(
        value.get("is_captain").is_none(),
        "\"the mechanism does not apply here\" must omit the key, not send `false`"
    );
    assert!(
        value.get("armed_singleton_jobs").is_none(),
        "an empty armed list must omit the key, not send `[]`"
    );
}

#[test]
fn host_health_sends_is_captain_false_distinctly_from_omitting_it() {
    // `Some(false)` — a captain IS declared and it is not this host — is a
    // real, load-bearing observation and must reach the wire, unlike `None`.
    let record = host_health_with_captain(Some(false), Vec::new());
    let value = serde_json::to_value(&record).unwrap();
    assert_eq!(
        value.get("is_captain"),
        Some(&serde_json::Value::Bool(false)),
        "a declared captain that is not this host must be reported, not omitted"
    );
}

#[test]
fn host_health_decodes_a_pre_8848_record_as_no_captain() {
    // Backward compatibility: a record from a daemon that predates #8848
    // carries neither key. `#[serde(default)]` must decode it as
    // `None`/empty rather than failing the whole envelope.
    let baseline = host_health_with_captain(None, Vec::new());
    let mut value = serde_json::to_value(&baseline).unwrap();
    let obj = value.as_object_mut().unwrap();
    obj.remove("is_captain");
    obj.remove("armed_singleton_jobs");
    let decoded: TelemetryRecord = serde_json::from_value(value).unwrap();
    let TelemetryRecord::HostHealth(health) = decoded else {
        panic!("expected a host.health record");
    };
    assert_eq!(health.is_captain, None);
    assert!(health.armed_singleton_jobs.is_empty());
}
