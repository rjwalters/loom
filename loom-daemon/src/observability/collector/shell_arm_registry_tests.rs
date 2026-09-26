//! Unit tests for the durable shell-arm registry's contribution to
//! `host.health` (Issue #8901) — the `armed_singleton_jobs` field of
//! [`super::sample_host_health`] as fed by
//! [`crate::fleet_captain::record_shell_arm`].
//!
//! A sibling module rather than more lines in `collector/tests.rs`, which sits
//! at the 1000-code-line ratchet threshold
//! (`scripts/check-file-size-budget.sh`) — the file-size policy's preferred
//! remedy is "put the new code in a new sibling module", not "trim something
//! unrelated to make room".

use super::*;

/// Mirrors `collector/tests.rs`'s own helper rather than importing it: the two
/// modules are independent `#[cfg(test)]` trees, and a shared fixture between
/// them would couple these tests to edits in the host-health ones.
fn empty_pool() -> WorkspacePool {
    WorkspacePool::new(Arc::new(EventBus::new()), tokio::runtime::Handle::current())
}

fn empty_slug_cache() -> HashMap<String, String> {
    HashMap::new()
}

#[tokio::test]
async fn host_health_sample_merges_the_durable_shell_arm_registry_8901() {
    // The daemon/CLI split #8901 exists to close: a `loom-daemon
    // fleet-captain <job>` invocation's durable arm (never touching the
    // in-process registry) must still surface in `host.health` once
    // `sample_host_health` merges it in.
    let dir = tempfile::tempdir().unwrap();
    let pool = empty_pool();
    crate::fleet_captain::record_shell_arm(dir.path(), "shell-driven-job", chrono::Utc::now())
        .unwrap();
    let record = sample_host_health(
        dir.path(),
        Instant::now(),
        &pool,
        &mut empty_slug_cache(),
        (None, None),
    )
    .await;
    assert!(
        record
            .armed_singleton_jobs
            .contains(&"shell-driven-job".to_string()),
        "{:?}",
        record.armed_singleton_jobs
    );
}

#[tokio::test]
async fn host_health_sample_omits_a_stale_shell_arm() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join(crate::config_resolver::LEGACY_CONFIG_REL);
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    std::fs::write(&config_path, r#"{"fleet": {"captainArmTtlSecs": 1}}"#).unwrap();
    let pool = empty_pool();
    let long_ago = chrono::Utc::now() - chrono::Duration::hours(1);
    crate::fleet_captain::record_shell_arm(dir.path(), "stale-shell-job", long_ago).unwrap();
    let record = sample_host_health(
        dir.path(),
        Instant::now(),
        &pool,
        &mut empty_slug_cache(),
        (None, None),
    )
    .await;
    assert!(
        !record
            .armed_singleton_jobs
            .contains(&"stale-shell-job".to_string()),
        "a 1-second configured TTL must age this out: {:?}",
        record.armed_singleton_jobs
    );
}
