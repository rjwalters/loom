//! `host.health` → `admission_brake` wire-schema tests (Issue #8478).
//!
//! A sibling module rather than more lines in `telemetry/tests.rs`, which is at
//! the `scripts/check-file-size-budget.sh` threshold.
//!
//! What is asserted here is the *contract* a fleet consumer depends on, not the
//! sampling (that lives in `observability/collector/admission_brake_tests.rs`):
//! an absent brake must be absent from the wire rather than a fabricated
//! not-suppressed verdict, and a record emitted by a pre-#8478 daemon must still
//! decode.

use serde_json::Value;

use super::super::*;
use super::ts;

fn host_health_with(brake: Option<AdmissionBrakeSummary>) -> TelemetryRecord {
    TelemetryRecord::HostHealth(HostHealthRecord {
        captured_at: ts(),
        daemon_version: "0.19.250".to_string(),
        build_commit: "deadbeef".to_string(),
        built_at: None,
        uptime_sec: 43_440,
        logical_cpus: 18,
        cpu_idle_fraction: Some(0.0),
        load_per_core: Some(2.9),
        worktree_root_free_gb: None,
        worktree_root_total_gb: None,
        active_sweep_ids: Vec::new(),
        dispatch_halted: brake
            .as_ref()
            .is_some_and(|b| b.dispatch_suppressed_by_foreign_load),
        halt_reason: None,
        managed_repos: Vec::new(),
        roles: RoleTickHealth::default(),
        protection: None,
        admission_brake: brake,
    })
}

/// The incident's own shape: 18-core host, brake held 12h04m with zero sweeps
/// in flight, load entirely from 25 orphaned `ngspice` processes.
fn incident_summary() -> AdmissionBrakeSummary {
    AdmissionBrakeSummary {
        held: true,
        starving_since: Some(ts() - chrono::Duration::seconds(43_440)),
        starving_secs: Some(43_440),
        starvation_warn_secs: 300,
        escape_hatch_grants: 47,
        dispatch_suppressed_by_foreign_load: true,
        top_cpu_consumers: Some(
            " \u{2014} TOP CPU (best-effort host-wide `ps` sample, includes work Loom does not \
             own): ngspice \u{d7}25 (1843% cpu, parent launchd[1], reparented to pid 1)"
                .to_string(),
        ),
    }
}

#[test]
fn host_health_omits_the_brake_when_none_is_registered() {
    // `None` (no work-finder loop on this host, or a pre-#8478 daemon) must be
    // absent from the wire. A consumer reading an absent key as "dispatch not
    // suppressed" would reproduce exactly the #8478 blind spot.
    let record = host_health_with(None);
    let value = serde_json::to_value(&record).unwrap();
    assert!(
        value.get("admission_brake").is_none(),
        "absent must mean 'not reported', never a fabricated verdict"
    );
    let decoded: TelemetryRecord = serde_json::from_value(value).unwrap();
    assert_eq!(decoded, record);
}

#[test]
fn host_health_round_trips_a_starving_brake_with_its_duration_and_attribution() {
    let record = host_health_with(Some(incident_summary()));
    let value = serde_json::to_value(&record).unwrap();
    let brake = value
        .get("admission_brake")
        .expect("a registered brake must reach the wire");
    assert_eq!(brake.get("starving_secs").and_then(Value::as_i64), Some(43_440));
    assert_eq!(brake.get("starvation_warn_secs").and_then(Value::as_i64), Some(300));
    assert_eq!(
        brake
            .get("dispatch_suppressed_by_foreign_load")
            .and_then(Value::as_bool),
        Some(true),
        "the one field a fleet-level check should alert on"
    );
    assert!(brake
        .get("top_cpu_consumers")
        .and_then(Value::as_str)
        .is_some_and(|s| s.contains("ngspice")));
    let decoded: TelemetryRecord = serde_json::from_value(value).unwrap();
    assert_eq!(decoded, record);
}

#[test]
fn an_unstarved_brake_omits_the_duration_and_attribution_keys() {
    let record = host_health_with(Some(AdmissionBrakeSummary {
        held: false,
        starving_since: None,
        starving_secs: None,
        starvation_warn_secs: 300,
        escape_hatch_grants: 0,
        dispatch_suppressed_by_foreign_load: false,
        top_cpu_consumers: None,
    }));
    let value = serde_json::to_value(&record).unwrap();
    let brake = value.get("admission_brake").expect("registered");
    assert!(brake.get("starving_since").is_none());
    assert!(brake.get("starving_secs").is_none());
    assert!(
        brake.get("top_cpu_consumers").is_none(),
        "a healthy host never pays for a `ps` sample, so it has nothing to report"
    );
    // The always-present fields still state the healthy case positively, so a
    // consumer can distinguish "reported, fine" from "not reported".
    assert_eq!(brake.get("held").and_then(Value::as_bool), Some(false));
    assert_eq!(
        brake
            .get("dispatch_suppressed_by_foreign_load")
            .and_then(Value::as_bool),
        Some(false)
    );
    let decoded: TelemetryRecord = serde_json::from_value(value).unwrap();
    assert_eq!(decoded, record);
}

#[test]
fn a_pre_8478_host_health_payload_still_decodes() {
    // The backward-compatibility contract `protection` (#5352) established: an
    // envelope queued on disk by an older daemon must not fail to send after
    // an upgrade.
    let mut value = serde_json::to_value(host_health_with(None)).unwrap();
    value
        .as_object_mut()
        .expect("record serializes as an object")
        .remove("admission_brake");
    let decoded: TelemetryRecord = serde_json::from_value(value).unwrap();
    match decoded {
        TelemetryRecord::HostHealth(record) => assert!(record.admission_brake.is_none()),
        other => panic!("expected HostHealth, got {other:?}"),
    }
}
