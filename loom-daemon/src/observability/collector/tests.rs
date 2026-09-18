//! Unit tests for [`super`] — extracted from the parent module's inline
//! `#[cfg(test)] mod tests` (Issue #8056) so the parent stays inside the
//! file-size ratchet (`scripts/check-file-size-budget.sh`). Content is
//! unchanged apart from dedenting and the new-module additions.

use super::*;
use crate::health;
use serial_test::serial;

fn dispatch_event(issue: u32, sweep_id: &str) -> Event {
    Event::SweepGlobalDispatch {
        sweep_id: sweep_id.to_string(),
        kind: SweepKind::Issue(issue),
        runtime: None,
        runtime_source: None,
        repo: Some("/repos/loom".to_string()),
    }
}

fn phase_event(issue: u32, phase: &str) -> Event {
    Event::SweepPhase {
        issue,
        phase: phase.to_string(),
        pr_number: None,
        repo: Some("/repos/loom".to_string()),
    }
}

fn exited_event(issue: u32, exit_code: Option<i32>, duration_sec: i64) -> Event {
    Event::SweepExited {
        issue,
        exit_code,
        duration_sec,
        no_progress: false,
        death_class: None,
        repo: Some("/repos/loom".to_string()),
    }
}

fn crashed_event(issue: u32) -> Event {
    Event::SweepCrashed {
        issue,
        checkpoint_phase: Some("builder".to_string()),
        classification: None,
        death_class: None,
        repo: Some("/repos/loom".to_string()),
    }
}

#[test]
fn dispatch_emits_sweep_started_and_tracks_state() {
    let mut dispatches = HashMap::new();
    let records = map_event_to_records(
        &dispatch_event(42, "sweep-issue-42-0"),
        42,
        "rjwalters/loom",
        RepoVisibility::Public,
        &mut dispatches,
    );
    assert_eq!(records.len(), 1);
    match &records[0] {
        TelemetryRecord::SweepStarted(r) => {
            assert_eq!(r.issue, 42);
            assert_eq!(r.sweep_id, "sweep-issue-42-0");
            assert_eq!(r.repo, "rjwalters/loom");
            assert_eq!(r.visibility, RepoVisibility::Public);
        }
        other => panic!("expected SweepStarted, got {other:?}"),
    }
    assert!(dispatches.contains_key(&42));
}

#[test]
fn phase_after_dispatch_carries_the_tracked_sweep_id() {
    let mut dispatches = HashMap::new();
    map_event_to_records(
        &dispatch_event(42, "sweep-issue-42-0"),
        42,
        "rjwalters/loom",
        RepoVisibility::Private,
        &mut dispatches,
    );
    let records = map_event_to_records(
        &phase_event(42, "builder"),
        42,
        "rjwalters/loom",
        RepoVisibility::Private,
        &mut dispatches,
    );
    match &records[0] {
        TelemetryRecord::SweepPhase(r) => {
            assert_eq!(r.sweep_id, "sweep-issue-42-0");
            assert_eq!(r.phase, "builder");
        }
        other => panic!("expected SweepPhase, got {other:?}"),
    }
}

#[test]
fn phase_without_a_tracked_dispatch_uses_a_synthesized_sweep_id() {
    // Simulates a daemon restart mid-sweep: no SweepGlobalDispatch was
    // observed in this process's lifetime for issue 99.
    let mut dispatches = HashMap::new();
    let records = map_event_to_records(
        &phase_event(99, "judge"),
        99,
        "rjwalters/loom",
        RepoVisibility::Private,
        &mut dispatches,
    );
    match &records[0] {
        TelemetryRecord::SweepPhase(r) => assert_eq!(r.sweep_id, "unknown-issue-99"),
        other => panic!("expected SweepPhase, got {other:?}"),
    }
}

#[test]
fn clean_exit_zero_maps_to_success_and_clears_dispatch_state() {
    let mut dispatches = HashMap::new();
    map_event_to_records(
        &dispatch_event(7, "sweep-issue-7-0"),
        7,
        "rjwalters/loom",
        RepoVisibility::Public,
        &mut dispatches,
    );
    let records = map_event_to_records(
        &exited_event(7, Some(0), 120),
        7,
        "rjwalters/loom",
        RepoVisibility::Public,
        &mut dispatches,
    );
    assert_eq!(records.len(), 2, "a terminal event yields completed + outcome");
    match &records[0] {
        TelemetryRecord::SweepCompleted(r) => assert_eq!(r.result, SweepResult::Success),
        other => panic!("expected SweepCompleted, got {other:?}"),
    }
    match &records[1] {
        TelemetryRecord::SweepOutcome(r) => {
            assert_eq!(r.result, SweepResult::Success);
            assert_eq!(r.total_duration_sec, 120);
            assert_eq!(r.sweep_id, "sweep-issue-7-0");
        }
        other => panic!("expected SweepOutcome, got {other:?}"),
    }
    assert!(!dispatches.contains_key(&7), "terminal event must clear tracked state");
}

#[test]
fn nonzero_exit_maps_to_failure() {
    let mut dispatches = HashMap::new();
    let records = map_event_to_records(
        &exited_event(8, Some(1), 30),
        8,
        "rjwalters/loom",
        RepoVisibility::Private,
        &mut dispatches,
    );
    match &records[0] {
        TelemetryRecord::SweepCompleted(r) => assert_eq!(r.result, SweepResult::Failure),
        other => panic!("expected SweepCompleted, got {other:?}"),
    }
}

#[test]
fn crash_maps_to_failure_with_duration_from_tracked_dispatch() {
    let mut dispatches = HashMap::new();
    dispatches.insert(
        5,
        DispatchState {
            sweep_id: "sweep-issue-5-0".to_string(),
            started_at: Utc::now() - chrono::Duration::seconds(60),
        },
    );
    let records = map_event_to_records(
        &crashed_event(5),
        5,
        "rjwalters/loom",
        RepoVisibility::Private,
        &mut dispatches,
    );
    match &records[1] {
        TelemetryRecord::SweepOutcome(r) => {
            assert_eq!(r.result, SweepResult::Failure);
            assert!(r.total_duration_sec >= 59, "duration should reflect elapsed time");
        }
        other => panic!("expected SweepOutcome, got {other:?}"),
    }
    assert!(!dispatches.contains_key(&5));
}

#[test]
fn blocker_event_yields_no_records() {
    let mut dispatches = HashMap::new();
    let event = Event::SweepBlocker {
        issue: 1,
        reason: "human decision".to_string(),
        label_added: "loom:blocked".to_string(),
        repo: None,
    };
    let records =
        map_event_to_records(&event, 1, "rjwalters/loom", RepoVisibility::Private, &mut dispatches);
    assert!(records.is_empty());
}

#[test]
fn event_issue_ignores_pr_set_dispatch() {
    let event = Event::SweepGlobalDispatch {
        sweep_id: "sweep-prs-0".to_string(),
        kind: SweepKind::PrSet(vec![1, 2]),
        runtime: None,
        runtime_source: None,
        repo: None,
    };
    assert_eq!(event_issue(&event), None);
}

#[test]
fn event_repo_path_reads_the_stamped_workspace_root() {
    let event = phase_event(1, "curator");
    assert_eq!(event_repo_path(&event).as_deref(), Some("/repos/loom"));
}

// ------------------------------------------------------------------
// Host-level snapshot samplers — no `gh` dependency, safe in CI.
// ------------------------------------------------------------------

#[test]
fn token_snapshot_reads_a_ranking_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".loom/tokens")).unwrap();
    std::fs::write(
        dir.path().join(".loom/tokens/.ranking"),
        "agent-1|available|0.42\nagent-2|exhausted|0.99\n",
    )
    .unwrap();
    let record = sample_token_snapshot(dir.path());
    assert_eq!(record.accounts.len(), 2);
    assert_eq!(record.accounts[0].account, "agent-1");
    assert_eq!(record.accounts[0].rank, Some(0));
    assert!(!record.accounts[0].exhausted);
    assert_eq!(record.accounts[1].account, "agent-2");
    assert!(record.accounts[1].exhausted);
}

#[test]
fn token_snapshot_missing_ranking_is_empty_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let record = sample_token_snapshot(dir.path());
    assert!(record.accounts.is_empty());
}

#[test]
fn token_snapshot_populates_limit_window_reset_at_from_the_ranking() {
    // Issue #4874: this field was hardcoded `None`, so every exhausted
    // account fleet-wide pushed `limit_window_reset_at: null` and the
    // dashboard's countdown column was permanently `—`. An exhausted
    // account with a reset in the ranking must now report it.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".loom/tokens")).unwrap();
    std::fs::write(
        dir.path().join(".loom/tokens/.ranking"),
        "agent-1|available|0.42\n\
         agent-2|exhausted|0.00|2026-08-02T03:00:00Z\n\
         agent-3|exhausted||2026-08-04T11:00:00Z\n",
    )
    .unwrap();
    let record = sample_token_snapshot(dir.path());
    assert_eq!(record.accounts.len(), 3);

    // No reset field -> unknown, not a fabricated date.
    assert_eq!(record.accounts[0].limit_window_reset_at, None);

    let expected = DateTime::parse_from_rfc3339("2026-08-02T03:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    assert_eq!(record.accounts[1].limit_window_reset_at, Some(expected));
    assert!(record.accounts[1].exhausted);
    assert_eq!(record.accounts[1].usage_fraction, Some(0.00));

    // Reset known, utilization unknown: the reset still lands, and the
    // absent utilization is not coerced to 0.0.
    assert_eq!(record.accounts[2].usage_fraction, None);
    assert_eq!(
        record.accounts[2].limit_window_reset_at,
        Some(
            DateTime::parse_from_rfc3339("2026-08-04T11:00:00Z")
                .unwrap()
                .with_timezone(&Utc)
        )
    );
}

#[test]
fn token_snapshot_carries_the_writers_binding_reset_end_to_end() {
    // The whole #4874 chain in one test: the *real* ranking writer renders
    // the file, the collector reads it back, and the instant that lands in
    // `tokens.snapshot` is the one the account is actually waiting on.
    //
    // The rate_limited row is the case that makes this more than plumbing.
    // It is 7d-healthy and 5h-spent, so it returns at the 5h boundary
    // (07:00Z) — writing its 7d reset (a week out) would tell the dashboard
    // the fleet was stalled for six days when it recovers within the hour.
    use crate::tokens_pool::check::{format_ranking_lines, AccountResult, ProbeReport};

    let mut spent = AccountResult::new("agent-1", "exhausted");
    spent.s5h_utilization = Some(0.0);
    spent.s5h_reset = Some("2026-08-01T05:20:00Z".into());
    spent.s7d_reset = Some("2026-08-02T03:00:00Z".into());
    let mut limited = AccountResult::new("agent-2", "rate_limited");
    limited.s5h_utilization = Some(1.0);
    limited.s5h_reset = Some("2026-08-01T07:00:00Z".into());
    limited.s7d_reset = Some("2026-08-07T01:00:00Z".into());

    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".loom/tokens")).unwrap();
    std::fs::write(
        dir.path().join(".loom/tokens/.ranking"),
        format_ranking_lines(&ProbeReport {
            ranked_at: "2026-08-01T05:22:00Z".into(),
            accounts: vec![spent, limited],
        }),
    )
    .unwrap();

    let record = sample_token_snapshot(dir.path());
    let at = |iso: &str| {
        Some(
            DateTime::parse_from_rfc3339(iso)
                .unwrap()
                .with_timezone(&Utc),
        )
    };
    assert_eq!(record.accounts[0].limit_window_reset_at, at("2026-08-02T03:00:00Z"));
    assert!(record.accounts[0].exhausted);
    assert_eq!(
        record.accounts[1].limit_window_reset_at,
        at("2026-08-01T07:00:00Z"),
        "a 5h-limited account must report the 5h boundary it actually returns at"
    );
}

#[test]
fn token_snapshot_unparseable_reset_degrades_to_unknown() {
    // The on-disk file is a trust boundary: junk in the reset field must
    // not propagate to the pushed telemetry as a bogus instant.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".loom/tokens")).unwrap();
    std::fs::write(
        dir.path().join(".loom/tokens/.ranking"),
        "agent-1|exhausted|0.00|not-a-timestamp\n",
    )
    .unwrap();
    let record = sample_token_snapshot(dir.path());
    assert_eq!(record.accounts.len(), 1, "a bad reset must not drop the whole row");
    assert_eq!(record.accounts[0].limit_window_reset_at, None);
    assert!(record.accounts[0].exhausted);
}

fn empty_pool() -> WorkspacePool {
    WorkspacePool::new(Arc::new(EventBus::new()), tokio::runtime::Handle::current())
}

fn empty_slug_cache() -> HashMap<String, String> {
    HashMap::new()
}

#[tokio::test]
async fn host_health_sample_populates_daemon_version_and_uptime() {
    let dir = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let pool = empty_pool();
    let record = sample_host_health(dir.path(), started, &pool, &mut empty_slug_cache()).await;
    assert_eq!(record.daemon_version, env!("CARGO_PKG_VERSION"));
    assert!(record.logical_cpus >= 1);
}

#[tokio::test]
async fn host_health_sample_populates_worktree_root_free_and_total_together() {
    // #5356: both readings come from the SAME df probe
    // (`disk_headroom::worktree_root_disk_gb`), so a real sample against a
    // real tempdir should report both, with total >= free.
    let dir = tempfile::tempdir().unwrap();
    let pool = empty_pool();
    let record =
        sample_host_health(dir.path(), Instant::now(), &pool, &mut empty_slug_cache()).await;
    let free = record
        .worktree_root_free_gb
        .expect("a real df probe against a real tempdir should measure free space");
    let total = record
        .worktree_root_total_gb
        .expect("a real df probe against a real tempdir should measure total capacity");
    assert!(total >= free, "total {total} GB should be >= free {free} GB");
}

#[tokio::test]
async fn host_health_sample_stamps_the_running_binarys_build_identity() {
    let dir = tempfile::tempdir().unwrap();
    let pool = empty_pool();
    let record =
        sample_host_health(dir.path(), Instant::now(), &pool, &mut empty_slug_cache()).await;

    // The sample must carry the SAME commit `loom-daemon --version`
    // prints — that identity is what lets the dashboard tell two
    // same-`daemon_version` builds apart (#4956).
    assert_eq!(record.build_commit, crate::self_update::BUILT_COMMIT);
    assert!(
        !record.build_commit.is_empty(),
        "build_commit must always be populated (\"unknown\" is the no-git fallback)"
    );
    assert_eq!(record.built_at, crate::self_update::built_at());

    // `built_at` is absent only when the compile-time stamp itself is the
    // "unknown" fallback; whenever it IS present it must be a real instant
    // no later than the sample, never a fabricated epoch.
    if let Some(built_at) = record.built_at {
        assert!(
            built_at <= record.captured_at,
            "built_at ({built_at}) must not be after captured_at ({})",
            record.captured_at
        );
    }
}

#[test]
fn built_at_parses_the_compile_time_stamp_or_reports_unknown() {
    // Pins the "unknown != fabricated instant" half of the contract: the
    // helper resolves to `Some` exactly when the raw stamp is parseable.
    let raw = crate::self_update::BUILT_AT_RAW;
    assert_eq!(
        crate::self_update::built_at().is_some(),
        chrono::DateTime::parse_from_rfc3339(raw).is_ok(),
        "built_at() must be Some iff the raw stamp {raw:?} parses"
    );
}

#[tokio::test]
async fn host_health_sample_reports_no_active_sweeps_when_pool_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let pool = empty_pool();
    let record = sample_host_health(dir.path(), started, &pool, &mut empty_slug_cache()).await;
    assert!(
        record.active_sweep_ids.is_empty(),
        "an empty pool has no in-flight sweeps to report"
    );
}

#[tokio::test]
async fn host_health_sample_reports_no_managed_repos_when_pool_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let pool = empty_pool();
    let record = sample_host_health(dir.path(), started, &pool, &mut empty_slug_cache()).await;
    assert!(
        record.managed_repos.is_empty(),
        "an empty pool has no registered repos to report"
    );
}

// ------------------------------------------------------------------
// sample_role_tick_health (#5022) — the pure classifier, tested directly
// against a hand-built fixture so it needs neither the process-global
// ring `crate::role_runner::role_tick_records()` reads (shared across
// every test in this binary, with no `pub(crate)` reset hook reachable
// from this module) nor a daemon.
// ------------------------------------------------------------------

fn tick(
    root: &str,
    role: &str,
    ok: bool,
    detail: Option<&str>,
    at: DateTime<Utc>,
) -> RoleTickRecord {
    RoleTickRecord {
        root: PathBuf::from(root),
        role: role.to_string(),
        at,
        ok,
        detail: detail.map(str::to_string),
        pool_exhausted: false,
    }
}

#[test]
fn sample_role_tick_health_reports_totals_and_a_persistent_failure() {
    let now = Utc::now();
    let records = vec![
        tick(
            "/repos/loom",
            "judge",
            false,
            Some("no-token-pool"),
            now - chrono::Duration::minutes(5),
        ),
        tick("/repos/loom", "judge", false, Some("no-token-pool"), now),
        tick("/repos/loom", "curator", true, None, now),
    ];
    let health = sample_role_tick_health(&records);
    assert_eq!(health.total, 3);
    assert_eq!(health.ok, 1);
    assert_eq!(health.persistent.len(), 1);
    assert_eq!(health.persistent[0].role, "judge");
    assert_eq!(health.persistent[0].root, PathBuf::from("/repos/loom"));
    assert_eq!(health.persistent[0].failures, 2);
    assert_eq!(health.persistent[0].detail.as_deref(), Some("no-token-pool"));
}

#[test]
fn sample_role_tick_health_does_not_surface_a_self_recovered_transient_failure() {
    let now = Utc::now();
    let records = vec![
        tick(
            "/repos/loom",
            "guide",
            false,
            Some("timeout"),
            now - chrono::Duration::minutes(1),
        ),
        // Same pair's latest record is a success — self-recovered, so it
        // must not appear in `persistent` (mirrors `assess_roles`'s own
        // transient-vs-persistent rule).
        tick("/repos/loom", "guide", true, None, now),
    ];
    let health = sample_role_tick_health(&records);
    assert_eq!(health.total, 2);
    assert_eq!(health.ok, 1);
    assert!(health.persistent.is_empty());
}

#[test]
fn sample_role_tick_health_of_an_empty_ring_reports_zero_ticks_not_an_error() {
    // The role runner idle or disabled entirely — "no role ticks", not a
    // degraded state (the Test Plan's edge case).
    let health = sample_role_tick_health(&[]);
    assert_eq!(health.total, 0);
    assert_eq!(health.ok, 0);
    assert!(health.persistent.is_empty());
}

#[tokio::test]
#[serial(role_tick_ring)]
async fn host_health_sample_surfaces_a_persistent_role_tick_failure() {
    // Integration-level: goes through the real process-global ring
    // `crate::role_runner::record_role_tick_at` writes to and
    // `sample_host_health` reads from. A uniquely-named synthetic root
    // keeps this deterministic despite the ring being shared with every
    // other test in this binary. `#[serial(role_tick_ring)]` (#6239)
    // joins the same serial key `role_runner`'s own ring tests use — a
    // large-enough concurrent write burst elsewhere (e.g. a ring-
    // saturation regression fixture) could otherwise evict this test's
    // own just-recorded entry between the `record_role_tick_at` call and
    // the `sample_host_health` read below, purely from cross-test
    // interference on the shared global ring — this test asserts the
    // fixture's own pair is present, not the ring's total contents.
    let root = Path::new("/repos/collector-test-5022-fixture");
    let outcome = crate::role_runner::RoleTickOutcome::Failure("synthetic failure".to_string());
    crate::role_runner::record_role_tick_at("judge", root, &outcome, Utc::now());

    let dir = tempfile::tempdir().unwrap();
    let pool = empty_pool();
    let record =
        sample_host_health(dir.path(), Instant::now(), &pool, &mut empty_slug_cache()).await;
    assert!(
        record.roles.persistent.iter().any(|f| f.root == root
            && f.role == "judge"
            && f.detail.as_deref() == Some("synthetic failure")),
        "expected the just-recorded persistent failure in {:?}",
        record.roles.persistent
    );
    assert!(record.roles.total > 0);
}

/// A hermetic, `dispatch()`-able registry: `skip_label_flip = true` skips
/// runtime admission / the workspace-commands guard / every `gh` call
/// (mirrors `sweep_registry::test_support::fixture_registry`, which is
/// not reachable from here — `sweep_registry::test_support` is a
/// private, `#[cfg(test)]`-only module of a sibling module tree). The
/// fake spawn binary sleeps briefly so the dispatched entry stays
/// `Running` for the synchronous, single-threaded-until-`.await` body of
/// the test below.
fn dispatchable_registry(workspace: &Path) -> crate::sweep_registry::SweepRegistry {
    use crate::sweep_registry::{SweepRegistry, SweepRegistryConfig};
    use std::os::unix::fs::PermissionsExt;

    let scripts_dir = workspace.join(".loom").join("scripts");
    std::fs::create_dir_all(&scripts_dir).unwrap();
    let bin = scripts_dir.join("spawn-worker.sh");
    std::fs::write(&bin, "#!/bin/sh\nsleep 5\n").unwrap();
    let mut perms = std::fs::metadata(&bin).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&bin, perms).unwrap();

    let mut config = SweepRegistryConfig::new(workspace.to_path_buf());
    config.spawn_bin = Some(bin);
    config.skip_label_flip = true;
    config.journal_path = Some(workspace.join("sweeps-journal.json"));
    config.outcomes_journal_path = Some(workspace.join("sweep-outcomes.jsonl"));
    config.outcome_telemetry_path = Some(workspace.join("sweep-outcome-telemetry.jsonl"));
    SweepRegistry::new(config)
}

#[tokio::test]
async fn collect_active_sweep_ids_reports_running_sweeps_across_every_provisioned_registry() {
    let dir = tempfile::tempdir().unwrap();
    let a_root = dir.path().join("a");
    let b_root = dir.path().join("b");
    std::fs::create_dir_all(&a_root).unwrap();
    std::fs::create_dir_all(&b_root).unwrap();
    let pool = empty_pool();

    // `seed` (not `get_or_provision`) so no reaper/watchdog is spawned
    // for either fixture — nothing races the assertions below.
    pool.seed(a_root.clone(), Arc::new(std::sync::Mutex::new(dispatchable_registry(&a_root))));
    pool.seed(b_root.clone(), Arc::new(std::sync::Mutex::new(dispatchable_registry(&b_root))));

    let a_sweep_id = {
        let registry = pool.get_or_provision(&a_root);
        let mut registry = registry.lock().unwrap();
        registry
            .dispatch(&SweepKind::Issue(1), None, None, None, None)
            .expect("dispatch into workspace a")
            .sweep_id
    };
    let b_sweep_id = {
        let registry = pool.get_or_provision(&b_root);
        let mut registry = registry.lock().unwrap();
        registry
            .dispatch(&SweepKind::Issue(2), None, None, None, None)
            .expect("dispatch into workspace b")
            .sweep_id
    };

    let mut ids = collect_active_sweep_ids(&pool);
    ids.sort();
    let mut expected = vec![a_sweep_id, b_sweep_id];
    expected.sort();
    assert_eq!(ids, expected, "in-flight sweeps from BOTH provisioned registries are reported");
}

// ------------------------------------------------------------------
// dispatch_halt_from_breaker (Issue #4975)
// ------------------------------------------------------------------
//
// Exercised as a pure function over a hand-built `BreakerSnapshot` rather
// than through `host_breaker::register_global` — the breaker's `GLOBAL`
// handle is a process-wide `OnceLock` shared by every test in this
// binary, so mutating it here would leak into unrelated tests.

fn breaker_snapshot(
    phase: crate::host_breaker::BreakerPhase,
    reason: Option<&str>,
) -> crate::host_breaker::BreakerSnapshot {
    crate::host_breaker::BreakerSnapshot {
        enabled: true,
        phase,
        suppressed: phase.suppresses_dispatch(),
        reason: reason.map(str::to_string),
        tripped_at: None,
        releases_at: None,
        last_load_per_core: Some(4.24),
        load_per_core_threshold: 2.5,
        sustain_ticks: 3,
        cooldown_secs: 300,
        consecutive_over: 3,
    }
}

#[test]
fn dispatch_halt_from_breaker_reports_not_halted_when_no_breaker_registered() {
    assert_eq!(dispatch_halt_from_breaker(None), (false, None));
}

#[test]
fn dispatch_halt_from_breaker_reports_not_halted_when_closed() {
    let snapshot = breaker_snapshot(crate::host_breaker::BreakerPhase::Closed, None);
    assert_eq!(dispatch_halt_from_breaker(Some(snapshot)), (false, None));
}

#[test]
fn dispatch_halt_from_breaker_reports_halted_with_reason_when_open() {
    let snapshot = breaker_snapshot(
        crate::host_breaker::BreakerPhase::Open,
        Some("load-per-core 4.24 >= 2.50 sustained for 3 consecutive tick(s)"),
    );
    assert_eq!(
        dispatch_halt_from_breaker(Some(snapshot)),
        (
            true,
            Some("load-per-core 4.24 >= 2.50 sustained for 3 consecutive tick(s)".to_string())
        )
    );
}

#[test]
fn dispatch_halt_from_breaker_reports_halted_during_cooldown() {
    let snapshot = breaker_snapshot(
        crate::host_breaker::BreakerPhase::CoolDown,
        Some("load-per-core 1.10 < 2.50; cooling down for 300s"),
    );
    let (halted, reason) = dispatch_halt_from_breaker(Some(snapshot));
    assert!(halted, "CoolDown still suppresses dispatch, so it must count as halted");
    assert!(reason.is_some());
}

#[tokio::test]
async fn collect_managed_repos_reports_every_provisioned_registrys_repo_even_when_idle() {
    // Registered but never dispatched into: this is the exact "idle
    // roster" case #4976 exists for — `collect_active_sweep_ids` would
    // report nothing for either root, but the roster must still list
    // both.
    let dir = tempfile::tempdir().unwrap();
    let a_root = dir.path().join("a");
    let b_root = dir.path().join("b");
    std::fs::create_dir_all(&a_root).unwrap();
    std::fs::create_dir_all(&b_root).unwrap();
    let pool = empty_pool();
    pool.seed(a_root.clone(), Arc::new(std::sync::Mutex::new(dispatchable_registry(&a_root))));
    pool.seed(b_root.clone(), Arc::new(std::sync::Mutex::new(dispatchable_registry(&b_root))));

    // Pre-populate the slug cache so slug resolution never shells out to
    // `gh repo view` against these bare tempdirs (which have no git
    // remote at all) — the exact seam `resolve_repo_slug_cached` exists
    // for.
    let mut slug_cache = HashMap::new();
    slug_cache.insert(a_root.to_string_lossy().to_string(), "loom-test-fixture/repo-a".to_string());
    slug_cache.insert(b_root.to_string_lossy().to_string(), "loom-test-fixture/repo-b".to_string());

    // A deterministic fake visibility resolver — never a real `gh api`
    // call — that reports repo-a public and repo-b private, so the
    // per-entry visibility assertion below is not a coin flip on live
    // forge state.
    fn fake_visibility(slug: &str) -> RepoVisibility {
        if slug == "loom-test-fixture/repo-a" {
            RepoVisibility::Public
        } else {
            RepoVisibility::Private
        }
    }

    let mut repos = collect_managed_repos_with(&pool, &mut slug_cache, fake_visibility).await;
    repos.sort_by(|a, b| a.slug.cmp(&b.slug));
    assert_eq!(
        repos,
        vec![
            ManagedRepoEntry {
                slug: "loom-test-fixture/repo-a".to_string(),
                visibility: RepoVisibility::Public,
            },
            ManagedRepoEntry {
                slug: "loom-test-fixture/repo-b".to_string(),
                visibility: RepoVisibility::Private,
            },
        ],
        "both registered repos are in the roster, each with its derived visibility, \
         despite neither having a sweep in flight"
    );
}

// ------------------------------------------------------------------
// #5076 — final AC of epic #5004: a fixture with all managed repos'
// role-runner ticks persistently failing, and every other axis
// (dispatch/tokens/queues/throughput) healthy, must not present as
// green anywhere an operator looks by default. Exercises Gap 2 (role-
// tick health reaching `HostHealthRecord`, #5022/PR #5042), Gap 3
// (escalation of N consecutive identical failures, #5023/PR #5039) and
// Gap 4 (bounded, ANSI-stripped `roles.summary`, #5024/PR #5043)
// TOGETHER against `crate::health::assess`'s combined verdict (Gap 1's
// review-side queue axes, #5021/PR #5050, stay healthy here on
// purpose — this fixture is the cross-cutting scenario none of the
// four per-gap test suites individually exercises).
// ------------------------------------------------------------------

/// `count` consecutive identical-failure [`RoleTickRecord`]s for one
/// `(root, role)` pair, timestamped a minute apart and ending at `now`.
/// The detail deliberately carries a raw ANSI color escape and is far
/// longer than either of `health.rs`'s summary caps (60 chars per
/// failure, 2000 for the assembled line) — an uncleaned claude-wrapper
/// tail, exactly the shape Gap 4 exists to defend against, injected
/// straight into the ring rather than pre-cleaned by the caller so the
/// assertions below can only pass if the *defensive* re-clean/cap in
/// `health.rs` (independent of `role_runner`'s own source-side clean)
/// actually runs.
fn escalating_failure_records(
    root: &str,
    role: &str,
    now: DateTime<Utc>,
    count: usize,
) -> Vec<RoleTickRecord> {
    let raw_detail =
        format!("\u{1b}[31mERROR\u{1b}[0m: {role} invocation exited 1 — {}", "x".repeat(4000));
    (0..count)
        .map(|i| RoleTickRecord {
            root: PathBuf::from(root),
            role: role.to_string(),
            at: now - chrono::Duration::minutes(i64::try_from(count - i).unwrap_or(0)),
            ok: false,
            detail: Some(raw_detail.clone()),
            pool_exhausted: false,
        })
        .collect()
}

/// A `HealthInputs` baseline with every non-roles section green: healthy
/// dispatch (a recent, error-free work-finder tick), a healthy token
/// pool, and a per-repo pipeline snapshot for each of `roots` that is
/// both fully queried (no `?`/`Unknown`) and merging PRs (never a review
/// stall). Only `status.role_tick_records` is left for the caller to
/// populate — the one axis this fixture is about.
fn all_other_axes_healthy_inputs(now: DateTime<Utc>, roots: &[&str]) -> health::HealthInputs {
    let mut status = crate::types::DaemonStatusReport {
        capacity: crate::types::CapacityReport {
            ranking_present: true,
            total_accounts: 4,
            healthy_accounts: 4,
            exhausted_accounts: 0,
            token_axis_limit: 4,
            token_bound: false,
        },
        dynamic_cap: 4,
        work_finder_enabled: Some(true),
        ..Default::default()
    };
    status.last_work_finder_tick = Some(crate::types::WorkFinderTickSummary {
        at: now - chrono::Duration::seconds(30),
        max_concurrent: 4,
        seen: 3,
        dispatched: 1,
        ..Default::default()
    });
    health::HealthInputs {
        at: now,
        window: Duration::from_secs(health::DEFAULT_WINDOW_SECS),
        status: Some(status),
        ipc_error: None,
        install_state: None,
        pgrep_pids: vec![],
        pid_file: None,
        ranking_present: true,
        ranking_age_secs: Some(120),
        pipeline: Some(
            roots
                .iter()
                .map(|root| crate::pipeline_snapshot::RepoPipelineSnapshot {
                    root: PathBuf::from(root),
                    queued: Some(2),
                    building: Some(1),
                    review_requested: Some(1),
                    changes_requested: Some(0),
                    changes_requested_unclaimed: Some(0),
                    approved: Some(0),
                    merged_24h: Some(3),
                    ..Default::default()
                })
                .collect(),
        ),
        cli_build_commit: "unknown".to_string(),
        work_finder_log_tick_age_secs: None,
        // Pre-existing pipeline check: `HealthInputs` gained this field
        // in #5097 after this fixture was added in #5098 — `None` (gh
        // available) matches every other axis this fixture already
        // documents as healthy.
        gh_unavailable: None,
        // `HealthInputs` gained this field in #7584 after this fixture
        // was added — `None` is fine here since `status.auto_update_enabled`
        // defaults to `false` (disabled ⇒ Green) and this fixture is not
        // about the `auto_update` axis.
        self_update: None,
        // `HealthInputs` gained this field in #7605 after this fixture
        // was added — `None` (no codesign identity configured) is fine
        // here since this fixture is not about the `codesign_identity`
        // axis.
        codesign_preflight: None,
        // `HealthInputs` gained this field in #8163 after this fixture was
        // added — `None` (no host-load reading) is fine here since this
        // fixture's IPC round-trip succeeds, so the `indeterminate-busy`
        // corroboration this field feeds never comes into play.
        load_per_core: None,
    }
}

#[test]
fn all_repos_failing_roles_is_not_green_anywhere_while_every_other_axis_is_healthy() {
    let now = Utc::now();
    // Three "managed repos", each with its role runner persistently and
    // identically failing — well past `ROLE_TICK_ESCALATION_THRESHOLD` —
    // while nothing else about the fleet is unhealthy.
    let roots = [
        "/repos/collector-test-5076-loom",
        "/repos/collector-test-5076-anvil",
        "/repos/collector-test-5076-kicad-tools",
    ];
    let mut records = Vec::new();
    for root in &roots {
        records.extend(escalating_failure_records(
            root,
            "judge",
            now,
            health::ROLE_TICK_ESCALATION_THRESHOLD + 2,
        ));
    }

    let mut inputs = all_other_axes_healthy_inputs(now, &roots);
    inputs.status.as_mut().unwrap().role_tick_records = records.clone();

    let report = health::assess(&inputs);

    // -- the headline AC: the fleet-level verdict is not green -------
    assert_ne!(
        report.overall,
        health::Verdict::Green,
        "all roles failing must not read green anywhere an operator looks by default: {}",
        report.render_human()
    );
    assert_eq!(report.exit_code(), health::EXIT_DEGRADED);

    // -- roles is the section that is actually down -------------------
    let roles = report.section("roles").expect("roles section present");
    assert_eq!(roles.verdict, health::Verdict::Degraded, "{}", roles.summary);
    assert!(
        roles.summary.contains("ESCALATED"),
        "3 repos past the escalation threshold must be called out distinctly (Gap 3): {}",
        roles.summary
    );
    for root in &roots {
        let name = Path::new(root)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            roles.summary.contains(&name),
            "every failing repo must be named, not just the first: {}",
            roles.summary
        );
    }

    // -- every OTHER section stays green — this is a roles-only outage,
    //    not a masked fleet-wide failure (Gap 1's review axes included,
    //    even though this fixture does not stall them) --------------
    for key in ["dispatch", "tokens", "queues", "throughput"] {
        let section = report
            .section(key)
            .unwrap_or_else(|| panic!("{key} section present"));
        assert_eq!(
            section.verdict,
            health::Verdict::Green,
            "{key} must stay green — this is a roles-only outage: {}",
            section.summary
        );
    }

    // -- Gap 4: the rendered summary is bounded and ANSI-clean despite
    //    3 repos each contributing a multi-KB raw ANSI-laden detail ---
    assert!(
        !roles.summary.contains('\u{1b}'),
        "an ANSI escape leaked into the operator-facing summary: {:?}",
        roles.summary
    );
    assert!(
        roles.summary.chars().count() < 2500,
        "3 failing repos' raw multi-KB details must not multiply the summary's length \
         (got {} chars)",
        roles.summary.chars().count()
    );
    let human = report.render_human();
    assert!(!human.contains('\u{1b}'), "ANSI leaked into the full human report");

    // -- Gap 2: the same fixture's role-tick health reaches
    //    `HostHealthRecord.roles` — the field #5022/PR #5042 added so a
    //    role dying on one host is observable fleet-wide, not only to
    //    an operator running `loom-daemon health` locally ------------
    let roles_health = sample_role_tick_health(&records);
    let host_health = crate::telemetry::HostHealthRecord {
        captured_at: now,
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        build_commit: String::new(),
        built_at: None,
        uptime_sec: 3600,
        logical_cpus: 4,
        cpu_idle_fraction: None,
        load_per_core: None,
        worktree_root_free_gb: None,
        worktree_root_total_gb: None,
        active_sweep_ids: vec![],
        dispatch_halted: false,
        halt_reason: None,
        managed_repos: vec![],
        roles: roles_health,
        protection: None,
    };
    assert_eq!(
        host_health.roles.persistent.len(),
        roots.len(),
        "every failing repo's role-tick health must reach HostHealthRecord: {:?}",
        host_health.roles.persistent
    );
    for root in &roots {
        let root_path = PathBuf::from(root);
        let entry = host_health
            .roles
            .persistent
            .iter()
            .find(|f| f.root == root_path && f.role == "judge")
            .unwrap_or_else(|| {
                panic!("expected a persistent judge failure for {root} in {host_health:?}")
            });
        // Gap 2's structured detail also stays ANSI-clean, independent
        // of the source (`clean_structured_detail`'s own defensive
        // re-clean, mirroring the summary-line cap above).
        assert!(
            entry
                .detail
                .as_deref()
                .is_none_or(|d| !d.contains('\u{1b}')),
            "HostHealthRecord's structured detail must be ANSI-clean too: {:?}",
            entry.detail
        );
    }
}

// ------------------------------------------------------------------
// host.health watchdog/crash-protection state (#5352).
// ------------------------------------------------------------------

fn protection_report(
    state: crate::daemon_install_state::ProtectionState,
    watchdog_provisioned: Option<bool>,
) -> crate::daemon_install_state::ProtectionReport {
    crate::daemon_install_state::ProtectionReport {
        state,
        marker_present: matches!(
            state,
            crate::daemon_install_state::ProtectionState::Protected
                | crate::daemon_install_state::ProtectionState::WatchdogNotProvisioned
                | crate::daemon_install_state::ProtectionState::Unknown
        ),
        marker_path: PathBuf::from("/home/ubuntu/.loom/autonomy-desired"),
        job: crate::daemon_install_state::WatchdogJob::SystemdTimer {
            timer_unit: "loom-daemon-watchdog.timer".to_string(),
        },
        watchdog_provisioned,
        detail: "test fixture".to_string(),
    }
}

#[test]
fn protection_summary_carries_the_daemon_installs_own_classification_verbatim() {
    let summary = protection_summary_from_report(protection_report(
        crate::daemon_install_state::ProtectionState::Protected,
        Some(true),
    ));
    assert_eq!(summary.state, "protected");
    assert_eq!(summary.watchdog_provisioned, Some(true));
}

#[test]
fn protection_summary_reports_watchdog_not_provisioned() {
    let summary = protection_summary_from_report(protection_report(
        crate::daemon_install_state::ProtectionState::WatchdogNotProvisioned,
        Some(false),
    ));
    assert_eq!(summary.state, "watchdog-not-provisioned");
    assert_eq!(summary.watchdog_provisioned, Some(false));
}

#[test]
fn protection_summary_carries_none_when_the_provisioning_probe_could_not_answer() {
    // `ProtectionState::Unknown`: the probe ran but the watchdog-
    // provisioning check itself could not answer (no launchctl/systemctl).
    // `watchdog_provisioned` must stay `None`, never a fabricated `false`.
    let summary = protection_summary_from_report(protection_report(
        crate::daemon_install_state::ProtectionState::Unknown,
        None,
    ));
    assert_eq!(summary.state, "unknown");
    assert_eq!(summary.watchdog_provisioned, None);
}
