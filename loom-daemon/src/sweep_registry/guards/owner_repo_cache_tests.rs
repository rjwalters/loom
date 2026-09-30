//! Coverage for Issue #9572: `SweepRegistry::resolve_owner_repo` caches its
//! first *successful* `gh repo view` resolve per workspace for the life of
//! the registry, so the ~10 guard/probe call sites a single dispatch can
//! reach no longer each spawn their own `gh repo view` subprocess. A failed
//! resolve is never cached — every guard downstream fails open on `None`,
//! and caching a transient `gh` outage would turn one bad call into a
//! permanent one.
//!
//! Sibling file rather than an inline `mod`: `guards.rs` is over the
//! file-size ratchet threshold (`scripts/file-size-baseline.txt`), and
//! keeping test modules out of it is that policy's own preferred remedy
//! (see `repo_env_tests.rs`, the existing precedent this file follows).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::sweep_registry::test_support::install_fake_gh;
use serial_test::serial;
use tempfile::tempdir;

/// A successful `gh repo view` resolve is spawned at most once per registry,
/// no matter how many times `resolve_owner_repo` (or anything that calls it)
/// is invoked afterward.
#[test]
#[serial]
fn resolve_owner_repo_caches_a_successful_resolve() {
    std::env::remove_var("LOOM_REPO");
    let dir = tempdir().unwrap();
    let gh_log = dir.path().join("gh-invocations.log");
    let fake_gh = install_fake_gh(dir.path(), &gh_log, "rjwalters/loom", 0);

    let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
    config.gh_bin = Some(fake_gh);
    let registry = SweepRegistry::new(config);

    for _ in 0..5 {
        assert_eq!(
            registry.resolve_owner_repo(),
            Some(("rjwalters".to_string(), "loom".to_string()))
        );
    }

    let calls = std::fs::read_to_string(&gh_log)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .count();
    assert_eq!(
        calls, 1,
        "a successful resolve must be cached — expected exactly one `gh repo view` \
         invocation across 5 calls, got {calls}"
    );
}

/// A failed resolve (non-zero exit) is NEVER cached: every call re-invokes
/// `gh`, preserving the pre-#9572 fail-open contract (a transient `gh`
/// outage is retried on the very next call, not remembered forever).
#[test]
#[serial]
fn resolve_owner_repo_never_caches_a_failed_resolve() {
    std::env::remove_var("LOOM_REPO");
    let dir = tempdir().unwrap();
    let gh_log = dir.path().join("gh-invocations.log");
    let fake_gh = install_fake_gh(dir.path(), &gh_log, "", 1);

    let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
    config.gh_bin = Some(fake_gh);
    let registry = SweepRegistry::new(config);

    for _ in 0..3 {
        assert_eq!(registry.resolve_owner_repo(), None);
    }

    let calls = std::fs::read_to_string(&gh_log)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .count();
    assert_eq!(
        calls, 3,
        "a failed resolve must never be cached — expected 3 `gh repo view` invocations \
         across 3 calls, got {calls}"
    );
}

/// `LOOM_REPO` is checked before the cache on every call (unchanged from
/// pre-#9572), so a workspace with the env override set never spawns `gh`
/// at all, regardless of how many times `resolve_owner_repo` is called.
#[test]
#[serial]
fn resolve_owner_repo_loom_repo_override_never_spawns_gh() {
    let dir = tempdir().unwrap();
    let gh_log = dir.path().join("gh-invocations.log");
    let fake_gh = install_fake_gh(dir.path(), &gh_log, "should-not-be-used/repo", 0);

    let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
    config.gh_bin = Some(fake_gh);
    let registry = SweepRegistry::new(config);

    std::env::set_var("LOOM_REPO", "rjwalters/loom");
    for _ in 0..3 {
        assert_eq!(
            registry.resolve_owner_repo(),
            Some(("rjwalters".to_string(), "loom".to_string()))
        );
    }
    std::env::remove_var("LOOM_REPO");

    assert!(
        !gh_log.exists() || std::fs::read_to_string(&gh_log).unwrap().is_empty(),
        "the LOOM_REPO override must short-circuit before any `gh repo view` spawn"
    );
}
