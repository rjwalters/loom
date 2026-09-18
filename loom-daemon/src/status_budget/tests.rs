//! Unit + integration coverage for the #8163 root-count-aware status budget.

use super::*;

// ===================================================================
// The cost model itself
// ===================================================================

/// A single-workspace host must get **exactly** the pre-#8163 shape: the
/// per-root term contributes one root's worth, and nothing about root
/// scaling can shrink a budget below the fixed floor.
#[test]
fn a_single_root_budget_is_the_fixed_floor_plus_one_root() {
    assert_eq!(status_build_budget(1), STATUS_BUILD_FIXED_BUDGET + STATUS_BUILD_PER_ROOT_BUDGET);
    assert_eq!(
        client_probe_budget(1),
        (STATUS_BUILD_FIXED_BUDGET + STATUS_BUILD_PER_ROOT_BUDGET) * PROBE_BUDGET_SAFETY_FACTOR
    );
}

/// The whole point of #8163: the budget *grows with the root count*. A host
/// with several dozen workspaces must be given materially more than the
/// fixed `10s` escalated retry the pre-#8163 client used, because the builds
/// that issue measured were `13.1s`/`14.3s`.
#[test]
fn the_probe_budget_covers_the_builds_issue_8163_measured() {
    // "several dozen" from the #8163 report, read conservatively.
    let observed_worst_build = Duration::from_millis(14_300);
    let pre_8163_fixed_escalated_budget = Duration::from_secs(10);
    assert!(
        observed_worst_build > pre_8163_fixed_escalated_budget,
        "sanity: the reported build really did exceed the old fixed budget"
    );
    assert!(
        client_probe_budget(36) > observed_worst_build,
        "a 36-root host must be budgeted above the worst build #8163 measured, got {:?}",
        client_probe_budget(36)
    );
}

/// Monotonic in the root count, so a host that registers one more workspace
/// can never be given a *smaller* budget.
#[test]
fn budgets_are_monotonic_in_root_count() {
    let mut prev = status_build_budget(0);
    for n in 1..=DOCUMENTED_MAX_ROOTS * 2 {
        let next = status_build_budget(n);
        assert!(next >= prev, "status_build_budget regressed at n={n}");
        prev = next;
    }
}

/// A pathological/corrupted root count can never hang a one-shot CLI
/// invocation: the client budget is capped, and `usize::MAX` must not
/// overflow the `Duration` arithmetic.
#[test]
fn the_client_budget_is_capped_and_overflow_safe() {
    assert_eq!(client_probe_budget(usize::MAX), MAX_ROOT_SCALED_PROBE_TIMEOUT);
    assert!(client_probe_budget(DOCUMENTED_MAX_ROOTS) <= MAX_ROOT_SCALED_PROBE_TIMEOUT);
    // `status_build_budget` itself is allowed to exceed the client cap (it is
    // a *cost model*, not a wait), but must still not panic.
    let _ = status_build_budget(usize::MAX);
}

/// An unreadable/absent registry resolves to one root — the same fallback
/// `WorkspaceRegistry::effective_roots` applies — never zero, which would
/// budget below the fixed floor's intent.
#[test]
#[serial_test::serial]
fn an_absent_registry_counts_as_one_root() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("does-not-exist.json");
    std::env::set_var(crate::workspace_registry::REGISTRY_PATH_ENV, &missing);
    let count = registered_root_count();
    std::env::remove_var(crate::workspace_registry::REGISTRY_PATH_ENV);
    assert_eq!(count, 1);
}

/// The client reads the root count from the same host-level registry file the
/// daemon walks, so the two sides cannot disagree about how many roots the
/// build will cost.
#[test]
#[serial_test::serial]
fn the_root_count_comes_from_the_shared_registry_file() {
    let dir = tempfile::tempdir().unwrap();
    let reg_path = dir.path().join("workspaces.json");
    let mut reg = crate::workspace_registry::WorkspaceRegistry::default();
    for i in 0..5 {
        let root = dir.path().join(format!("repo-{i}"));
        std::fs::create_dir_all(&root).unwrap();
        reg.add(&root, None).unwrap();
    }
    reg.save(&reg_path).unwrap();
    std::env::set_var(crate::workspace_registry::REGISTRY_PATH_ENV, &reg_path);
    let count = registered_root_count();
    std::env::remove_var(crate::workspace_registry::REGISTRY_PATH_ENV);
    assert_eq!(count, 5);
    assert!(
        client_probe_budget(count) > client_probe_budget(1),
        "more roots must buy a wider client budget"
    );
}

// ===================================================================
// #8163 AC4 — synthetic registry at the documented root ceiling
// ===================================================================

/// A process-wide leaked runtime handle so a [`crate::workspace_pool::WorkspacePool`]
/// can be built in a synchronous `#[test]` (same fixture rationale as
/// `ipc::tests::test_runtime_handle`, duplicated rather than re-exported so
/// this suite does not depend on another module's private test scaffolding).
fn test_runtime_handle() -> tokio::runtime::Handle {
    use std::sync::OnceLock;
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| tokio::runtime::Runtime::new().unwrap())
        .handle()
        .clone()
}

fn test_credential_preflight() -> crate::types::CredentialPreflightReport {
    crate::types::CredentialPreflightReport {
        ok: true,
        mechanism: "test-fixture".to_string(),
        fingerprint: None,
        message: "test fixture — not a real preflight".to_string(),
        checked_at: chrono::Utc::now(),
    }
}

/// **Issue #8163 AC4.** A synthetic registry at [`DOCUMENTED_MAX_ROOTS`] must
/// build a `DaemonStatusReport` within the budget the client sizes its probe
/// from. This is the regression guard the issue asks for: before #8163 there
/// was no documented ceiling at all, so "the status build stays under the
/// probe budget" was not a falsifiable claim, and the existing multi-workspace
/// coverage used a handful of roots rather than "several dozen".
///
/// The assertion is against [`client_probe_budget`] (what a `health`
/// invocation actually waits) rather than [`status_build_budget`] (the
/// tighter daemon-side target), because CI runners are contended and the
/// point of the test is the *timeout* regression, not a microbenchmark. The
/// per-root cost is asserted separately, and loosely, so a change that makes
/// the loop an order of magnitude more expensive still fails here even if the
/// absolute number stays small on a fast runner.
#[test]
#[serial_test::serial]
fn the_status_build_stays_within_budget_at_the_documented_root_ceiling() {
    use crate::main_health_gate::WorkspaceHealthStates;
    use crate::workspace_pool::WorkspacePool;
    use crate::workspace_registry::{WorkspaceRegistry, REGISTRY_PATH_ENV};
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let reg_path = dir.path().join("workspaces.json");
    let mut reg = WorkspaceRegistry::default();
    let mut roots = Vec::with_capacity(DOCUMENTED_MAX_ROOTS);
    for i in 0..DOCUMENTED_MAX_ROOTS {
        let root = dir.path().join(format!("repo-{i:02}"));
        std::fs::create_dir_all(root.join(".loom")).unwrap();
        reg.add(&root, None).unwrap();
        roots.push(root);
    }
    reg.save(&reg_path).unwrap();
    std::env::set_var(REGISTRY_PATH_ENV, &reg_path);

    let pool = Arc::new(WorkspacePool::new(
        Arc::new(crate::event_bus::EventBus::new()),
        test_runtime_handle(),
    ));
    let health = WorkspaceHealthStates::new();

    // Warm the pool exactly as a running daemon's would be — the #8163
    // failure is a *steady-state* one (`status` is polled continuously), not
    // a first-call provisioning cost.
    let report =
        crate::ipc::build_daemon_status(&pool, &health, &roots[0], &test_credential_preflight());
    assert_eq!(report.per_repo.len(), DOCUMENTED_MAX_ROOTS, "every root is walked");

    let started = std::time::Instant::now();
    let report =
        crate::ipc::build_daemon_status(&pool, &health, &roots[0], &test_credential_preflight());
    let elapsed = started.elapsed();
    std::env::remove_var(REGISTRY_PATH_ENV);

    assert_eq!(report.per_repo.len(), DOCUMENTED_MAX_ROOTS);
    assert!(
        elapsed < client_probe_budget(DOCUMENTED_MAX_ROOTS),
        "build_daemon_status over {DOCUMENTED_MAX_ROOTS} roots took {elapsed:?}, which the \
         health client's {:?} probe budget would not cover — this is exactly the #8163 \
         regression (probe times out, `health` reports indeterminate-busy on an idle daemon)",
        client_probe_budget(DOCUMENTED_MAX_ROOTS),
    );
    let per_root = elapsed / u32::try_from(DOCUMENTED_MAX_ROOTS).unwrap();
    assert!(
        per_root < STATUS_BUILD_PER_ROOT_BUDGET * 4,
        "per-root status-build cost {per_root:?} is far above the {STATUS_BUILD_PER_ROOT_BUDGET:?} \
         the client budget is modelled on — the model, or the loop, has drifted"
    );
}
