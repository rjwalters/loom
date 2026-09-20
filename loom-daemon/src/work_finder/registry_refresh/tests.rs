//! Tests for [`refresh_local_workspace_state`](super::refresh_local_workspace_state)
//! (issue #8121).
//!
//! `loom-daemon workspace add <path> --priority N` writes the machine
//! registry and reports success immediately, but before #8121 the tick loop
//! only reloaded that registry (and provisioned a new root's
//! `SweepRegistry`/reaper/watchdog via `WorkspacePool::get_or_provision`)
//! AFTER the rate-limit circuit breaker's early-`continue`, so a workspace
//! registered while the breaker was suppressing gh polling would sit
//! un-provisioned — invisible to `loom-daemon status` and dispatch — for the
//! breaker's entire suppression window. These tests exercise the extracted
//! `refresh_local_workspace_state` helper the tick loop now calls BEFORE that
//! breaker check, proving the reload + provisioning is a pure,
//! always-safe-to-call local operation that never needs to consult (or be
//! gated by) the breaker at all.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use serial_test::serial;

use super::refresh_local_workspace_state;
use crate::event_bus::EventBus;
use crate::workspace_pool::WorkspacePool;
use crate::workspace_registry::WorkspaceRegistry;

/// Build a workspace pool wired to a throwaway event bus, mirroring
/// `workspace_pool::tests::pool()` (not shared across modules).
fn local_state_pool() -> Arc<WorkspacePool> {
    Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), tokio::runtime::Handle::current()))
}

/// Point `WorkspaceRegistry::load_default` at a scratch file for the
/// duration of the closure, restoring the previous value afterward. Not
/// `#[serial]`-safe on its own — callers must serialize with other tests
/// that touch `LOOM_WORKSPACES_PATH` (mirrors the pattern already used by
/// `workspace_registry::tests::default_registry_path_honours_env_override`
/// and `cli::tokens`'s tests).
fn with_scratch_registry_path<T>(path: &Path, f: impl FnOnce() -> T) -> T {
    std::env::set_var(crate::workspace_registry::REGISTRY_PATH_ENV, path);
    let result = f();
    std::env::remove_var(crate::workspace_registry::REGISTRY_PATH_ENV);
    result
}

#[tokio::test]
#[serial]
async fn refresh_local_workspace_state_provisions_every_registered_root() {
    let scratch = tempfile::tempdir().unwrap();
    let registry_path = scratch.path().join("workspaces.json");
    let repo_a = scratch.path().join("repo-a");
    let repo_b = scratch.path().join("repo-b");
    std::fs::create_dir_all(&repo_a).unwrap();
    std::fs::create_dir_all(&repo_b).unwrap();

    let mut registry = WorkspaceRegistry::default();
    registry.add(&repo_a, None).unwrap();
    registry.add(&repo_b, None).unwrap();
    registry.save(&registry_path).unwrap();

    let pool = local_state_pool();
    let mut missing_roots_warned = HashSet::new();
    let fallback_root = scratch.path().join("fallback-unused");

    let (_registry, roots) = with_scratch_registry_path(&registry_path, || {
        refresh_local_workspace_state(&pool, &fallback_root, &mut missing_roots_warned)
    });

    assert_eq!(roots.len(), 2, "both registered roots resolve: {roots:?}");
    for root in &roots {
        assert!(
            pool.provisioned_registry_for(root).is_some(),
            "{} must be provisioned by refresh_local_workspace_state",
            root.display()
        );
    }
}

/// The core hot-apply regression test: a workspace registered via the CLI
/// (`WorkspaceRegistry::add` + `save`, exactly what `workspace add` does on
/// disk) between two calls to `refresh_local_workspace_state` must be
/// picked up and provisioned by the very next call — no daemon restart, no
/// dependency on any circuit breaker's state, since the helper never reads
/// one.
#[tokio::test]
#[serial]
async fn refresh_local_workspace_state_hot_applies_a_newly_registered_workspace() {
    let scratch = tempfile::tempdir().unwrap();
    let registry_path = scratch.path().join("workspaces.json");
    let fallback_root = scratch.path().join("fallback-unused");
    let new_repo = scratch.path().join("newly-added-repo");
    std::fs::create_dir_all(&new_repo).unwrap();
    // `WorkspaceRegistry::add` stores the CANONICALIZED root (`normalize_path`
    // resolves symlinks), and `refresh_local_workspace_state` returns the
    // registry's own paths — on macOS a tempdir handed out as `/var/...` lives
    // behind the `/private/var/...` symlink, so every comparison below must
    // use the canonical form (the idiom `workspace_registry`'s own tests use)
    // or the assertion is symlink-layout-dependent and red on this host class
    // (#8328), env or no env.
    let new_repo = std::fs::canonicalize(&new_repo).unwrap();

    // Start with an empty registry (mirrors a freshly-installed daemon).
    WorkspaceRegistry::default().save(&registry_path).unwrap();

    let pool = local_state_pool();
    let mut missing_roots_warned = HashSet::new();

    let (_registry, roots_before) = with_scratch_registry_path(&registry_path, || {
        refresh_local_workspace_state(&pool, &fallback_root, &mut missing_roots_warned)
    });
    assert!(
        !roots_before.contains(&new_repo),
        "the new repo is not registered yet: {roots_before:?}"
    );
    assert!(pool.provisioned_registry_for(&new_repo).is_none());

    // Simulate `loom-daemon workspace add <new_repo> --priority N`: it loads
    // the registry, adds the root, and saves — nothing more.
    let mut registry = WorkspaceRegistry::load(&registry_path).unwrap();
    registry.add(&new_repo, None).unwrap();
    registry.save(&registry_path).unwrap();

    // The very next refresh (i.e. the tick loop's very next call, called
    // unconditionally regardless of rate-limit breaker suppression) must
    // see and provision it.
    let (_registry, roots_after) = with_scratch_registry_path(&registry_path, || {
        refresh_local_workspace_state(&pool, &fallback_root, &mut missing_roots_warned)
    });
    assert!(
        roots_after.contains(&new_repo),
        "the newly registered repo must be hot-applied on the next refresh: {roots_after:?}"
    );
    assert!(
        pool.provisioned_registry_for(&new_repo).is_some(),
        "the newly registered repo must be provisioned on the next refresh"
    );
}

/// #8328's spelling class, made host-independent: the hot-apply test above
/// only goes red on hosts whose `TMPDIR` hands out a non-canonical spelling
/// (macOS `/var/...` behind the `/private/var/...` symlink), so a revert of
/// the canonicalization would stay green on a canonical-`TMPDIR` CI runner.
/// This test MANUFACTURES that spelling — a symlink alias inside the scratch
/// dir pointing at the scratch dir itself — so `alias/newly-added-repo` is a
/// genuinely non-canonical spelling of the same directory on every host, and
/// the registry/comparison contract is exercised everywhere.
#[tokio::test]
#[serial]
async fn refresh_local_workspace_state_hot_applies_through_a_symlink_alias_spelling() {
    let scratch = tempfile::tempdir().unwrap();
    let registry_path = scratch.path().join("workspaces.json");
    let fallback_root = scratch.path().join("fallback-unused");
    // `alias -> scratch`: reading through `alias` resolves to `scratch`, so
    // the aliased path and its canonical form differ on ANY unix host.
    let alias = scratch.path().join("alias");
    std::os::unix::fs::symlink(scratch.path(), &alias).unwrap();
    let aliased_repo = alias.join("newly-added-repo");
    std::fs::create_dir_all(&aliased_repo).unwrap();
    let new_repo = std::fs::canonicalize(&aliased_repo).unwrap();
    assert_ne!(
        new_repo, aliased_repo,
        "the fixture must really be a non-canonical spelling, or it proves nothing"
    );

    // Start with an empty registry (mirrors a freshly-installed daemon).
    WorkspaceRegistry::default().save(&registry_path).unwrap();

    let pool = local_state_pool();
    let mut missing_roots_warned = HashSet::new();

    let (_registry, roots_before) = with_scratch_registry_path(&registry_path, || {
        refresh_local_workspace_state(&pool, &fallback_root, &mut missing_roots_warned)
    });
    assert!(
        !roots_before.contains(&new_repo),
        "the new repo is not registered yet: {roots_before:?}"
    );

    // Simulate `loom-daemon workspace add <aliased spelling on the CLI>`: the
    // operator's shell may hand the daemon either spelling; `add` normalizes
    // what it stores, and the refresh below must surface the canonical form.
    let mut registry = WorkspaceRegistry::load(&registry_path).unwrap();
    registry.add(&aliased_repo, None).unwrap();
    registry.save(&registry_path).unwrap();

    let (_registry, roots_after) = with_scratch_registry_path(&registry_path, || {
        refresh_local_workspace_state(&pool, &fallback_root, &mut missing_roots_warned)
    });
    assert!(
        roots_after.contains(&new_repo),
        "the repo registered through an aliased spelling must be hot-applied in \
         canonical form: {roots_after:?}"
    );
    assert!(
        pool.provisioned_registry_for(&new_repo).is_some(),
        "the repo registered through an aliased spelling must be provisioned"
    );
}
