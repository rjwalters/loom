//! `host.health` optional-field **omission** contract tests — the family
//! asserting that an unmeasured/inapplicable field is ABSENT from the wire
//! rather than a fabricated zero, empty list, or default verdict.
//!
//! Extracted from `telemetry/tests.rs` (Issue #8848) rather than grown in
//! place: adding `is_captain`/`armed_singleton_jobs` to `HostHealthRecord`
//! forced two new lines into every one of these struct literals, which pushed
//! `tests.rs` across `scripts/check-file-size-budget.sh`'s 1000-code-line
//! threshold. The policy's prescribed remedy is a new sibling module, and
//! these tests are one coherent family that already had two topical siblings
//! (`admission_brake.rs`, `fleet_captain.rs`). Test bodies are unchanged apart
//! from the two new struct fields.

use super::super::*;
use super::ts;

#[test]
fn host_health_omits_built_at_when_unknown() {
    // `build.rs` stamps the literal "unknown" when the build host had no
    // usable `date`; that must serialize as an ABSENT field, never as a
    // fabricated instant (the struct's "unknown != zero" contract).
    let record = TelemetryRecord::HostHealth(HostHealthRecord {
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
        is_captain: None,
        armed_singleton_jobs: Vec::new(),
        captainless_singleton_jobs: Vec::new(),
        memory: None,
    });
    let value = serde_json::to_value(&record).unwrap();
    assert!(
        value.get("built_at").is_none(),
        "an unknown build time must be absent, not a fabricated instant"
    );
    // The commit sentinel, unlike the instant, IS sent — "unknown" is a
    // meaningful answer for a tarball build, not a missing measurement.
    assert_eq!(
        value
            .get("build_commit")
            .and_then(serde_json::Value::as_str),
        Some("unknown")
    );
    // Still round-trips.
    let decoded: TelemetryRecord = serde_json::from_value(value).unwrap();
    assert_eq!(decoded, record);
}

#[test]
fn host_health_omits_managed_repos_when_empty() {
    // `skip_serializing_if = "Vec::is_empty"` — a host with no registered
    // workspaces sends no key at all, mirroring `active_sweep_ids`.
    let record = TelemetryRecord::HostHealth(HostHealthRecord {
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
        is_captain: None,
        armed_singleton_jobs: Vec::new(),
        captainless_singleton_jobs: Vec::new(),
        memory: None,
    });
    let value = serde_json::to_value(&record).unwrap();
    assert!(
        value.get("managed_repos").is_none(),
        "an empty roster must be absent from the wire, not an empty array"
    );
}

#[test]
fn host_health_omits_persistent_when_empty_but_still_carries_roles() {
    // A `total: 0` (or all-ok) summary must still be sent — "no role
    // ticks sampled" / "every tick ok" is meaningful information, not
    // nothing to report — but its empty `persistent` list is omitted from
    // the wire, mirroring `managed_repos`/`active_sweep_ids`.
    let record = TelemetryRecord::HostHealth(HostHealthRecord {
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
        roles: RoleTickHealth {
            total: 5,
            ok: 5,
            persistent: Vec::new(),
        },
        protection: None,
        admission_brake: None,
        is_captain: None,
        armed_singleton_jobs: Vec::new(),
        captainless_singleton_jobs: Vec::new(),
        memory: None,
    });
    let value = serde_json::to_value(&record).unwrap();
    let roles = value.get("roles").unwrap();
    assert_eq!(roles.get("total").and_then(serde_json::Value::as_u64), Some(5));
    assert!(roles.get("persistent").is_none());
    let decoded: TelemetryRecord = serde_json::from_value(value).unwrap();
    assert_eq!(decoded, record);
}

#[test]
fn host_health_omits_worktree_root_total_gb_when_unmeasurable() {
    // "unknown != zero" (#4164/#5356): an unmeasurable total must be
    // ABSENT from the wire, never a fabricated 0.
    let record = TelemetryRecord::HostHealth(HostHealthRecord {
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
        is_captain: None,
        armed_singleton_jobs: Vec::new(),
        captainless_singleton_jobs: Vec::new(),
        memory: None,
    });
    let value = serde_json::to_value(&record).unwrap();
    assert!(
        value.get("worktree_root_total_gb").is_none(),
        "an unmeasurable total must be absent, not a fabricated 0"
    );
    let decoded: TelemetryRecord = serde_json::from_value(value).unwrap();
    assert_eq!(decoded, record);
}

#[test]
fn host_health_omits_protection_when_absent() {
    // `None` (a pre-#5352 daemon, or a probe that could not construct a
    // report at all) must be absent from the wire, not a fabricated
    // `"unknown"` verdict.
    let record = TelemetryRecord::HostHealth(HostHealthRecord {
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
        is_captain: None,
        armed_singleton_jobs: Vec::new(),
        captainless_singleton_jobs: Vec::new(),
        memory: None,
    });
    let value = serde_json::to_value(&record).unwrap();
    assert!(
        value.get("protection").is_none(),
        "an absent protection report must omit the key, not fabricate a verdict"
    );
    let decoded: TelemetryRecord = serde_json::from_value(value).unwrap();
    assert_eq!(decoded, record);
}

#[test]
fn host_health_free_without_total_serializes_with_no_fabricated_denominator() {
    // Acceptance criterion: a record with free-but-no-total (e.g. a total
    // probe that failed independently, or simply a daemon that has not
    // measured total yet) must still send its free reading — and must
    // NEVER synthesize a total to go with it.
    let record = TelemetryRecord::HostHealth(HostHealthRecord {
        captured_at: ts(),
        daemon_version: "0.17.0".to_string(),
        build_commit: "8c16fb5b".to_string(),
        built_at: None,
        uptime_sec: 1,
        logical_cpus: 4,
        cpu_idle_fraction: None,
        load_per_core: None,
        worktree_root_free_gb: Some(200),
        worktree_root_total_gb: None,
        active_sweep_ids: Vec::new(),
        dispatch_halted: false,
        halt_reason: None,
        managed_repos: Vec::new(),
        roles: RoleTickHealth::default(),
        protection: None,
        admission_brake: None,
        is_captain: None,
        armed_singleton_jobs: Vec::new(),
        captainless_singleton_jobs: Vec::new(),
        memory: None,
    });
    let value = serde_json::to_value(&record).unwrap();
    assert_eq!(
        value
            .get("worktree_root_free_gb")
            .and_then(serde_json::Value::as_u64),
        Some(200),
        "the free reading must still be sent on its own"
    );
    assert!(
        value.get("worktree_root_total_gb").is_none(),
        "no denominator must be fabricated for a total the probe never measured"
    );
}

#[test]
fn host_health_omits_watchdog_provisioned_when_the_probe_could_not_answer() {
    // `ProtectionState::Unknown`: the probe ran but the watchdog-
    // provisioning check itself could not answer (no launchctl/systemctl).
    // `watchdog_provisioned` must be absent, not a fabricated `false`.
    let record = TelemetryRecord::HostHealth(HostHealthRecord {
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
        protection: Some(HostProtectionSummary {
            state: "unknown".to_string(),
            watchdog_provisioned: None,
        }),
        admission_brake: None,
        is_captain: None,
        armed_singleton_jobs: Vec::new(),
        captainless_singleton_jobs: Vec::new(),
        memory: None,
    });
    let value = serde_json::to_value(&record).unwrap();
    let protection = value.get("protection").unwrap();
    assert_eq!(protection.get("state").and_then(serde_json::Value::as_str), Some("unknown"));
    assert!(protection.get("watchdog_provisioned").is_none());
    let decoded: TelemetryRecord = serde_json::from_value(value).unwrap();
    assert_eq!(decoded, record);
}
