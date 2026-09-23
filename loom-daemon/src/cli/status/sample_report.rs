//! The fully-populated [`DaemonStatusReport`] fixture shared by the status
//! client tests in `status.rs` and the reachable-path status-rendering tests
//! (#4354), split out of `status.rs` (#7990) so the fixture can keep listing
//! every field explicitly without pushing its parent past the
//! `.loom/docs/file-size-policy.md` ratchet.

use chrono::Utc;
use loom_daemon::types::{CapacityReport, CredentialPreflightReport, DaemonStatusReport};

/// A fully-populated report the fake daemon can serialize back to the client
/// on a successful round-trip. Every field is compiler-checked, so a schema
/// change surfaces here rather than as a silently-skewed wire payload.
///
/// `pub(super)` so the reachable-path status-rendering tests (#4354) build
/// their payload from this same fixture instead of duplicating the whole
/// struct.
pub(crate) fn sample_report() -> DaemonStatusReport {
    DaemonStatusReport {
        forge_events: None,
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
        main_health_gate_halted: false,
        main_health_gate_not_evaluated: false,
        main_health_gate_not_evaluated_reason: None,
        main_health_gate_enabled: Some(true),
        main_health_gate_verdict_at: Some(Utc::now()),
        main_health_gate_deferred: false,
        main_health_gate_deferred_reason: None,
        main_health_gate_verdict_tier: None,
        capacity: CapacityReport {
            ranking_present: true,
            total_accounts: 4,
            healthy_accounts: 3,
            exhausted_accounts: 1,
            token_axis_limit: 3,
            token_bound: true,
        },
        per_repo: vec![],
        role_runner_host_env_override: None,
        role_runner_shard: None,
        credential_preflight: Some(CredentialPreflightReport {
            ok: true,
            mechanism: "test-fixture".to_string(),
            fingerprint: None,
            message: "test fixture — not a real preflight".to_string(),
            checked_at: Utc::now(),
        }),
        draining: false,
        drain_deadline: None,
        drain_note: None,
        auto_update_enabled: false,
        auto_update_last_check: None,
        auto_update_last_roll: None,
        auto_update_consecutive_failures: 0,
        auto_update_backoff_secs: None,
        auto_update_terminal_reason: None,
        auto_update_note: None,
        auto_update_artifact_version: None,
        auto_update_artifact_published_at: None,
        auto_update_stale_repo_ticks: 0,
        auto_update_stale_repo: None,
        host_breaker: None,
        admission_brake: None,
        rate_limit_breaker: None,
        safehouse: None,
        work_finder_enabled: Some(true),
        last_work_finder_tick: None,
        role_tick_records: vec![],
        role_last_tick: vec![],
        // #6102: role-agent load alongside the sweep in-flight list.
        active_role_agents: 2,
        role_agent_max_concurrent: Some(7),
        daemon_pid: Some(99917),
        pid_file: Some(std::path::PathBuf::from("/repo/a/.loom/.daemon.pid")),
        daemon_build_commit: Some("18887b5c".to_string()),
        daemon_built_at_raw: Some("2026-08-02T03:09:51Z".to_string()),
        work_finder_interval_secs: Some(60),
        observability_host_id_mismatch: None,
        observability_export: None,
        peer_claims: None,
        deep_clean: Vec::new(),
        idle_exit: None,
        stuck_worktree_reclaims: Vec::new(),
        pool_exhaustion_holds: Vec::new(),
    }
}
