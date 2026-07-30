// Integration test for the drain-then-exit teardown contract (Issue #4521).
//
// The incident this pins: `loom-daemon restart --drain --then-exit` drained
// correctly and then the daemon came straight back up, because the completion
// path exited `0`. Under launchd's `KeepAlive:{SuccessfulExit:true}` a `0` exit
// can NEVER stay stopped — the terminal exit code IS the contract, so it is
// asserted end-to-end against the real binary rather than only in a unit test.
//
// The assertion is deliberately narrow and behavioral:
//   1. the process exits `143` (EXIT_SHUTDOWN, non-zero), and
//   2. it does not re-listen (nothing answers on the socket afterwards).
//
// expect/unwrap are acceptable here since tests should panic on failure.
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]

mod common;

use common::{daemon_bin, isolate_daemon_state, TestClient};
use serial_test::serial;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Kills the spawned daemon if the test panics before it exits on its own, so a
/// failing assertion never leaks a daemon that keeps polling this host.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `EXIT_SHUTDOWN` — the "exited on purpose, stay down" code. Hardcoded rather
/// than imported so this test pins the *observable* number a supervisor sees.
const EXPECTED_EXIT_CODE: i32 = 143;

/// Liveness bound for the drain-then-exit round trip. Sized like the other
/// daemon integration bounds (#4385): generous, because the poll loop below
/// returns as soon as the child exits, so a large bound costs a passing run
/// nothing and only stops a saturated host from turning a liveness check into a
/// latency assertion.
const DRAIN_EXIT_WAIT: Duration = Duration::from_secs(60);

/// Requesting `--drain --then-exit` with zero in-flight sweeps must terminate
/// the daemon with a NON-ZERO exit (143) and leave nothing listening.
#[tokio::test]
#[serial]
async fn test_drain_then_exit_exits_143_and_stays_down() {
    let temp_dir = tempfile::TempDir::new().expect("temp dir");
    let socket_path = temp_dir.path().join("daemon.sock");
    // An empty workspace registry + a scratch workspace root guarantee the
    // cross-root in-flight count is 0, so the drain reaches its terminal tick
    // immediately instead of waiting on this host's real sweeps.
    let workspace_root = temp_dir.path().join("workspace");
    std::fs::create_dir_all(&workspace_root).expect("create workspace root");

    let mut cmd = Command::new(daemon_bin());
    // Shared #4556 confinement (empty fixture workspace registry, fixture sweep
    // journal / watch files, fixture cwd) — the generalized form of the
    // isolation this test has always needed.
    isolate_daemon_state(&mut cmd, temp_dir.path());
    let mut child = ChildGuard(
        cmd.current_dir(&workspace_root)
            .env("LOOM_SOCKET_PATH", &socket_path)
            // Pinned, not merely inherited: this test may run on a host that is
            // itself running Loom, where an inherited `LOOM_WORKSPACE` makes the
            // scratch daemon count that repo's REAL in-flight sweeps and the
            // drain never completes.
            .env("LOOM_WORKSPACE", &workspace_root)
            // Fail-closed autonomy toggles (#4573). This is the suite's only
            // daemon spawn outside `TestDaemon::start()`, so it needs the same
            // guard: each of these env vars wins outright over config in the
            // daemon's env > config > default chain, so a test daemon can never
            // dispatch a real sweep or run a real role session regardless of
            // which checkout's `.loom/config.json` it ends up resolving.
            .env("LOOM_ROLE_RUNNER", "0")
            .env("LOOM_WORK_FINDER", "0")
            .env("LOOM_EPIC_SUPERVISOR", "0")
            // Pinned for the same reason as `LOOM_WORKSPACE` above: an
            // inherited `LOOM_WORKTREE_ROOT` is the highest-priority source in
            // `worktree_root()`, so without this the scratch daemon would
            // resolve worktrees onto the host's real scratch volume (#4573).
            .env("LOOM_WORKTREE_ROOT", temp_dir.path().join("worktrees"))
            .env("RUST_LOG", "debug")
            .env("LOOM_NO_RESTORE", "1")
            // Deliberately UNSET: a then-exit drain must not require a supervisor
            // (there is nothing to relaunch into), so this also pins that the
            // supervisor refusal gate is skipped for `then_exit`.
            .env_remove("LOOM_DAEMON_SUPERVISOR")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn daemon"),
    );

    // Wait for the socket to appear, then connect.
    let start = Instant::now();
    while !socket_path.exists() {
        assert!(start.elapsed() < DRAIN_EXIT_WAIT, "daemon never created its socket");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut client = TestClient::connect(&socket_path)
        .await
        .expect("connect to daemon");
    client
        .ping()
        .await
        .expect("daemon answers Ping before drain");

    // Request the teardown drain. The reply is best-effort on purpose: the
    // supervisor task can win the race against the reply flush when there are
    // zero in-flight sweeps, and this test asserts the *exit contract*, not the
    // ack. When a reply does arrive it must report `then_exit: true` (the
    // active drain's terminal action, Issue #4521).
    let request = serde_json::json!({
        "type": "DrainAndRestartDaemon",
        "payload": {
            "timeout_secs": 60,
            "force_after_timeout": false,
            "then_exit": true
        }
    });
    if let Ok(reply) = client.send_request(request).await {
        assert_eq!(
            reply.get("type").and_then(serde_json::Value::as_str),
            Some("DaemonDrain"),
            "unexpected reply: {reply:?}"
        );
        let payload = reply.get("payload").expect("DaemonDrain payload");
        assert_eq!(
            payload.get("accepted").and_then(serde_json::Value::as_bool),
            Some(true),
            "then-exit drain must be accepted without a supervisor: {reply:?}"
        );
        assert_eq!(
            payload
                .get("then_exit")
                .and_then(serde_json::Value::as_bool),
            Some(true),
            "the reply must report the active drain's terminal action: {reply:?}"
        );
    }
    drop(client);

    // The daemon must END. Not restart, not linger.
    let start = Instant::now();
    let status = loop {
        match child.0.try_wait().expect("try_wait") {
            Some(status) => break status,
            None => {
                assert!(
                    start.elapsed() < DRAIN_EXIT_WAIT,
                    "daemon did not exit after drain-then-exit with 0 in-flight"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    };

    assert_eq!(
        status.code(),
        Some(EXPECTED_EXIT_CODE),
        "drain-then-exit must exit {EXPECTED_EXIT_CODE} (EXIT_SHUTDOWN); a 0 exit is relaunched \
         forever by launchd's KeepAlive:{{SuccessfulExit:true}}. Got {status:?}"
    );
    assert!(
        !status.success(),
        "the exit MUST be non-zero — that is the whole stays-down contract"
    );

    // And nothing re-listens: a fresh connect+Ping must not be answered.
    let re_listened = match TestClient::connect(&socket_path).await {
        Ok(mut c) => c.ping().await.is_ok(),
        Err(_) => false,
    };
    assert!(
        !re_listened,
        "nothing may answer on the socket after a drain-then-exit — the daemon must stay down"
    );
}
