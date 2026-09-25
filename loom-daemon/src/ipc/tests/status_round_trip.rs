//! The `Request::DaemonStatus` / `Response::DaemonStatus` wire round-trip,
//! split out of `ipc/tests.rs` (#7990) so its deliberately exhaustive
//! [`crate::types::DaemonStatusReport`] literal — every field named, so a
//! schema change fails here rather than skewing the wire silently — can keep
//! growing with the schema without pushing its parent past the
//! `.loom/docs/file-size-policy.md` ratchet.

use super::*;

/// `Request::DaemonStatus` / `Response::DaemonStatus` must survive a serde
/// round-trip over the wire (pattern: the existing Ping/Pong probe + the
/// dispatch serde round-trips).
#[test]
fn test_daemon_status_request_response_round_trip() {
    // Request: unit variant, `{"type":"DaemonStatus"}`.
    let req = Request::DaemonStatus;
    let json = serde_json::to_string(&req).expect("serialize request");
    assert_eq!(json, r#"{"type":"DaemonStatus"}"#);
    let back: Request = serde_json::from_str(&json).expect("deserialize request");
    assert!(matches!(back, Request::DaemonStatus));

    // Response: carries the full report.
    let report = DaemonStatusReport {
        journal_adopted_at_startup: 0,
        in_flight: vec![],
        unregistered_locked: vec![],
        stale_sweeps: vec![],
        token_pool_size: 4,
        token_pool_dir: Some(std::path::PathBuf::from("/repo/a/.loom/tokens")),
        disk_headroom: 10,
        ram_headroom: 10,
        logical_cpus: 8,
        loadavg_1m: Some(1.25),
        cpu_idle_fraction: Some(0.90),
        capacity_bound: false,
        preflight_advisory_active: false,
        preflight_advisory_message: None,
        preflight_advisory_changed_at: None,
        configured_max: 5,
        dynamic_cap: 3,
        main_health_gate_halted: true,
        main_health_gate_not_evaluated: false,
        main_health_gate_not_evaluated_reason: None,
        main_health_gate_enabled: Some(true),
        main_health_gate_verdict_at: Some(chrono::Utc::now()),
        main_health_gate_deferred: false,
        main_health_gate_deferred_reason: None,
        main_health_gate_verdict_tier: Some("full".to_string()),
        capacity: crate::types::CapacityReport {
            ranking_present: true,
            total_accounts: 4,
            healthy_accounts: 3,
            exhausted_accounts: 1,
            token_axis_limit: 3,
            token_bound: true,
        },
        per_repo: vec![crate::types::RepoStatus {
            root: std::path::PathBuf::from("/repo/a"),
            priority: 100,
            in_flight_count: 0,
            health_gate_halted: true,
            quarantined_issues: vec![101, 202],
            health_gate_not_evaluated: false,
            health_gate_not_evaluated_reason: None,
            health_gate_enabled: Some(true),
            health_gate_verdict_at: Some(chrono::Utc::now()),
            root_missing: false,
            health_gate_deferred: false,
            health_gate_deferred_reason: None,
            health_gate_verdict_tier: Some("full".to_string()),
            role_runner_enabled: true,
            role_runner_roles: vec!["champion".to_string()],
            role_runner_intervals: std::collections::BTreeMap::new(),
            role_runner_on_idle_roles: vec![],
            role_runner_on_idle_promotions: vec![],
            role_runner_env_override: None,
            role_runner_shard: None,
            token_pool_dir: Some(std::path::PathBuf::from("/repo/a/.loom/tokens")),
            ranking_present: true,
            ranking_age_secs: Some(120),
            stash_total_count: 0,
            stash_quarantine_count: 0,
            stash_oldest_age_secs: None,
            stash_non_quarantine_unrecoverable_count: 0,
            stash_non_quarantine_unrecoverable_oldest_age_secs: None,
            sweep_command_missing: false,
        }],
        role_runner_host_env_override: None,
        role_runner_shard: None,
        credential_preflight: Some(test_credential_preflight()),
        draining: false,
        drain_deadline: None,
        drain_note: None,
        drain_roll: None,
        drain_paused_by_day: std::collections::BTreeMap::new(),
        auto_update_enabled: true,
        auto_update_last_check: Some(chrono::Utc::now()),
        auto_update_last_roll: Some(chrono::Utc::now()),
        auto_update_consecutive_failures: 2,
        auto_update_backoff_secs: Some(120),
        auto_update_terminal_reason: None,
        auto_update_note: Some("within settle window".to_string()),
        auto_update_artifact_version: Some("0.19.24".to_string()),
        auto_update_artifact_published_at: Some("2026-09-13T12:00:00Z".to_string()),
        auto_update_stale_repo_ticks: 0,
        auto_update_stale_repo: None,
        host_breaker: None,
        admission_brake: None,
        rate_limit_breaker: None,
        safehouse: Some(crate::types::SafehouseStatus {
            state: "connected".to_string(),
            socket: Some(std::path::PathBuf::from("/tmp/safehoused.sock")),
            room: Some("fleet".to_string()),
            reason: None,
        }),
        work_finder_enabled: Some(true),
        last_work_finder_tick: Some(crate::types::WorkFinderTickSummary {
            at: chrono::Utc::now(),
            max_concurrent: 3,
            seen: 9,
            dispatched: 1,
            skipped_in_flight: 8,
            ..Default::default()
        }),
        role_tick_records: vec![crate::types::RoleTickRecord {
            root: std::path::PathBuf::from("/repo/a"),
            role: "champion".to_string(),
            at: chrono::Utc::now(),
            ok: true,
            detail: None,
            pool_exhausted: false,
        }],
        role_last_tick: vec![crate::types::RoleLastTick {
            root: std::path::PathBuf::from("/repo/a"),
            role: "champion".to_string(),
            at: chrono::Utc::now(),
            ok: true,
            detail: None,
            consecutive_identical_failures: 0,
        }],
        active_role_agents: 3,
        role_agent_max_concurrent: Some(7),
        daemon_pid: Some(99917),
        pid_file: Some(std::path::PathBuf::from("/repo/a/.loom/.daemon.pid")),
        daemon_build_commit: Some("18887b5c".to_string()),
        daemon_built_at_raw: Some("2026-08-02T03:09:51Z".to_string()),
        work_finder_interval_secs: Some(60),
        observability_host_id_mismatch: Some(crate::types::ObservabilityHostIdMismatch {
            daemon_host_id: "robb-studio".to_string(),
            ingest_host_id: "robb-pro".to_string(),
            first_seen_at: chrono::Utc::now(),
        }),
        observability_export: Some(crate::types::ObservabilityExportStatus {
            state: crate::types::ObservabilityExportState::HostIdMismatch,
            host_id: Some("robb-studio".to_string()),
            ingest_host_id: Some("robb-pro".to_string()),
            endpoint: Some("https://dashboard.example/ingest".to_string()),
            exporter: Some("https".to_string()),
            started_at: Some(chrono::Utc::now()),
            last_success_at: Some(chrono::Utc::now()),
            records_exported: 128,
            ..Default::default()
        }),
        // Per-exporter cells (#8756) — the round trip must preserve each
        // sink's independent entry.
        observability_exports: [(
            "https".to_string(),
            crate::types::ObservabilityExportStatus {
                state: crate::types::ObservabilityExportState::Healthy,
                exporter: Some("https".to_string()),
                ..Default::default()
            },
        )]
        .into_iter()
        .collect(),
        forge_events: Some(crate::types::ForgeEventsStatus {
            state: crate::types::ForgeEventsState::Backoff,
            endpoint: Some("https://events.internal".to_string()),
            host_id: Some("robb-studio".to_string()),
            cursor: 4096,
            pages_observed: 12,
            events_observed: 340,
            consecutive_failures: 5,
            poll_interval_secs: 300,
            last_error: Some("auth_failed".to_string()),
            ..Default::default()
        }),
        peer_claims: None,
        deep_clean: Vec::new(),
        idle_exit: Some(crate::types::IdleExitStatus {
            enabled: true,
            eligible: false,
            trigger: None,
            idle_minutes: 60,
            in_flight_sweeps: 0,
            active_role_runs: 0,
            healthy_tokens: 3,
            total_tokens: 4,
            idle_elapsed_secs: 900,
            starved_elapsed_secs: 0,
            starvation_enabled: true,
            observed_at: Some(chrono::Utc::now()),
        }),
        stuck_worktree_reclaims: Vec::new(),
        pool_exhaustion_holds: Vec::new(),
    };
    let resp = Response::DaemonStatus(Box::new(report));
    let json = serde_json::to_string(&resp).expect("serialize response");
    let back: Response = serde_json::from_str(&json).expect("deserialize response");
    match back {
        Response::DaemonStatus(r) => {
            assert_eq!(r.token_pool_size, 4);
            assert_eq!(r.token_pool_dir, Some(std::path::PathBuf::from("/repo/a/.loom/tokens")));
            assert_eq!(r.disk_headroom, 10);
            assert_eq!(r.logical_cpus, 8);
            assert!(r.auto_update_enabled);
            assert_eq!(r.auto_update_consecutive_failures, 2);
            assert_eq!(r.auto_update_backoff_secs, Some(120));
            assert_eq!(r.auto_update_note.as_deref(), Some("within settle window"));
            assert_eq!(r.loadavg_1m, Some(1.25));
            assert_eq!(r.cpu_idle_fraction, Some(0.90));
            assert!(!r.capacity_bound);
            assert_eq!(r.configured_max, 5);
            assert_eq!(r.dynamic_cap, 3);
            assert!(r.main_health_gate_halted);
            assert!(!r.main_health_gate_not_evaluated);
            assert!(r.in_flight.is_empty());
            assert!(r.capacity.ranking_present);
            assert_eq!(r.capacity.healthy_accounts, 3);
            assert_eq!(r.capacity.exhausted_accounts, 1);
            assert_eq!(r.capacity.token_axis_limit, 3);
            assert!(r.capacity.token_bound);
            assert_eq!(r.per_repo.len(), 1);
            assert_eq!(r.per_repo[0].in_flight_count, 0);
            assert!(r.per_repo[0].health_gate_halted);
            assert!(!r.per_repo[0].health_gate_not_evaluated);
            assert_eq!(r.main_health_gate_enabled, Some(true));
            assert!(r.main_health_gate_verdict_at.is_some());
            // #4830: the host-identity mismatch survives the wire so a
            // `health` client in another process can render the note.
            let mismatch = r
                .observability_host_id_mismatch
                .as_ref()
                .expect("mismatch round-trips");
            assert_eq!(mismatch.daemon_host_id, "robb-studio");
            assert_eq!(mismatch.ingest_host_id, "robb-pro");
            // #5083: the positive export record survives the wire too —
            // this is what lets a `status`/`health` client in another
            // process state that telemetry IS (or is not) landing rather
            // than infer it from the absence of a warning.
            let export = r
                .observability_export
                .as_ref()
                .expect("export status round-trips");
            assert_eq!(export.state, crate::types::ObservabilityExportState::HostIdMismatch);
            assert_eq!(export.host_id.as_deref(), Some("robb-studio"));
            assert_eq!(export.ingest_host_id.as_deref(), Some("robb-pro"));
            assert_eq!(export.records_exported, 128);
            // ADR-0021 (#8765): the feed-consumer record survives the wire
            // too, including the error CLASS underneath a backoff promotion
            // — `backoff` answers "how often is it retrying", never "why".
            let feed = r
                .forge_events
                .as_ref()
                .expect("forge_events status round-trips");
            assert_eq!(feed.state, crate::types::ForgeEventsState::Backoff);
            assert_eq!(feed.host_id.as_deref(), Some("robb-studio"));
            assert_eq!(feed.cursor, 4096);
            assert_eq!(feed.events_observed, 340);
            assert_eq!(feed.last_error.as_deref(), Some("auth_failed"));
            assert_eq!(feed.poll_interval_secs, 300);
            assert_eq!(r.per_repo[0].health_gate_enabled, Some(true));
            assert!(r.per_repo[0].health_gate_verdict_at.is_some());
            assert_eq!(
                r.credential_preflight
                    .as_ref()
                    .map(|c| c.mechanism.as_str()),
                Some("test-fixture")
            );
            assert_eq!(r.work_finder_enabled, Some(true));
        }
        other => panic!("Expected DaemonStatus, got: {other:?}"),
    }
}
