use super::*;
use crate::activity::ActivityDb;
use crate::sweep_registry::{SweepRegistry, SweepRegistryConfig};
use crate::types::SweepKind;
use tempfile::tempdir;

type TestContext = (
    Arc<Mutex<TerminalManager>>,
    Arc<Mutex<ActivityDb>>,
    Arc<Mutex<SweepRegistry>>,
    Arc<EventBus>,
);

/// A process-wide leaked runtime handle so [`WorkspacePool`]s can be built in
/// synchronous `#[test]` cases (Issue #3929). Reapers spawned onto it during
/// provisioning are harmless in tests.
fn test_runtime_handle() -> tokio::runtime::Handle {
    use std::sync::OnceLock;
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| tokio::runtime::Runtime::new().unwrap())
        .handle()
        .clone()
}

/// A fixture credential-preflight snapshot for `build_daemon_status` tests
/// (#4005) — these tests exercise the dynamic-cap/health-gate machinery,
/// not credential resolution, so a fixed `Ok` snapshot keeps them focused.
fn test_credential_preflight() -> CredentialPreflightReport {
    CredentialPreflightReport {
        ok: true,
        mechanism: "test-fixture".to_string(),
        fingerprint: None,
        message: "test fixture — not a real preflight".to_string(),
        checked_at: Utc::now(),
    }
}

/// A [`WorkspacePool`] for `handle_request` tests (Issue #3929). The
/// default-workspace (`workspace_root: None`) paths these tests exercise
/// never provision, so no task is actually spawned.
fn test_pool() -> Arc<WorkspacePool> {
    Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()))
}

fn setup_test_context() -> TestContext {
    let tm = Arc::new(Mutex::new(TerminalManager::new()));
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test_activity.db");
    let db = ActivityDb::new(db_path).unwrap();
    let db = Arc::new(Mutex::new(db));
    let mut sr_config = SweepRegistryConfig::new(dir.path().to_path_buf());
    sr_config.skip_label_flip = true;
    let bus = Arc::new(EventBus::new());
    let mut registry = SweepRegistry::new(sr_config);
    registry.set_event_bus(bus.clone());
    let sr = Arc::new(Mutex::new(registry));
    // Keep dir alive so the temp directory isn't deleted
    std::mem::forget(dir);
    (tm, db, sr, bus)
}

// ===== Ping/Pong =====

#[test]
fn test_handle_request_ping() {
    let (tm, db, sr, bus) = setup_test_context();
    let response = handle_request(Request::Ping, &tm, &db, &sr, &bus, &test_pool());
    assert!(matches!(response, Response::Pong));
}

// ===== ListTerminals =====

#[test]
fn test_handle_request_list_terminals_empty() {
    let (tm, db, sr, bus) = setup_test_context();
    // Set LOOM_NO_RESTORE to prevent tmux restore attempts
    std::env::set_var("LOOM_NO_RESTORE", "1");
    let response = handle_request(Request::ListTerminals, &tm, &db, &sr, &bus, &test_pool());
    std::env::remove_var("LOOM_NO_RESTORE");
    match response {
        Response::TerminalList { terminals } => {
            assert!(terminals.is_empty());
        }
        other => panic!("Expected TerminalList, got: {other:?}"),
    }
}

// ===== GetCurrentCommit =====

#[test]
fn test_handle_request_get_current_commit_nonexistent_dir() {
    let (tm, db, sr, bus) = setup_test_context();
    let response = handle_request(
        Request::GetCurrentCommit {
            working_dir: "/nonexistent/path".to_string(),
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    match response {
        Response::CurrentCommit { commit } => {
            assert!(commit.is_none());
        }
        other => panic!("Expected CurrentCommit, got: {other:?}"),
    }
}

// ===== GetTerminalActivity =====

#[test]
fn test_handle_request_get_terminal_activity_empty() {
    let (tm, db, sr, bus) = setup_test_context();
    let response = handle_request(
        Request::GetTerminalActivity {
            id: "nonexistent".to_string(),
            limit: 10,
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    match response {
        Response::TerminalActivity { entries } => {
            assert!(entries.is_empty());
        }
        other => panic!("Expected TerminalActivity, got: {other:?}"),
    }
}

// ===== GetAllClaims =====

#[test]
fn test_handle_request_get_all_claims_empty() {
    let (tm, db, sr, bus) = setup_test_context();
    let response = handle_request(Request::GetAllClaims, &tm, &db, &sr, &bus, &test_pool());
    match response {
        Response::Claims(claims) => {
            assert!(claims.is_empty());
        }
        other => panic!("Expected Claims, got: {other:?}"),
    }
}

// ===== GetClaimsSummary =====

#[test]
fn test_handle_request_get_claims_summary() {
    let (tm, db, sr, bus) = setup_test_context();
    let response = handle_request(
        Request::GetClaimsSummary {
            stale_threshold_secs: Some(3600),
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    match response {
        Response::ClaimsSummary(summary) => {
            assert_eq!(summary.total_claims, 0);
        }
        other => panic!("Expected ClaimsSummary, got: {other:?}"),
    }
}

// ===== CaptureGitChanges with nonexistent dir =====

#[test]
fn test_handle_request_capture_git_changes_no_repo() {
    let (tm, db, sr, bus) = setup_test_context();
    let response = handle_request(
        Request::CaptureGitChanges {
            input_id: 1,
            working_dir: "/nonexistent/path".to_string(),
            before_commit: None,
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    match response {
        Response::GitChangesCaptured {
            files_changed,
            lines_added,
            lines_removed,
        } => {
            assert_eq!(files_changed, 0);
            assert_eq!(lines_added, 0);
            assert_eq!(lines_removed, 0);
        }
        other => panic!("Expected GitChangesCaptured, got: {other:?}"),
    }
}

// ===== SendInput / GetTerminalOutput input correlation (Issue #4554) =====

/// End-to-end proof that a `SendInput` turn's `agent_inputs.id` is threaded
/// through to the `resource_usage` and `prompt_github` rows written by the
/// following `GetTerminalOutput` call, so `get_cost_by_issue` — which joins
/// `resource_usage -> agent_inputs -> prompt_github` on `input_id` — returns
/// a non-empty result for the turn's issue. Before the #4554 fix, both
/// writes hardcoded `input_id: None` and this join could never match in
/// production.
#[test]
fn test_send_input_then_get_terminal_output_correlates_cost_by_issue() {
    let (tm, db, sr, bus) = setup_test_context();
    let terminal_id = format!("ipc-test-4554-{}", std::process::id());

    // Seed the terminal's output file directly: `get_terminal_output` reads
    // from `/tmp/loom-<id>.out` unconditionally, regardless of whether `id`
    // is a live, registered terminal (see `TerminalManager::get_terminal_output`),
    // so this test doesn't need a real tmux-backed terminal.
    let output_path = format!("/tmp/loom-{terminal_id}.out");
    let output_body = "Creating pull request...\n\
             https://github.com/rjwalters/loom/issues/4554\n\
             Tokens: 1,234 in / 567 out\n\
             Model: claude-3-5-sonnet\n";
    std::fs::write(&output_path, output_body).unwrap();

    // Cleanup guard so a failing assertion below still removes the fixture
    // file rather than leaking it into later test runs.
    struct CleanupGuard(String);
    impl Drop for CleanupGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _cleanup = CleanupGuard(output_path.clone());

    // `SendInput` records the `agent_inputs` row and, via the #4554 fix,
    // tracks its id for this terminal. `id` isn't a real, registered
    // terminal, so delivery itself fails — that's fine: the DB write and
    // the correlation tracking both happen unconditionally before delivery
    // is attempted (mirrors production, where the two are also decoupled).
    let send_response = handle_request(
        Request::SendInput {
            id: terminal_id.clone(),
            data: "some command".to_string(),
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    assert!(
        matches!(send_response, Response::StructuredError(_)),
        "expected delivery to fail for an unregistered terminal id, got: {send_response:?}"
    );

    // `GetTerminalOutput` reads the seeded file and, with the fix,
    // correlates its forge-event/resource-usage writes to the input
    // recorded above instead of writing `input_id: None`.
    let output_response = handle_request(
        Request::GetTerminalOutput {
            id: terminal_id.clone(),
            start_byte: None,
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    assert!(
        matches!(output_response, Response::TerminalOutput { .. }),
        "expected TerminalOutput, got: {output_response:?}"
    );

    let cost = db.lock().unwrap().get_cost_by_issue(Some(4554)).unwrap();
    assert!(
        !cost.is_empty(),
        "expected a non-empty cost-by-issue rollup for issue #4554 after a recorded turn \
             (the resource_usage -> agent_inputs -> prompt_github join must match)"
    );
    assert_eq!(cost[0].issue_number, 4554);
    assert!(cost[0].total_cost > 0.0);
}

// ===== get_git_branch tests =====

#[test]
fn test_get_git_branch_none_input() {
    assert!(get_git_branch(None).is_none());
}

#[test]
fn test_get_git_branch_nonexistent_dir() {
    let dir = "/nonexistent/path".to_string();
    assert!(get_git_branch(Some(&dir)).is_none());
}

// ===== Sweep registry IPC handlers (Issue #3452) =====

/// Build a SweepRegistry that won't actually launch real children.
/// The fixture spawn binary writes its argv AND a handful of env vars
/// (notably `LOOM_SWEEP_CLAIM_OWNED`, Issue #3823/#3967) to a sibling log
/// and exits immediately (same pattern as the sweep_registry unit tests).
fn setup_sweep_registry_in_tempdir(
) -> (Arc<Mutex<SweepRegistry>>, tempfile::TempDir, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempdir().unwrap();
    let scripts_dir = dir.path().join(".loom").join("scripts");
    std::fs::create_dir_all(&scripts_dir).unwrap();
    let fake_bin = scripts_dir.join("spawn-claude.sh");
    let record_log = dir.path().join("ipc-fake-spawn.log");
    let script = format!(
        r#"#!/usr/bin/env bash
{{
  echo "argv: $*"
  printf 'LOOM_SWEEP_CLAIM_OWNED=%s\n' "${{LOOM_SWEEP_CLAIM_OWNED:-unset}}"
}} >> "{rec}"
exit 0
"#,
        rec = record_log.display()
    );
    std::fs::write(&fake_bin, script).unwrap();
    let mut perms = std::fs::metadata(&fake_bin).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_bin, perms).unwrap();

    let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
    config.spawn_bin = Some(fake_bin);
    config.skip_label_flip = true;
    // Confine the #3953 sweep journal to this test's tempdir — never the
    // real machine-level `~/.loom/sweeps.json`.
    config.journal_path = Some(dir.path().join("test-sweeps-journal.json"));
    let sr = Arc::new(Mutex::new(SweepRegistry::new(config)));
    (sr, dir, record_log)
}

// ========================================================================
// dispatch_sweep headroom advisory (#4234 — Gap 1 of #4231's decomposition)
// ========================================================================

fn fake_headroom(occupancy: usize, dynamic_cap: usize) -> DispatchHeadroom {
    DispatchHeadroom {
        occupancy,
        dynamic_cap,
        disk_headroom: 10,
        ram_headroom: 10,
        token_axis_limit: 5,
    }
}

#[test]
fn dispatch_headroom_predicate_boundary() {
    assert!(
        !dispatch_would_meet_or_exceed_headroom(&fake_headroom(2, 3)),
        "below cap: headroom remains"
    );
    assert!(
        dispatch_would_meet_or_exceed_headroom(&fake_headroom(3, 3)),
        "at cap: no headroom left for one more"
    );
    assert!(
        dispatch_would_meet_or_exceed_headroom(&fake_headroom(5, 3)),
        "over cap: definitely no headroom"
    );
}

#[test]
fn dispatch_headroom_message_names_every_axis() {
    let h = DispatchHeadroom {
        occupancy: 4,
        dynamic_cap: 3,
        disk_headroom: 9,
        ram_headroom: 7,
        token_axis_limit: 6,
    };
    let kind = SweepKind::Issue(123);
    let repo = Path::new("/tmp/loom-test-repo");

    let entered = dispatch_headroom_message(repo, true, &h, &kind);
    assert!(entered.contains("occupancy=4"), "{entered}");
    assert!(entered.contains("dynamic_cap=3"), "{entered}");
    assert!(entered.contains("disk_headroom=9"), "{entered}");
    assert!(entered.contains("ram_headroom=7"), "{entered}");
    assert!(entered.contains("token_axis_limit=6"), "{entered}");
    assert!(entered.contains("123"), "{entered}");
    assert!(entered.contains("advisory only"), "{entered}");

    let recovered = dispatch_headroom_message(repo, false, &h, &kind);
    assert!(recovered.contains("recovered"), "{recovered}");
}

#[test]
fn dispatch_headroom_advisory_dedups_on_state_change() {
    let bus = Arc::new(EventBus::new());
    let mut sub = bus.subscribe(["daemon.dispatch.headroom_advisory"]);
    // A fresh, unique tempdir path keys the process-global dedup state
    // independently of any other test (#4234's per-repo dedup design).
    let repo_dir = tempdir().unwrap();
    let repo_root = repo_dir.path().to_path_buf();
    let kind = SweepKind::Issue(4234);
    let low = fake_headroom(5, 3);
    let ok = fake_headroom(1, 3);

    // Entering low headroom fires the advisory.
    emit_dispatch_headroom_advisory_on_change(&bus, &repo_root, true, &low, &kind);
    match sub
        .try_recv()
        .expect("advisory published on entering low headroom")
    {
        Event::Generic { topic, payload } => {
            assert_eq!(topic, "daemon.dispatch.headroom_advisory");
            assert_eq!(payload["low_headroom"].as_bool(), Some(true));
            assert_eq!(payload["occupancy"].as_u64(), Some(5));
            assert_eq!(payload["dynamic_cap"].as_u64(), Some(3));
        }
        other => panic!("expected Generic advisory event, got {other:?}"),
    }

    // Still low on the next call — deduped, no second event.
    emit_dispatch_headroom_advisory_on_change(&bus, &repo_root, true, &low, &kind);
    assert!(
        matches!(sub.try_recv(), Err(crate::event_bus::RecvError::Empty)),
        "no duplicate advisory while headroom stays low"
    );

    // Recovers — symmetric recovery event.
    emit_dispatch_headroom_advisory_on_change(&bus, &repo_root, false, &ok, &kind);
    match sub.try_recv().expect("recovery event published") {
        Event::Generic { topic, payload } => {
            assert_eq!(topic, "daemon.dispatch.headroom_advisory");
            assert_eq!(payload["low_headroom"].as_bool(), Some(false));
        }
        other => panic!("expected Generic recovery event, got {other:?}"),
    }

    // Staying recovered — deduped again.
    emit_dispatch_headroom_advisory_on_change(&bus, &repo_root, false, &ok, &kind);
    assert!(
        matches!(sub.try_recv(), Err(crate::event_bus::RecvError::Empty)),
        "no duplicate recovery event while headroom stays healthy"
    );
}

/// End-to-end (#4234): `dispatch_sweep` must dispatch even when the
/// computed headroom is fully saturated — advisory-first, never a hard
/// gate. Forces `configured_max=1` via env (the smallest term always wins
/// the `min()` in `resolve_dynamic_max_concurrent`), which is deterministic
/// regardless of the real host's token/disk/cpu state.
#[test]
#[serial_test::serial]
fn test_dispatch_sweep_still_dispatches_under_synthetic_low_headroom() {
    let (tm, db, _, bus) = setup_test_context();
    let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
    // Absent `workspace_root` now consults the on-disk workspace registry
    // (#4299) — pin it to an empty temp registry so this test's outcome
    // never depends on the host's real `~/.loom/workspaces.json`.
    let _registry_guard = seed_temp_registry(&[]);

    std::env::set_var(crate::work_finder::WORK_FINDER_MAX_CONCURRENT_ENV, "1");

    let dispatch_issue = |n: u32| {
        handle_request(
            Request::DispatchSweep {
                kind: SweepKind::Issue(n),
                idempotency_key: None,
                model: None,
                effort: None,
                depends_on: None,
                workspace_root: None,
                force: false,
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        )
    };

    // First dispatch always succeeds regardless of computed headroom.
    match dispatch_issue(90_001) {
        Response::SweepDispatched { .. } => {}
        other => panic!("Expected SweepDispatched, got: {other:?}"),
    }

    // Second dispatch: occupancy is now >= the forced ceiling of 1, so this
    // call is guaranteed to be at/over the dynamic cap — and it STILL
    // dispatches (advisory-only per #4234, never a hard gate).
    match dispatch_issue(90_002) {
        Response::SweepDispatched { .. } => {}
        other => panic!(
            "Expected SweepDispatched even at/over headroom (advisory-first policy), \
                 got: {other:?}"
        ),
    }

    std::env::remove_var(crate::work_finder::WORK_FINDER_MAX_CONCURRENT_ENV);
}

#[test]
fn test_handle_request_list_sweeps_empty() {
    let (tm, db, _, bus) = setup_test_context();
    let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
    let response = handle_request(
        Request::ListSweeps {
            state_filter: None,
            workspace_root: None,
            all_workspaces: false,
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    match response {
        Response::SweepList { sweeps } => {
            assert!(sweeps.is_empty());
        }
        other => panic!("Expected SweepList, got: {other:?}"),
    }
}

/// Issue #3929: a request carrying an explicit `workspace_root` routes to
/// that repo's registry (via the pool), not the default workspace — and the
/// returned `SweepInfo` carries the owning `repo`. Omitting `workspace_root`
/// preserves default-workspace-only behavior (regression guard).
#[test]
#[serial_test::serial]
fn test_sweep_requests_route_to_explicit_workspace_root() {
    let (tm, db, _, bus) = setup_test_context();

    // Default workspace (repo A) and a second managed repo (repo B), each a
    // fixture registry with a fake spawn bin + skip_label_flip.
    let (sr_default, dir_a, _rec_a) = setup_sweep_registry_in_tempdir();
    let (sr_b, dir_b, _rec_b) = setup_sweep_registry_in_tempdir();
    let root_a = crate::workspace_registry::normalize_path(dir_a.path());
    let root_b = crate::workspace_registry::normalize_path(dir_b.path());

    // A pool seeded with both registries (mirrors main's seed of the default
    // workspace, plus repo B provisioned by the autonomous loops).
    let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
    pool.seed(root_a, sr_default.clone());
    pool.seed(root_b.clone(), sr_b.clone());

    // #5210: an explicit `workspace_root` on DispatchSweep must now name a
    // *registered* workspace (`seed_temp_registry` is defined below in this
    // module; it also points `WorkspaceRegistry::load_default()` at a temp
    // file so this test never touches the real `~/.loom/workspaces.json`).
    let _guard = seed_temp_registry(&[dir_b.path()]);

    // Dispatch issue #42 into repo B explicitly.
    let dispatched = handle_request(
        Request::DispatchSweep {
            kind: SweepKind::Issue(42),
            idempotency_key: None,
            model: None,
            effort: None,
            depends_on: None,
            workspace_root: Some(dir_b.path().to_string_lossy().into_owned()),
            force: false,
        },
        &tm,
        &db,
        &sr_default,
        &bus,
        &pool,
    );
    let sweep_id = match dispatched {
        Response::SweepDispatched { sweep_id, .. } => sweep_id,
        other => panic!("Expected SweepDispatched, got: {other:?}"),
    };

    // repo B's registry sees the sweep, and its SweepInfo.repo names repo B.
    let listed_b = handle_request(
        Request::ListSweeps {
            state_filter: None,
            workspace_root: Some(dir_b.path().to_string_lossy().into_owned()),
            all_workspaces: false,
        },
        &tm,
        &db,
        &sr_default,
        &bus,
        &pool,
    );
    match listed_b {
        Response::SweepList { sweeps } => {
            assert_eq!(sweeps.len(), 1, "repo B registry should hold the sweep");
            assert_eq!(
                sweeps[0].repo.as_deref(),
                Some(dir_b.path().display().to_string().as_str()),
                "SweepInfo.repo must name the owning workspace root"
            );
        }
        other => panic!("Expected SweepList, got: {other:?}"),
    }

    // The default workspace (workspace_root: None) must NOT see repo B's
    // sweep — this is the identity guarantee (two repos' issue #42 differ).
    let listed_default = handle_request(
        Request::ListSweeps {
            state_filter: None,
            workspace_root: None,
            all_workspaces: false,
        },
        &tm,
        &db,
        &sr_default,
        &bus,
        &pool,
    );
    match listed_default {
        Response::SweepList { sweeps } => {
            assert!(sweeps.is_empty(), "default workspace must not see repo B's sweep");
        }
        other => panic!("Expected SweepList, got: {other:?}"),
    }

    // GetSweepStatus is likewise workspace-scoped: found in repo B, absent
    // from the default workspace.
    let status_b = handle_request(
        Request::GetSweepStatus {
            sweep_id: sweep_id.clone(),
            workspace_root: Some(dir_b.path().to_string_lossy().into_owned()),
        },
        &tm,
        &db,
        &sr_default,
        &bus,
        &pool,
    );
    assert!(
        matches!(status_b, Response::SweepStatus { info: Some(_) }),
        "sweep is observable via repo B's registry"
    );
    let status_default = handle_request(
        Request::GetSweepStatus {
            sweep_id,
            workspace_root: None,
        },
        &tm,
        &db,
        &sr_default,
        &bus,
        &pool,
    );
    assert!(
        matches!(status_default, Response::SweepStatus { info: None }),
        "sweep is NOT observable via the default workspace"
    );
}

// ===== ListSweeps fleet-wide fan-out (Issue #6006 — deferred follow-up
// to #3930) =====
//
// `all_workspaces: true` enumerates every registered managed workspace
// the same way `ListQuarantines`'s `None` case does, so these tests seed
// `REGISTRY_PATH_ENV` at a temp file (via `seed_temp_registry`) rather
// than touching the real `~/.loom/workspaces.json`.

/// `all_workspaces: true` aggregates sweeps from every registered root in
/// one call — no caller-supplied `workspace_root` needed — and each
/// returned `SweepInfo` still carries the `repo` field naming its owner,
/// so a fleet-wide caller never needs to already know the individual repo
/// roots (the issue's core acceptance criterion).
#[test]
#[serial_test::serial]
fn test_list_sweeps_all_workspaces_fans_out_across_registered_roots() {
    let (tm, db, _, bus) = setup_test_context();

    let (sr_default, dir_a, _rec_a) = setup_sweep_registry_in_tempdir();
    let (sr_b, dir_b, _rec_b) = setup_sweep_registry_in_tempdir();
    let root_a = crate::workspace_registry::normalize_path(dir_a.path());
    let root_b = crate::workspace_registry::normalize_path(dir_b.path());

    let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
    pool.seed(root_a, sr_default.clone());
    pool.seed(root_b.clone(), sr_b.clone());

    // Register BOTH roots so `effective_roots` enumerates both.
    let _guard = seed_temp_registry(&[dir_a.path(), dir_b.path()]);

    let dispatched_a = handle_request(
        Request::DispatchSweep {
            kind: SweepKind::Issue(60_060),
            idempotency_key: None,
            model: None,
            effort: None,
            depends_on: None,
            workspace_root: Some(dir_a.path().to_string_lossy().into_owned()),
            force: false,
        },
        &tm,
        &db,
        &sr_default,
        &bus,
        &pool,
    );
    assert!(
        matches!(dispatched_a, Response::SweepDispatched { .. }),
        "expected SweepDispatched for repo A, got: {dispatched_a:?}"
    );

    let dispatched_b = handle_request(
        Request::DispatchSweep {
            kind: SweepKind::Issue(60_061),
            idempotency_key: None,
            model: None,
            effort: None,
            depends_on: None,
            workspace_root: Some(dir_b.path().to_string_lossy().into_owned()),
            force: false,
        },
        &tm,
        &db,
        &sr_default,
        &bus,
        &pool,
    );
    assert!(
        matches!(dispatched_b, Response::SweepDispatched { .. }),
        "expected SweepDispatched for repo B, got: {dispatched_b:?}"
    );

    // Fleet-wide fan-out: no `workspace_root`, just `all_workspaces: true`.
    let listed_all = handle_request(
        Request::ListSweeps {
            state_filter: None,
            workspace_root: None,
            all_workspaces: true,
        },
        &tm,
        &db,
        &sr_default,
        &bus,
        &pool,
    );
    match listed_all {
        Response::SweepList { sweeps } => {
            assert_eq!(sweeps.len(), 2, "fan-out must see both repos' sweeps");
            let repos: std::collections::BTreeSet<_> =
                sweeps.iter().map(|s| s.repo.clone()).collect();
            assert_eq!(
                repos,
                std::collections::BTreeSet::from([
                    Some(dir_a.path().display().to_string()),
                    Some(dir_b.path().display().to_string()),
                ]),
                "each SweepInfo must carry its owning repo, no repo omitted"
            );
        }
        other => panic!("Expected SweepList, got: {other:?}"),
    }
}

/// Regression guard: `all_workspaces` absent/`false` reproduces
/// byte-for-byte pre-#6006 `workspace_root`-scoped (or
/// default-workspace-only) behavior even when multiple workspaces are
/// registered and populated — the fan-out is strictly opt-in, never a
/// reinterpretation of the existing `None`/absent `workspace_root`
/// contract. Also asserts an explicit `workspace_root` still scopes to
/// that one repo when `all_workspaces` is left at its default.
#[test]
#[serial_test::serial]
fn test_list_sweeps_all_workspaces_false_preserves_single_workspace_behavior() {
    let (tm, db, _, bus) = setup_test_context();

    let (sr_default, dir_a, _rec_a) = setup_sweep_registry_in_tempdir();
    let (sr_b, dir_b, _rec_b) = setup_sweep_registry_in_tempdir();
    let root_a = crate::workspace_registry::normalize_path(dir_a.path());
    let root_b = crate::workspace_registry::normalize_path(dir_b.path());

    let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
    pool.seed(root_a, sr_default.clone());
    pool.seed(root_b.clone(), sr_b.clone());

    let _guard = seed_temp_registry(&[dir_a.path(), dir_b.path()]);

    // Dispatch into repo B only.
    let dispatched_b = handle_request(
        Request::DispatchSweep {
            kind: SweepKind::Issue(60_062),
            idempotency_key: None,
            model: None,
            effort: None,
            depends_on: None,
            workspace_root: Some(dir_b.path().to_string_lossy().into_owned()),
            force: false,
        },
        &tm,
        &db,
        &sr_default,
        &bus,
        &pool,
    );
    assert!(matches!(dispatched_b, Response::SweepDispatched { .. }));

    // Default (`workspace_root: None`, `all_workspaces: false`) must NOT
    // see repo B's sweep, exactly as before #6006 — even though repo B is
    // now registered and populated.
    let listed_default = handle_request(
        Request::ListSweeps {
            state_filter: None,
            workspace_root: None,
            all_workspaces: false,
        },
        &tm,
        &db,
        &sr_default,
        &bus,
        &pool,
    );
    match listed_default {
        Response::SweepList { sweeps } => assert!(
            sweeps.is_empty(),
            "default-only listing must ignore repo B's sweep even though it exists"
        ),
        other => panic!("Expected SweepList, got: {other:?}"),
    }

    // An explicit `workspace_root` still scopes to that one repo when
    // `all_workspaces` is left at its default.
    let listed_b = handle_request(
        Request::ListSweeps {
            state_filter: None,
            workspace_root: Some(dir_b.path().to_string_lossy().into_owned()),
            all_workspaces: false,
        },
        &tm,
        &db,
        &sr_default,
        &bus,
        &pool,
    );
    match listed_b {
        Response::SweepList { sweeps } => {
            assert_eq!(sweeps.len(), 1, "explicit workspace_root still scopes to repo B");
        }
        other => panic!("Expected SweepList, got: {other:?}"),
    }
}

/// `all_workspaces: true` and an explicit `workspace_root` are mutually
/// exclusive by design — the flag always wins. Repo A's sweep is still
/// visible in the fan-out even though `workspace_root` names repo B.
#[test]
#[serial_test::serial]
fn test_list_sweeps_all_workspaces_true_ignores_explicit_workspace_root() {
    let (tm, db, _, bus) = setup_test_context();

    let (sr_default, dir_a, _rec_a) = setup_sweep_registry_in_tempdir();
    let (sr_b, dir_b, _rec_b) = setup_sweep_registry_in_tempdir();
    let root_a = crate::workspace_registry::normalize_path(dir_a.path());
    let root_b = crate::workspace_registry::normalize_path(dir_b.path());

    let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
    pool.seed(root_a, sr_default.clone());
    pool.seed(root_b.clone(), sr_b.clone());

    let _guard = seed_temp_registry(&[dir_a.path(), dir_b.path()]);

    let dispatched_a = handle_request(
        Request::DispatchSweep {
            kind: SweepKind::Issue(60_063),
            idempotency_key: None,
            model: None,
            effort: None,
            depends_on: None,
            workspace_root: Some(dir_a.path().to_string_lossy().into_owned()),
            force: false,
        },
        &tm,
        &db,
        &sr_default,
        &bus,
        &pool,
    );
    assert!(matches!(dispatched_a, Response::SweepDispatched { .. }));

    // `workspace_root` names repo B, but `all_workspaces: true` wins —
    // repo A's sweep is still visible in the aggregated response.
    let listed = handle_request(
        Request::ListSweeps {
            state_filter: None,
            workspace_root: Some(dir_b.path().to_string_lossy().into_owned()),
            all_workspaces: true,
        },
        &tm,
        &db,
        &sr_default,
        &bus,
        &pool,
    );
    match listed {
        Response::SweepList { sweeps } => {
            assert_eq!(
                sweeps.len(),
                1,
                "fan-out must include repo A's sweep despite workspace_root naming repo B"
            );
            assert_eq!(
                sweeps[0].repo.as_deref(),
                Some(dir_a.path().display().to_string().as_str())
            );
        }
        other => panic!("Expected SweepList, got: {other:?}"),
    }
}

// ===== Dispatch-path workspace resolution (#4299) =====

/// Registers `path` as the sole workspace at a temp registry file (via
/// [`crate::workspace_registry::REGISTRY_PATH_ENV`]) and returns a guard
/// that clears the env var on drop, so `WorkspaceRegistry::load_default()`
/// inside `resolve_dispatch_registry` never touches the real
/// `~/.loom/workspaces.json`.
struct RegistryEnvGuard {
    _dir: tempfile::TempDir,
}
impl Drop for RegistryEnvGuard {
    fn drop(&mut self) {
        std::env::remove_var(crate::workspace_registry::REGISTRY_PATH_ENV);
    }
}
fn seed_temp_registry(roots: &[&Path]) -> RegistryEnvGuard {
    let dir = tempdir().unwrap();
    let path = dir.path().join("workspaces.json");
    std::env::set_var(crate::workspace_registry::REGISTRY_PATH_ENV, &path);
    let mut registry = WorkspaceRegistry::default();
    for root in roots {
        registry.add(root, None).unwrap();
    }
    registry.save(&path).unwrap();
    RegistryEnvGuard { _dir: dir }
}

/// Issue #4299 — the Linux worker-host shape this issue exists to fix:
/// exactly one workspace is registered and it is NOT the daemon's seeded
/// default (its cwd). An absent `workspace_root` on `DispatchSweep` must
/// still target the single registration, not the unregistered default.
#[test]
#[serial_test::serial]
fn test_dispatch_sweep_absent_workspace_root_targets_single_registration() {
    let (tm, db, _, bus) = setup_test_context();
    let (sr_default, dir_a, _rec_a) = setup_sweep_registry_in_tempdir();
    let (sr_b, dir_b, _rec_b) = setup_sweep_registry_in_tempdir();
    let root_a = crate::workspace_registry::normalize_path(dir_a.path());
    let root_b = crate::workspace_registry::normalize_path(dir_b.path());

    let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
    pool.seed(root_a, sr_default.clone());
    pool.seed(root_b.clone(), sr_b.clone());

    // Registry names ONLY repo B — repo A (the seeded default) is
    // unregistered, mirroring a machine-checkout daemon cwd with one
    // registered product repo.
    let _guard = seed_temp_registry(&[dir_b.path()]);

    let dispatched = handle_request(
        Request::DispatchSweep {
            kind: SweepKind::Issue(4299),
            idempotency_key: None,
            model: None,
            effort: None,
            depends_on: None,
            workspace_root: None,
            force: false,
        },
        &tm,
        &db,
        &sr_default,
        &bus,
        &pool,
    );
    assert!(
        matches!(dispatched, Response::SweepDispatched { .. }),
        "expected SweepDispatched, got: {dispatched:?}"
    );

    // Repo B's registry sees the dispatched sweep...
    let listed_b = handle_request(
        Request::ListSweeps {
            state_filter: None,
            workspace_root: Some(dir_b.path().to_string_lossy().into_owned()),
            all_workspaces: false,
        },
        &tm,
        &db,
        &sr_default,
        &bus,
        &pool,
    );
    match listed_b {
        Response::SweepList { sweeps } => {
            assert_eq!(sweeps.len(), 1, "the single registered workspace must receive the sweep")
        }
        other => panic!("Expected SweepList, got: {other:?}"),
    }

    // ...and the unregistered default (repo A / daemon cwd) does NOT.
    let listed_default = handle_request(
        Request::ListSweeps {
            state_filter: None,
            workspace_root: None,
            all_workspaces: false,
        },
        &tm,
        &db,
        &sr_default,
        &bus,
        &pool,
    );
    match listed_default {
        Response::SweepList { sweeps } => assert!(
            sweeps.is_empty(),
            "the daemon's own (unregistered) cwd must NOT receive the sweep"
        ),
        other => panic!("Expected SweepList, got: {other:?}"),
    }
}

/// Issue #4299 — with multiple registered workspaces and a seeded default
/// that is itself unregistered, an absent `workspace_root` must return a
/// structured ambiguity error naming every registered root, never a silent
/// cwd fallback.
#[test]
#[serial_test::serial]
fn test_dispatch_sweep_ambiguous_registry_errors_without_explicit_param() {
    let (tm, db, _, bus) = setup_test_context();
    let (sr_default, dir_a, _rec_a) = setup_sweep_registry_in_tempdir();
    let (sr_b, dir_b, _rec_b) = setup_sweep_registry_in_tempdir();
    let (sr_c, dir_c, _rec_c) = setup_sweep_registry_in_tempdir();
    let root_a = crate::workspace_registry::normalize_path(dir_a.path());
    let root_b = crate::workspace_registry::normalize_path(dir_b.path());
    let root_c = crate::workspace_registry::normalize_path(dir_c.path());

    let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
    pool.seed(root_a, sr_default.clone());
    pool.seed(root_b.clone(), sr_b.clone());
    pool.seed(root_c.clone(), sr_c.clone());

    let _guard = seed_temp_registry(&[dir_b.path(), dir_c.path()]);

    let dispatched = handle_request(
        Request::DispatchSweep {
            kind: SweepKind::Issue(4299),
            idempotency_key: None,
            model: None,
            effort: None,
            depends_on: None,
            workspace_root: None,
            force: false,
        },
        &tm,
        &db,
        &sr_default,
        &bus,
        &pool,
    );
    match dispatched {
        Response::StructuredError(err) => {
            assert_eq!(err.code.0, crate::errors::ErrorCode::CONFIG_WORKSPACE_AMBIGUOUS);
            assert!(
                err.message.contains(&root_b.display().to_string())
                    && err.message.contains(&root_c.display().to_string()),
                "ambiguity error must name every registered root, got: {}",
                err.message
            );
        }
        other => panic!("Expected StructuredError, got: {other:?}"),
    }
}

/// Issue #5210, AC #1 — an explicit `workspace_root` that names a path the
/// daemon has never registered must return a structured
/// `workspace_unregistered` error naming both the offending path and every
/// registered root, instead of silently provisioning a registry for an
/// arbitrary directory via `get_or_provision`.
#[test]
#[serial_test::serial]
fn test_dispatch_sweep_unregistered_explicit_workspace_root_is_structured_error() {
    let (tm, db, _, bus) = setup_test_context();
    let (sr_default, dir_a, _rec_a) = setup_sweep_registry_in_tempdir();
    let dir_unregistered = tempdir().unwrap();
    let root_a = crate::workspace_registry::normalize_path(dir_a.path());
    let unregistered = crate::workspace_registry::normalize_path(dir_unregistered.path());

    let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
    pool.seed(root_a, sr_default.clone());

    // Only repo A is registered; `dir_unregistered` is never added.
    let _guard = seed_temp_registry(&[dir_a.path()]);

    let dispatched = handle_request(
        Request::DispatchSweep {
            kind: SweepKind::Issue(5210),
            idempotency_key: None,
            model: None,
            effort: None,
            depends_on: None,
            workspace_root: Some(dir_unregistered.path().to_string_lossy().into_owned()),
            force: false,
        },
        &tm,
        &db,
        &sr_default,
        &bus,
        &pool,
    );
    match dispatched {
        Response::StructuredError(err) => {
            assert_eq!(err.code.0, crate::errors::ErrorCode::CONFIG_WORKSPACE_UNREGISTERED);
            assert!(
                err.message.contains(&unregistered.display().to_string()),
                "error must name the offending unregistered path, got: {}",
                err.message
            );
            assert!(
                err.message.contains(&dir_a.path().display().to_string())
                    || err
                        .details
                        .as_ref()
                        .and_then(|d| d.get("registered"))
                        .map(|v| v.to_string())
                        .unwrap_or_default()
                        .contains(&dir_a.path().display().to_string()),
                "error must list the registered roots, got message={} details={:?}",
                err.message,
                err.details
            );
        }
        other => panic!("Expected StructuredError, got: {other:?}"),
    }
}

/// Issue #5345 — the `workspace_unregistered` recovery hint must branch on
/// the **target** root's own `daemon.delegatedTo`, not the daemon
/// process's cwd: an unregistered target that itself declares delegation
/// gets pointed at its delegate repo instead of the generic "run
/// `workspace add` here" suggestion. This is the triggering incident
/// (dispatch into a delegated repo hitting this exact error) end-to-end.
#[test]
#[serial_test::serial]
fn test_dispatch_sweep_unregistered_delegated_target_hint_names_delegate() {
    let (tm, db, _, bus) = setup_test_context();
    let (sr_default, dir_a, _rec_a) = setup_sweep_registry_in_tempdir();
    let dir_unregistered = tempdir().unwrap();
    std::fs::create_dir_all(dir_unregistered.path().join(".loom")).unwrap();
    std::fs::write(
        dir_unregistered.path().join(".loom").join("config.json"),
        r#"{"daemon": {"delegatedTo": "/Users/alice/GitHub/other-repo"}}"#,
    )
    .unwrap();
    let root_a = crate::workspace_registry::normalize_path(dir_a.path());

    let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
    pool.seed(root_a, sr_default.clone());

    // Only repo A is registered; `dir_unregistered` (delegated) is never added.
    let _guard = seed_temp_registry(&[dir_a.path()]);

    let dispatched = handle_request(
        Request::DispatchSweep {
            kind: SweepKind::Issue(5345),
            idempotency_key: None,
            model: None,
            effort: None,
            depends_on: None,
            workspace_root: Some(dir_unregistered.path().to_string_lossy().into_owned()),
            force: false,
        },
        &tm,
        &db,
        &sr_default,
        &bus,
        &pool,
    );
    match dispatched {
        Response::StructuredError(err) => {
            assert_eq!(err.code.0, crate::errors::ErrorCode::CONFIG_WORKSPACE_UNREGISTERED);
            let hint = err.recovery_hint.expect("recovery hint must be present");
            assert!(
                hint.contains("/Users/alice/GitHub/other-repo"),
                "recovery hint must name the target's own delegate, got: {hint}"
            );
        }
        other => panic!("Expected StructuredError, got: {other:?}"),
    }
}

/// Issue #5345 AC — `daemon.delegatedTo` gates only the CLI admin
/// entry points (`workspace add/set-priority/remove`, `tokens
/// bootstrap`); `dispatch_sweep` into an **already-registered** target
/// that happens to declare `daemon.delegatedTo` must dispatch exactly as
/// it would without the key present — daemon-client actions are
/// unaffected by delegation.
#[test]
#[serial_test::serial]
fn test_dispatch_sweep_succeeds_into_a_registered_delegated_workspace() {
    let (tm, db, _, bus) = setup_test_context();
    let (sr_default, dir_a, _rec_a) = setup_sweep_registry_in_tempdir();
    let (sr_b, dir_b, _rec_b) = setup_sweep_registry_in_tempdir();
    std::fs::write(
        dir_b.path().join(".loom").join("config.json"),
        r#"{"daemon": {"delegatedTo": "/Users/alice/GitHub/other-repo"}}"#,
    )
    .unwrap();
    let root_a = crate::workspace_registry::normalize_path(dir_a.path());
    let root_b = crate::workspace_registry::normalize_path(dir_b.path());

    let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
    pool.seed(root_a, sr_default.clone());
    pool.seed(root_b, sr_b.clone());

    // Repo B (delegated) is registered like any other managed workspace.
    let _guard = seed_temp_registry(&[dir_b.path()]);

    let dispatched = handle_request(
        Request::DispatchSweep {
            kind: SweepKind::Issue(5345),
            idempotency_key: None,
            model: None,
            effort: None,
            depends_on: None,
            workspace_root: Some(dir_b.path().to_string_lossy().into_owned()),
            force: false,
        },
        &tm,
        &db,
        &sr_default,
        &bus,
        &pool,
    );
    assert!(
        matches!(dispatched, Response::SweepDispatched { .. }),
        "dispatch_sweep must be unaffected by daemon.delegatedTo, got: {dispatched:?}"
    );
}

/// Issue #5210, AC #2/#3 — once an unregistered root is filtered out by AC
/// #1, a spawn failure unrelated to registration (a *registered* workspace
/// missing `spawn-worker.sh`) must still surface `resolve_spawn_bin`'s
/// specific message through `dispatch_sweep failed: {e:#}` — distinct from
/// the AC #1 registration error and no longer collapsed into the opaque
/// "failed to spawn sweep child" outer context alone.
#[test]
#[serial_test::serial]
fn test_dispatch_sweep_registered_workspace_missing_spawn_bin_surfaces_inner_error() {
    let (tm, db, _, bus) = setup_test_context();
    let dir = tempdir().unwrap();
    // Deliberately do NOT create `.loom/scripts/spawn-worker.sh` (or
    // `defaults/scripts/spawn-worker.sh`), and leave `spawn_bin` unset —
    // this workspace IS registered, but is misconfigured.
    let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
    config.skip_label_flip = true; // bypass runtime admission / #4027 guard, not spawn_bin resolution
    config.journal_path = Some(dir.path().join("test-sweeps-journal.json"));
    let sr = Arc::new(Mutex::new(SweepRegistry::new(config)));

    let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
    let root = crate::workspace_registry::normalize_path(dir.path());
    pool.seed(root, sr.clone());
    let _guard = seed_temp_registry(&[dir.path()]);

    // Isolate from a stray real `LOOM_SWEEP_SPAWN_BIN` in the test env.
    std::env::remove_var(crate::sweep_registry::SPAWN_BIN_ENV);

    let dispatched = handle_request(
        Request::DispatchSweep {
            kind: SweepKind::Issue(5210),
            idempotency_key: None,
            model: None,
            effort: None,
            depends_on: None,
            workspace_root: Some(dir.path().to_string_lossy().into_owned()),
            force: false,
        },
        &tm,
        &db,
        &sr,
        &bus,
        &pool,
    );
    match dispatched {
        Response::Error { message } => {
            assert!(
                    message.contains("spawn-worker.sh not found under"),
                    "expected the specific resolve_spawn_bin message to survive `{{e:#}}`, got: {message}"
                );
            assert!(
                message.contains("failed to spawn sweep child"),
                "outer context should still be present alongside the inner detail, got: {message}"
            );
        }
        other => panic!("Expected Response::Error, got: {other:?}"),
    }
}

/// Issue #4299 — the #4027 wedge-loop guard must evaluate the *resolved*
/// workspace (the single registration), not the daemon's own unregistered
/// cwd: the error names repo B's root, not repo A's.
#[test]
#[serial_test::serial]
fn test_dispatch_sweep_wedge_guard_names_resolved_workspace_not_cwd() {
    let (tm, db, _, bus) = setup_test_context();
    let dir_a = tempdir().unwrap();
    let dir_b = tempdir().unwrap();
    // Runtime admission is the first dispatch decision. Install a valid
    // zero-config Claude surface in both candidate roots so this fixture
    // reaches (and continues to assert) the downstream workspace-command
    // guard rather than bypassing admission.
    for root in [dir_a.path(), dir_b.path()] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(root.join(".loom/roles")).unwrap();
        std::fs::create_dir_all(root.join(".loom/runtimes")).unwrap();
        std::fs::create_dir_all(root.join(".loom/scripts")).unwrap();
        std::fs::write(
            root.join(".loom/roles/builder.json"),
            r#"{"runtimeRequirements":["worktreeIsolation","mcp"]}"#,
        )
        .unwrap();
        std::fs::write(
            root.join(".loom/runtimes/claude.json"),
            r#"{"runtime":"claude","capabilities":{"worktreeIsolation":"yes","mcp":"yes"}}"#,
        )
        .unwrap();
        let adapter = root.join(".loom/scripts/spawn-claude.sh");
        std::fs::write(&adapter, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(adapter, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    // Neither workspace has `.claude/commands/loom/sweep.md`, and
    // `skip_label_flip` is left at its default `false` so the #4027 guard
    // is actually evaluated (unlike `setup_sweep_registry_in_tempdir`,
    // which sets `skip_label_flip = true` for its other fixtures).
    let sr_default = Arc::new(Mutex::new(SweepRegistry::new(SweepRegistryConfig::new(
        dir_a.path().to_path_buf(),
    ))));
    let sr_b = Arc::new(Mutex::new(SweepRegistry::new(SweepRegistryConfig::new(
        dir_b.path().to_path_buf(),
    ))));
    let root_a = crate::workspace_registry::normalize_path(dir_a.path());
    let root_b = crate::workspace_registry::normalize_path(dir_b.path());

    let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
    pool.seed(root_a.clone(), sr_default.clone());
    pool.seed(root_b.clone(), sr_b.clone());

    let _guard = seed_temp_registry(&[dir_b.path()]);

    let dispatched = handle_request(
        Request::DispatchSweep {
            kind: SweepKind::Issue(4299),
            idempotency_key: None,
            model: None,
            effort: None,
            depends_on: None,
            workspace_root: None,
            force: false,
        },
        &tm,
        &db,
        &sr_default,
        &bus,
        &pool,
    );
    match dispatched {
        Response::Error { message } => {
            // The guard message embeds `SweepRegistryConfig::workspace_root`
            // verbatim, which here is the *raw* tempdir path each `sr_*` was
            // constructed with (not the canonicalized `root_a`/`root_b` used
            // as the pool's dedup key) — assert against that raw form.
            assert!(
                message.contains(&dir_b.path().display().to_string()),
                "wedge-guard error must name the resolved workspace (repo B), got: {message}"
            );
            assert!(
                !message.contains(&dir_a.path().display().to_string()),
                "wedge-guard error must NOT name the daemon's own unregistered cwd, got: {message}"
            );
        }
        other => panic!("Expected Error (wedge-guard refusal), got: {other:?}"),
    }
}

#[test]
fn runtime_rejection_response_is_structured_and_secret_free_on_the_wire() {
    let response = Response::RuntimeRejected(crate::runtime_admission::RuntimeRejection {
        role: "sweep-lifecycle".into(),
        runtime: "codex".into(),
        source: crate::runtime_admission::RuntimeSource::DefaultConfig,
        unmet_capabilities: vec!["worktreeIsolation".into()],
        reason: "unmet capabilities: worktreeIsolation".into(),
    });
    let wire = serde_json::to_string(&response).unwrap();
    assert!(wire.contains("\"type\":\"RuntimeRejected\""));
    assert!(wire.contains("\"source\":\"default-config\""));
    assert!(wire.contains("\"unmet_capabilities\":[\"worktreeIsolation\"]"));
    assert!(!wire.contains("oauth"));
    assert!(!wire.contains("token"));
    assert!(matches!(
        serde_json::from_str::<Response>(&wire).unwrap(),
        Response::RuntimeRejected(_)
    ));
}

#[test]
#[serial_test::serial]
fn test_handle_request_dispatch_sweep_happy_path() {
    let (tm, db, _, bus) = setup_test_context();
    let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
    // #4299: pin the registry to empty so `workspace_root: None` resolution
    // is deterministic regardless of the host's real registry.
    let _registry_guard = seed_temp_registry(&[]);

    let response = handle_request(
        Request::DispatchSweep {
            kind: SweepKind::Issue(2024),
            idempotency_key: None,
            model: None,
            effort: None,
            depends_on: None,
            workspace_root: None,
            force: false,
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    match response {
        Response::SweepDispatched {
            sweep_id,
            pid,
            token_name,
            log_path,
        } => {
            assert!(sweep_id.starts_with("sweep-issue-2024-"));
            assert!(pid > 0);
            assert_eq!(token_name, "unknown");
            assert!(log_path.to_string_lossy().contains("sweep-issue-2024.log"));
        }
        other => panic!("Expected SweepDispatched, got: {other:?}"),
    }

    // Follow-up ListSweeps should see the new entry. The fake spawn exits
    // immediately, so reap-on-read (Issue #3893) promptly reconciles the
    // entry to a terminal `Exited` state rather than over-reporting it as
    // `Running` — the entry is still listed, just no longer stale-Running.
    let response = handle_request(
        Request::ListSweeps {
            state_filter: None,
            workspace_root: None,
            all_workspaces: false,
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    match response {
        Response::SweepList { sweeps } => {
            assert_eq!(sweeps.len(), 1);
            assert!(
                sweeps[0].state.is_terminal(),
                "reap-on-read should have transitioned the exited fake child \
                     out of Running (#3893); got {:?}",
                sweeps[0].state
            );
        }
        other => panic!("Expected SweepList, got: {other:?}"),
    }
}

/// Issue #3967: reproduce the reported daemon-dispatched sweep self-skip at
/// the **IPC dispatch-path level** — through `handle_request` itself, not
/// `SweepRegistry::dispatch()` called directly (the existing
/// `dispatch_exports_claim_ownership_marker` unit test in
/// `sweep_registry.rs` covers that narrower scope). `handle_request`'s
/// `Request::DispatchSweep` arm is the exact server-side code both the
/// `loom-daemon dispatch <issue>` operator CLI (#3952) and the MCP
/// `dispatch_sweep` tool round-trip into over the Unix socket — so a
/// regression here would have caught the incident regardless of which of
/// those two client surfaces initiated the request. Asserts the spawned
/// child's env carries `LOOM_SWEEP_CLAIM_OWNED=<issue>` end-to-end, AND
/// (#4111) that its argv carries the equivalent `--claim-owned <issue>`
/// flag — the positional signal `/loom:sweep`'s pre-flight actually reads.
#[test]
#[serial_test::serial]
fn test_handle_request_dispatch_sweep_exports_claim_ownership_marker() {
    let (tm, db, _, bus) = setup_test_context();
    let (sr, _dir, record_log) = setup_sweep_registry_in_tempdir();
    // #4299: pin the registry to empty so `workspace_root: None` resolution
    // is deterministic regardless of the host's real registry.
    let _registry_guard = seed_temp_registry(&[]);

    let response = handle_request(
        Request::DispatchSweep {
            kind: SweepKind::Issue(3964),
            idempotency_key: None,
            model: None,
            effort: None,
            depends_on: None,
            workspace_root: None,
            force: false,
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    assert!(
        matches!(response, Response::SweepDispatched { .. }),
        "expected SweepDispatched, got: {response:?}"
    );

    // The fake spawn-claude.sh exits immediately; give it a brief window
    // to flush its record log rather than racing the write.
    let start = std::time::Instant::now();
    let mut recorded = String::new();
    while start.elapsed().as_millis() < 5000 {
        if let Ok(s) = std::fs::read_to_string(&record_log) {
            if s.contains("LOOM_SWEEP_CLAIM_OWNED=") {
                recorded = s;
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        recorded.contains("LOOM_SWEEP_CLAIM_OWNED=3964"),
        "expected the daemon-owned-child self-claim marker to reach the \
             spawned child via the IPC DispatchSweep handler; got: {recorded:?}"
    );
    // #4111: the positional argv flag must also reach the child via this
    // same IPC path.
    assert!(
        recorded.contains("--claim-owned 3964"),
        "expected --claim-owned 3964 in the spawned child's argv via the IPC \
             DispatchSweep handler (#4111); got: {recorded:?}"
    );
}

/// Issue #4666: `Request::DispatchSweep` previously consulted only the
/// host-distress breaker (`host_breaker`), never the GitHub rate-limit
/// breaker (`rate_limit_breaker`, #4429/#4440) — so a brand-new dispatch
/// could still land while the shared forge API budget was in a known
/// cooldown. These tests exercise [`rate_limit_dispatch_refusal`] — the
/// exact decision `handle_request`'s `DispatchSweep` arm makes — directly
/// with a manually constructed [`crate::rate_limit_breaker::RateLimitSnapshot`]
/// rather than through the process-global breaker: see that function's
/// doc comment for why (registering the real global would permanently
/// poison every other `DispatchSweep` test sharing this test binary under
/// plain `cargo test --workspace`, which this repo's CI still runs
/// alongside `cargo nextest run`).
mod rate_limit_dispatch_refusal_tests {
    use super::*;

    fn suppressed_snapshot(
        cooldown_until: Option<chrono::DateTime<Utc>>,
    ) -> crate::rate_limit_breaker::RateLimitSnapshot {
        crate::rate_limit_breaker::RateLimitSnapshot {
            enabled: true,
            phase: crate::rate_limit_breaker::BreakerPhase::Cooldown,
            suppressed: true,
            source: Some("test_source".to_string()),
            tripped_at: Some(Utc::now()),
            cooldown_until,
            trips_total: 1,
            core_remaining: None,
            graphql_remaining: None,
            budget_probed_at: None,
        }
    }

    /// No breaker registered at all (`global_snapshot()` returns `None`)
    /// must be a complete no-op — zero behavior change for daemons that
    /// never enabled the breaker.
    #[test]
    fn no_snapshot_never_refuses() {
        let kind = SweepKind::Issue(4666);
        assert!(rate_limit_dispatch_refusal(&kind, None, false).is_none());
        assert!(rate_limit_dispatch_refusal(&kind, None, true).is_none());
    }

    /// A registered breaker that is Closed (not suppressed) must not
    /// refuse either.
    #[test]
    fn closed_breaker_never_refuses() {
        let kind = SweepKind::Issue(4666);
        let snap = crate::rate_limit_breaker::RateLimitSnapshot {
            enabled: true,
            phase: crate::rate_limit_breaker::BreakerPhase::Closed,
            suppressed: false,
            source: None,
            tripped_at: None,
            cooldown_until: None,
            trips_total: 0,
            core_remaining: None,
            graphql_remaining: None,
            budget_probed_at: None,
        };
        assert!(rate_limit_dispatch_refusal(&kind, Some(&snap), false).is_none());
    }

    /// The core #4666 fix: a suppressed (Cooldown) snapshot refuses the
    /// dispatch by default, with a message that (a) names the rate-limit
    /// breaker and its cooldown release time, and (b) does not reuse the
    /// host-distress breaker's wording — the two must never be conflated
    /// since they have different root causes and different remediations.
    #[test]
    fn suppressed_breaker_refuses_with_distinct_message() {
        let kind = SweepKind::Issue(4666);
        let until = Utc::now() + chrono::Duration::seconds(600);
        let snap = suppressed_snapshot(Some(until));

        let response = rate_limit_dispatch_refusal(&kind, Some(&snap), false);
        match response {
            Some(Response::Error { message }) => {
                assert!(
                    message.contains("rate-limit"),
                    "expected the rate-limit breaker refusal message, got: {message}"
                );
                assert!(
                    message.contains(&until.to_string()),
                    "expected the cooldown release time in the message, got: {message}"
                );
                assert!(
                    !message.contains("host circuit breaker") && !message.contains("host distress"),
                    "rate-limit refusal must not be conflated with the host-distress \
                         breaker's wording: {message}"
                );
            }
            other => panic!("Expected Some(Response::Error), got: {other:?}"),
        }
    }

    /// A suppressed snapshot with no probed cooldown time yet must still
    /// refuse, with an informative (not panicking/empty) fallback phrase.
    #[test]
    fn suppressed_breaker_without_cooldown_time_still_refuses() {
        let kind = SweepKind::Issue(4666);
        let snap = suppressed_snapshot(None);
        let response = rate_limit_dispatch_refusal(&kind, Some(&snap), false);
        assert!(
            matches!(response, Some(Response::Error { .. })),
            "expected a refusal even without a known cooldown release time, got: {response:?}"
        );
    }

    /// `force: true` overrides the rate-limit breaker independently of
    /// the host-distress breaker's own `force` handling, even while the
    /// snapshot itself remains suppressed throughout.
    #[test]
    fn force_true_overrides_suppressed_breaker() {
        let kind = SweepKind::Issue(4666);
        let snap = suppressed_snapshot(Some(Utc::now() + chrono::Duration::seconds(600)));
        assert!(
            rate_limit_dispatch_refusal(&kind, Some(&snap), true).is_none(),
            "force: true must bypass the rate-limit breaker refusal"
        );
    }
}

/// Issue #5340: `Request::DispatchSweep` — routed through `handle_client`,
/// not `handle_request` — is the one dispatch producer whose admission was
/// never actually gated on `DrainState`'s flag, unlike the work-finder,
/// epic supervisor, and role runner, which all read it in-process each
/// tick. These tests exercise [`drain_dispatch_refusal`] — the exact
/// decision `handle_client` makes before ever calling `handle_request` —
/// directly with a plain `bool`, matching the
/// [`rate_limit_dispatch_refusal_tests`] pattern just above (a real
/// `DrainState` is a `Mutex`-guarded singleton per daemon process, not
/// something a unit test wants to mutate to exercise one decision).
mod drain_dispatch_refusal_tests {
    use super::*;

    /// Not draining ⇒ never refuses, regardless of `force`. This is the
    /// overwhelmingly common case (no drain in progress) and must be a
    /// complete no-op.
    #[test]
    fn not_draining_never_refuses() {
        let kind = SweepKind::Issue(5340);
        assert!(drain_dispatch_refusal(&kind, false, false).is_none());
        assert!(drain_dispatch_refusal(&kind, false, true).is_none());
    }

    /// The core #5340 fix: an active drain refuses a plain (non-forced)
    /// explicit dispatch, with a message that names the drain, points at
    /// `loom-daemon status` to check progress, and `restart --abort-drain`
    /// to resume dispatch immediately.
    #[test]
    fn draining_refuses_without_force() {
        let kind = SweepKind::Issue(5340);
        let response = drain_dispatch_refusal(&kind, true, false);
        match response {
            Some(Response::Error { message }) => {
                assert!(
                    message.contains("drain"),
                    "expected the drain refusal message, got: {message}"
                );
                assert!(
                    message.contains("loom-daemon status"),
                    "expected a pointer to checking status, got: {message}"
                );
                assert!(
                    message.contains("--abort-drain"),
                    "expected the abort-drain escape hatch, got: {message}"
                );
            }
            other => panic!("Expected Some(Response::Error), got: {other:?}"),
        }
    }

    /// `force: true` overrides the drain refusal independently of the
    /// host-distress/rate-limit breakers' own `force` handling, even while
    /// `is_draining` remains `true` throughout — an operator can still push
    /// an urgent dispatch through a drain window.
    #[test]
    fn force_true_overrides_active_drain() {
        let kind = SweepKind::Issue(5340);
        assert!(
            drain_dispatch_refusal(&kind, true, true).is_none(),
            "force: true must bypass the drain refusal"
        );
    }
}

/// Issue #5342: `Request::DispatchSweep` accepts `SweepKind::PrSet` and
/// spawns it through the exact same `handle_request` arm as `Issue`
/// (no protocol change — the arm already forwarded `kind` generically to
/// `sr.dispatch`).
#[test]
#[serial_test::serial]
fn test_handle_request_dispatch_sweep_accepts_prset() {
    let (tm, db, _, bus) = setup_test_context();
    let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
    // #4299: pin the registry to empty so `workspace_root: None` resolution
    // is deterministic regardless of the host's real registry.
    let _registry_guard = seed_temp_registry(&[]);

    let response = handle_request(
        Request::DispatchSweep {
            kind: SweepKind::PrSet(vec![100, 200]),
            idempotency_key: None,
            model: None,
            effort: None,
            depends_on: None,
            workspace_root: None,
            force: false,
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    match response {
        Response::SweepDispatched { sweep_id, .. } => {
            assert!(sweep_id.contains("prs"), "expected a PrSet-shaped sweep id; got: {sweep_id}");
        }
        other => panic!("Expected SweepDispatched, got: {other:?}"),
    }
}

// ===== DispatchSweep IPC-level burst behavior (Issue #6592) =====

/// Build a `SweepRegistry` whose fixture `spawn-claude.sh` stays alive
/// and logs its account selection only after `poll_delay` — so
/// `poll_and_classify_spawned_child`'s wait genuinely blocks for that
/// long, the same fixture shape `dispatch.rs`'s
/// `concurrent_issue_dispatches_do_not_serialize_on_the_account_selection_poll`
/// test uses at the `SweepRegistry` layer. Registers `dir`'s path as the
/// sole entry in a temp workspace registry (via `seed_temp_registry`, the
/// caller's job — kept out of this helper so the returned guard's
/// lifetime is the caller's to manage) is NOT done here; see call sites.
fn slow_poll_sweep_registry_in_tempdir(
    poll_delay: Duration,
) -> (Arc<Mutex<SweepRegistry>>, tempfile::TempDir) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempdir().unwrap();
    let scripts_dir = dir.path().join(".loom").join("scripts");
    std::fs::create_dir_all(&scripts_dir).unwrap();
    let fake_bin = scripts_dir.join("spawn-claude.sh");
    let script = format!(
        "#!/usr/bin/env bash\nset -euo pipefail\nsleep {:.2}\n\
             echo \"spawn-claude: using OAuth account 'agent-ipc-burst' (mode=random)\" >&2\n\
             sleep 5\n",
        poll_delay.as_secs_f64()
    );
    std::fs::write(&fake_bin, script).unwrap();
    let mut perms = std::fs::metadata(&fake_bin).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_bin, perms).unwrap();

    let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
    config.spawn_bin = Some(fake_bin);
    config.skip_label_flip = true;
    config.journal_path = Some(dir.path().join("test-sweeps-journal.json"));
    let sr = Arc::new(Mutex::new(SweepRegistry::new(config)));
    (sr, dir)
}

/// Multiplier for a wall-clock test bound, widened on a **shared** or
/// CPU-quota-throttled host (#6625).
///
/// A bound tuned on a dedicated GitHub-hosted VM measures the host as much
/// as the code when the same suite runs in a 4-core container slot on a
/// busy 32-core machine: every scheduling decision the test depends on
/// competes with other tenants. Rather than loosening the bound
/// unconditionally — which would blind the test on the idle hosts where it
/// is most able to catch a real regression — the tolerance is derived from
/// the same host-load signal production code already scales its IPC
/// budgets by ([`crate::cpu_headroom::load_per_core`], the rule behind
/// `cli::status::scale_timeout_for_load`).
///
/// `1.0` (no widening, the original strict bound) on any host at or below
/// one runnable task per core, including every GitHub-hosted runner and a
/// developer laptop. Above that, proportional to observed load-per-core
/// and capped at `4.0` so a pathological reading cannot widen a bound
/// without limit. Callers MUST additionally cap the widened bound below
/// whatever value would make their assertion vacuous — see the call site.
fn shared_host_timing_tolerance() -> f64 {
    timing_tolerance_from_load(crate::cpu_headroom::load_per_core())
}

/// Pure core of [`shared_host_timing_tolerance`] — the live
/// `/proc/loadavg` read stays in the wrapper so the widening rule itself
/// is deterministically testable (see
/// `timing_tolerance_is_neutral_on_an_unloaded_host`).
fn timing_tolerance_from_load(load_per_core: Option<f64>) -> f64 {
    match load_per_core {
        Some(lpc) if lpc.is_finite() && lpc > 1.0 => lpc.min(4.0),
        _ => 1.0,
    }
}

/// #6625: the widening must be a no-op on the hosts where the strict
/// bound is trustworthy (idle laptop, GitHub-hosted runner, or any host
/// with no load reading at all), so this tolerance can never quietly
/// blunt the starvation assertion where it is most able to catch a real
/// regression.
#[test]
fn timing_tolerance_is_neutral_on_an_unloaded_host() {
    assert!((timing_tolerance_from_load(None) - 1.0).abs() < f64::EPSILON);
    assert!((timing_tolerance_from_load(Some(0.0)) - 1.0).abs() < f64::EPSILON);
    assert!((timing_tolerance_from_load(Some(1.0)) - 1.0).abs() < f64::EPSILON);
    assert!((timing_tolerance_from_load(Some(f64::NAN)) - 1.0).abs() < f64::EPSILON);
}

/// #6625: on a shared/throttled host the tolerance grows with load but is
/// capped, so a pathological reading cannot widen a bound without limit.
/// `7.5` is the load-per-core the fleet's 4-core CI slot actually read
/// from the 32-core host's un-namespaced `/proc/loadavg`.
#[test]
fn timing_tolerance_widens_under_load_but_is_capped() {
    assert!((timing_tolerance_from_load(Some(2.5)) - 2.5).abs() < f64::EPSILON);
    assert!((timing_tolerance_from_load(Some(7.5)) - 4.0).abs() < f64::EPSILON);
    assert!((timing_tolerance_from_load(Some(1_000.0)) - 4.0).abs() < f64::EPSILON);
}

/// Supplementary widening signal for
/// [`list_sweeps_is_not_starved_behind_a_concurrent_dispatch_burst`]
/// (#7025). `shared_host_timing_tolerance`'s 1-minute load average is too
/// slow to catch contention confined to this specific test's own
/// multi-second execution window — measured directly (#7025): driving all
/// cores to 100% for ~4s on an idle 8-core host moved the reported
/// 1-minute `load_per_core` by only hundredths, nowhere near the `> 1.0`
/// widening threshold `timing_tolerance_from_load` requires. This takes a
/// CPU-busy fraction **bracketed to exactly the operation's own window**
/// (two `/proc/stat` snapshots, before and after — see
/// [`crate::cpu_headroom::sample_proc_stat_cpu`]) instead of a smoothed
/// system-wide history, so it reacts within the test's own timeframe
/// rather than lagging a full sampling interval behind it.
///
/// Neutral (`1.0`) at/below `0.9` busy — ordinary, expected `cargo test`
/// parallelism routinely saturates a few cores without indicating the
/// kind of host-wide contention this test's timing bound needs slack for.
/// Above `0.9`, ramps linearly to the same `4.0` cap
/// `timing_tolerance_from_load` uses, for the same reason: a pathological
/// reading must not widen the bound without limit.
fn timing_tolerance_from_busy_fraction(busy_fraction: Option<f64>) -> f64 {
    match busy_fraction {
        Some(bf) if bf.is_finite() && bf > 0.9 => (1.0 + (bf - 0.9) * 30.0).min(4.0),
        _ => 1.0,
    }
}

/// #7025: neutral below the saturation threshold (including no reading
/// at all), matching `timing_tolerance_from_load`'s no-quiet-blunting
/// guarantee for the same reason — see
/// `timing_tolerance_is_neutral_on_an_unloaded_host`.
#[test]
fn timing_tolerance_from_busy_fraction_is_neutral_below_threshold() {
    assert!((timing_tolerance_from_busy_fraction(None) - 1.0).abs() < f64::EPSILON);
    assert!((timing_tolerance_from_busy_fraction(Some(0.0)) - 1.0).abs() < f64::EPSILON);
    assert!((timing_tolerance_from_busy_fraction(Some(0.9)) - 1.0).abs() < f64::EPSILON);
    assert!((timing_tolerance_from_busy_fraction(Some(f64::NAN)) - 1.0).abs() < f64::EPSILON);
}

/// #7025: widens near full saturation but is capped at the same `4.0`
/// ceiling `timing_tolerance_from_load` uses.
#[test]
fn timing_tolerance_from_busy_fraction_widens_near_saturation_but_is_capped() {
    // `1e-9`, not `f64::EPSILON`: `(0.95 - 0.9) * 30.0` is not bit-exact
    // (unlike the other tolerance tests' inputs, which pass straight
    // through `min`/no-op arithmetic).
    assert!((timing_tolerance_from_busy_fraction(Some(0.95)) - 2.5).abs() < 1e-9);
    assert!((timing_tolerance_from_busy_fraction(Some(1.0)) - 4.0).abs() < 1e-9);
    assert!((timing_tolerance_from_busy_fraction(Some(1_000.0)) - 4.0).abs() < 1e-9);
}

/// Issue #6592, AC2: a burst of 10+ concurrent `dispatch_sweep` calls
/// must all ack well under the client's 30s deadline. Drives the actual
/// IPC-layer entry point (`dispatch_sweep_nonblocking`, what
/// `handle_client` calls for a real `DispatchSweep` request) concurrently
/// via `tokio::spawn`, against a fixture whose spawn script blocks the
/// account-selection poll for `POLL_DELAY` — proving the burst does not
/// serialize behind the registry mutex (which would take
/// `BURST * POLL_DELAY`, ~7s for 10x700ms, dwarfed by 30s only by
/// coincidence of this test's chosen delay — the point is the burst
/// completes in close to ONE delay, not N).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn dispatch_sweep_nonblocking_burst_acks_well_under_the_client_deadline() {
    const BURST: u32 = 10;
    let poll_delay = Duration::from_millis(700);
    let (sr, dir) = slow_poll_sweep_registry_in_tempdir(poll_delay);
    let _guard = seed_temp_registry(&[dir.path()]);
    let bus = Arc::new(EventBus::new());
    let pool = Arc::new(WorkspacePool::new(bus.clone(), test_runtime_handle()));

    let start = std::time::Instant::now();
    let mut handles = Vec::new();
    for i in 0..BURST {
        let sr = sr.clone();
        let bus = bus.clone();
        let pool = pool.clone();
        handles.push(tokio::spawn(async move {
            dispatch_sweep_nonblocking(
                &sr,
                &pool,
                &bus,
                SweepKind::Issue(83_000 + i),
                None,
                None,
                None,
                None,
                None,
                false,
            )
            .await
        }));
    }
    let mut sweep_ids = Vec::new();
    for h in handles {
        match h.await.expect("dispatch task panicked") {
            Response::SweepDispatched { sweep_id, .. } => sweep_ids.push(sweep_id),
            other => panic!("Expected SweepDispatched, got: {other:?}"),
        }
    }
    let elapsed = start.elapsed();
    assert_eq!(sweep_ids.len(), BURST as usize);

    let serialized_bound = poll_delay * BURST;
    assert!(
        elapsed < serialized_bound / 2,
        "burst of {BURST} concurrent dispatch_sweep calls took {elapsed:?} — looks \
             serialized behind the registry mutex (serialized bound ~{serialized_bound:?})"
    );
    assert!(
        elapsed < Duration::from_secs(30),
        "burst took {elapsed:?}, at or over the 30s client ack deadline (AC2)"
    );

    for id in &sweep_ids {
        let mut sr = sr.lock().unwrap();
        let _ = sr.cancel(id, Duration::from_millis(50));
    }
}

/// Issue #6592, AC1/AC2's second half: a `ListSweeps` request issued
/// WHILE a `DispatchSweep` burst is in flight must not be starved behind
/// it. Runs `ListSweeps` (via the ordinary synchronous `handle_request`,
/// on a `spawn_blocking` thread — exactly how it reaches the registry
/// mutex in production) concurrently with the same burst as the test
/// above, and asserts it returns quickly rather than waiting out the
/// whole burst.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn list_sweeps_is_not_starved_behind_a_concurrent_dispatch_burst() {
    const BURST: u32 = 10;
    let poll_delay = Duration::from_millis(700);
    let (sr, dir) = slow_poll_sweep_registry_in_tempdir(poll_delay);
    let _guard = seed_temp_registry(&[dir.path()]);
    let bus = Arc::new(EventBus::new());
    let pool = Arc::new(WorkspacePool::new(bus.clone(), test_runtime_handle()));

    // #7307: build the two INERT `handle_request` arguments up front —
    // before the burst is spawned, before the busy-fraction window opens,
    // and (critically) before the timed window below.
    //
    // `handle_request`'s `ListSweeps` arm never reads the terminal manager
    // or the activity DB; they are here only to satisfy the signature. But
    // `ActivityDb::new` is not a free constructor: it creates a fresh
    // SQLite file and executes the whole schema DDL (~57 `CREATE TABLE` /
    // `CREATE INDEX` statements) against it. Measured with the constructor
    // still inside the timed region on an *idle* 28-core laptop, it was
    // **262ms of a 317ms window** — 83% of what this test was reporting as
    // "ListSweeps latency", with `handle_request` itself accounting for
    // only 44ms.
    //
    // That cost is disk-I/O bound, and I/O wait registers as neither
    // runnable-task load nor CPU-busy time — so it is structurally
    // invisible to BOTH tolerance signals below. That is exactly the shape
    // of the #7307 recurrence: 4.49s wall clock at `load-per-core 0.83` and
    // `busy-fraction 0.28`, i.e. a host that was not CPU-contended at all.
    // Widening the tolerance a third time would have papered over a
    // measurement defect in the test harness rather than the registry-mutex
    // path this test exists to guard. Hoisting the construction out of the
    // window removes the I/O from the measurement instead.
    //
    // Binding the `TempDir` to a named local also fixes a latent bug in the
    // previous inline form: `tempdir().unwrap().path().join(..)` produced a
    // temporary whose `Drop` deleted the directory out from under the open
    // SQLite connection at the end of the enclosing statement.
    let terminals = Arc::new(Mutex::new(TerminalManager::new()));
    let activity_dir = tempdir().unwrap();
    let activity_db = Arc::new(Mutex::new(
        ActivityDb::new(activity_dir.path().join("list-sweeps-activity.db")).unwrap(),
    ));

    // #7025: bracket the contended window with two un-memoized
    // `/proc/stat` snapshots (Linux only — see
    // `crate::cpu_headroom::sample_proc_stat_cpu`), so the busy-fraction
    // signal below covers exactly this test's own execution window
    // rather than lagging behind it the way the 1-minute load average
    // does.
    #[cfg(target_os = "linux")]
    let cpu_before = crate::cpu_headroom::sample_proc_stat_cpu();

    let mut handles = Vec::new();
    for i in 0..BURST {
        let sr = sr.clone();
        let bus = bus.clone();
        let pool = pool.clone();
        handles.push(tokio::spawn(async move {
            dispatch_sweep_nonblocking(
                &sr,
                &pool,
                &bus,
                SweepKind::Issue(84_000 + i),
                None,
                None,
                None,
                None,
                None,
                false,
            )
            .await
        }));
    }

    // Give the burst a moment to actually acquire the registry mutex at
    // least once (begin_issue_dispatch's lock-scoped phase), so this
    // `ListSweeps` genuinely races a live burst rather than running
    // before it starts.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let list_start = std::time::Instant::now();
    let sr_for_list = sr.clone();
    let bus_for_list = bus.clone();
    let pool_for_list = pool.clone();
    // `request_elapsed` brackets ONLY `handle_request` itself; the outer
    // `list_elapsed` additionally covers `spawn_blocking` dispatch and the
    // `.await` resumption (both genuinely part of "was this request
    // starved"). Reporting the split is what makes a future failure
    // diagnosable: #7307 was mis-triaged twice as CPU contention precisely
    // because the failure message showed one opaque wall-clock number.
    let (list_response, request_elapsed) = tokio::task::spawn_blocking(move || {
        let request_start = std::time::Instant::now();
        let response = handle_request(
            Request::ListSweeps {
                state_filter: None,
                workspace_root: None,
                all_workspaces: false,
            },
            &terminals,
            &activity_db,
            &sr_for_list,
            &bus_for_list,
            &pool_for_list,
        );
        (response, request_start.elapsed())
    })
    .await
    .expect("ListSweeps task panicked");
    let list_elapsed = list_start.elapsed();
    assert!(
        matches!(list_response, Response::SweepList { .. }),
        "expected SweepList, got: {list_response:?}"
    );

    // Well under the burst's serialized-would-be duration (~7s for
    // 10x700ms) — a starved ListSweeps would take close to that; a
    // healthy one returns in low milliseconds regardless of the burst.
    //
    // The bound is half that serialized duration on an unloaded host (the
    // original assertion, unchanged where it can be trusted), widened in
    // proportion to observed host load on a shared or CPU-quota-throttled
    // one (#6625): the fleet's self-hosted runner gives each job 4 cores
    // on a busy 32-core host, where a healthy-but-descheduled ListSweeps
    // measured 4.41s and turned this into a false RED for #3974's
    // CI-corroboration backstop. The widened bound is hard-capped at 90%
    // of the full serialized duration so it can never become vacuous —
    // a genuinely starved ListSweeps waits out the *whole* burst, so it
    // still fails this assertion no matter how loaded the host is.
    //
    // #7025: `shared_host_timing_tolerance` alone recurred as a false RED
    // at `load-per-core 0.99` — just under its `> 1.0` widening
    // threshold, and measurably too slow (a 1-minute decaying average) to
    // register contention confined to this test's own multi-second
    // window. A second, faster-reacting signal — CPU-busy fraction
    // bracketed to exactly this test's own window via two `/proc/stat`
    // snapshots (Linux only) — supplements it; the wider of the two
    // tolerances wins, so either a sustained host-wide load *or* a burst
    // confined to this test's own timeframe can widen the bound, and
    // absence of either signal (non-Linux, or no `/proc/loadavg`) stays
    // neutral (`1.0`, the original strict bound) exactly as before.
    //
    // #7307: the bound and both tolerance signals are DELIBERATELY
    // UNCHANGED by that issue's fix. Its third recurrence was not a
    // too-tight bound — it was a too-wide measurement: 83% of the timed
    // window was the harness's own `ActivityDb` schema-init disk I/O (see
    // the hoist at the top of this test), which no CPU-derived tolerance
    // can ever observe. Removing that I/O from the window is what fixes
    // it; widening a CPU tolerance a third time would not have.
    #[cfg(target_os = "linux")]
    let busy_fraction = crate::cpu_headroom::sample_proc_stat_cpu()
        .zip(cpu_before)
        .and_then(|(cur, prev)| cur.idle_fraction_since(&prev))
        .map(|idle_fraction| 1.0 - idle_fraction);
    #[cfg(not(target_os = "linux"))]
    let busy_fraction: Option<f64> = None;

    let strict_bound = poll_delay * BURST / 2;
    let vacuity_cap = poll_delay * BURST * 9 / 10;
    let tolerance =
        shared_host_timing_tolerance().max(timing_tolerance_from_busy_fraction(busy_fraction));
    let bound = strict_bound.mul_f64(tolerance).min(vacuity_cap);
    assert!(
        list_elapsed < bound,
        "ListSweeps took {list_elapsed:?} while a dispatch_sweep burst was in flight \
             (bound {bound:?}, of which handle_request itself was {request_elapsed:?} and \
             {scheduling_overhead:?} was spawn_blocking dispatch + await resumption; \
             load-per-core {:?}, busy-fraction {busy_fraction:?}) — looks starved behind the \
             registry mutex",
        crate::cpu_headroom::load_per_core(),
        scheduling_overhead = list_elapsed.saturating_sub(request_elapsed),
    );

    for h in handles {
        if let Ok(Response::SweepDispatched { sweep_id, .. }) = h.await {
            let mut sr = sr.lock().unwrap();
            let _ = sr.cancel(&sweep_id, Duration::from_millis(50));
        }
    }
}

/// #7307: the executable rationale for hoisting the inert `handle_request`
/// arguments out of
/// [`list_sweeps_is_not_starved_behind_a_concurrent_dispatch_burst`]'s
/// timed window.
///
/// `ActivityDb::new` reads like a cheap in-memory constructor at the call
/// site — which is why it sat inside that window for three flake cycles
/// (#6625, #7025, #7307) while two successive CPU-derived tolerance
/// widenings failed to explain the failures. It is not cheap: it
/// materializes a real SQLite database on disk and runs the full schema
/// DDL against it, so its cost is disk-I/O bound and therefore invisible to
/// every CPU-load / CPU-busy signal a timing test could consult.
///
/// Asserted here as a durable property rather than a duration: a duration
/// assertion would be its own flake. If a future refactor makes this
/// constructor genuinely free (in-memory), this test fails and whoever
/// changes it can re-evaluate the hoist — but until then, no timing window
/// anywhere may enclose it.
#[test]
fn activity_db_construction_materializes_a_real_on_disk_database() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("activity-cost.db");
    let db = ActivityDb::new(db_path.clone()).unwrap();
    drop(db);

    let size = std::fs::metadata(&db_path)
        .expect("ActivityDb::new must create its database file")
        .len();
    assert!(
        size > 0,
        "ActivityDb::new wrote a zero-length file — expected the schema DDL to have been \
             materialized on disk. If this constructor became free, revisit the #7307 hoist in \
             list_sweeps_is_not_starved_behind_a_concurrent_dispatch_burst."
    );
}

// ===== DispatchSweep serde compat (Issue #3477, Phase 1) =====

/// A wire payload WITHOUT the `model` field (the pre-#3477 client shape)
/// must deserialize with `model == None` — `#[serde(default)]` keeps
/// existing clients compatible.
#[test]
fn test_dispatch_sweep_deserializes_without_model_field() {
    let json = r#"{"type":"DispatchSweep","payload":{"kind":{"type":"Issue","value":42},"idempotency_key":null}}"#;
    let request: Request = serde_json::from_str(json).expect("pre-#3477 payload must parse");
    match request {
        Request::DispatchSweep {
            kind,
            idempotency_key,
            model,
            effort,
            depends_on: _,
            workspace_root: _,
            force: _,
        } => {
            assert!(matches!(kind, SweepKind::Issue(42)));
            assert!(idempotency_key.is_none());
            assert!(model.is_none(), "absent model field must default to None");
            assert!(effort.is_none(), "absent effort field must default to None");
        }
        other => panic!("Expected DispatchSweep, got: {other:?}"),
    }
}

#[test]
fn test_dispatch_sweep_serde_round_trip_with_model() {
    let request = Request::DispatchSweep {
        kind: SweepKind::Issue(7),
        idempotency_key: Some("key-B".to_string()),
        model: Some("claude-sonnet-4-6".to_string()),
        effort: None,
        depends_on: None,
        workspace_root: None,
        force: false,
    };
    let json = serde_json::to_string(&request).expect("serialize");
    let back: Request = serde_json::from_str(&json).expect("deserialize");
    match back {
        Request::DispatchSweep {
            kind,
            idempotency_key,
            model,
            effort,
            depends_on: _,
            workspace_root: _,
            force: _,
        } => {
            assert!(matches!(kind, SweepKind::Issue(7)));
            assert_eq!(idempotency_key.as_deref(), Some("key-B"));
            assert_eq!(model.as_deref(), Some("claude-sonnet-4-6"));
            assert!(effort.is_none());
        }
        other => panic!("Expected DispatchSweep, got: {other:?}"),
    }
}

#[test]
fn test_dispatch_sweep_serde_round_trip_without_model() {
    let request = Request::DispatchSweep {
        kind: SweepKind::Issue(8),
        idempotency_key: None,
        model: None,
        effort: None,
        depends_on: None,
        workspace_root: None,
        force: false,
    };
    let json = serde_json::to_string(&request).expect("serialize");
    let back: Request = serde_json::from_str(&json).expect("deserialize");
    match back {
        Request::DispatchSweep { model, .. } => assert!(model.is_none()),
        other => panic!("Expected DispatchSweep, got: {other:?}"),
    }
}

// ===== DispatchSweep serde compat for `effort` (Issue #3716) =====

/// A wire payload WITHOUT the `effort` field (the pre-#3716 client shape)
/// must deserialize with `effort == None` — `#[serde(default)]` keeps
/// existing clients compatible.
#[test]
fn test_dispatch_sweep_deserializes_without_effort_field() {
    let json = r#"{"type":"DispatchSweep","payload":{"kind":{"type":"Issue","value":42},"idempotency_key":null,"model":"claude-sonnet-4-6"}}"#;
    let request: Request = serde_json::from_str(json).expect("pre-#3716 payload must parse");
    match request {
        Request::DispatchSweep { model, effort, .. } => {
            assert_eq!(model.as_deref(), Some("claude-sonnet-4-6"));
            assert!(effort.is_none(), "absent effort field must default to None");
        }
        other => panic!("Expected DispatchSweep, got: {other:?}"),
    }
}

#[test]
fn test_dispatch_sweep_serde_round_trip_with_effort() {
    let request = Request::DispatchSweep {
        kind: SweepKind::Issue(9),
        idempotency_key: Some("key-E".to_string()),
        model: Some("claude-sonnet-4-6".to_string()),
        effort: Some("xhigh".to_string()),
        depends_on: None,
        workspace_root: None,
        force: false,
    };
    let json = serde_json::to_string(&request).expect("serialize");
    let back: Request = serde_json::from_str(&json).expect("deserialize");
    match back {
        Request::DispatchSweep { model, effort, .. } => {
            assert_eq!(model.as_deref(), Some("claude-sonnet-4-6"));
            assert_eq!(effort.as_deref(), Some("xhigh"));
        }
        other => panic!("Expected DispatchSweep, got: {other:?}"),
    }
}

#[test]
fn test_dispatch_sweep_serde_round_trip_with_empty_effort() {
    let request = Request::DispatchSweep {
        kind: SweepKind::Issue(10),
        idempotency_key: None,
        model: None,
        effort: Some(String::new()),
        depends_on: None,
        workspace_root: None,
        force: false,
    };
    let json = serde_json::to_string(&request).expect("serialize");
    let back: Request = serde_json::from_str(&json).expect("deserialize");
    match back {
        // Empty string round-trips as-is at the wire layer; normalization
        // to None happens spawn-side (registry) exactly like `model`.
        Request::DispatchSweep { effort, .. } => {
            assert_eq!(effort.as_deref(), Some(""));
        }
        other => panic!("Expected DispatchSweep, got: {other:?}"),
    }
}

// ===== DispatchSweep serde compat for `depends_on` (Issue #3729) =====

/// A wire payload WITHOUT the `depends_on` field (the pre-#3729 client
/// shape) must deserialize with `depends_on == None` — `#[serde(default)]`
/// keeps existing clients compatible.
#[test]
fn test_dispatch_sweep_deserializes_without_depends_on_field() {
    let json = r#"{"type":"DispatchSweep","payload":{"kind":{"type":"Issue","value":42},"idempotency_key":null,"model":"claude-sonnet-4-6","effort":"xhigh"}}"#;
    let request: Request = serde_json::from_str(json).expect("pre-#3729 payload must parse");
    match request {
        Request::DispatchSweep { depends_on, .. } => {
            assert!(depends_on.is_none(), "absent depends_on must default to None");
        }
        other => panic!("Expected DispatchSweep, got: {other:?}"),
    }
}

#[test]
fn test_dispatch_sweep_serde_round_trip_with_depends_on() {
    let request = Request::DispatchSweep {
        kind: SweepKind::Issue(3725),
        idempotency_key: None,
        model: None,
        effort: None,
        depends_on: Some(3726),
        workspace_root: None,
        force: false,
    };
    let json = serde_json::to_string(&request).expect("serialize");
    let back: Request = serde_json::from_str(&json).expect("deserialize");
    match back {
        Request::DispatchSweep { depends_on, .. } => {
            assert_eq!(depends_on, Some(3726));
        }
        other => panic!("Expected DispatchSweep, got: {other:?}"),
    }
}

// ===== Event bus IPC handlers (Issue #3453, Phase B) =====

#[tokio::test]
async fn test_handle_request_publish_event_routes_to_subscribers() {
    let (tm, db, sr, bus) = setup_test_context();
    let mut sub = bus.subscribe::<[&str; 0], &str>([]);

    let response = handle_request(
        Request::PublishEvent {
            topic: "sweep.issue.123.phase".to_string(),
            payload: serde_json::json!({"phase": "builder"}),
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );

    match response {
        Response::EventPublished { topic, receivers } => {
            assert_eq!(topic, "sweep.issue.123.phase");
            assert!(receivers >= 1, "expected at least 1 receiver; got {receivers}");
        }
        other => panic!("Expected EventPublished, got: {other:?}"),
    }

    // Issue #4466: the documented child-published `sweep.issue.{N}.phase`
    // topic is upgraded to the typed `Event::SweepPhase` variant (was
    // previously delivered as `Event::Generic`, which the narration sink
    // never narrated).
    let ev = sub.recv().await.unwrap();
    match ev {
        Event::SweepPhase {
            issue,
            phase,
            pr_number,
            repo,
        } => {
            assert_eq!(issue, 123);
            assert_eq!(phase, "builder");
            assert_eq!(pr_number, None);
            assert_eq!(repo, None);
        }
        other => panic!("Expected SweepPhase event, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_handle_request_publish_event_upgrades_blocker_topic() {
    // Issue #4466: `sweep.issue.{N}.blocker` upgrades to `Event::SweepBlocker`
    // with the full documented payload (incl. the optional `repo`).
    let (tm, db, sr, bus) = setup_test_context();
    let mut sub = bus.subscribe::<[&str; 0], &str>([]);

    handle_request(
        Request::PublishEvent {
            topic: "sweep.issue.456.blocker".to_string(),
            payload: serde_json::json!({
                "reason": "needs human decision",
                "label_added": "loom:operator-only",
                "repo": "/work/loom",
            }),
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );

    match sub.recv().await.unwrap() {
        Event::SweepBlocker {
            issue,
            reason,
            label_added,
            repo,
        } => {
            assert_eq!(issue, 456);
            assert_eq!(reason, "needs human decision");
            assert_eq!(label_added, "loom:operator-only");
            assert_eq!(repo.as_deref(), Some("/work/loom"));
        }
        other => panic!("Expected SweepBlocker event, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_handle_request_publish_event_phase_carries_pr_and_repo() {
    // Issue #4466: the optional `pr_number` + `repo` fields survive the
    // typed upgrade (the phase narration line appends ` · PR #M open`).
    let (tm, db, sr, bus) = setup_test_context();
    let mut sub = bus.subscribe::<[&str; 0], &str>([]);

    handle_request(
        Request::PublishEvent {
            topic: "sweep.issue.789.phase".to_string(),
            payload: serde_json::json!({
                "phase": "judge",
                "pr_number": 501,
                "repo": "/work/loom",
            }),
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );

    match sub.recv().await.unwrap() {
        Event::SweepPhase {
            issue,
            phase,
            pr_number,
            repo,
        } => {
            assert_eq!(issue, 789);
            assert_eq!(phase, "judge");
            assert_eq!(pr_number, Some(501));
            assert_eq!(repo.as_deref(), Some("/work/loom"));
        }
        other => panic!("Expected SweepPhase event, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_handle_request_publish_event_malformed_and_unknown_stay_generic() {
    // Issue #4466: publish is fire-and-forget advisory — a malformed
    // payload (missing required `phase`), an unknown sweep sub-topic, a
    // non-integer issue segment, and an entirely unrelated topic all stay
    // `Event::Generic` with the payload passed through UNCHANGED.
    let (tm, db, sr, bus) = setup_test_context();

    let cases: &[(&str, serde_json::Value)] = &[
        // Documented topic, but the required `phase` field is missing.
        ("sweep.issue.123.phase", serde_json::json!({"pr_number": 5})),
        // Documented blocker topic, but `label_added` is missing.
        ("sweep.issue.123.blocker", serde_json::json!({"reason": "x"})),
        // Unknown sweep sub-topic.
        ("sweep.issue.123.other", serde_json::json!({"phase": "builder"})),
        // Non-integer issue segment.
        ("sweep.issue.abc.phase", serde_json::json!({"phase": "builder"})),
        // Entirely unrelated topic.
        ("custom.topic", serde_json::json!({"phase": "builder"})),
    ];

    for (topic, payload) in cases {
        let mut sub = bus.subscribe::<[&str; 0], &str>([]);
        handle_request(
            Request::PublishEvent {
                topic: (*topic).to_string(),
                payload: payload.clone(),
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
        match sub.recv().await.unwrap() {
            Event::Generic {
                topic: got_topic,
                payload: got_payload,
            } => {
                assert_eq!(&got_topic, topic, "topic preserved for {topic}");
                assert_eq!(&got_payload, payload, "payload passed through for {topic}");
            }
            other => panic!("Expected Generic event for {topic}, got: {other:?}"),
        }
    }
}

// ===== Sweep monitoring IPC handlers (Issue #3455, Phase C) =====

#[test]
fn test_handle_request_get_sweep_status_missing() {
    let (tm, db, _, bus) = setup_test_context();
    let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
    let response = handle_request(
        Request::GetSweepStatus {
            sweep_id: "no-such-sweep".to_string(),
            workspace_root: None,
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    match response {
        Response::SweepStatus { info } => assert!(info.is_none()),
        other => panic!("Expected SweepStatus, got: {other:?}"),
    }
}

#[test]
fn test_handle_request_tail_sweep_log_missing_sweep_returns_error() {
    let (tm, db, _, bus) = setup_test_context();
    let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
    let response = handle_request(
        Request::TailSweepLog {
            sweep_id: "no-such-sweep".to_string(),
            lines: 10,
            workspace_root: None,
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    match response {
        Response::Error { message } => {
            assert!(
                message.contains("unknown sweep_id"),
                "expected unknown sweep_id; got: {message}"
            );
        }
        other => panic!("Expected Error, got: {other:?}"),
    }
}

#[test]
fn test_handle_request_clear_quarantine_noop() {
    // Issue #3939/#3960: clearing an issue that is not quarantined is an
    // idempotent no-op success routed through the full IPC dispatcher.
    let (tm, db, _, bus) = setup_test_context();
    let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
    let response = handle_request(
        Request::ClearQuarantine {
            issue: 4242,
            workspace_root: None,
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    match response {
        Response::QuarantineCleared {
            issue,
            was_quarantined,
        } => {
            assert_eq!(issue, 4242);
            assert!(!was_quarantined, "no entry existed -> false");
        }
        other => panic!("Expected QuarantineCleared, got: {other:?}"),
    }
}

#[test]
fn test_handle_request_clear_quarantine_clears_existing() {
    // Seed a quarantine directly, then clear it via the IPC dispatcher and
    // assert the in-memory state was released (was_quarantined: true).
    let (tm, db, _, bus) = setup_test_context();
    let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
    {
        let mut reg = sr.lock().unwrap();
        reg.seed_quarantine_for_test(808);
        assert!(reg.is_quarantined(808));
    }
    let response = handle_request(
        Request::ClearQuarantine {
            issue: 808,
            workspace_root: None,
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    match response {
        Response::QuarantineCleared {
            issue,
            was_quarantined,
        } => {
            assert_eq!(issue, 808);
            assert!(was_quarantined, "seeded entry existed -> true");
        }
        other => panic!("Expected QuarantineCleared, got: {other:?}"),
    }
    assert!(!sr.lock().unwrap().is_quarantined(808));
}

// ===== RecordDispatchFailure (Issue #6192) =====

#[test]
fn test_handle_request_record_dispatch_failure_arms_backoff() {
    // Issue #6192: a build-gate step timeout (or any other script-side
    // caller with no direct registry access) records a failed dispatch
    // via IPC and gets back the resulting consecutive count + window,
    // mirroring the reaper's own automatic bookkeeping (#4485).
    let (tm, db, _, bus) = setup_test_context();
    let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
    let response = handle_request(
        Request::RecordDispatchFailure {
            issue: 6192,
            reason: Some("build-gate timeout: cargo test (1800s elapsed)".to_string()),
            workspace_root: None,
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    match response {
        Response::DispatchFailureRecorded {
            issue,
            consecutive,
            backoff_secs,
        } => {
            assert_eq!(issue, 6192);
            assert_eq!(consecutive, 1);
            assert!(
                backoff_secs.is_some_and(|s| s > 0),
                "expected a positive backoff window (default config is enabled), got: \
                     {backoff_secs:?}"
            );
        }
        other => panic!("Expected DispatchFailureRecorded, got: {other:?}"),
    }
    assert_eq!(sr.lock().unwrap().dispatch_failure_count(6192), 1);
}

#[test]
fn test_handle_request_record_dispatch_failure_accumulates_consecutive() {
    // Two calls for the same issue accumulate — mirrors the reaper
    // calling `record_dispatch_failure` on repeated failed dispatches.
    let (tm, db, _, bus) = setup_test_context();
    let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
    for _ in 0..2 {
        handle_request(
            Request::RecordDispatchFailure {
                issue: 6193,
                reason: None,
                workspace_root: None,
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
    }
    let response = handle_request(
        Request::RecordDispatchFailure {
            issue: 6193,
            reason: None,
            workspace_root: None,
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    match response {
        Response::DispatchFailureRecorded { consecutive, .. } => {
            assert_eq!(consecutive, 3, "three calls -> three consecutive failures");
        }
        other => panic!("Expected DispatchFailureRecorded, got: {other:?}"),
    }
}

#[test]
fn test_handle_request_record_dispatch_failure_disabled_is_noop() {
    // A repo/operator with the backoff mechanism disabled gets an
    // idempotent no-op: `consecutive` stays 0 and `backoff_secs` is None,
    // never a hard error — mirrors `record_dispatch_failure`'s own early
    // return when `dispatch_backoff_config.enabled` is false.
    let (tm, db, _, bus) = setup_test_context();
    let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
    {
        let mut reg = sr.lock().unwrap();
        let mut cfg = reg.dispatch_backoff_config();
        cfg.enabled = false;
        reg.set_dispatch_backoff_config(cfg);
    }
    let response = handle_request(
        Request::RecordDispatchFailure {
            issue: 6194,
            reason: None,
            workspace_root: None,
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    match response {
        Response::DispatchFailureRecorded {
            issue,
            consecutive,
            backoff_secs,
        } => {
            assert_eq!(issue, 6194);
            assert_eq!(consecutive, 0, "disabled backoff never records a state entry");
            assert!(backoff_secs.is_none(), "disabled backoff reports no window");
        }
        other => panic!("Expected DispatchFailureRecorded, got: {other:?}"),
    }
}

// ===== RecordNoopRelease (Issue #6670) =====

#[test]
fn test_handle_request_record_noop_release_arms_cooldown() {
    // Issue #6670: the `/loom:sweep` orchestrator (no direct registry
    // access) records a self-reported no-op release via IPC and gets back
    // the resulting consecutive count + window, mirroring
    // `RecordDispatchFailure`'s own contract.
    let (tm, db, _, bus) = setup_test_context();
    let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
    let response = handle_request(
        Request::RecordNoopRelease {
            issue: 6670,
            reason: Some("no update needed — diff touched only tooling-resync".to_string()),
            workspace_root: None,
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    match response {
        Response::NoopReleaseRecorded {
            issue,
            consecutive,
            cooldown_secs,
        } => {
            assert_eq!(issue, 6670);
            assert_eq!(consecutive, 1);
            assert!(
                cooldown_secs.is_some_and(|s| s > 0),
                "expected a positive cooldown window (default config is enabled), got: \
                     {cooldown_secs:?}"
            );
        }
        other => panic!("Expected NoopReleaseRecorded, got: {other:?}"),
    }
    assert_eq!(sr.lock().unwrap().noop_release_count(6670), 1);
}

#[test]
fn test_handle_request_record_noop_release_accumulates_consecutive() {
    // Two calls for the same issue accumulate — mirrors the sweep
    // orchestrator reporting "still nothing to do" on repeated passes.
    let (tm, db, _, bus) = setup_test_context();
    let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
    for _ in 0..2 {
        handle_request(
            Request::RecordNoopRelease {
                issue: 6671,
                reason: None,
                workspace_root: None,
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
    }
    let response = handle_request(
        Request::RecordNoopRelease {
            issue: 6671,
            reason: None,
            workspace_root: None,
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    match response {
        Response::NoopReleaseRecorded { consecutive, .. } => {
            assert_eq!(consecutive, 3, "three calls -> three consecutive no-op releases");
        }
        other => panic!("Expected NoopReleaseRecorded, got: {other:?}"),
    }
}

#[test]
fn test_handle_request_record_noop_release_disabled_is_noop() {
    // A repo/operator with the cooldown mechanism disabled gets an
    // idempotent no-op: `consecutive` stays 0 and `cooldown_secs` is None,
    // never a hard error — mirrors `record_noop_release`'s own early
    // return when `noop_cooldown_config.enabled` is false.
    let (tm, db, _, bus) = setup_test_context();
    let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
    {
        let mut reg = sr.lock().unwrap();
        let mut cfg = reg.noop_cooldown_config();
        cfg.enabled = false;
        reg.set_noop_cooldown_config(cfg);
    }
    let response = handle_request(
        Request::RecordNoopRelease {
            issue: 6672,
            reason: None,
            workspace_root: None,
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    match response {
        Response::NoopReleaseRecorded {
            issue,
            consecutive,
            cooldown_secs,
        } => {
            assert_eq!(issue, 6672);
            assert_eq!(consecutive, 0, "disabled cooldown never records a state entry");
            assert!(cooldown_secs.is_none(), "disabled cooldown reports no window");
        }
        other => panic!("Expected NoopReleaseRecorded, got: {other:?}"),
    }
}

/// Issue #6957: a `RecordNoopRelease` call carrying an explicit
/// `workspace_root` for a **non-default** registered workspace arms
/// THAT workspace's own registry — not the daemon's cwd-seeded default
/// registry — and the effect is visible via the exact read path the
/// epic supervisor's multi-workspace fan-out (#3928) and the
/// tick-based work finder both use to decide whether to re-offer a
/// candidate: [`SweepRegistry::noop_cooldown_issues`].
///
/// Root cause this guards against: `record-noop-release.sh` (the
/// `/loom:sweep` orchestrator's caller, see `sweep.md`) never used to
/// pass `--workspace-root` at all, so every no-op release recorded from
/// ANY repo landed in `resolve_registry`'s `None` fallback — the
/// daemon's single cwd-seeded "default" registry — regardless of which
/// repo the sweep was actually running against. On a multi-workspace
/// daemon (a fleet host managing more than one repo) that registry is
/// very unlikely to be the repo's OWN per-repo registry (obtained via
/// `WorkspacePool::get_or_provision`, the same call the epic
/// supervisor's dispatch guard uses), so the cooldown was armed
/// somewhere nobody ever reads it — producing an unbounded re-dispatch
/// loop on a candidate whose conclusion never changes. The fix lives in
/// `record-noop-release.sh` (auto-derives `--workspace-root` from the
/// caller's own repo root), but the daemon-side routing this test
/// exercises — `resolve_registry`'s `Some(root)` arm — was already
/// correct; this test locks that contract down explicitly, covering the
/// two-or-more-registered-workspaces shape the prior #6670/#6685/#6740
/// tests (single-workspace only) never exercised.
///
/// Also covers the report's specific `loom:epic-phase`
/// container/tracking-issue shape (#6957): the cooldown mechanism keys
/// purely on issue number, independent of any label state, so an issue
/// that only ever cycles `loom:issue`<->`loom:building` (never
/// `loom:blocked`) is exercised identically to any other candidate here
/// — there is no separate code path for it to fall through.
#[test]
#[serial_test::serial]
fn test_record_noop_release_routes_to_explicit_non_default_workspace() {
    let (tm, db, _, bus) = setup_test_context();

    // Default workspace (repo A, e.g. `rjwalters/loom`) and a second
    // managed repo (repo B, e.g. the live report's `2AMLogic/gf180-usb2-phy`)
    // — mirrors `test_sweep_requests_route_to_explicit_workspace_root`'s
    // multi-workspace fixture shape (#3929).
    let (sr_default, dir_a, _rec_a) = setup_sweep_registry_in_tempdir();
    let (sr_b, dir_b, _rec_b) = setup_sweep_registry_in_tempdir();
    let root_a = crate::workspace_registry::normalize_path(dir_a.path());
    let root_b = crate::workspace_registry::normalize_path(dir_b.path());

    let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
    pool.seed(root_a, sr_default.clone());
    pool.seed(root_b.clone(), sr_b.clone());

    // The `loom:epic-phase` tracking issue from the live report (#51 in
    // the reporting repo) — any issue number works since the cooldown
    // keys purely on the number, but a distinctive one documents intent.
    const EPIC_PHASE_ISSUE: u32 = 6951;

    // Record a no-op release for repo B's issue, using an EXPLICIT
    // `workspace_root` naming repo B — this is the call shape the fixed
    // `record-noop-release.sh` now produces automatically (it used to
    // never pass this at all, landing in the default registry below).
    let response = handle_request(
        Request::RecordNoopRelease {
            issue: EPIC_PHASE_ISSUE,
            reason: Some(
                "4/6 acceptance criteria merged, remaining 2 delegated to sub-issues, sole \
                     open sub-issue is loom:operator-only"
                    .to_string(),
            ),
            workspace_root: Some(dir_b.path().to_string_lossy().into_owned()),
        },
        &tm,
        &db,
        // Note: the handler's own `sweep_registry` param (`sr_default`)
        // is deliberately the DEFAULT registry here, exactly like every
        // production IPC call — a single daemon process shares one
        // `handle_request` call site across every repo it manages, and
        // `resolve_registry`'s explicit-workspace_root arm is what must
        // route away from it.
        &sr_default,
        &bus,
        &pool,
    );
    match response {
        Response::NoopReleaseRecorded {
            issue,
            consecutive,
            cooldown_secs,
        } => {
            assert_eq!(issue, EPIC_PHASE_ISSUE);
            assert_eq!(consecutive, 1);
            assert!(cooldown_secs.is_some_and(|s| s > 0));
        }
        other => panic!("Expected NoopReleaseRecorded, got: {other:?}"),
    }

    // The CORRECT registry (repo B's own) sees the cooldown armed — this
    // is exactly `SweepRegistry::noop_cooldown_issues`, the read path
    // both the epic supervisor's per-repo dispatch guard
    // (`SweepRegistry::dispatch`, step 2.75) and the tick-based work
    // finder (`WorkDispatcher::noop_cooldown`) call against a repo's own
    // registry.
    let now = Utc::now();
    assert!(
        sr_b.lock()
            .unwrap()
            .noop_cooldown_issues(now)
            .contains(&EPIC_PHASE_ISSUE),
        "repo B's own registry must see the armed cooldown for its issue"
    );
    assert_eq!(sr_b.lock().unwrap().noop_release_count(EPIC_PHASE_ISSUE), 1);

    // The WRONG registry (the daemon's cwd-seeded default, repo A) must
    // NOT see it — this is the exact failure mode #6957 reports: a
    // no-op recorded "somewhere" that repo B's own dispatch guard never
    // reads, so the very next dispatch re-offers the same candidate.
    assert!(
        !sr_default
            .lock()
            .unwrap()
            .noop_cooldown_issues(now)
            .contains(&EPIC_PHASE_ISSUE),
        "the default (wrong) registry must NOT see repo B's cooldown"
    );
    assert_eq!(
        sr_default
            .lock()
            .unwrap()
            .noop_release_count(EPIC_PHASE_ISSUE),
        0
    );
}

/// Issue #6957 (edge case from the acceptance criteria): a
/// single-workspace daemon must be completely unaffected by the
/// multi-workspace routing fix above — the same issue number, recorded
/// with `workspace_root: None` (the pre-#6957 call shape record-noop-
/// release.sh used to always produce), must still land in — and only
/// in — the one registry that exists. This locks down the existing
/// #6670/#6685/#6740 single-workspace tests' assumption explicitly,
/// rather than relying on it only being implicit in their `None`
/// `workspace_root` calls.
#[test]
fn test_record_noop_release_none_workspace_root_still_targets_default() {
    let (tm, db, _, bus) = setup_test_context();
    let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();

    let response = handle_request(
        Request::RecordNoopRelease {
            issue: 6959,
            reason: None,
            workspace_root: None,
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    assert!(matches!(
        response,
        Response::NoopReleaseRecorded {
            issue: 6959,
            consecutive: 1,
            ..
        }
    ));
    assert_eq!(sr.lock().unwrap().noop_release_count(6959), 1);
}

// ===== ListQuarantines (Issue #4215) =====
//
// `workspace_root: None` enumerates every registered workspace (unlike
// `ClearQuarantine`'s `None` == default-workspace-only), so these tests
// seed the pool with the default registry at its own workspace root —
// exactly the way `main.rs` wires `workspace_pool.seed(sweep_workspace,
// sweep_registry)` in production — and pin `REGISTRY_PATH_ENV` to an empty
// file so `effective_roots` resolves to exactly `[root]` regardless of any
// real `~/.loom/workspaces.json` on the host running the test.

#[test]
#[serial_test::serial]
fn test_handle_request_list_quarantines_empty_registry() {
    use crate::workspace_registry::REGISTRY_PATH_ENV;

    let (tm, db, _, bus) = setup_test_context();
    let (sr, dir, _rec) = setup_sweep_registry_in_tempdir();
    let root = dir.path().to_path_buf();
    let empty_reg = dir.path().join("no-such-workspaces.json");
    std::env::set_var(REGISTRY_PATH_ENV, &empty_reg);

    let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
    pool.seed(root, sr.clone());

    let response = handle_request(
        Request::ListQuarantines {
            workspace_root: None,
        },
        &tm,
        &db,
        &sr,
        &bus,
        &pool,
    );
    std::env::remove_var(REGISTRY_PATH_ENV);

    match response {
        Response::QuarantineList { entries } => {
            assert!(entries.is_empty(), "no quarantines seeded -> empty list");
        }
        other => panic!("Expected QuarantineList, got: {other:?}"),
    }
}

#[test]
#[serial_test::serial]
fn test_handle_request_list_quarantines_seeded_entries() {
    use crate::workspace_registry::REGISTRY_PATH_ENV;

    let (tm, db, _, bus) = setup_test_context();
    let (sr, dir, _rec) = setup_sweep_registry_in_tempdir();
    let root = dir.path().to_path_buf();
    let empty_reg = dir.path().join("no-such-workspaces.json");
    std::env::set_var(REGISTRY_PATH_ENV, &empty_reg);

    let applied_at = Utc::now();
    {
        let mut reg = sr.lock().unwrap();
        reg.seed_quarantine_with_details_for_test(4215, applied_at, 2);
    }

    let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
    pool.seed(root.clone(), sr.clone());

    let response = handle_request(
        Request::ListQuarantines {
            workspace_root: None,
        },
        &tm,
        &db,
        &sr,
        &bus,
        &pool,
    );
    std::env::remove_var(REGISTRY_PATH_ENV);

    match response {
        Response::QuarantineList { entries } => {
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].issue, 4215);
            assert_eq!(entries[0].workspace_root, root);
            assert_eq!(entries[0].insta_crash_count, 2);
            assert_eq!(entries[0].quarantined_at, applied_at);
            assert!(
                entries[0].ttl_remaining_secs > 0,
                "freshly-applied quarantine should have TTL remaining"
            );
        }
        other => panic!("Expected QuarantineList, got: {other:?}"),
    }
}

#[test]
#[serial_test::serial]
fn test_handle_request_list_quarantines_ttl_clamps_to_zero() {
    use crate::workspace_registry::REGISTRY_PATH_ENV;

    let (tm, db, _, bus) = setup_test_context();
    let (sr, dir, _rec) = setup_sweep_registry_in_tempdir();
    let root = dir.path().to_path_buf();
    let empty_reg = dir.path().join("no-such-workspaces.json");
    std::env::set_var(REGISTRY_PATH_ENV, &empty_reg);

    // Default TTL is 3600s (Issue #3939) — quarantine this issue as though
    // it were applied 2 hours ago, well past TTL. `reap_once` (the actual
    // expiry sweep) never runs in this test, so the stale entry survives in
    // memory; `ttl_remaining_secs` must still clamp to 0 rather than
    // reporting a nonsensical negative remainder.
    let long_ago = Utc::now() - chrono::Duration::seconds(7200);
    {
        let mut reg = sr.lock().unwrap();
        reg.seed_quarantine_with_details_for_test(9001, long_ago, 5);
    }

    let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
    pool.seed(root, sr.clone());

    let response = handle_request(
        Request::ListQuarantines {
            workspace_root: None,
        },
        &tm,
        &db,
        &sr,
        &bus,
        &pool,
    );
    std::env::remove_var(REGISTRY_PATH_ENV);

    match response {
        Response::QuarantineList { entries } => {
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].issue, 9001);
            assert_eq!(entries[0].ttl_remaining_secs, 0, "past-TTL entry must clamp to 0");
        }
        other => panic!("Expected QuarantineList, got: {other:?}"),
    }
}

#[test]
fn test_handle_request_cancel_sweep_unknown_returns_error() {
    let (tm, db, _, bus) = setup_test_context();
    let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
    let response = handle_request(
        Request::CancelSweep {
            sweep_id: "no-such-sweep".to_string(),
            grace_secs: 1,
            workspace_root: None,
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    match response {
        Response::Error { message } => {
            assert!(
                message.contains("unknown sweep_id"),
                "expected unknown sweep_id; got: {message}"
            );
        }
        other => panic!("Expected Error, got: {other:?}"),
    }
}

/// Issue #4980 acceptance criterion 3: `loom-daemon cancel` (CLI) and
/// `cancel_sweep` (MCP) must **share** the termination implementation.
///
/// The structural guarantee is that both surfaces put the *same frame* on
/// the wire, so there is exactly one server-side path and nothing to
/// diverge. This asserts it at the byte level: the JSON `mcp-loom`'s
/// `cancelSweep` sends and the JSON the CLI serializes from
/// `build_cancel_request` deserialize to the identical
/// `Request::CancelSweep`.
#[test]
fn test_cancel_sweep_cli_and_mcp_frames_are_identical_on_the_wire() {
    // Exactly what `mcp-loom/src/tools/sweeps.ts` `cancelSweep` sends
    // (`sendDaemonRequest` writes the `{type, payload}` shape verbatim).
    let mcp_frame = r#"{"type":"CancelSweep","payload":{"sweep_id":"sweep-issue-4980-1","grace_secs":30,"workspace_root":null}}"#;
    let from_mcp: Request = serde_json::from_str(mcp_frame).expect("MCP frame must parse");

    // Exactly what the `loom-daemon cancel` CLI serializes.
    let from_cli: Request = serde_json::from_str(
        &serde_json::to_string(&Request::CancelSweep {
            sweep_id: "sweep-issue-4980-1".to_string(),
            grace_secs: 30,
            workspace_root: None,
        })
        .unwrap(),
    )
    .expect("CLI frame must parse");

    match (&from_mcp, &from_cli) {
        (
            Request::CancelSweep {
                sweep_id: mcp_id,
                grace_secs: mcp_grace,
                workspace_root: mcp_ws,
            },
            Request::CancelSweep {
                sweep_id: cli_id,
                grace_secs: cli_grace,
                workspace_root: cli_ws,
            },
        ) => {
            assert_eq!(mcp_id, cli_id);
            assert_eq!(mcp_grace, cli_grace);
            assert_eq!(mcp_ws, cli_ws);
        }
        other => panic!("expected two CancelSweep requests, got {other:?}"),
    }
}

/// Issue #4980: a CLI-invoked cancel runs the full daemon-side termination
/// path — terminal transition plus claim-lock release — exercised through a
/// frame parsed off the wire rather than one constructed in-process, so a
/// wire-format regression fails here too.
#[test]
#[serial_test::serial]
fn test_handle_request_cancel_sweep_from_cli_frame_runs_the_shared_path() {
    let (tm, db, _, bus) = setup_test_context();
    let (sr, dir, _rec) = setup_sweep_registry_in_tempdir();
    let _registry_guard = seed_temp_registry(&[]);

    let dispatched = handle_request(
        Request::DispatchSweep {
            kind: SweepKind::Issue(4980),
            idempotency_key: None,
            model: None,
            effort: None,
            depends_on: None,
            // `None` resolves to the default registry `sr` — the same
            // tempdir-rooted registry the cancel below targets.
            workspace_root: None,
            force: false,
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    let sweep_id = match dispatched {
        Response::SweepDispatched { sweep_id, .. } => sweep_id,
        other => panic!("Expected SweepDispatched, got: {other:?}"),
    };
    let lock_dir = dir.path().join(".loom").join("locks").join("issue-4980");
    assert!(lock_dir.exists(), "dispatch should have taken the claim lock");

    // Parse the CLI's frame off the wire, exactly as `handle_client` would.
    let frame = serde_json::to_string(&Request::CancelSweep {
        sweep_id: sweep_id.clone(),
        grace_secs: 1,
        workspace_root: None,
    })
    .unwrap();
    let request: Request = serde_json::from_str(&frame).expect("CLI frame must parse");

    let response = handle_request(request, &tm, &db, &sr, &bus, &test_pool());
    match response {
        Response::SweepCancelled {
            sweep_id: cancelled,
            ..
        } => assert_eq!(cancelled, sweep_id),
        other => panic!("Expected SweepCancelled, got: {other:?}"),
    }

    let state = sr
        .lock()
        .unwrap()
        .get(&sweep_id)
        .expect("entry should still be tracked")
        .state
        .clone();
    assert!(
        matches!(state, crate::types::SweepState::Exited { .. }),
        "a CLI-invoked cancel must run the same terminal transition the MCP tool does, \
             got {state:?}"
    );
    assert!(
        !lock_dir.exists(),
        "a CLI-invoked cancel must release the claim lock, exactly like the MCP path"
    );
}

#[test]
#[serial_test::serial]
fn test_handle_request_get_sweep_status_returns_existing() {
    let (tm, db, _, bus) = setup_test_context();
    let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
    // #4299: pin the registry to empty so `workspace_root: None` resolution
    // is deterministic regardless of the host's real registry.
    let _registry_guard = seed_temp_registry(&[]);

    // Dispatch a sweep to get a real entry in the registry.
    let dispatched = handle_request(
        Request::DispatchSweep {
            kind: SweepKind::Issue(444),
            idempotency_key: None,
            model: None,
            effort: None,
            depends_on: None,
            workspace_root: None,
            force: false,
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    let sweep_id = match dispatched {
        Response::SweepDispatched { sweep_id, .. } => sweep_id,
        other => panic!("Expected SweepDispatched, got: {other:?}"),
    };

    let response = handle_request(
        Request::GetSweepStatus {
            sweep_id: sweep_id.clone(),
            workspace_root: None,
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    match response {
        Response::SweepStatus { info } => {
            let info = info.expect("status should be Some");
            assert_eq!(info.sweep_id, sweep_id);
            assert!(matches!(info.kind, SweepKind::Issue(444)));
        }
        other => panic!("Expected SweepStatus, got: {other:?}"),
    }
}

// ===== Singleton guard liveness probe (Issue #3806) =====

#[tokio::test]
async fn test_socket_has_live_listener_absent_path() {
    // A path that doesn't exist at all → not live.
    let dir = tempdir().unwrap();
    let missing = dir.path().join("nope.sock");
    assert!(!socket_has_live_listener(&missing).await);
}

#[tokio::test]
async fn test_socket_has_live_listener_stale_file() {
    // A regular file at the socket path (a crashed daemon's leftover) has
    // nothing listening behind it → not live, safe to remove/rebind.
    let dir = tempdir().unwrap();
    let stale = dir.path().join("stale.sock");
    std::fs::write(&stale, b"").unwrap();
    assert!(!socket_has_live_listener(&stale).await);
}

#[tokio::test]
async fn test_socket_has_live_listener_non_daemon_listener() {
    // A bound UnixListener that never answers Ping (no accept/respond loop)
    // must be treated as NOT a live, responsive daemon so startup can still
    // recover rather than wedging forever.
    let dir = tempdir().unwrap();
    let sock = dir.path().join("silent.sock");
    let _listener = UnixListener::bind(&sock).unwrap();
    // We never accept()/respond, so the Ping/Pong roundtrip times out.
    assert!(!socket_has_live_listener(&sock).await);
}

#[tokio::test]
async fn test_socket_has_live_listener_true_for_ponging_daemon() {
    // Stand up a minimal accept loop that answers Ping with Pong, exactly
    // like the real IPC server, and confirm the probe reports it live.
    let dir = tempdir().unwrap();
    let sock = dir.path().join("live.sock");
    let listener = UnixListener::bind(&sock).unwrap();

    let server = tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            if let Ok(Some(line)) = lines.next_line().await {
                if let Ok(Request::Ping) = serde_json::from_str::<Request>(&line) {
                    let json = serde_json::to_string(&Response::Pong).unwrap();
                    let _ = writer.write_all(json.as_bytes()).await;
                    let _ = writer.write_all(b"\n").await;
                    let _ = writer.flush().await;
                }
            }
        }
    });

    assert!(socket_has_live_listener(&sock).await);
    server.abort();
}

// ===== Autonomous daemon status (Issue #3891) =====

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
        auto_update_enabled: true,
        auto_update_last_check: Some(chrono::Utc::now()),
        auto_update_last_roll: Some(chrono::Utc::now()),
        auto_update_consecutive_failures: 2,
        auto_update_backoff_secs: Some(120),
        auto_update_terminal_reason: None,
        auto_update_note: Some("within settle window".to_string()),
        auto_update_artifact_version: Some("0.19.24".to_string()),
        auto_update_artifact_published_at: Some("2026-09-13T12:00:00Z".to_string()),
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

// ===== Supervised restart primitive (Issue #4054) =====

/// `Request::RestartDaemon` / `Response::DaemonRestart` must survive a serde
/// round-trip over the wire (same pattern as the Ping/Pong + DaemonStatus
/// round-trips above).
#[test]
fn test_restart_daemon_request_response_round_trip() {
    // Request: unit variant, `{"type":"RestartDaemon"}`.
    let req = Request::RestartDaemon;
    let json = serde_json::to_string(&req).expect("serialize request");
    assert_eq!(json, r#"{"type":"RestartDaemon"}"#);
    let back: Request = serde_json::from_str(&json).expect("deserialize request");
    assert!(matches!(back, Request::RestartDaemon));

    // Response (supervised / scheduled).
    let resp = Response::DaemonRestart {
        scheduled: true,
        supervisor: Some("launchd".to_string()),
        message: "restart scheduled".to_string(),
    };
    let json = serde_json::to_string(&resp).expect("serialize response");
    let back: Response = serde_json::from_str(&json).expect("deserialize response");
    match back {
        Response::DaemonRestart {
            scheduled,
            supervisor,
            message,
        } => {
            assert!(scheduled);
            assert_eq!(supervisor.as_deref(), Some("launchd"));
            assert_eq!(message, "restart scheduled");
        }
        other => panic!("Expected DaemonRestart, got: {other:?}"),
    }

    // Response (unsupervised / refused).
    let resp = Response::DaemonRestart {
        scheduled: false,
        supervisor: None,
        message: "refused".to_string(),
    };
    let json = serde_json::to_string(&resp).expect("serialize response");
    let back: Response = serde_json::from_str(&json).expect("deserialize response");
    match back {
        Response::DaemonRestart {
            scheduled,
            supervisor,
            ..
        } => {
            assert!(!scheduled);
            assert!(supervisor.is_none());
        }
        other => panic!("Expected DaemonRestart, got: {other:?}"),
    }
}

/// `build_restart_decision` ends the process (do_exit == true) ONLY when the
/// daemon proves it is supervised (launchd or systemd) via
/// `LOOM_DAEMON_SUPERVISOR`; an unsupervised host refuses and stays running.
/// Also pins the shutdown-intent exit-code contract (#4054): only the
/// restart primitive exits 0, so under a supervisor's "successful exit
/// restarts" policy (launchd `KeepAlive:SuccessfulExit`, systemd
/// `Restart=on-success`) it is the only path that relaunches.
///
/// NOTE: this is the sole test touching `LOOM_DAEMON_SUPERVISOR`, so the
/// env-var mutation cannot race another test reading it.
#[test]
fn test_build_restart_decision_supervisor_gated() {
    // Exit-code contract: exactly one exit-0 path.
    assert_eq!(EXIT_RESTART, 0, "restart is the only successful (relaunch) exit");
    assert_ne!(EXIT_SIGTERM, 0, "SIGTERM stop must be non-zero (no relaunch)");
    assert_ne!(EXIT_SIGINT, 0, "SIGINT/Ctrl-C must be non-zero (no relaunch)");
    assert_ne!(EXIT_SHUTDOWN, 0, "explicit Shutdown must be non-zero (no relaunch)");
    // #4531: the self-reported startup-failure exit must stay non-zero (no
    // relaunch) AND keep the value `ExitCode::FAILURE` used to produce, so
    // callers that only knew the old `Termination`-driven exit see no change.
    assert_ne!(EXIT_STARTUP_FAILURE, 0, "startup failure must be non-zero (no relaunch)");
    assert_eq!(EXIT_STARTUP_FAILURE, 1, "startup failure must match ExitCode::FAILURE");

    // Supervised: scheduled + do_exit.
    std::env::set_var("LOOM_DAEMON_SUPERVISOR", "launchd");
    assert_eq!(detect_supervisor().as_deref(), Some("launchd"));
    let (resp, do_exit) = build_restart_decision(0);
    assert!(do_exit, "supervised daemon must end its process for a relaunch");
    match resp {
        Response::DaemonRestart {
            scheduled,
            supervisor,
            message,
        } => {
            assert!(scheduled);
            assert_eq!(supervisor.as_deref(), Some("launchd"));
            // #5119: on launchd, sweeps GENUINELY survive (children reparent
            // to pid 1) — the message must still say so.
            assert!(
                message.contains("survive"),
                "launchd restart message must state sweeps survive: {message}"
            );
            assert!(
                !message.contains("do NOT survive"),
                "launchd restart message must NOT claim sweeps are terminated: {message}"
            );
        }
        other => panic!("Expected DaemonRestart, got: {other:?}"),
    }

    // Case-insensitive acceptance.
    std::env::set_var("LOOM_DAEMON_SUPERVISOR", "LaunchD");
    assert_eq!(detect_supervisor().as_deref(), Some("launchd"));

    // Unsupervised (var unset): refuse, keep running.
    std::env::remove_var("LOOM_DAEMON_SUPERVISOR");
    assert!(detect_supervisor().is_none());
    let (resp, do_exit) = build_restart_decision(0);
    assert!(!do_exit, "unsupervised daemon must NOT exit — nothing would relaunch it");
    match resp {
        Response::DaemonRestart {
            scheduled,
            supervisor,
            message,
        } => {
            assert!(!scheduled);
            assert!(supervisor.is_none());
            // #4640: the refusal must mention the systemd retrofit for a
            // fleet worker provisioned before the fix (missing
            // LOOM_DAEMON_SUPERVISOR despite being systemd-supervised).
            assert!(
                message.contains("LOOM_DAEMON_SUPERVISOR=systemd"),
                "restart refusal must mention the systemd retrofit: {message}"
            );
            assert!(
                    message.contains("Restart=on-success"),
                    "restart refusal retrofit hint must include the corrected Restart= policy: {message}"
                );
        }
        other => panic!("Expected DaemonRestart, got: {other:?}"),
    }

    // systemd (#4267): recognized alongside launchd, case-insensitive —
    // ⇒ Some("systemd"), scheduled + do_exit, and a message that does not
    // hardcode "launchd".
    std::env::set_var("LOOM_DAEMON_SUPERVISOR", "systemd");
    assert_eq!(detect_supervisor().as_deref(), Some("systemd"));
    let (resp, do_exit) = build_restart_decision(3);
    assert!(do_exit, "systemd-supervised daemon must end its process for a relaunch");
    match resp {
        Response::DaemonRestart {
            scheduled,
            supervisor,
            message,
        } => {
            assert!(scheduled);
            assert_eq!(supervisor.as_deref(), Some("systemd"));
            assert!(
                !message.contains("launchd"),
                "systemd restart message must not hardcode launchd wording: {message}"
            );
            // #5119: the systemd message must be HONEST — sweeps are reaped
            // with the cgroup, NOT preserved. It must not print the old
            // macOS-only "In-flight sweeps survive by design" claim, it must
            // name the in-flight count it is about to destroy, and it must
            // point at --drain as the preserving alternative.
            assert!(
                message.contains("do NOT survive"),
                "systemd restart message must state in-flight sweeps do NOT survive: {message}"
            );
            assert!(
                    !message.contains("survive by design"),
                    "systemd restart message must not repeat the false 'survive by design' claim: {message}"
                );
            assert!(
                message.contains("cgroup"),
                "systemd restart message must name the cgroup as the reason: {message}"
            );
            assert!(
                message.contains("3 sweep(s)"),
                "systemd ack must name the in-flight count it is about to destroy: {message}"
            );
            assert!(
                message.contains("--drain"),
                "systemd restart message must point at --drain to preserve sweeps: {message}"
            );
        }
        other => panic!("Expected DaemonRestart, got: {other:?}"),
    }

    // Mixed-case systemd.
    std::env::set_var("LOOM_DAEMON_SUPERVISOR", "SyStEmD");
    assert_eq!(detect_supervisor().as_deref(), Some("systemd"));

    // Empty string is not a recognized supervisor.
    std::env::set_var("LOOM_DAEMON_SUPERVISOR", "");
    assert!(detect_supervisor().is_none());

    // Whitespace-only value is not a recognized supervisor (no trimming).
    std::env::set_var("LOOM_DAEMON_SUPERVISOR", "  ");
    assert!(detect_supervisor().is_none());

    // A genuinely unrelated value is also unsupervised.
    std::env::set_var("LOOM_DAEMON_SUPERVISOR", "runit");
    assert!(detect_supervisor().is_none());
    std::env::remove_var("LOOM_DAEMON_SUPERVISOR");
}

/// Issue #5119 AC2: the two supervisors have OPPOSITE in-flight semantics,
/// and the restart primitive must say which one it is on. Pure, so both
/// wordings are pinned here without a supervisor on the host.
///
/// This exercises [`restart_scheduled_message`] directly — the single
/// canonical composer for this ack. An earlier revision of this PR carried a
/// second, near-duplicate `restart_in_flight_fate()` composing the same
/// wording; it was folded into this one function during the rebase onto main
/// so there is exactly one place the honest-restart wording can drift.
#[test]
fn restart_scheduled_message_is_supervisor_specific() {
    // launchd: the survival claim is TRUE there — children reparent to pid 1
    // and keep running (verified repeatedly, #5081). The count is deliberately
    // NOT named: nothing is at risk, so there is nothing to warn about.
    let launchd = restart_scheduled_message("launchd", 4);
    assert!(launchd.contains("In-flight sweeps survive by design"), "got: {launchd}");
    assert!(!launchd.contains("WARNING"), "got: {launchd}");
    assert!(!launchd.contains("sweep(s)"), "got: {launchd}");

    // systemd: the claim is FALSE (the stop job reaps the unit's cgroup),
    // so the message must say so plainly, name the count, and point at the
    // alternative that genuinely preserves the work.
    let systemd = restart_scheduled_message("systemd", 4);
    assert!(!systemd.contains("survive by design"), "got: {systemd}");
    assert!(!systemd.contains("launchd"), "got: {systemd}");
    assert!(systemd.contains("WARNING:"), "got: {systemd}");
    assert!(systemd.contains("do NOT survive"), "got: {systemd}");
    assert!(systemd.contains("cgroup"), "got: {systemd}");
    assert!(systemd.contains("4 sweep(s)"), "got: {systemd}");
    // Both kill shapes are named: the canonical KillMode=mixed unit (#4862)
    // and the older KillMode=control-group one a pre-#4862 worker may still
    // be running.
    assert!(systemd.contains("KillMode=mixed"), "got: {systemd}");
    assert!(systemd.contains("KillMode=control-group"), "got: {systemd}");
    // Role runs are the compounding factor from the 2026-08-03 incident and
    // have no registry entry, so the count must not be presented as covering
    // them.
    assert!(systemd.contains("role runs"), "got: {systemd}");
    assert!(systemd.contains("restart --drain"), "got: {systemd}");

    // Zero in flight still warns: role runs are uncounted, and the daemon
    // launches them on a timer, so "0 sweeps" never means "nothing to lose".
    let idle = restart_scheduled_message("systemd", 0);
    assert!(idle.contains("0 sweep(s)"), "got: {idle}");
    assert!(idle.contains("role runs"), "got: {idle}");
    assert!(idle.contains("nothing to lose"), "got: {idle}");
}

/// A pre-#3902 `DaemonStatus` JSON payload (no `capacity` field, no
/// `per_repo` field) still deserializes — `#[serde(default)]` fills the
/// capacity section and leaves `per_repo` empty (pre-#3930 compat).
#[test]
fn test_daemon_status_backward_compat_missing_capacity() {
    let legacy = r#"{"in_flight":[],"token_pool_size":2,"disk_headroom":9,"configured_max":3,"dynamic_cap":2,"main_health_gate_halted":false}"#;
    let report: DaemonStatusReport =
        serde_json::from_str(legacy).expect("legacy payload deserializes");
    assert_eq!(report.token_pool_size, 2);
    assert!(!report.capacity.ranking_present);
    assert_eq!(report.capacity.healthy_accounts, 0);
    assert!(!report.capacity.token_bound);
    assert!(report.per_repo.is_empty(), "absent per_repo defaults to empty");
    assert!(
        !report.main_health_gate_not_evaluated,
        "absent main_health_gate_not_evaluated (#3950) defaults to false"
    );
    // Absent pre-#3978 fields default rather than failing to parse. (The
    // retired `cpu_headroom` field a pre-#4512 daemon still SENDS is
    // likewise tolerated: serde ignores unknown fields, so an old daemon
    // and a new CLI stay wire-compatible in both directions.)
    assert_eq!(report.logical_cpus, 0);
    assert_eq!(report.loadavg_1m, None);
    // Absent pre-#4012 fields must default to `None` — NOT `false` — so a
    // legacy payload from an older, perfectly healthy daemon is read as
    // "unknown" rather than misreported as "gate disabled" (#4012).
    assert_eq!(
        report.main_health_gate_enabled, None,
        "absent main_health_gate_enabled (#4012) must default to None, not Some(false)"
    );
    assert_eq!(
        report.main_health_gate_verdict_at, None,
        "absent main_health_gate_verdict_at (#4012) defaults to None (reads as pending)"
    );
    // Absent pre-#4031 fields default rather than failing to parse: no
    // measured idle fraction, and "not capacity-bound".
    assert_eq!(report.cpu_idle_fraction, None);
    assert!(!report.capacity_bound);
}

/// `build_daemon_status` sets `capacity_bound` only when in-flight occupancy
/// has actually reached the dynamic cap (#4031) — the "currently binding" vs
/// "smallest ceiling" distinction. With a real token pool (cap > 0) and no
/// in-flight sweeps, the cap is a ceiling but is NOT binding.
#[test]
#[serial_test::serial]
fn test_build_daemon_status_capacity_bound_tracks_occupancy() {
    use crate::main_health_gate::WorkspaceHealthStates;
    use crate::workspace_registry::REGISTRY_PATH_ENV;

    let (sr, dir, _rec) = setup_sweep_registry_in_tempdir();
    let root = dir.path().to_path_buf();
    let empty_reg = dir.path().join("no-such-workspaces.json");
    std::env::set_var(REGISTRY_PATH_ENV, &empty_reg);
    let prev_shared = std::env::var("LOOM_SHARED_TOKENS_DIR").ok();
    std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");

    // Provision a two-token pool so the dynamic cap is > 0 (a real ceiling).
    let tokens_dir = root.join(".loom").join("tokens");
    std::fs::create_dir_all(&tokens_dir).unwrap();
    std::fs::write(tokens_dir.join("acct-a.token"), "sk-ant-oat01-a").unwrap();
    std::fs::write(tokens_dir.join("acct-b.token"), "sk-ant-oat01-b").unwrap();

    let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
    pool.seed(root.clone(), sr.clone());
    let health = WorkspaceHealthStates::new();

    // No sweeps in flight, cap > 0 ⇒ the cap is a ceiling but NOT binding.
    let report = build_daemon_status(&pool, &health, &root, &test_credential_preflight());
    assert!(report.dynamic_cap > 0, "two tokens should yield a positive cap");
    assert!(report.in_flight.is_empty());
    assert!(
        !report.capacity_bound,
        "0 in-flight against cap {} must not be capacity-bound",
        report.dynamic_cap
    );

    // Fill the cap: dispatch sweeps until in-flight reaches the cap.
    let cap = report.dynamic_cap;
    {
        let mut reg = sr.lock().unwrap();
        for i in 0..cap {
            reg.dispatch(&crate::types::SweepKind::Issue(4031 + i as u32), None, None, None, None)
                .expect("dispatch");
        }
    }
    let report = build_daemon_status(&pool, &health, &root, &test_credential_preflight());
    assert_eq!(report.in_flight.len(), cap);
    assert!(
        report.capacity_bound,
        "{cap} in-flight against cap {cap} must be capacity-bound",
    );

    if let Some(v) = prev_shared {
        std::env::set_var("LOOM_SHARED_TOKENS_DIR", v);
    } else {
        std::env::remove_var("LOOM_SHARED_TOKENS_DIR");
    }
}

/// #5305: `capacity.token_bound` must be reachable again — it was
/// hardcoded `false` after #5304, permanently dead-ending the
/// `status_render.rs` add-accounts guidance branch. It means genuine
/// starvation (zero healthy accounts in the ranking), NOT "tokens bound
/// the dynamic cap" (that cross-axis meaning was retired by #5270).
#[test]
#[serial_test::serial]
fn test_build_daemon_status_token_bound_reflects_zero_healthy_accounts() {
    use crate::main_health_gate::WorkspaceHealthStates;
    use crate::workspace_registry::REGISTRY_PATH_ENV;

    let (sr, dir, _rec) = setup_sweep_registry_in_tempdir();
    let root = dir.path().to_path_buf();
    let empty_reg = dir.path().join("no-such-workspaces.json");
    std::env::set_var(REGISTRY_PATH_ENV, &empty_reg);
    let prev_shared = std::env::var("LOOM_SHARED_TOKENS_DIR").ok();
    std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");

    let tokens_dir = root.join(".loom").join("tokens");
    std::fs::create_dir_all(&tokens_dir).unwrap();
    std::fs::write(tokens_dir.join("acct-a.token"), "sk-ant-oat01-a").unwrap();
    std::fs::write(tokens_dir.join("acct-b.token"), "sk-ant-oat01-b").unwrap();
    std::fs::write(tokens_dir.join("acct-c.token"), "sk-ant-oat01-c").unwrap();

    let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
    pool.seed(root.clone(), sr.clone());
    let health = WorkspaceHealthStates::new();

    // A partially-exhausted pool (1/3 healthy) must NOT report token_bound
    // — this is the false add-accounts advisory this issue fixes (#5304
    // over-removal, item 1/2): a healthy account remains, so this is not
    // starvation, regardless of how the dynamic cap (disk/ram/ceiling)
    // happens to compare.
    std::fs::write(
        tokens_dir.join(".ranking"),
        "acct-a|available|0.1\nacct-b|exhausted|0.99\nacct-c|exhausted|0.99\n",
    )
    .unwrap();
    let report = build_daemon_status(&pool, &health, &root, &test_credential_preflight());
    assert_eq!(report.capacity.healthy_accounts, 1);
    assert!(
        !report.capacity.token_bound,
        "one healthy account remains ⇒ not starved, no add-accounts advisory"
    );

    // Every account exhausted/blocked ⇒ genuinely starved: `token_bound`
    // must be reachable and true so the operator guidance branch fires.
    std::fs::write(
        tokens_dir.join(".ranking"),
        "acct-a|exhausted|0.99\nacct-b|blocked|0.99\nacct-c|exhausted|0.99\n",
    )
    .unwrap();
    let report = build_daemon_status(&pool, &health, &root, &test_credential_preflight());
    assert_eq!(report.capacity.healthy_accounts, 0);
    assert!(
        report.capacity.token_bound,
        "zero healthy accounts ⇒ genuinely starved, guidance branch must be reachable"
    );

    if let Some(v) = prev_shared {
        std::env::set_var("LOOM_SHARED_TOKENS_DIR", v);
    } else {
        std::env::remove_var("LOOM_SHARED_TOKENS_DIR");
    }
}

/// `build_daemon_status` reflects the per-repo main-health halt flag and
/// lists a live dispatched sweep as in-flight. Single-workspace case (empty
/// registry): exactly one `per_repo` entry for the daemon's own workspace,
/// byte-for-byte the pre-#3930 top-level behavior.
#[test]
#[serial_test::serial]
fn test_build_daemon_status_reports_halt_and_in_flight() {
    use crate::main_health_gate::WorkspaceHealthStates;
    use crate::workspace_registry::REGISTRY_PATH_ENV;

    let (sr, dir, _rec) = setup_sweep_registry_in_tempdir();
    let root = dir.path().to_path_buf();

    // Point the workspace registry at a nonexistent file so effective_roots
    // falls back to [root] (single-workspace equivalence).
    let empty_reg = dir.path().join("no-such-workspaces.json");
    std::env::set_var(REGISTRY_PATH_ENV, &empty_reg);

    // Disable the shared machine-level pool fallback (#3940) so the
    // token-pool assertions see only the tempdir workspace, not the
    // host's real ~/.loom/tokens (empty value = operator opt-out).
    let prev_shared = std::env::var("LOOM_SHARED_TOKENS_DIR").ok();
    std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");

    // Seed the pool with the default registry keyed at `root`.
    let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
    pool.seed(root.clone(), sr.clone());
    let health = WorkspaceHealthStates::new();

    // Fresh state: not halted, no sweeps.
    let report = build_daemon_status(&pool, &health, &root, &test_credential_preflight());
    assert!(!report.main_health_gate_halted);
    assert!(report.in_flight.is_empty());
    // The tempdir has no `.loom/tokens/`, so the pool is 0 — but since
    // #5270 the token axis no longer participates in the dynamic cap, so
    // an empty token pool no longer pins `dynamic_cap` to 0. The cap is
    // `min(disk_headroom, ram_headroom, configured_max)`, where
    // `configured_max` is resolved the same way `build_daemon_status`
    // resolves it (not assumed to be the built-in default — a host-level
    // config tier may set it, exactly the ambient config this assertion
    // must tolerate).
    assert_eq!(report.token_pool_size, 0);
    let expected_configured_max = crate::work_finder::resolve_max_concurrent_with_config(
        &crate::work_finder::read_work_finder_config(&root),
    );
    assert_eq!(
        report.dynamic_cap,
        report
            .disk_headroom
            .min(report.ram_headroom)
            .min(expected_configured_max),
        "dynamic cap is min(disk, ram, configured_max) regardless of the empty token pool"
    );
    // #4345: a pool that never called start_safehouse_narration /
    // start_peer_coordination still reports a live safehouse state — the
    // cell's own default, not a missing/`None` field.
    let safehouse = report
        .safehouse
        .as_ref()
        .expect("safehouse status always present");
    assert_eq!(safehouse.state, "not_configured");
    assert!(safehouse.socket.is_none());
    // Per-repo breakdown: exactly one entry for the single workspace.
    assert_eq!(report.per_repo.len(), 1);
    assert_eq!(report.per_repo[0].root, root);
    assert_eq!(report.per_repo[0].in_flight_count, 0);
    assert!(!report.per_repo[0].health_gate_halted);
    // No `.loom/config.json` buildGate/autonomous block exists yet, so the
    // gate is effectively disabled for this root (#4012).
    assert_eq!(report.main_health_gate_enabled, Some(false));
    assert_eq!(report.per_repo[0].health_gate_enabled, Some(false));
    assert_eq!(report.main_health_gate_verdict_at, None);
    assert_eq!(report.per_repo[0].health_gate_verdict_at, None);

    // #4012: a root the gate loop HAS never evaluated (no verdict yet)
    // reports the same `Some(false)`/`None` pair while genuinely disabled
    // -- but once the config turns the gate on, a fresh `MainHealthState`
    // reports "enabled, pending" (verdict_at still `None`), NOT "clear".
    // This is the exact ambiguity the issue is about: `pending` and
    // `disabled` both still allow dispatch, but they must not be
    // confused with `clear` (verified green).
    std::env::remove_var(crate::main_health_gate::MAIN_HEALTH_GATE_ENABLE_ENV);
    std::fs::write(
            root.join(".loom").join("config.json"),
            r#"{"autonomous": {"mainHealthGate": {"enabled": true}}, "buildGate": {"enabled": true, "command": "true"}}"#,
        )
        .unwrap();
    let report = build_daemon_status(&pool, &health, &root, &test_credential_preflight());
    assert_eq!(
        report.main_health_gate_enabled,
        Some(true),
        "config now enables the gate for this root"
    );
    assert_eq!(
        report.main_health_gate_verdict_at, None,
        "no gate run has completed yet -- must report pending, not clear"
    );
    assert!(!report.main_health_gate_halted, "pending must still read as dispatch-allowed");

    // Dispatch a sweep -> it should show up as in-flight (Running).
    {
        let mut reg = sr.lock().unwrap();
        reg.dispatch(&crate::types::SweepKind::Issue(3891), None, None, None, None)
            .expect("dispatch");
    }
    let report = build_daemon_status(&pool, &health, &root, &test_credential_preflight());
    assert_eq!(report.in_flight.len(), 1);
    assert!(matches!(report.in_flight[0].kind, crate::types::SweepKind::Issue(3891)));
    assert_eq!(report.per_repo[0].in_flight_count, 1);

    // Flip the halt flag for this root -> the report tracks it (top-level and
    // per-repo).
    health.set_halted(&root, true);
    let report = build_daemon_status(&pool, &health, &root, &test_credential_preflight());
    assert!(report.main_health_gate_halted);
    assert!(report.per_repo[0].health_gate_halted);
    assert!(!report.main_health_gate_not_evaluated, "no skip has happened yet");

    // A skip (dirty tree) is independent of halt (#3950 AC3): it leaves any
    // prior halt untouched but surfaces its own "not evaluated" flag, so
    // "halted (red main)" and "not evaluated (dirty tree)" can both be
    // true — a prior red run's halt persisting while a later tick can't
    // even evaluate because the tree went dirty.
    health.get_or_create(&root).note_gate_tick(
        Some((
            crate::main_health_gate::UnevaluatedClass::DirtyTree,
            "operator edit in src/main.rs",
        )),
        std::time::Duration::from_secs(3600),
    );
    let report = build_daemon_status(&pool, &health, &root, &test_credential_preflight());
    assert!(report.main_health_gate_halted, "prior halt persists through a skip");
    assert!(report.main_health_gate_not_evaluated, "skip surfaces as not-evaluated");
    assert!(report.per_repo[0].health_gate_halted);
    assert!(report.per_repo[0].health_gate_not_evaluated);
    // #3974 AC2: the report names the actual failure class + detail rather
    // than leaving the renderer to assume "workspace tree is dirty".
    let reason = report
        .main_health_gate_not_evaluated_reason
        .as_deref()
        .expect("not-evaluated reason recorded");
    assert!(reason.starts_with("dirty-tree: "), "got: {reason}");
    assert!(reason.contains("src/main.rs"), "got: {reason}");
    assert_eq!(
        report.per_repo[0]
            .health_gate_not_evaluated_reason
            .as_deref(),
        Some(reason)
    );

    match prev_shared {
        Some(v) => std::env::set_var("LOOM_SHARED_TOKENS_DIR", v),
        None => std::env::remove_var("LOOM_SHARED_TOKENS_DIR"),
    }
    std::env::remove_var(REGISTRY_PATH_ENV);
}

/// Regression test for Issue #4279 (the silent-EOF `status` incident): once
/// the sweep-registry `Mutex` is **poisoned** (a thread panicked while
/// holding it), `build_daemon_status` must still return a report by
/// recovering the guard — NOT re-panic on every subsequent call. Before the
/// fix, the `.expect("Sweep registry mutex poisoned")` turned one panic into
/// a permanent server-side failure: every later `status` request panicked in
/// its detached per-connection task and dropped the socket with zero bytes
/// written, which the client saw as an empty response.
#[test]
#[serial_test::serial]
fn test_build_daemon_status_recovers_from_poisoned_registry() {
    use crate::main_health_gate::WorkspaceHealthStates;
    use crate::workspace_registry::REGISTRY_PATH_ENV;

    let (sr, dir, _rec) = setup_sweep_registry_in_tempdir();
    let root = dir.path().to_path_buf();
    let empty_reg = dir.path().join("no-such-workspaces.json");
    std::env::set_var(REGISTRY_PATH_ENV, &empty_reg);
    let prev_shared = std::env::var("LOOM_SHARED_TOKENS_DIR").ok();
    std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");

    let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
    pool.seed(root.clone(), sr.clone());
    let health = WorkspaceHealthStates::new();

    // Poison the registry mutex exactly as a panic under the lock would: a
    // helper thread takes the lock and panics, leaving the `Mutex` poisoned.
    let poison_target = sr.clone();
    let joined = std::thread::spawn(move || {
        let _guard = poison_target.lock().expect("lock to poison");
        panic!("intentional panic to poison the registry mutex");
    })
    .join();
    assert!(joined.is_err(), "the poisoning thread must have panicked");
    assert!(sr.is_poisoned(), "registry mutex should now be poisoned");

    // The core invariant: a poisoned registry no longer crashes the status
    // build. It returns a report (recovering the guard) so `status` stays
    // answerable rather than EOF-ing every connection for the process's life.
    let report = build_daemon_status(&pool, &health, &root, &test_credential_preflight());
    assert_eq!(report.per_repo.len(), 1, "single-workspace report still built");
    assert_eq!(report.per_repo[0].root, root);

    match prev_shared {
        Some(v) => std::env::set_var("LOOM_SHARED_TOKENS_DIR", v),
        None => std::env::remove_var("LOOM_SHARED_TOKENS_DIR"),
    }
    std::env::remove_var(REGISTRY_PATH_ENV);
}

/// Regression test for Issue #4214 (the "vanish window" incident): a sweep
/// whose per-issue lock is live (`owner_pid` alive) but which has **no**
/// matching in-flight registry entry — the exact shape of the observed
/// incident, where the in-memory union of live entries silently lost track
/// of a sweep the filesystem lock proves is still alive — must be surfaced
/// via `DaemonStatusReport::unregistered_locked`, not silently omitted from
/// `in_flight` with no trace at all. Also exercises the full JSON
/// round-trip so a client (CLI, monitor script) sees the same shape.
#[test]
#[serial_test::serial]
fn test_build_daemon_status_surfaces_unregistered_locked_sweep() {
    use crate::main_health_gate::WorkspaceHealthStates;
    use crate::workspace_registry::REGISTRY_PATH_ENV;

    let (sr, dir, _rec) = setup_sweep_registry_in_tempdir();
    let root = dir.path().to_path_buf();
    let empty_reg = dir.path().join("no-such-workspaces.json");
    std::env::set_var(REGISTRY_PATH_ENV, &empty_reg);
    let prev_shared = std::env::var("LOOM_SHARED_TOKENS_DIR").ok();
    std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");

    let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
    pool.seed(root.clone(), sr.clone());
    let health = WorkspaceHealthStates::new();

    // Baseline: no lock, nothing in flight, nothing unregistered.
    let report = build_daemon_status(&pool, &health, &root, &test_credential_preflight());
    assert!(report.in_flight.is_empty());
    assert!(report.unregistered_locked.is_empty());

    // Simulate the observed incident: a live, locked sweep (owner_pid alive,
    // lock dir valid) with NO corresponding registry entry — i.e. it never
    // went through `reconstruct()` or `dispatch()` in this process's
    // lifetime, exactly the read-path-gap shape the issue's forensics
    // pointed to (a registry mutation would have shown up as a Crashed/
    // Exited terminal entry instead, not a bare absence).
    let locks_dir = root.join(".loom").join("locks").join("issue-4201");
    std::fs::create_dir_all(&locks_dir).unwrap();
    // `LockOwner` is private to `sweep_registry`; write its wire schema
    // directly (mirrors what `acquire_lock` writes) rather than reaching
    // across the module boundary.
    let owner = serde_json::json!({
        "issue": 4201,
        "owner_pid": std::process::id(),
        "acquired_at": chrono::Utc::now().to_rfc3339(),
        "sweep_id": "sweep-issue-4201-1785221507",
    });
    std::fs::write(locks_dir.join("owner.json"), serde_json::to_string_pretty(&owner).unwrap())
        .unwrap();

    let report = build_daemon_status(&pool, &health, &root, &test_credential_preflight());
    assert!(
        report.in_flight.is_empty(),
        "the sweep never went through dispatch/reconstruct in this test, so it \
             is deliberately still absent from in_flight -- that's the omission \
             this test targets"
    );
    assert_eq!(
        report.unregistered_locked.len(),
        1,
        "a live-locked issue with no registry entry must surface as unregistered_locked, \
             got: {:?}",
        report.unregistered_locked
    );
    let entry = &report.unregistered_locked[0];
    assert_eq!(entry.issue, 4201);
    assert_eq!(entry.owner_pid, std::process::id());
    assert_eq!(entry.root, root);

    // JSON round-trip: the field survives serialize -> deserialize (the
    // wire contract `loom-daemon status --json` and any monitor script rely
    // on), and stays byte-identical to a re-parse of the legacy fixture
    // from `test_daemon_status_backward_compat_missing_capacity` (an absent
    // field there must still default to empty -- covered by that test; this
    // one only asserts our populated case survives the round trip).
    let json = serde_json::to_string(&report).expect("serialize DaemonStatusReport");
    let back: DaemonStatusReport =
        serde_json::from_str(&json).expect("deserialize DaemonStatusReport");
    assert_eq!(back.unregistered_locked.len(), 1);
    assert_eq!(back.unregistered_locked[0].issue, 4201);
    assert_eq!(back.unregistered_locked[0].owner_pid, std::process::id());

    match prev_shared {
        Some(v) => std::env::set_var("LOOM_SHARED_TOKENS_DIR", v),
        None => std::env::remove_var("LOOM_SHARED_TOKENS_DIR"),
    }
    std::env::remove_var(REGISTRY_PATH_ENV);
}

/// `build_daemon_status` with two registered workspaces returns one `per_repo`
/// entry per root with correct in-flight counts (a sweep dispatched into a
/// non-default repo is now visible) and independent per-repo halt state
/// (Issue #3930 — a red repo B does not mark repo A halted).
#[test]
#[serial_test::serial]
fn test_build_daemon_status_multi_workspace_per_repo_breakdown() {
    use crate::main_health_gate::WorkspaceHealthStates;
    use crate::workspace_registry::{normalize_path, WorkspaceRegistry, REGISTRY_PATH_ENV};

    let (sr_a, dir_a, _rec_a) = setup_sweep_registry_in_tempdir();
    let (sr_b, dir_b, _rec_b) = setup_sweep_registry_in_tempdir();
    let root_a = dir_a.path().to_path_buf();
    let root_b = dir_b.path().to_path_buf();

    // A registry listing BOTH managed repos (roots stored canonicalized).
    let reg_path = dir_a.path().join("workspaces.json");
    let mut reg = WorkspaceRegistry::default();
    reg.add(&root_a, None).unwrap();
    reg.add(&root_b, None).unwrap();
    reg.save(&reg_path).unwrap();
    std::env::set_var(REGISTRY_PATH_ENV, &reg_path);

    // Seed the pool with both registries under their normalized (canonical)
    // roots — the same key `effective_roots` returns.
    let canon_a = normalize_path(&root_a);
    let canon_b = normalize_path(&root_b);
    let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
    pool.seed(canon_a.clone(), sr_a.clone());
    pool.seed(canon_b.clone(), sr_b.clone());

    let health = WorkspaceHealthStates::new();
    // Repo B is red; repo A green — independently.
    health.set_halted(&canon_b, true);

    // Dispatch a sweep into repo A only.
    {
        let mut reg = sr_a.lock().unwrap();
        reg.dispatch(&crate::types::SweepKind::Issue(42), None, None, None, None)
            .expect("dispatch");
    }

    let report = build_daemon_status(&pool, &health, &root_a, &test_credential_preflight());
    assert_eq!(report.per_repo.len(), 2, "both managed repos are listed");
    // Union of in-flight across repos = repo A's single sweep.
    assert_eq!(report.in_flight.len(), 1);

    let a = report
        .per_repo
        .iter()
        .find(|r| r.root == canon_a)
        .expect("repo A present");
    let b = report
        .per_repo
        .iter()
        .find(|r| r.root == canon_b)
        .expect("repo B present");
    assert_eq!(a.in_flight_count, 1, "repo A has the dispatched sweep");
    assert!(!a.health_gate_halted, "repo A is green");
    assert_eq!(b.in_flight_count, 0, "repo B has no sweeps");
    assert!(b.health_gate_halted, "repo B is red, independently of A");
    // Neither repo has a `.loom/config.json` buildGate block, so both
    // resolve as effectively disabled (#4012) — independent of the raw
    // halt flag test-injected directly on repo B above (`set_halted`
    // bypasses the gate loop's own disabled soft-fail path, so this
    // combination only arises in a test; the renderer must still prefer
    // "halted" over "disabled" when both are true, see `main.rs`).
    assert_eq!(a.health_gate_enabled, Some(false));
    assert_eq!(b.health_gate_enabled, Some(false));
    assert_eq!(a.health_gate_verdict_at, None);
    assert_eq!(b.health_gate_verdict_at, None);

    std::env::remove_var(REGISTRY_PATH_ENV);
}

/// Issue #5269: a daemon whose `fallback_root` (launch CWD) is repo A must
/// still report repo B's OWN `.ranking` freshness in `per_repo`, reading
/// repo B's own per-repo pool — NOT repo A's, and not whatever the
/// top-level `fallback_root`-anchored `token_pool_dir`/`capacity` fields
/// resolved to. This is the exact scope mismatch the issue reports: an
/// operator asking about repo B from a daemon anchored at repo A
/// previously got no answer about repo B's pool at all.
#[test]
#[serial_test::serial]
fn test_build_daemon_status_per_repo_ranking_reflects_each_repos_own_pool() {
    use crate::main_health_gate::WorkspaceHealthStates;
    use crate::workspace_registry::{normalize_path, WorkspaceRegistry, REGISTRY_PATH_ENV};

    let (sr_a, dir_a, _rec_a) = setup_sweep_registry_in_tempdir();
    let (sr_b, dir_b, _rec_b) = setup_sweep_registry_in_tempdir();
    let root_a = dir_a.path().to_path_buf();
    let root_b = dir_b.path().to_path_buf();

    // Disable the shared machine-level pool fallback so each repo's own
    // per-repo `.loom/tokens/` is the only pool `resolve_tokens_dir` can
    // find for it — otherwise a repo with no per-repo pool would silently
    // fall through to the host's real `~/.loom/tokens` (or a stale value
    // left by another `#[serial]` test) instead of reporting "absent".
    let prev_shared = std::env::var("LOOM_SHARED_TOKENS_DIR").ok();
    std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");

    // A registry listing BOTH managed repos, exactly like the sibling
    // multi-workspace test above.
    let reg_path = dir_a.path().join("workspaces.json");
    let mut reg = WorkspaceRegistry::default();
    reg.add(&root_a, None).unwrap();
    reg.add(&root_b, None).unwrap();
    reg.save(&reg_path).unwrap();
    std::env::set_var(REGISTRY_PATH_ENV, &reg_path);

    let canon_a = normalize_path(&root_a);
    let canon_b = normalize_path(&root_b);

    // Repo A's own pool: present but STALE (mtime forced far in the past).
    let tokens_a = canon_a.join(".loom").join("tokens");
    std::fs::create_dir_all(&tokens_a).unwrap();
    std::fs::write(tokens_a.join("acct-a.token"), "sk-ant-oat01-a").unwrap();
    let ranking_a_path = tokens_a.join(".ranking");
    std::fs::write(&ranking_a_path, "acct-a|available|0.1\n").unwrap();
    let stale_mtime = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
    std::fs::File::options()
        .write(true)
        .open(&ranking_a_path)
        .unwrap()
        .set_modified(stale_mtime)
        .unwrap();

    // Repo B's own pool: present and FRESH.
    let tokens_b = canon_b.join(".loom").join("tokens");
    std::fs::create_dir_all(&tokens_b).unwrap();
    std::fs::write(tokens_b.join("acct-b.token"), "sk-ant-oat01-b").unwrap();
    std::fs::write(tokens_b.join(".ranking"), "acct-b|available|0.1\n").unwrap();

    let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
    pool.seed(canon_a.clone(), sr_a.clone());
    pool.seed(canon_b.clone(), sr_b.clone());
    let health = WorkspaceHealthStates::new();

    // The daemon's own `fallback_root`/launch CWD is repo A.
    let report = build_daemon_status(&pool, &health, &root_a, &test_credential_preflight());
    assert_eq!(report.per_repo.len(), 2, "both managed repos are listed");

    let a = report
        .per_repo
        .iter()
        .find(|r| r.root == canon_a)
        .expect("repo A present");
    let b = report
        .per_repo
        .iter()
        .find(|r| r.root == canon_b)
        .expect("repo B present");

    // Each repo's own resolved pool is its OWN per-repo directory, not the
    // daemon's single anchored `token_pool_dir`.
    assert_eq!(a.token_pool_dir.as_deref(), Some(tokens_a.as_path()));
    assert_eq!(b.token_pool_dir.as_deref(), Some(tokens_b.as_path()));

    // Repo A's own ranking is present but stale.
    assert!(a.ranking_present, "repo A has its own .ranking");
    assert!(
        a.ranking_age_secs.unwrap_or(0) >= 7000,
        "repo A's own ranking must read as ~2h old, got {:?}",
        a.ranking_age_secs
    );

    // Repo B's own ranking is present and fresh — this is the exact
    // scenario the bug report describes ("worker-1: 5h-stale machine
    // pool" while the operator's own repo's self-refresh loop kept its
    // OWN pool current): the per-repo report must reflect repo B's own
    // freshness, independent of the daemon's `fallback_root`-anchored
    // primary-workspace value.
    assert!(b.ranking_present, "repo B has its own .ranking");
    assert!(
        b.ranking_age_secs.unwrap_or(u64::MAX) < 60,
        "repo B's own ranking must read as fresh, got {:?}",
        b.ranking_age_secs
    );

    match prev_shared {
        Some(v) => std::env::set_var("LOOM_SHARED_TOKENS_DIR", v),
        None => std::env::remove_var("LOOM_SHARED_TOKENS_DIR"),
    }
    std::env::remove_var(REGISTRY_PATH_ENV);
}

/// Issue #4326: a registry entry whose root directory has been deleted
/// (without a matching `workspace remove`) is flagged `root_missing: true`
/// in `build_daemon_status`'s per-repo breakdown, while a sibling whose
/// directory still exists is unaffected — this is the daemon-side half of
/// the missing-root hygiene backstop (the work-finder's per-tick skip is
/// covered separately in `work_finder::tests`).
#[test]
#[serial_test::serial]
fn test_build_daemon_status_flags_missing_root() {
    use crate::main_health_gate::WorkspaceHealthStates;
    use crate::workspace_registry::{normalize_path, WorkspaceRegistry, REGISTRY_PATH_ENV};

    let (sr_a, dir_a, _rec_a) = setup_sweep_registry_in_tempdir();
    let root_a = dir_a.path().to_path_buf();
    // A second root that exists at registration time, then gets removed
    // from disk — exactly the dangling-entry shape from #4326 (a scratch
    // dir was deleted without `loom-daemon workspace remove`).
    let dangling_dir = tempfile::tempdir().unwrap();
    let root_dangling = dangling_dir.path().to_path_buf();
    let canon_dangling = normalize_path(&root_dangling);

    let reg_path = dir_a.path().join("workspaces.json");
    let mut reg = WorkspaceRegistry::default();
    reg.add(&root_a, None).unwrap();
    reg.add(&root_dangling, None).unwrap();
    reg.save(&reg_path).unwrap();
    std::env::set_var(REGISTRY_PATH_ENV, &reg_path);

    // Delete the second root's directory after registration — the entry
    // itself stays registered (warn-and-skip, never auto-remove).
    drop(dangling_dir);
    assert!(!canon_dangling.exists(), "precondition: dangling root is gone");

    let canon_a = normalize_path(&root_a);
    let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
    pool.seed(canon_a.clone(), sr_a.clone());

    let health = WorkspaceHealthStates::new();
    let report = build_daemon_status(&pool, &health, &root_a, &test_credential_preflight());

    assert_eq!(report.per_repo.len(), 2);
    let a = report
        .per_repo
        .iter()
        .find(|r| r.root == canon_a)
        .expect("repo A present");
    let dangling = report
        .per_repo
        .iter()
        .find(|r| r.root == canon_dangling)
        .expect("dangling entry still present in the registry");
    assert!(!a.root_missing, "repo A's directory still exists");
    assert!(dangling.root_missing, "the deleted root is flagged missing");

    std::env::remove_var(REGISTRY_PATH_ENV);
}

/// If `DaemonStatus` ever reaches the synchronous dispatcher (it is meant to
/// be intercepted in `handle_client`), it returns a loud Error sentinel.
#[test]
fn test_handle_request_daemon_status_short_circuits_to_error() {
    let (tm, db, sr, bus) = setup_test_context();
    let response = handle_request(Request::DaemonStatus, &tm, &db, &sr, &bus, &test_pool());
    match response {
        Response::Error { message } => {
            assert!(
                message.contains("DaemonStatus must be handled by build_daemon_status"),
                "expected internal-bug error message; got: {message}"
            );
        }
        other => panic!("Expected Error sentinel, got: {other:?}"),
    }
}

#[test]
fn test_handle_request_subscribe_events_short_circuits_to_error() {
    // SubscribeEvents must be handled by stream_events (not the
    // dispatcher). If it ever reaches handle_request, the dispatcher
    // returns an Error sentinel so the bug is visible.
    let (tm, db, sr, bus) = setup_test_context();
    let response = handle_request(
        Request::SubscribeEvents {
            topics: vec!["sweep".to_string()],
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    match response {
        Response::Error { message } => {
            assert!(
                message.contains("SubscribeEvents must be handled by stream_events"),
                "expected internal-bug error message; got: {message}"
            );
        }
        other => panic!("Expected Error sentinel, got: {other:?}"),
    }
}

// ===== Workspace Registry (Issue #3926) =====

/// End-to-end exercise of the Register / List / Deregister IPC handlers
/// against a temp registry file (via `LOOM_WORKSPACES_PATH`). Serialized
/// because it mutates the process env that resolves the registry path.
#[test]
#[serial_test::serial]
fn test_workspace_registry_ipc_roundtrip() {
    let (tm, db, sr, bus) = setup_test_context();
    let dir = tempdir().unwrap();
    let registry_path = dir.path().join("workspaces.json");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let canonical = std::fs::canonicalize(&repo).unwrap();

    std::env::set_var("LOOM_WORKSPACES_PATH", &registry_path);

    // Empty registry: list returns no workspaces.
    let response = handle_request(Request::ListWorkspaces, &tm, &db, &sr, &bus, &test_pool());
    match response {
        Response::WorkspaceList { workspaces } => assert!(workspaces.is_empty()),
        other => panic!("Expected WorkspaceList, got: {other:?}"),
    }

    // Register.
    let response = handle_request(
        Request::RegisterWorkspace {
            root: repo.to_string_lossy().into_owned(),
            config_overrides: None,
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    match response {
        Response::WorkspaceRegistered {
            root,
            already_present,
            ..
        } => {
            assert_eq!(root, canonical);
            assert!(!already_present);
        }
        other => panic!("Expected WorkspaceRegistered, got: {other:?}"),
    }

    // Re-register is idempotent (already_present = true).
    let response = handle_request(
        Request::RegisterWorkspace {
            root: repo.to_string_lossy().into_owned(),
            config_overrides: None,
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    match response {
        Response::WorkspaceRegistered {
            already_present, ..
        } => assert!(already_present),
        other => panic!("Expected WorkspaceRegistered, got: {other:?}"),
    }

    // List now shows exactly one.
    let response = handle_request(Request::ListWorkspaces, &tm, &db, &sr, &bus, &test_pool());
    match response {
        Response::WorkspaceList { workspaces } => {
            assert_eq!(workspaces.len(), 1);
            assert_eq!(workspaces[0].root, canonical);
        }
        other => panic!("Expected WorkspaceList, got: {other:?}"),
    }

    // Deregister.
    let response = handle_request(
        Request::DeregisterWorkspace {
            root: repo.to_string_lossy().into_owned(),
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    match response {
        Response::WorkspaceDeregistered { was_present, .. } => assert!(was_present),
        other => panic!("Expected WorkspaceDeregistered, got: {other:?}"),
    }

    // Deregister again is a no-op success.
    let response = handle_request(
        Request::DeregisterWorkspace {
            root: repo.to_string_lossy().into_owned(),
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    match response {
        Response::WorkspaceDeregistered { was_present, .. } => assert!(!was_present),
        other => panic!("Expected WorkspaceDeregistered, got: {other:?}"),
    }

    std::env::remove_var("LOOM_WORKSPACES_PATH");
}

#[test]
#[serial_test::serial]
fn test_watch_registry_ipc_roundtrip() {
    use crate::watch_registry::WatchKind;

    let (tm, db, sr, bus) = setup_test_context();
    let dir = tempdir().unwrap();
    let watches_path = dir.path().join("watches.json");
    std::env::set_var(crate::watch_registry::WATCHES_PATH_ENV, &watches_path);

    // Empty registry: list returns none.
    let response = handle_request(Request::ListWatches, &tm, &db, &sr, &bus, &test_pool());
    match response {
        Response::WatchList { watches } => assert!(watches.is_empty()),
        other => panic!("Expected WatchList, got: {other:?}"),
    }

    // Register a cross-repo issue watch (the motivating #6193 case).
    let response = handle_request(
        Request::RegisterWatch {
            kind: WatchKind::Issue,
            number: 6193,
            repo: Some("rjwalters/vibesql".to_string()),
            workspace_root: None,
            note: Some("canary".to_string()),
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    let watch_id = match response {
        Response::WatchRegistered {
            watch,
            already_present,
        } => {
            assert!(!already_present);
            assert_eq!(watch.number, 6193);
            assert_eq!(watch.repo.as_deref(), Some("rjwalters/vibesql"));
            watch.id
        }
        other => panic!("Expected WatchRegistered, got: {other:?}"),
    };

    // Re-register the same target dedups (already_present = true).
    let response = handle_request(
        Request::RegisterWatch {
            kind: WatchKind::Issue,
            number: 6193,
            repo: Some("rjwalters/vibesql".to_string()),
            workspace_root: None,
            note: None,
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    match response {
        Response::WatchRegistered {
            already_present, ..
        } => assert!(already_present),
        other => panic!("Expected WatchRegistered, got: {other:?}"),
    }

    // List shows exactly one, and it survives being re-loaded from disk
    // (the whole point — a watch outlives the registering session).
    let response = handle_request(Request::ListWatches, &tm, &db, &sr, &bus, &test_pool());
    match response {
        Response::WatchList { watches } => {
            assert_eq!(watches.len(), 1);
            assert_eq!(watches[0].id, watch_id);
        }
        other => panic!("Expected WatchList, got: {other:?}"),
    }

    // Remove by id.
    let response = handle_request(
        Request::RemoveWatch {
            id: watch_id.clone(),
        },
        &tm,
        &db,
        &sr,
        &bus,
        &test_pool(),
    );
    match response {
        Response::WatchRemoved { was_present, .. } => assert!(was_present),
        other => panic!("Expected WatchRemoved, got: {other:?}"),
    }

    // Removing again is a no-op success.
    let response =
        handle_request(Request::RemoveWatch { id: watch_id }, &tm, &db, &sr, &bus, &test_pool());
    match response {
        Response::WatchRemoved { was_present, .. } => assert!(!was_present),
        other => panic!("Expected WatchRemoved, got: {other:?}"),
    }

    std::env::remove_var(crate::watch_registry::WATCHES_PATH_ENV);
}

// ===== Scheduled drain-and-restart (Issue #4090) =====

/// The pure drain-decision function (AC2 / AC3): zero in-flight always
/// completes (restart), even at/after the deadline; a passed deadline with
/// sweeps still in flight refuses (fail-safe) or forces per the flag; before
/// the deadline it keeps waiting.
#[test]
fn test_evaluate_drain_tick_decisions() {
    // Still in flight, deadline not reached ⇒ keep waiting.
    assert_eq!(evaluate_drain_tick(2, false, false), DrainTick::Continue);
    assert_eq!(evaluate_drain_tick(1, false, true), DrainTick::Continue);
    // Zero in-flight ⇒ complete regardless of deadline/force.
    assert_eq!(evaluate_drain_tick(0, false, false), DrainTick::Complete);
    assert_eq!(evaluate_drain_tick(0, true, false), DrainTick::Complete);
    assert_eq!(evaluate_drain_tick(0, true, true), DrainTick::Complete);
    // Deadline passed with work left: refuse (fail-safe) vs. force.
    assert_eq!(evaluate_drain_tick(3, true, false), DrainTick::TimedOutRefuse);
    assert_eq!(evaluate_drain_tick(3, true, true), DrainTick::TimedOutForce);
}

/// The "2 → 1 → 0" completion sequence (AC2): a supervisor stepping through
/// a decreasing in-flight count keeps waiting until it hits exactly zero,
/// then completes exactly once. Driven through the pure decision function so
/// no process actually exits.
#[test]
fn test_drain_tick_completes_only_at_zero() {
    let mut completed = 0;
    for n in [2usize, 1, 0] {
        match evaluate_drain_tick(n, false, false) {
            DrainTick::Continue => assert!(n > 0, "must still be waiting while n>0"),
            DrainTick::Complete => {
                assert_eq!(n, 0, "must only complete at zero");
                completed += 1;
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
    assert_eq!(completed, 1, "completes exactly once, at n==0");
}

/// The `DrainState` machine: begin sets the flag and a deadline, a second
/// begin is idempotent (does not stack / move the deadline), abort clears
/// the flag and bumps the generation, and the timeout path clears + notes.
#[test]
fn test_drain_state_lifecycle() {
    let drain = DrainState::new();
    assert!(!drain.is_draining());
    assert_eq!(drain.generation(), 0);

    // begin ⇒ Started, flag set, deadline recorded, generation bumped.
    let (gen1, deadline) = match drain.begin(Duration::from_secs(120), false, false) {
        DrainBegin::Started {
            generation,
            deadline,
        } => (generation, deadline),
        other => panic!("expected Started, got {other:?}"),
    };
    assert!(drain.is_draining());
    assert_eq!(gen1, 1);
    assert_eq!(drain.snapshot().deadline, Some(deadline));
    assert!(!drain.snapshot().force_after_timeout);

    // A second begin while already draining is idempotent: same generation,
    // same deadline, flag still set (AC edge: second drain does not stack).
    match drain.begin(Duration::from_secs(9999), true, false) {
        DrainBegin::AlreadyDraining {
            active_then_exit,
            escalated,
            force_escalated,
        } => {
            assert!(!active_then_exit, "active drain is still a relaunch drain");
            assert!(!escalated, "a then_exit=false request escalates nothing");
            assert!(
                !force_escalated,
                "#6007: force escalation applies only to a PENDING roll — a \
                     first-attempt drain's force flag stays pinned (#4521)"
            );
        }
        other => panic!("expected AlreadyDraining, got {other:?}"),
    }
    assert!(
        !drain.snapshot().force_after_timeout,
        "#4521 invariant: the active first-attempt drain's force flag is pinned"
    );
    assert_eq!(drain.generation(), gen1, "idempotent begin does not bump gen");
    assert_eq!(
        drain.snapshot().deadline,
        Some(deadline),
        "idempotent begin does not move the deadline"
    );

    // abort ⇒ flag cleared, generation bumped (so a live supervisor stops),
    // note recorded.
    assert!(drain.abort());
    assert!(!drain.is_draining());
    assert_eq!(drain.generation(), gen1 + 1);
    assert!(drain.snapshot().note.unwrap().contains("aborted"));
    // abort again ⇒ no-op.
    assert!(!drain.abort());

    // timeout resolution clears + notes + bumps generation.
    let gen_before = drain.generation();
    let _ = drain.begin(Duration::from_secs(1), false, false);
    drain.resolve_timeout("timed out".to_string());
    assert!(!drain.is_draining());
    assert_eq!(drain.snapshot().note.as_deref(), Some("timed out"));
    assert!(drain.generation() > gen_before);
}

/// Issue #4521 AC1 — `then_exit` on the already-draining path is escalated
/// **one way** (relaunch → stay-down) and the outcome reported back is the
/// ACTIVE drain's terminal action, never a blind echo of the request.
#[test]
fn test_drain_then_exit_escalates_one_way() {
    // A relaunch-drain is in flight (this is the auto-update roll's shape:
    // `then_exit=false`).
    let drain = DrainState::new();
    let deadline = match drain.begin(Duration::from_secs(120), false, false) {
        DrainBegin::Started { deadline, .. } => deadline,
        other => panic!("expected Started, got {other:?}"),
    };
    assert!(!drain.snapshot().then_exit);

    // An operator teardown request lands mid-roll: it must NOT be silently
    // ignored (the #4521 defect) — the active drain escalates to stay-down.
    match drain.begin(Duration::from_secs(9999), true, true) {
        DrainBegin::AlreadyDraining {
            active_then_exit,
            escalated,
            force_escalated,
        } => {
            assert!(active_then_exit, "the active drain now stays down");
            assert!(escalated, "the escalation must be reported to the caller");
            assert!(!force_escalated, "no roll is pending, so force stays pinned (#4521 / #6007)");
        }
        other => panic!("expected AlreadyDraining, got {other:?}"),
    }
    assert!(
        drain.snapshot().then_exit,
        "the escalation must be visible to the already-running supervisor, \
             which re-reads the descriptor"
    );
    // Everything else about the active drain is still pinned.
    assert_eq!(drain.snapshot().deadline, Some(deadline));
    assert!(!drain.snapshot().force_after_timeout);

    // Escalating again is a no-op that still reports the truth.
    match drain.begin(Duration::from_secs(1), false, true) {
        DrainBegin::AlreadyDraining {
            active_then_exit,
            escalated,
            ..
        } => {
            assert!(active_then_exit);
            assert!(!escalated, "already stay-down — nothing to escalate");
        }
        other => panic!("expected AlreadyDraining, got {other:?}"),
    }

    // A relaunch request against an active teardown drain must NOT downgrade
    // it: the reply still says "will stay down".
    match drain.begin(Duration::from_secs(1), false, false) {
        DrainBegin::AlreadyDraining {
            active_then_exit,
            escalated,
            ..
        } => {
            assert!(active_then_exit, "then-exit is never downgraded");
            assert!(!escalated);
        }
        other => panic!("expected AlreadyDraining, got {other:?}"),
    }
    assert!(drain.snapshot().then_exit);

    // After an abort, a fresh drain starts from the requested terminal
    // action again (the escalation does not leak across drains).
    assert!(drain.abort());
    match drain.begin(Duration::from_secs(30), false, false) {
        DrainBegin::Started { .. } => {}
        other => panic!("expected Started, got {other:?}"),
    }
    assert!(!drain.snapshot().then_exit, "a fresh drain honors its own then_exit");
}

/// Issue #4521 AC3 — the drain-completion exit-code contract: a then-exit
/// drain must exit `EXIT_SHUTDOWN` (143, **non-zero**) so a launchd job with
/// `KeepAlive:{SuccessfulExit:true}` stays down; a relaunch drain exits
/// `EXIT_RESTART` (0) so it comes straight back. Exiting 0 on the then-exit
/// path is the "drained, then relaunched anyway" failure.
#[test]
fn test_drain_exit_code_selection() {
    assert_eq!(drain_exit_code(true), EXIT_SHUTDOWN);
    assert_eq!(drain_exit_code(true), 143);
    assert_ne!(drain_exit_code(true), 0, "then-exit must never exit 0");
    assert_eq!(drain_exit_code(false), EXIT_RESTART);
    assert_eq!(drain_exit_code(false), 0);
}

/// Issue #4521 AC3 — the supervisor's branch selection follows the LIVE
/// descriptor, not a value captured when it was spawned. This is what makes
/// a mid-drain escalation effective: the supervisor re-reads `then_exit`
/// from `DrainState` on each tick (see `run_drain_supervisor`), so the same
/// read modeled here flips 0 → 143 after an escalation.
#[test]
fn test_supervisor_branch_follows_live_then_exit() {
    let drain = DrainState::new();
    let _ = drain.begin(Duration::from_secs(120), false, false);

    // Tick 1 (pre-escalation): zero in-flight ⇒ Complete ⇒ relaunch exit.
    assert_eq!(evaluate_drain_tick(0, false, false), DrainTick::Complete);
    assert_eq!(drain_exit_code(drain.snapshot().then_exit), EXIT_RESTART);

    // An operator teardown request escalates the drain in place.
    let _ = drain.begin(Duration::from_secs(120), false, true);

    // Tick 2 (post-escalation, same supervisor): the very same read now
    // selects the stay-down exit.
    assert_eq!(evaluate_drain_tick(0, false, false), DrainTick::Complete);
    assert_eq!(drain_exit_code(drain.snapshot().then_exit), EXIT_SHUTDOWN);

    // The forced-timeout terminal branch reads the same field.
    assert_eq!(evaluate_drain_tick(2, true, true), DrainTick::TimedOutForce);
    assert_eq!(drain_exit_code(drain.snapshot().then_exit), EXIT_SHUTDOWN);
}

/// Issue #4521 AC4 (regression pin) — the two drain-complete log lines stay
/// **distinct**, so a host log tells an operator which terminal action fired
/// without guessing. Asserted against the exact bodies
/// `run_drain_supervisor`'s `DrainTick::Complete` arm emits.
#[test]
fn test_drain_complete_log_lines_remain_distinct() {
    let then_exit_line = drain_complete_log_line(true, "launchd", 30);
    let relaunch_line = drain_complete_log_line(false, "launchd", 30);
    assert_ne!(then_exit_line, relaunch_line);
    assert!(then_exit_line.contains("staying down"));
    assert!(then_exit_line.contains("143"));
    assert!(relaunch_line.contains("supervised relaunch"));
    assert!(!relaunch_line.contains("staying down"));
}

/// Issue #6969 AC2 — the relaunch line states BOTH the expected relaunch
/// path (which supervisor mechanism) and the detached verifier's bound, so
/// a future gap (the ~4-minute launchd observation this issue records) is
/// attributable from the log alone.
#[test]
fn test_drain_complete_log_line_states_relaunch_path_and_verifier_bound() {
    let relaunch_line = drain_complete_log_line(false, "launchd", 45);
    assert!(relaunch_line.contains("launchd"));
    assert!(relaunch_line.contains("KeepAlive"));
    assert!(relaunch_line.contains("45s"));
    assert!(relaunch_line.contains("verify-only"));
    // The then_exit branch has no relaunch to verify, so it must not carry
    // the note even though a bound is technically passed in.
    let then_exit_line = drain_complete_log_line(true, "launchd", 45);
    assert!(!then_exit_line.contains("verify-only"));
}

#[test]
fn test_relaunch_verify_note_names_path_and_bound() {
    let note = relaunch_verify_note("systemd", 30);
    assert!(note.contains("systemd"));
    assert!(note.contains("30s"));
    assert!(note.contains("watchdog"));
    // #7707 review: the systemd note must flag the detached verifier as
    // best-effort rather than promising a bound `KillMode=mixed` prevents
    // it from delivering; the launchd note carries no such caveat because
    // `process_group(0)` really does let the child survive there.
    assert!(
        note.contains("best-effort under systemd"),
        "expected the KillMode=mixed caveat, got: {note}"
    );
    let launchd = relaunch_verify_note("launchd", 30);
    assert!(
        !launchd.contains("best-effort"),
        "launchd's verifier is not best-effort — it survives the pgid sweep: {launchd}"
    );
}

/// Issue #5340 (AC: the `TimedOutRefuse` message names the exact local
/// retry command instead of leaving the operator to guess at a nonexistent
/// bare `drain` subcommand or the unrelated `fleet drain <ssh_host>`
/// remote-decommission command). Asserted against the exact body
/// `run_drain_supervisor`'s `DrainTick::TimedOutRefuse` arm records via
/// `DrainState::resolve_timeout` and that `loom-daemon status` renders
/// verbatim as `Drain: not draining (last: <note>)`.
#[test]
fn test_drain_timeout_refuse_note_names_exact_retry_command() {
    let note = drain_timeout_refuse_note(3);
    assert!(
        note.contains("3 sweep(s) still in flight"),
        "expected the in-flight count in the note, got: {note}"
    );
    assert!(
        note.contains("loom-daemon restart --drain --force-after-timeout --timeout <secs>"),
        "expected the exact LOCAL retry command (no `fleet` prefix, no ssh_host arg), \
             got: {note}"
    );
    assert!(
        !note.contains("fleet drain"),
        "must not point at the unrelated remote worker-decommission command: {note}"
    );
    // Regression pin (#4090): the original refusal wording survives verbatim
    // as a prefix so `loom-daemon status`'s rendering and any log-scraping
    // tooling keyed on it keep matching.
    assert!(
        note.starts_with(
            "drain timed out with 3 sweep(s) still in flight — refused restart \
                 (no --force-after-timeout); dispatch resumed, daemon stays up."
        ),
        "expected the original refusal prefix to survive verbatim, got: {note}"
    );
}

// ===== Pending roll: drain/work-finder livelock (Issue #6007) =====

/// The refusal policy is a *widen-then-give-up* sequence, not an unbounded
/// hold: each refusal re-arms a geometrically wider window (the operator's
/// manual "re-run with a bigger --timeout" workaround, automated), capped by
/// [`MAX_DRAIN_RETRY_WINDOW_SECS`] and by whatever total paused-dispatch
/// budget remains — and once the budget is spent it abandons the roll so a
/// wedged sweep can never starve the host of work forever.
#[test]
fn test_drain_refusal_decision_widens_then_abandons() {
    let base = Duration::from_secs(1800);
    let budget = drain_pending_budget(base);
    assert_eq!(budget, Duration::from_secs(7200), "4 × the requested timeout");

    // First refusal, at the original 1800s deadline: re-arm 2 × base.
    match drain_refusal_decision(base, 0, Duration::from_secs(1800)) {
        RefusalDecision::Defer { window } => {
            assert_eq!(window, Duration::from_secs(3600), "widened to 2 × base");
        }
        other => panic!("expected Defer, got {other:?}"),
    }
    // Second refusal, 5400s in: 4 × base would be 7200s but only 1800s of
    // budget remains, so the window is clamped to the remaining budget.
    match drain_refusal_decision(base, 1, Duration::from_secs(5400)) {
        RefusalDecision::Defer { window } => {
            assert_eq!(window, Duration::from_secs(1800), "clamped to remaining budget");
        }
        other => panic!("expected Defer, got {other:?}"),
    }
    // Budget spent ⇒ abandon (dispatch resumes — the pre-#6007 outcome, but
    // only after the roll genuinely tried).
    assert_eq!(
        drain_refusal_decision(base, 2, Duration::from_secs(7200)),
        RefusalDecision::Abandon
    );
    // Less than a useful window left ⇒ abandon rather than arm a stub window.
    assert_eq!(
        drain_refusal_decision(base, 2, Duration::from_secs(7200 - 30)),
        RefusalDecision::Abandon
    );

    // A single window never exceeds the hard cap, however large the base.
    match drain_refusal_decision(Duration::from_secs(3600), 3, Duration::from_secs(60)) {
        RefusalDecision::Defer { window } => {
            assert_eq!(window, Duration::from_secs(MAX_DRAIN_RETRY_WINDOW_SECS));
        }
        other => panic!("expected Defer, got {other:?}"),
    }
}

/// The budget follows the operator's own `--timeout` (so a deliberately short
/// drain stays short) and is capped in absolute terms (so an enormous
/// `--timeout` cannot quiesce a host for a day).
#[test]
fn test_drain_pending_budget_scales_and_caps() {
    assert_eq!(drain_pending_budget(Duration::from_secs(60)), Duration::from_secs(240));
    assert_eq!(
        drain_pending_budget(Duration::from_secs(DEFAULT_DRAIN_TIMEOUT_SECS)),
        Duration::from_secs(7200)
    );
    assert_eq!(
        drain_pending_budget(Duration::from_secs(100_000)),
        Duration::from_secs(MAX_DRAIN_PENDING_BUDGET_SECS)
    );
    // A zero timeout buys no pending window at all — the very first refusal
    // abandons, i.e. exactly the pre-#6007 behavior.
    assert_eq!(drain_pending_budget(Duration::ZERO), Duration::ZERO);
    assert_eq!(
        drain_refusal_decision(Duration::ZERO, 0, Duration::ZERO),
        RefusalDecision::Abandon
    );
}

/// **The livelock regression test.** A refused deadline on a relaunch (roll)
/// drain must NOT hand the admission window back to the work finder: the
/// pause flag stays set, the roll is marked pending, the generation is
/// unchanged (so the *same* supervisor keeps polling), and the deadline is
/// re-armed. Only when the budget is spent does it clear the flag.
///
/// Pre-#6007 this path called `resolve_timeout`, which cleared the flag on
/// the very first refusal — the work finder then admitted more sweeps and the
/// next drain was strictly harder to satisfy, so a busy host never rolled.
#[test]
fn test_roll_refusal_keeps_dispatch_paused_and_retains_the_roll() {
    let drain = DrainState::new();
    let base = Duration::from_secs(1800);
    let gen = match drain.begin(base, false, false) {
        DrainBegin::Started { generation, .. } => generation,
        other => panic!("expected Started, got {other:?}"),
    };
    assert!(drain.is_draining());
    let started = drain
        .snapshot()
        .started_at
        .expect("begin records started_at");

    // First deadline refusal.
    match drain.refuse_roll_deadline(started + chrono::Duration::seconds(1800)) {
        RollRefusal::Deferred {
            attempt,
            window,
            budget,
            ..
        } => {
            assert_eq!(attempt, 1);
            assert_eq!(window, Duration::from_secs(3600));
            assert_eq!(budget, Duration::from_secs(7200));
        }
        other => panic!("expected Deferred, got {other:?}"),
    }
    assert!(
        drain.is_draining(),
        "#6007: a refused roll must NOT resume dispatch — that is the livelock"
    );
    assert!(drain.snapshot().roll_pending, "the roll intent survives the refusal");
    assert!(drain.snapshot().active, "the drain is still active");
    assert_eq!(
        drain.generation(),
        gen,
        "the same supervisor must keep supervising (no generation bump)"
    );
    assert_eq!(
        drain.snapshot().deadline,
        Some(started + chrono::Duration::seconds(1800) + chrono::Duration::seconds(3600)),
        "the deadline is re-armed, not cleared"
    );

    // Second refusal: still pending, still paused, attempt counter advances.
    match drain.refuse_roll_deadline(started + chrono::Duration::seconds(5400)) {
        RollRefusal::Deferred {
            attempt, window, ..
        } => {
            assert_eq!(attempt, 2);
            assert_eq!(window, Duration::from_secs(1800));
        }
        other => panic!("expected Deferred, got {other:?}"),
    }
    assert!(drain.is_draining());
    assert_eq!(drain.generation(), gen);

    // Budget spent: the roll gives up, dispatch resumes, and the supervisor
    // is retired via a generation bump.
    match drain.refuse_roll_deadline(started + chrono::Duration::seconds(7200)) {
        RollRefusal::Abandoned {
            attempts, elapsed, ..
        } => {
            assert_eq!(attempts, 2, "two re-arms happened before giving up");
            assert_eq!(elapsed, Duration::from_secs(7200));
        }
        other => panic!("expected Abandoned, got {other:?}"),
    }
    assert!(!drain.is_draining(), "an abandoned roll resumes dispatch");
    assert!(!drain.snapshot().roll_pending);
    assert!(!drain.snapshot().active);
    assert!(drain.generation() > gen, "the stale supervisor must be retired");
}

/// AC2 — a pending roll converges without an operator: the retained roll's
/// supervisor is still the current generation, so the very next tick that
/// observes zero in-flight completes the restart. Driven through the same
/// pure decision function the supervisor uses.
#[test]
fn test_pending_roll_rearms_and_completes_when_in_flight_hits_zero() {
    let drain = DrainState::new();
    let gen = match drain.begin(Duration::from_secs(1800), false, false) {
        DrainBegin::Started { generation, .. } => generation,
        other => panic!("expected Started, got {other:?}"),
    };
    let started = drain.snapshot().started_at.expect("started_at");

    // Deadline passes with work in flight ⇒ refuse ⇒ roll retained.
    assert_eq!(evaluate_drain_tick(3, true, false), DrainTick::TimedOutRefuse);
    assert!(matches!(
        drain.refuse_roll_deadline(started + chrono::Duration::seconds(1800)),
        RollRefusal::Deferred { .. }
    ));

    // Dispatch is still paused, so the in-flight set can actually reach zero.
    // The next tick that sees zero completes — with the relaunch exit code,
    // and from the SAME supervisor generation (nothing re-issued the command).
    assert_eq!(evaluate_drain_tick(0, true, false), DrainTick::Complete);
    assert_eq!(drain.generation(), gen);
    assert_eq!(drain_exit_code(drain.snapshot().then_exit), EXIT_RESTART);
}

/// Only a **relaunch (roll)** drain retains its intent. A then-exit teardown
/// keeps the historical refuse-and-resume behavior, because `fleet drain`
/// detects a remote refusal by observing `drain.draining == false` on a
/// still-reachable daemon and reports it as its documented exit code 2.
#[test]
fn test_teardown_drain_keeps_the_historical_refuse_and_resume_path() {
    assert_eq!(drain_refusal_path(false), RefusalPath::RetainRoll);
    assert_eq!(drain_refusal_path(true), RefusalPath::ResumeDispatch);

    let drain = DrainState::new();
    let _ = drain.begin(Duration::from_secs(1800), false, true);
    assert!(drain.is_draining());
    // The supervisor's then-exit arm: resolve_timeout, exactly as before.
    drain.resolve_timeout(drain_timeout_refuse_note(2));
    assert!(!drain.is_draining(), "a refused teardown resumes dispatch immediately");
    assert!(!drain.snapshot().roll_pending);
    assert!(!drain.snapshot().active);
}

/// AC4 — the refusal message says what happens about the **recurrence**. The
/// pre-#6007 note's advice ("re-run with a larger --timeout") is precisely
/// what reproduced the livelock on a busy host, so the pending note must not
/// give it, must not claim dispatch resumed, and must name both operator
/// escape hatches.
#[test]
fn test_drain_roll_pending_note_addresses_the_recurrence() {
    let note = drain_roll_pending_note(3, 1, Duration::from_secs(3600), Duration::from_secs(7200));
    assert!(note.contains("3 sweep(s) still in flight"), "got: {note}");
    assert!(note.contains("ROLL PENDING (retry 1)"), "got: {note}");
    assert!(note.contains("stays PAUSED"), "got: {note}");
    assert!(note.contains("re-arms itself"), "got: {note}");
    assert!(note.contains("Nothing to re-run"), "got: {note}");
    assert!(note.contains("Next deadline in 3600s"), "got: {note}");
    assert!(note.contains("budget 7200s"), "got: {note}");
    assert!(note.contains("loom-daemon restart --abort-drain"), "got: {note}");
    assert!(
        note.contains("loom-daemon restart --drain --force-after-timeout"),
        "got: {note}"
    );
    // The two lies the old wording would tell on this path:
    assert!(
        !note.contains("dispatch resumed"),
        "dispatch is NOT resumed on a pending roll: {note}"
    );
    assert!(
        !note.contains("re-run with a larger --timeout if"),
        "must not re-issue the advice that reproduces the livelock: {note}"
    );
    assert!(!note.contains("fleet drain"), "got: {note}");
}

/// The give-up note keeps #5340's contract (same prefix, same exact retry
/// command) and adds the recurrence advice that actually applies once the
/// roll has already waited out its whole budget with dispatch paused: the
/// sweep is stuck, so cancel it rather than widening the window again.
#[test]
fn test_drain_roll_abandoned_note_keeps_the_5340_contract() {
    let note = drain_roll_abandoned_note(2, 2, Duration::from_secs(7200));
    assert!(
        note.starts_with(
            "drain timed out with 2 sweep(s) still in flight — refused restart \
                 (no --force-after-timeout); dispatch resumed, daemon stays up."
        ),
        "the #4090/#5340 prefix must survive verbatim, got: {note}"
    );
    assert!(
        note.contains("loom-daemon restart --drain --force-after-timeout --timeout <secs>"),
        "got: {note}"
    );
    assert!(note.contains("re-armed 2 time(s)"), "got: {note}");
    assert!(note.contains("7200s of PAUSED dispatch"), "got: {note}");
    assert!(note.contains("ABANDONED"), "got: {note}");
    assert!(note.contains("loom-daemon cancel --sweep <id>"), "got: {note}");
    assert!(!note.contains("fleet drain"), "got: {note}");
}

/// The pending note tells the operator to run
/// `restart --drain --force-after-timeout`; that command must actually do
/// something. On a **pending** roll it escalates the force flag one-way and
/// pulls the re-armed deadline in to now, so the next supervisor tick reaches
/// `TimedOutForce` — while a first-attempt drain keeps #4521's pinning.
#[test]
fn test_force_escalation_applies_only_to_a_pending_roll() {
    let drain = DrainState::new();
    let base = Duration::from_secs(1800);
    let first_deadline = match drain.begin(base, false, false) {
        DrainBegin::Started { deadline, .. } => deadline,
        other => panic!("expected Started, got {other:?}"),
    };

    // Before any refusal: force stays pinned and the deadline does not move.
    match drain.begin(base, true, false) {
        DrainBegin::AlreadyDraining {
            force_escalated, ..
        } => assert!(!force_escalated, "no roll pending yet ⇒ #4521 pinning holds"),
        other => panic!("expected AlreadyDraining, got {other:?}"),
    }
    assert!(!drain.snapshot().force_after_timeout);
    assert_eq!(drain.snapshot().deadline, Some(first_deadline));

    // Refuse once ⇒ the roll is pending.
    let started = drain.snapshot().started_at.expect("started_at");
    assert!(matches!(
        drain.refuse_roll_deadline(started + chrono::Duration::seconds(1800)),
        RollRefusal::Deferred { .. }
    ));
    let rearmed = drain.snapshot().deadline.expect("re-armed deadline");

    // Now the documented force command takes effect immediately.
    match drain.begin(base, true, false) {
        DrainBegin::AlreadyDraining {
            force_escalated, ..
        } => assert!(force_escalated, "a pending roll honors --force-after-timeout"),
        other => panic!("expected AlreadyDraining, got {other:?}"),
    }
    assert!(drain.snapshot().force_after_timeout);
    let pulled_in = drain.snapshot().deadline.expect("deadline");
    assert!(
        pulled_in < rearmed,
        "the re-armed deadline must be pulled in to now ({pulled_in} vs {rearmed})"
    );
    assert!(pulled_in <= Utc::now(), "already past ⇒ the next tick forces through");
    assert_eq!(evaluate_drain_tick(2, true, true), DrainTick::TimedOutForce);
    assert!(
        drain
            .snapshot()
            .note
            .as_deref()
            .is_some_and(|n| n.contains("--force-after-timeout")),
        "the escalation must be visible in status"
    );
}

/// An operator's `--abort-drain` is the way OUT of a retained roll: it clears
/// the pending state, resumes dispatch, and says so (a bare "dispatch
/// resumed" would read identically to aborting a first-attempt drain).
#[test]
fn test_abort_clears_a_pending_roll_and_explains_itself() {
    let drain = DrainState::new();
    let _ = drain.begin(Duration::from_secs(1800), false, false);
    let started = drain.snapshot().started_at.expect("started_at");
    assert!(matches!(
        drain.refuse_roll_deadline(started + chrono::Duration::seconds(1800)),
        RollRefusal::Deferred { .. }
    ));
    assert!(drain.snapshot().roll_pending);

    assert!(drain.abort());
    assert!(!drain.is_draining());
    assert!(!drain.snapshot().roll_pending);
    let note = drain.snapshot().note.expect("abort records a note");
    assert!(note.contains("aborted"), "got: {note}");
    assert!(note.contains("pending roll"), "got: {note}");
}

/// A fresh drain never inherits a previous roll's retry history — otherwise a
/// host that abandoned one roll would give up faster on the next.
#[test]
fn test_fresh_drain_resets_the_pending_roll_bookkeeping() {
    let drain = DrainState::new();
    let _ = drain.begin(Duration::from_secs(1800), false, false);
    let started = drain.snapshot().started_at.expect("started_at");
    let _ = drain.refuse_roll_deadline(started + chrono::Duration::seconds(1800));
    assert_eq!(drain.snapshot().refusals, 1);
    assert!(drain.abort());

    let _ = drain.begin(Duration::from_secs(600), false, false);
    let snap = drain.snapshot();
    assert_eq!(snap.refusals, 0, "retry history must not leak across drains");
    assert!(!snap.roll_pending);
    assert_eq!(snap.base_timeout, Duration::from_secs(600));
}

/// AC5 (immediate unsupervised refusal): with no supervisor, a drain request
/// is refused with `accepted: false` **and the drain flag is never set** —
/// pausing dispatch then refusing would be a silent outage.
///
/// NOTE: shares the `LOOM_DAEMON_SUPERVISOR` env with
/// `test_build_restart_decision_supervisor_gated`; `#[serial]` keeps them
/// from racing the process-global env var.
#[test]
#[serial_test::serial]
fn test_drain_request_unsupervised_refuses_without_pausing() {
    std::env::remove_var("LOOM_DAEMON_SUPERVISOR");
    let (sr, dir, _rec) = setup_sweep_registry_in_tempdir();
    let root = dir.path().to_path_buf();
    let empty_reg = dir.path().join("no-such-workspaces.json");
    std::env::set_var(crate::workspace_registry::REGISTRY_PATH_ENV, &empty_reg);
    let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
    pool.seed(root.clone(), sr);
    let bus = Arc::new(EventBus::new());
    let drain = Arc::new(DrainState::new());

    let resp = handle_drain_request(&drain, &pool, &root, &bus, Some(60), false, false);
    match resp {
        Response::DaemonDrain {
            accepted,
            supervisor,
            message,
            ..
        } => {
            assert!(!accepted, "unsupervised host must refuse the drain");
            assert!(supervisor.is_none());
            // #4640: the refusal must mention the systemd retrofit for a
            // fleet worker provisioned before the fix (missing
            // LOOM_DAEMON_SUPERVISOR despite being systemd-supervised).
            assert!(
                message.contains("LOOM_DAEMON_SUPERVISOR=systemd"),
                "drain refusal must mention the systemd retrofit: {message}"
            );
            assert!(
                message.contains("Restart=on-success"),
                "drain refusal retrofit hint must include the corrected Restart= policy: {message}"
            );
        }
        other => panic!("expected DaemonDrain, got {other:?}"),
    }
    assert!(!drain.is_draining(), "refused drain must NOT pause dispatch");
    assert_eq!(drain.generation(), 0, "refused drain must not bump generation");

    std::env::remove_var(crate::workspace_registry::REGISTRY_PATH_ENV);
}

/// Issue #4521 AC1 — the `AlreadyDraining` ack reports the ACTIVE drain's
/// terminal action, not the requested one.
///
/// The pre-fix behavior echoed the request: an operator's
/// `--drain --then-exit` landing on an in-progress auto-update roll-drain
/// was acked `then_exit: true` ("will stop") while the daemon exited 0 and
/// launchd relaunched it — the exact incident shape.
///
/// No supervisor task is spawned on this path (only `DrainBegin::Started`
/// spawns one), so the drain state is primed with `DrainState::begin`
/// directly and the process is never at risk of the supervisor's `exit`.
#[test]
#[serial_test::serial]
fn test_already_draining_ack_reports_active_terminal_action() {
    let (sr, dir, _rec) = setup_sweep_registry_in_tempdir();
    let root = dir.path().to_path_buf();
    let empty_reg = dir.path().join("no-such-workspaces.json");
    std::env::set_var(crate::workspace_registry::REGISTRY_PATH_ENV, &empty_reg);
    let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
    pool.seed(root.clone(), sr);
    let bus = Arc::new(EventBus::new());

    // An auto-update roll-drain is already in flight (`then_exit=false`).
    let drain = Arc::new(DrainState::new());
    let _ = drain.begin(Duration::from_secs(600), false, false);

    // Operator teardown request during that window. `then_exit=true` skips
    // the supervisor gate, so no `LOOM_DAEMON_SUPERVISOR` is needed.
    let resp = handle_drain_request(&drain, &pool, &root, &bus, Some(60), false, true);
    match resp {
        Response::DaemonDrain {
            accepted,
            then_exit,
            ref message,
            ..
        } => {
            assert!(accepted);
            assert!(
                then_exit,
                "the ack must report the ACTIVE drain's terminal action (escalated to \
                     stay-down), not a blind echo"
            );
            assert!(message.contains("ESCALATED"), "message was: {message}");
        }
        other => panic!("expected DaemonDrain, got {other:?}"),
    }
    assert!(drain.snapshot().then_exit, "the active drain now stays down");

    // The reverse: a plain relaunch drain request against the now-teardown
    // drain must be acked with `then_exit: true` — it is NOT downgraded, and
    // the ack must not promise a restart that will never happen.
    std::env::set_var("LOOM_DAEMON_SUPERVISOR", "launchd");
    let resp = handle_drain_request(&drain, &pool, &root, &bus, Some(60), false, false);
    match resp {
        Response::DaemonDrain {
            accepted,
            then_exit,
            ref message,
            ..
        } => {
            assert!(accepted);
            assert!(then_exit, "then-exit is never downgraded");
            assert!(
                message.contains("not be honored") || message.contains("NOT relaunch"),
                "message was: {message}"
            );
        }
        other => panic!("expected DaemonDrain, got {other:?}"),
    }
    assert!(drain.snapshot().then_exit);

    std::env::remove_var("LOOM_DAEMON_SUPERVISOR");
    std::env::remove_var(crate::workspace_registry::REGISTRY_PATH_ENV);
}

/// Cross-root in-flight counting (Finding 5): sweeps live in the SECONDARY
/// managed repo only must still be counted, so a drain that reads them does
/// not restart while that repo has live work. Also asserts terminal sweeps
/// are excluded.
#[test]
#[serial_test::serial]
fn test_count_in_flight_sweeps_cross_root() {
    use crate::workspace_registry::{normalize_path, WorkspaceRegistry, REGISTRY_PATH_ENV};

    let (sr_a, dir_a, _rec_a) = setup_sweep_registry_in_tempdir();
    let (sr_b, dir_b, _rec_b) = setup_sweep_registry_in_tempdir();
    let root_a = dir_a.path().to_path_buf();
    let root_b = dir_b.path().to_path_buf();

    let reg_path = dir_a.path().join("workspaces.json");
    let mut reg = WorkspaceRegistry::default();
    reg.add(&root_a, None).unwrap();
    reg.add(&root_b, None).unwrap();
    reg.save(&reg_path).unwrap();
    std::env::set_var(REGISTRY_PATH_ENV, &reg_path);

    let canon_a = normalize_path(&root_a);
    let canon_b = normalize_path(&root_b);
    let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
    pool.seed(canon_a.clone(), sr_a);
    pool.seed(canon_b.clone(), sr_b.clone());

    // No sweeps anywhere ⇒ zero.
    assert_eq!(count_in_flight_sweeps(&pool, &canon_a), 0);

    // Dispatch a live sweep into the SECONDARY repo only.
    {
        let mut reg_b = sr_b.lock().unwrap();
        reg_b
            .dispatch(&crate::types::SweepKind::Issue(4090), None, None, None, None)
            .expect("dispatch");
    }
    // Counted even though the primary (root_a) registry is empty — a drain
    // reading only the primary would wrongly see zero and restart.
    assert_eq!(
        count_in_flight_sweeps(&pool, &canon_a),
        1,
        "a live sweep in the secondary repo must be counted"
    );

    std::env::remove_var(REGISTRY_PATH_ENV);
}

/// A pre-#4090 `DaemonStatus` JSON payload (no `drain` fields) still
/// deserializes — `#[serde(default)]` fills `draining: false` and leaves the
/// deadline/note `None` (mirrors the `capacity_bound` compat rationale).
#[test]
fn test_daemon_status_backward_compat_missing_drain_fields() {
    let legacy = r#"{"in_flight":[],"token_pool_size":2,"disk_headroom":9,"configured_max":3,"dynamic_cap":2,"main_health_gate_halted":false}"#;
    let report: DaemonStatusReport =
        serde_json::from_str(legacy).expect("legacy payload deserializes");
    assert!(!report.draining, "absent draining (#4090) defaults to false");
    assert_eq!(report.drain_deadline, None);
    assert_eq!(report.drain_note, None);
    // Pre-#4055 payload has no auto_update fields either — they default.
    assert!(!report.auto_update_enabled, "absent auto_update (#4055) defaults to disabled");
    assert_eq!(report.auto_update_last_check, None);
    assert_eq!(report.auto_update_last_roll, None);
    assert_eq!(report.auto_update_consecutive_failures, 0);
    assert_eq!(report.auto_update_backoff_secs, None);
    assert_eq!(report.auto_update_terminal_reason, None);
    assert_eq!(report.auto_update_note, None);
}

/// The new drain fields round-trip through serde, and
/// `build_daemon_status_with_drain` overlays the live drain state onto the
/// base report (AC4).
#[test]
#[serial_test::serial]
fn test_build_daemon_status_with_drain_overlays_state() {
    let (sr, dir, _rec) = setup_sweep_registry_in_tempdir();
    let root = dir.path().to_path_buf();
    let empty_reg = dir.path().join("no-such-workspaces.json");
    std::env::set_var(crate::workspace_registry::REGISTRY_PATH_ENV, &empty_reg);
    let prev_shared = std::env::var("LOOM_SHARED_TOKENS_DIR").ok();
    std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");

    let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
    pool.seed(root.clone(), sr);
    let health = WorkspaceHealthStates::new();
    let drain = DrainState::new();

    // No drain ⇒ overlay is a no-op.
    let report =
        build_daemon_status_with_drain(&pool, &health, &root, &test_credential_preflight(), &drain);
    assert!(!report.draining);
    assert_eq!(report.drain_deadline, None);

    // Begin a drain ⇒ overlay reports draining + deadline.
    let deadline = match drain.begin(Duration::from_secs(300), false, false) {
        DrainBegin::Started { deadline, .. } => deadline,
        other => panic!("expected Started, got {other:?}"),
    };
    let report =
        build_daemon_status_with_drain(&pool, &health, &root, &test_credential_preflight(), &drain);
    assert!(report.draining);
    assert_eq!(report.drain_deadline, Some(deadline));

    // Round-trips over the wire.
    let json = serde_json::to_string(&report).unwrap();
    let back: DaemonStatusReport = serde_json::from_str(&json).unwrap();
    assert!(back.draining);
    assert_eq!(back.drain_deadline, Some(deadline));

    match prev_shared {
        Some(v) => std::env::set_var("LOOM_SHARED_TOKENS_DIR", v),
        None => std::env::remove_var("LOOM_SHARED_TOKENS_DIR"),
    }
    std::env::remove_var(crate::workspace_registry::REGISTRY_PATH_ENV);
}

/// Wire-compat (Finding 3): the new `DrainAndRestartDaemon` variant
/// round-trips, and the untouched `RestartDaemon` unit variant STILL
/// serializes to exactly `{"type":"RestartDaemon"}`. The
/// `test_restart_daemon_request_response_round_trip` assertion above must
/// also keep passing unmodified.
#[test]
fn test_drain_request_wire_compat() {
    // RestartDaemon is unchanged — byte-for-byte the pre-#4090 shape.
    assert_eq!(
        serde_json::to_string(&Request::RestartDaemon).unwrap(),
        r#"{"type":"RestartDaemon"}"#
    );

    // The new variant round-trips with its payload.
    let req = Request::DrainAndRestartDaemon {
        timeout_secs: Some(600),
        force_after_timeout: true,
        then_exit: false,
    };
    let json = serde_json::to_string(&req).unwrap();
    let back: Request = serde_json::from_str(&json).unwrap();
    match back {
        Request::DrainAndRestartDaemon {
            timeout_secs,
            force_after_timeout,
            then_exit,
        } => {
            assert_eq!(timeout_secs, Some(600));
            assert!(force_after_timeout);
            assert!(!then_exit);
        }
        other => panic!("expected DrainAndRestartDaemon, got {other:?}"),
    }

    // Pre-#4343 wire data (no `then_exit` key at all) still parses, as
    // `then_exit: false` — the original #4090 restart-when-drained
    // behavior.
    let legacy_json = r#"{"type":"DrainAndRestartDaemon","payload":{"timeout_secs":600,"force_after_timeout":true}}"#;
    let back: Request = serde_json::from_str(legacy_json).unwrap();
    match back {
        Request::DrainAndRestartDaemon { then_exit, .. } => assert!(!then_exit),
        other => panic!("expected DrainAndRestartDaemon, got {other:?}"),
    }

    // AbortDrain round-trips (unit-with-payload-none shape).
    let json = serde_json::to_string(&Request::AbortDrain).unwrap();
    assert_eq!(json, r#"{"type":"AbortDrain"}"#);
    let back: Request = serde_json::from_str(&json).unwrap();
    assert!(matches!(back, Request::AbortDrain));

    // The DaemonDrain response round-trips, including `then_exit`.
    let resp = Response::DaemonDrain {
        accepted: true,
        supervisor: Some("launchd".to_string()),
        in_flight: 3,
        message: "draining".to_string(),
        then_exit: true,
    };
    let json = serde_json::to_string(&resp).unwrap();
    let back: Response = serde_json::from_str(&json).unwrap();
    match back {
        Response::DaemonDrain {
            accepted,
            supervisor,
            in_flight,
            then_exit,
            ..
        } => {
            assert!(accepted);
            assert_eq!(supervisor.as_deref(), Some("launchd"));
            assert_eq!(in_flight, 3);
            assert!(then_exit);
        }
        other => panic!("expected DaemonDrain, got {other:?}"),
    }
}

/// Abort clears the flag and bumps the generation, so a supervisor holding
/// the old generation stops without exiting — even if in-flight later
/// reaches zero (AC6, the "abort then the queue empties anyway" race). We
/// assert the generation contract the async supervisor relies on rather than
/// spawning it (its Complete branch calls `process::exit`).
#[test]
fn test_abort_supersedes_running_supervisor_generation() {
    let drain = DrainState::new();
    let gen = match drain.begin(Duration::from_secs(300), false, false) {
        DrainBegin::Started { generation, .. } => generation,
        other => panic!("expected Started, got {other:?}"),
    };
    // A supervisor captured `gen`; abort moves the generation on.
    assert!(drain.abort());
    assert_ne!(
        drain.generation(),
        gen,
        "abort must bump the generation so the running supervisor detects supersession"
    );
    assert!(!drain.is_draining(), "abort resumes dispatch");
}
