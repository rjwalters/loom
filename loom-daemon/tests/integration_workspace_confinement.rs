// Regression test for Issue #4573: `TestDaemon::start()` must confine every
// spawned test daemon to its own throwaway `TempDir`, never the real repo
// checkout this test binary was built from.
//
// The incident this pins: `TestDaemon::start()` only set `LOOM_SOCKET_PATH`,
// `RUST_LOG`, and `LOOM_NO_RESTORE` on the spawned daemon's environment. It
// neither disabled the daemon's autonomous behavior (role runner / work
// finder / epic supervisor) nor repointed its workspace root — so the
// spawned test daemon inherited this repo's real `LOOM_WORKSPACE`/cwd and
// read the real committed `.loom/config.json`
// (`autonomous.roleRunner.enabled: true`), and could dispatch real
// `/loom:sweep` sessions, burning API/GitHub rate-limit quota.
//
// Two complementary assertions pin the fix:
//
// 1. Via the daemon's own `Request::DaemonStatus` IPC query — the resolved
//    workspace root (`per_repo[0].root`) must be inside the `TestDaemon`'s own
//    `TempDir`, not the real checkout this test binary lives under, and that
//    root's `role_runner_enabled` must be `false`. This proves the child
//    *honored* the env, not merely that it was handed to `Command`.
// 2. Via the child's real environment (`/proc/<pid>/environ`, Linux) — every
//    autonomy toggle is `0` and every path is inside the `TempDir`. This is the
//    only coverage for the vars the daemon consumes internally and never
//    reports over IPC (notably `LOOM_WORKTREE_ROOT`).

// Integration tests - expect/unwrap are acceptable here since tests should panic on failure
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]

mod common;

use common::{TestClient, TestDaemon};

/// Query `Request::DaemonStatus` and return the daemon's single resolved
/// `per_repo` entry (the only root it can see, given the isolated registry).
async fn resolved_repo_status(client: &mut TestClient) -> serde_json::Value {
    let response = client
        .send_request(serde_json::json!({"type": "DaemonStatus"}))
        .await
        .expect("DaemonStatus request failed");

    assert_eq!(
        response.get("type").and_then(serde_json::Value::as_str),
        Some("DaemonStatus"),
        "unexpected response: {response:?}"
    );

    let payload = response.get("payload").expect("DaemonStatus payload");
    let per_repo = payload
        .get("per_repo")
        .and_then(serde_json::Value::as_array)
        .expect("per_repo array");

    assert_eq!(
        per_repo.len(),
        1,
        "expected exactly one resolved workspace root (the isolated \
         `LOOM_WORKSPACES_PATH` registry is empty, so `effective_roots()` \
         must reduce to the single seeded default): {per_repo:?}"
    );

    per_repo[0].clone()
}

/// Canonicalize for comparison, tolerating a path that does not exist (a
/// platform whose temp dir is itself a symlink — e.g. macOS `/tmp` ->
/// `/private/tmp` — must still compare equal).
fn canon(path: &std::path::Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// A spawned `TestDaemon`'s resolved workspace root must be the daemon's own
/// throwaway `TempDir` — never the real repo checkout this test binary was
/// built from (#4573).
#[tokio::test]
async fn test_daemon_workspace_root_is_confined_to_its_own_temp_dir() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let mut client = TestClient::connect(daemon.socket_path())
        .await
        .expect("Failed to connect");

    let repo_status = resolved_repo_status(&mut client).await;
    let resolved_root = std::path::PathBuf::from(
        repo_status
            .get("root")
            .and_then(serde_json::Value::as_str)
            .expect("per_repo[0].root"),
    );

    // The daemon must resolve to (a canonical form of) the `TestDaemon`'s own
    // `TempDir`.
    let expected_root = daemon.workspace_path();
    let resolved_canon = canon(&resolved_root);
    let expected_canon = canon(expected_root);
    assert_eq!(
        resolved_canon, expected_canon,
        "daemon resolved workspace root {resolved_root:?} does not match its own \
         TempDir {expected_root:?} — LOOM_WORKSPACE was not honored"
    );

    // And, the load-bearing negative assertion: the resolved root must NOT be
    // the real repo checkout this test binary was built from — the exact
    // leak #4573 reports (a spawned test daemon inheriting the real
    // `LOOM_WORKSPACE`/cwd and reading the real committed `.loom/config.json`).
    let real_repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("loom-daemon has a parent directory (the repo root)");
    let real_repo_canon = canon(real_repo_root);
    assert_ne!(
        resolved_canon, real_repo_canon,
        "daemon resolved workspace root must not be the real repo checkout {real_repo_canon:?} \
         — a spawned test daemon must never see the real repo's committed .loom/config.json"
    );

    // The autonomy toggle actually took effect *inside the child*, not merely
    // "was passed to `Command`": `RepoStatus::role_runner_enabled` is resolved
    // daemon-side via `role_runner::resolve_enabled(..)`, the same
    // env > config > default chain `LOOM_ROLE_RUNNER=0` overrides. This repo's
    // committed `.loom/config.json` has `autonomous.roleRunner.enabled: true`,
    // so a daemon that had leaked back onto the real checkout would report
    // `true` here.
    assert_eq!(
        repo_status
            .get("role_runner_enabled")
            .and_then(serde_json::Value::as_bool),
        Some(false),
        "test daemon must resolve the role runner as DISABLED (LOOM_ROLE_RUNNER=0), \
         got: {repo_status:?}"
    );
}

/// The confinement env vars must actually reach the spawned child's
/// environment, with every path inside the daemon's own `TempDir` (#4573).
///
/// This complements the IPC assertion above: `LOOM_WORKTREE_ROOT` and
/// `LOOM_WORK_FINDER` / `LOOM_EPIC_SUPERVISOR` are consumed internally and
/// never surfaced over IPC, so reading the child's real environment is the only
/// way to catch a future refactor that drops one of them.
///
/// Linux-only: it reads `/proc/<pid>/environ`. CI runs Linux; on other
/// platforms the IPC assertions above still cover the workspace root.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn test_daemon_child_env_is_fail_closed_and_temp_dir_confined() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let pid = daemon.pid().expect("daemon child is still running");

    let raw = std::fs::read(format!("/proc/{pid}/environ"))
        .unwrap_or_else(|e| panic!("failed to read /proc/{pid}/environ: {e}"));
    let env: std::collections::HashMap<String, String> = String::from_utf8_lossy(&raw)
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| {
            entry
                .split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect();

    // Every autonomy toggle is explicitly off. `resolve_enabled()` treats a set
    // env var as authoritative over config and only `1`/`true`/`yes`/`on`
    // enables, so `"0"` fails closed regardless of the checkout's config.
    for key in [
        "LOOM_ROLE_RUNNER",
        "LOOM_WORK_FINDER",
        "LOOM_EPIC_SUPERVISOR",
    ] {
        assert_eq!(
            env.get(key).map(String::as_str),
            Some("0"),
            "{key} must be explicitly disabled in the test daemon's environment"
        );
    }

    // Every path the daemon resolves state from is inside its own `TempDir`.
    let temp_canon = canon(daemon.workspace_path());
    for key in [
        "LOOM_WORKSPACE",
        "LOOM_WORKSPACES_PATH",
        "LOOM_WORKTREE_ROOT",
    ] {
        let value = env
            .get(key)
            .unwrap_or_else(|| panic!("{key} must be set in the test daemon's environment"));
        let value_path = std::path::PathBuf::from(value);
        assert!(
            value_path.is_absolute(),
            "{key}={value} must be absolute (a relative LOOM_WORKTREE_ROOT is \
             rejected by worktree_root() and silently falls back to the default)"
        );
        // Compare against the canonical TempDir: for a path that does not exist
        // yet (the daemon creates these lazily), `canon` returns it unchanged,
        // so also accept the non-canonical prefix.
        let confined = canon(&value_path).starts_with(&temp_canon)
            || value_path.starts_with(daemon.workspace_path());
        assert!(
            confined,
            "{key}={value} escapes the test daemon's TempDir {temp_canon:?} — a \
             test daemon must never resolve state from outside its own scratch dir"
        );
    }
}
