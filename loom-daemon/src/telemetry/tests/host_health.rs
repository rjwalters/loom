//! `host.health` wire-format tests — build identity (#4956), managed repos
//! (#4976), roles summary (#5022), worktree root (#5356), and protection
//! state (#5352), plus their back-compat decode tests.
//!
//! Extracted from `telemetry/tests.rs` rather than grown in place: the merge
//! of #11488's `phase_tap_usage` fixture lines pushed `tests.rs` across
//! `scripts/check-file-size-budget.sh`'s 1000-code-line threshold, and the
//! policy's prescribed remedy is a new sibling module (as for
//! `host_health_omissions.rs`). Test bodies are unchanged.

use super::super::*;
use super::{host_health, ts};

// ------------------------------------------------------------------
// host.health build identity (#4956).
// ------------------------------------------------------------------

#[test]
fn host_health_serializes_build_identity_alongside_version() {
    let value = serde_json::to_value(host_health()).unwrap();
    assert_eq!(
        value
            .get("daemon_version")
            .and_then(serde_json::Value::as_str),
        Some("0.16.0")
    );
    // The whole point of #4956: two builds sharing `daemon_version` are
    // told apart by the commit, so it must be on the wire.
    assert_eq!(
        value
            .get("build_commit")
            .and_then(serde_json::Value::as_str),
        Some("8c16fb5b")
    );
    assert_eq!(
        value.get("built_at").and_then(serde_json::Value::as_str),
        Some("2026-07-30T12:00:00Z")
    );
}

#[test]
fn host_health_round_trips_build_identity() {
    let json = serde_json::to_string(&host_health()).unwrap();
    let decoded: TelemetryRecord = serde_json::from_str(&json).unwrap();
    match decoded {
        TelemetryRecord::HostHealth(r) => {
            assert_eq!(r.build_commit, "8c16fb5b");
            assert_eq!(r.built_at, Some(ts()));
        }
        other => panic!("expected HostHealth, got {other:?}"),
    }
}

#[test]
fn host_health_from_a_pre_4956_daemon_still_decodes() {
    // Backward compatibility: a record emitted by a daemon that predates
    // the build-identity fields must decode, not poison the batch.
    let json = r#"{
        "kind": "host.health",
        "captured_at": "2026-07-30T12:00:00Z",
        "daemon_version": "0.16.0",
        "uptime_sec": 86400,
        "logical_cpus": 28
    }"#;
    let decoded: TelemetryRecord = serde_json::from_str(json).unwrap();
    match decoded {
        TelemetryRecord::HostHealth(r) => {
            assert_eq!(r.daemon_version, "0.16.0");
            assert_eq!(r.build_commit, "");
            assert_eq!(r.built_at, None);
        }
        other => panic!("expected HostHealth, got {other:?}"),
    }
}

// ------------------------------------------------------------------
// host.health managed_repos roster (#4976).
// ------------------------------------------------------------------

#[test]
fn host_health_serializes_managed_repos_with_slug_and_visibility() {
    let value = serde_json::to_value(host_health()).unwrap();
    let repos = value
        .get("managed_repos")
        .and_then(serde_json::Value::as_array)
        .unwrap();
    assert_eq!(repos.len(), 2);
    assert_eq!(repos[0].get("slug").and_then(serde_json::Value::as_str), Some("rjwalters/loom"));
    assert_eq!(
        repos[0]
            .get("visibility")
            .and_then(serde_json::Value::as_str),
        Some("public")
    );
    assert_eq!(
        repos[1].get("slug").and_then(serde_json::Value::as_str),
        Some("2AMLogic/gf180-pll")
    );
    assert_eq!(
        repos[1]
            .get("visibility")
            .and_then(serde_json::Value::as_str),
        Some("private")
    );
    // #9244: a registered repo's dispatch priority ships; an unknown one is
    // omitted.
    assert_eq!(repos[0]["priority"], 0);
    assert!(repos[1].get("priority").is_none());
}

#[test]
fn host_health_from_a_pre_4976_daemon_still_decodes() {
    // Backward compatibility: a record emitted by a daemon that predates
    // the managed-repo roster must decode with an empty roster, not fail
    // the whole envelope.
    let json = r#"{
        "kind": "host.health",
        "captured_at": "2026-07-30T12:00:00Z",
        "daemon_version": "0.16.0",
        "uptime_sec": 86400,
        "logical_cpus": 28
    }"#;
    let decoded: TelemetryRecord = serde_json::from_str(json).unwrap();
    match decoded {
        TelemetryRecord::HostHealth(r) => {
            assert!(r.managed_repos.is_empty());
        }
        other => panic!("expected HostHealth, got {other:?}"),
    }
}

#[test]
fn managed_repo_entry_with_missing_visibility_defaults_to_private() {
    // A repo entry with no `visibility` tag at all must decode Private —
    // the same private-safe-default every other visibility field holds.
    let json = r#"{"slug": "owner/repo"}"#;
    let decoded: ManagedRepoEntry = serde_json::from_str(json).unwrap();
    assert_eq!(decoded.visibility, RepoVisibility::Private);
    assert_eq!(decoded.slug, "owner/repo");
    // No `priority` either (an older daemon): it decodes as absent.
    assert_eq!(decoded.priority, None);
}

#[test]
fn managed_repo_entry_priority_is_on_the_wire_only_when_known() {
    // #9244 / loom-ui#153: `priority` ships for a registered workspace and is
    // omitted (never a fabricated default) for one that is not.
    let known = ManagedRepoEntry {
        slug: "rjwalters/loom".to_string(),
        visibility: RepoVisibility::Public,
        priority: Some(0),
        stale_blocked_release: None,
    };
    let wire = serde_json::to_value(&known).unwrap();
    assert_eq!(wire["priority"], 0);
    let unknown = ManagedRepoEntry {
        priority: None,
        ..known
    };
    let wire = serde_json::to_value(&unknown).unwrap();
    assert!(wire.get("priority").is_none(), "{wire}");
}

#[test]
fn managed_repo_entry_carries_release_counters_only_once_ticked() {
    // #10763: the release pass's per-repo tallies ride `managed_repos`, and
    // are omitted (never a fabricated zero) before the pass ticked the repo.
    let mut entry = ManagedRepoEntry {
        slug: "2AMLogic/loom-ui".to_string(),
        visibility: RepoVisibility::Private,
        priority: None,
        stale_blocked_release: None,
    };
    let wire = serde_json::to_value(&entry).unwrap();
    assert!(wire.get("stale_blocked_release").is_none(), "{wire}");
    entry.stale_blocked_release = Some(StaleBlockedReleaseCounters {
        released: 3,
        reparked: 2,
        last_outcome: Some("ran".to_string()),
        ticks: std::collections::BTreeMap::from([("ran".to_string(), 1)]),
    });
    let wire = serde_json::to_value(&entry).unwrap();
    assert_eq!(wire["stale_blocked_release"]["released"], 3);
    assert_eq!(wire["stale_blocked_release"]["reparked"], 2);
    assert_eq!(wire["stale_blocked_release"]["ticks"]["ran"], 1);
    let back: ManagedRepoEntry = serde_json::from_value(wire).unwrap();
    assert_eq!(back, entry);
    // A pre-#10763 entry still decodes.
    let old: ManagedRepoEntry =
        serde_json::from_value(serde_json::json!({"slug": "o/r", "visibility": "private"}))
            .unwrap();
    assert!(old.stale_blocked_release.is_none());
}

// ------------------------------------------------------------------
// host.health role-tick health (#5022).
// ------------------------------------------------------------------

#[test]
fn host_health_serializes_role_tick_health_persistent_failures() {
    let value = serde_json::to_value(host_health()).unwrap();
    let roles = value.get("roles").unwrap();
    assert_eq!(roles.get("total").and_then(serde_json::Value::as_u64), Some(12));
    assert_eq!(roles.get("ok").and_then(serde_json::Value::as_u64), Some(10));
    let persistent = roles
        .get("persistent")
        .and_then(serde_json::Value::as_array)
        .unwrap();
    assert_eq!(persistent.len(), 1);
    assert_eq!(
        persistent[0]
            .get("role")
            .and_then(serde_json::Value::as_str),
        Some("judge")
    );
    assert_eq!(
        persistent[0]
            .get("detail")
            .and_then(serde_json::Value::as_str),
        Some("no-token-pool")
    );
}

#[test]
fn host_health_from_a_pre_5022_daemon_decodes_with_a_zero_value_roles_summary() {
    // Backward compatibility: a record emitted by a daemon that predates
    // role-tick health must decode with the zero-value ("nothing sampled")
    // summary, never fail the whole envelope.
    let json = r#"{
        "kind": "host.health",
        "captured_at": "2026-07-30T12:00:00Z",
        "daemon_version": "0.16.0",
        "uptime_sec": 86400,
        "logical_cpus": 28
    }"#;
    let decoded: TelemetryRecord = serde_json::from_str(json).unwrap();
    match decoded {
        TelemetryRecord::HostHealth(r) => {
            assert_eq!(r.roles, RoleTickHealth::default());
            assert_eq!(r.roles.total, 0);
            assert!(r.roles.persistent.is_empty());
        }
        other => panic!("expected HostHealth, got {other:?}"),
    }
}

// ------------------------------------------------------------------
// host.health worktree_root_total_gb (#5356).
// ------------------------------------------------------------------

#[test]
fn host_health_serializes_worktree_root_total_gb_when_measurable() {
    let value = serde_json::to_value(host_health()).unwrap();
    assert_eq!(
        value
            .get("worktree_root_free_gb")
            .and_then(serde_json::Value::as_u64),
        Some(200)
    );
    assert_eq!(
        value
            .get("worktree_root_total_gb")
            .and_then(serde_json::Value::as_u64),
        Some(1000)
    );
}

// ------------------------------------------------------------------
// host.health watchdog/crash-protection state (#5352).
// ------------------------------------------------------------------

#[test]
fn host_health_serializes_protection_state_and_watchdog_provisioned() {
    let value = serde_json::to_value(host_health()).unwrap();
    let protection = value.get("protection").unwrap();
    assert_eq!(protection.get("state").and_then(serde_json::Value::as_str), Some("protected"));
    assert_eq!(
        protection
            .get("watchdog_provisioned")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
}

#[test]
fn host_health_from_a_pre_5356_daemon_still_decodes() {
    // Backward compatibility: a record emitted by a daemon that predates
    // worktree_root_total_gb (this includes a record that DOES carry
    // worktree_root_free_gb, since that field existed first) must decode
    // with an absent total, not fail the whole envelope.
    let json = r#"{
        "kind": "host.health",
        "captured_at": "2026-07-30T12:00:00Z",
        "daemon_version": "0.16.0",
        "uptime_sec": 86400,
        "logical_cpus": 28,
        "worktree_root_free_gb": 200
    }"#;
    let decoded: TelemetryRecord = serde_json::from_str(json).unwrap();
    match decoded {
        TelemetryRecord::HostHealth(r) => {
            assert_eq!(r.worktree_root_free_gb, Some(200));
            assert_eq!(
                r.worktree_root_total_gb, None,
                "a pre-#5356 record has no total key at all, which must decode as None, not 0"
            );
        }
        other => panic!("expected HostHealth, got {other:?}"),
    }
}

#[test]
fn host_health_from_a_pre_5352_daemon_decodes_with_protection_absent() {
    // Backward compatibility: a record emitted by a daemon that predates
    // watchdog/crash-protection telemetry must decode with `protection:
    // None`, never fail the whole envelope — and never a false
    // "unprotected" negative synthesized from its absence.
    let json = r#"{
        "kind": "host.health",
        "captured_at": "2026-07-30T12:00:00Z",
        "daemon_version": "0.16.0",
        "uptime_sec": 86400,
        "logical_cpus": 28
    }"#;
    let decoded: TelemetryRecord = serde_json::from_str(json).unwrap();
    match decoded {
        TelemetryRecord::HostHealth(r) => {
            assert!(r.protection.is_none());
        }
        other => panic!("expected HostHealth, got {other:?}"),
    }
}
