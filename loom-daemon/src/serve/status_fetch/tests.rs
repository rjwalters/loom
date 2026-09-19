//! **Issue #8224.** The dashboard's `DaemonStatus` round-trip must be
//! budgeted by the registered workspace-root count, mirroring
//! `crate::status_budget::tests::the_status_build_stays_within_budget_at_the_documented_root_ceiling`
//! from the client side: a many-root synthetic registry must not leave
//! `/api/status` and `/api/health` timing out at the old fixed `5s`.

use super::{fetch_budget, fetch_report};
use crate::serve::FETCH_TIMEOUT;
use crate::status_budget::{self, DOCUMENTED_MAX_ROOTS, MAX_ROOT_SCALED_PROBE_TIMEOUT};
use crate::types::{Request, Response};
use crate::workspace_registry::{WorkspaceRegistry, REGISTRY_PATH_ENV};
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

// ===================================================================
// The budget decision (no socket needed)
// ===================================================================

/// A single-workspace host is bit-for-bit unchanged: at `root_count == 1` the
/// probe budget is `1.4s`, well under [`FETCH_TIMEOUT`], so the raise-only
/// `max` is a no-op and the dashboard's latency profile does not move.
#[test]
fn a_single_root_host_keeps_the_fixed_fetch_timeout() {
    assert_eq!(fetch_budget(1), FETCH_TIMEOUT);
}

/// The #8224 regression: at the documented root ceiling the budget must cover
/// an `O(roots)` `build_daemon_status`, which means growing past the fixed
/// `5s` that returned a false `503 daemon unreachable` on the #8163 host.
#[test]
fn many_roots_raise_the_budget_past_the_old_fixed_five_seconds() {
    let budget = fetch_budget(DOCUMENTED_MAX_ROOTS);
    assert!(
        budget > FETCH_TIMEOUT,
        "a {DOCUMENTED_MAX_ROOTS}-root host must out-budget the fixed {FETCH_TIMEOUT:?}, got \
         {budget:?} — this is the #8224 regression (dashboard 503s on a healthy daemon)"
    );
    assert_eq!(budget, status_budget::client_probe_budget(DOCUMENTED_MAX_ROOTS));
    assert!(
        budget > Duration::from_millis(14_300),
        "must cover the worst build #8163 actually measured, got {budget:?}"
    );
}

/// Monotonic and bounded: one more registered workspace never buys a *smaller*
/// budget, and a corrupted/enormous registry can never pin a dashboard poller
/// for minutes.
#[test]
fn the_budget_is_monotonic_and_bounded() {
    let mut prev = fetch_budget(0);
    for n in 1..=DOCUMENTED_MAX_ROOTS * 2 {
        let next = fetch_budget(n);
        assert!(next >= prev, "fetch_budget regressed at n={n}");
        prev = next;
    }
    assert_eq!(fetch_budget(usize::MAX), MAX_ROOT_SCALED_PROBE_TIMEOUT);
}

// ===================================================================
// End-to-end: the budget is actually the one `fetch_report` waits under
// ===================================================================

/// A fake daemon that accepts one connection, waits `delay`, then answers the
/// `DaemonStatus` request. Returns the socket path (the tempdir is leaked for
/// the process lifetime, as the sibling `serve::tests` fixtures do).
async fn spawn_slow_fake_daemon(delay: Duration) -> PathBuf {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("slow-daemon.sock");
    std::mem::forget(dir);
    let listener = UnixListener::bind(&socket_path).expect("bind fake socket");
    tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            if let Ok(Some(line)) = lines.next_line().await {
                let _req: Request = serde_json::from_str(&line).expect("valid request");
                tokio::time::sleep(delay).await;
                let response = Response::DaemonStatus(Box::default());
                let json = serde_json::to_string(&response).expect("serialize");
                let _ = writer.write_all(json.as_bytes()).await;
                let _ = writer.write_all(b"\n").await;
                let _ = writer.flush().await;
            }
        }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    socket_path
}

/// Point `REGISTRY_PATH_ENV` at a throwaway registry holding `n` roots; the
/// returned tempdir must outlive the call under test.
fn synthetic_registry(n: usize) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let reg_path = dir.path().join("workspaces.json");
    let mut reg = WorkspaceRegistry::default();
    for i in 0..n {
        let root = dir.path().join(format!("repo-{i:02}"));
        std::fs::create_dir_all(root.join(".loom")).expect("mkdir");
        reg.add(&root, None).expect("register root");
    }
    reg.save(&reg_path).expect("save registry");
    std::env::set_var(REGISTRY_PATH_ENV, &reg_path);
    dir
}

/// **The regression guard.** With a synthetic registry at the documented root
/// ceiling, a daemon that takes longer than the old fixed `5s` to answer must
/// still be read successfully — the exact shape of the #8163/#8224 field
/// report, where `build_daemon_status` over "several dozen" roots took
/// `13.1s`/`14.3s` and the dashboard reported the daemon unreachable.
///
/// Deliberately a real (not mocked) delay: `tokio`'s paused clock auto-advances
/// whenever the runtime is idle, which here would race the socket I/O and make
/// the test flaky in both directions. The delay is kept just over `5s` — the
/// smallest value that fails before this fix and passes after it.
#[tokio::test]
#[serial_test::serial]
async fn a_slow_many_root_daemon_is_no_longer_a_false_timeout() {
    let _dir = synthetic_registry(DOCUMENTED_MAX_ROOTS);
    assert_eq!(status_budget::registered_root_count(), DOCUMENTED_MAX_ROOTS);

    let socket_path = spawn_slow_fake_daemon(FETCH_TIMEOUT + Duration::from_millis(300)).await;
    let result = fetch_report(&socket_path).await;
    std::env::remove_var(REGISTRY_PATH_ENV);

    assert!(
        result.is_ok(),
        "a {DOCUMENTED_MAX_ROOTS}-root host must tolerate a reply slower than the fixed \
         {FETCH_TIMEOUT:?}; got {:?}",
        result.err()
    );
}

/// The budget is still *bounded*: a daemon that never answers must fail, and
/// the error must name both the budget and the root count that produced it, so
/// an operator reading a dashboard 503 can tell a genuinely dead daemon from a
/// too-small budget without reading source.
#[tokio::test]
#[serial_test::serial]
async fn a_never_answering_daemon_still_times_out_and_names_the_root_count() {
    let _dir = synthetic_registry(1);
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("silent.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind fake socket");
    tokio::spawn(async move {
        // Accept and hold the connection open, never replying — a dropped
        // stream would surface as a clean EOF, not the timeout under test.
        if let Ok((stream, _)) = listener.accept().await {
            let _held = stream;
            std::future::pending::<()>().await;
        }
    });

    let err = fetch_report(&socket_path)
        .await
        .expect_err("silent daemon must time out");
    std::env::remove_var(REGISTRY_PATH_ENV);

    let rendered = err.to_string();
    assert!(rendered.contains("timed out"), "unexpected error: {rendered}");
    assert!(
        rendered.contains("1 registered workspace root(s)"),
        "the timeout must name the root count that sized it, got: {rendered}"
    );
}
