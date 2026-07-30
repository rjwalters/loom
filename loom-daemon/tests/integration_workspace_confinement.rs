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
// This test asserts the fix structurally, via the daemon's own
// `Request::DaemonStatus` IPC query: the resolved workspace root
// (`per_repo[0].root`) must be inside the `TestDaemon`'s own `TempDir` — not
// the real checkout this test binary lives under.

// Integration tests - expect/unwrap are acceptable here since tests should panic on failure
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]

mod common;

use common::{TestClient, TestDaemon};

/// Query `Request::DaemonStatus` and return the (single) resolved workspace
/// root from `per_repo[0].root`.
async fn resolved_workspace_root(client: &mut TestClient) -> std::path::PathBuf {
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

    let root = per_repo[0]
        .get("root")
        .and_then(serde_json::Value::as_str)
        .expect("per_repo[0].root");
    std::path::PathBuf::from(root)
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

    let resolved_root = resolved_workspace_root(&mut client).await;

    // The daemon must resolve to (a canonical form of) the `TestDaemon`'s own
    // `TempDir` — canonicalize both sides so a platform whose temp dir is
    // itself a symlink (e.g. macOS `/tmp` -> `/private/tmp`) still compares
    // correctly.
    let expected_root = daemon.workspace_path();
    let resolved_canon = std::fs::canonicalize(&resolved_root).unwrap_or(resolved_root.clone());
    let expected_canon =
        std::fs::canonicalize(expected_root).unwrap_or_else(|_| expected_root.to_path_buf());
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
    let real_repo_canon =
        std::fs::canonicalize(real_repo_root).unwrap_or_else(|_| real_repo_root.to_path_buf());
    assert_ne!(
        resolved_canon, real_repo_canon,
        "daemon resolved workspace root must not be the real repo checkout {real_repo_canon:?} \
         — a spawned test daemon must never see the real repo's committed .loom/config.json"
    );
}
