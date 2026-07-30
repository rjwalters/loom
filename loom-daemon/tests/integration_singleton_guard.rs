// Integration tests for the daemon singleton guard (Issue #3806).
//
// A second daemon started against the same socket must refuse to start rather
// than unlink the incumbent's socket and silently orphan it. A genuinely stale
// socket file (crashed daemon leftover) must still be reclaimed normally.
//
// expect/unwrap are acceptable here since tests should panic on failure.
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]

mod common;

use common::{cleanup_all_loom_sessions, daemon_bin, TestClient, TestDaemon};
use serial_test::serial;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn setup() {
    cleanup_all_loom_sessions();
}

/// How long to wait for a second daemon to notice the incumbent and exit.
///
/// See the call site for why this is sized as a liveness bound rather than a
/// latency assertion.
const DAEMON_REFUSAL_WAIT: Duration = Duration::from_secs(60);

/// Spawn a raw `loom-daemon` process pointed at `socket_path`, wait up to
/// `wait` for it to exit, and return its exit status (or `None` if still
/// running when the wait elapses). Captures no output beyond piping it away.
fn spawn_daemon_and_wait(socket_path: &Path, wait: Duration) -> (std::process::Child, bool) {
    let mut child = Command::new(daemon_bin())
        .env("LOOM_SOCKET_PATH", socket_path)
        .env("RUST_LOG", "debug")
        .env("LOOM_NO_RESTORE", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn second daemon");

    let start = Instant::now();
    loop {
        match child.try_wait().expect("try_wait failed") {
            Some(_status) => return (child, true),
            None => {
                if start.elapsed() > wait {
                    return (child, false);
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

/// The core singleton-guard scenario: a live daemon owns the socket, a second
/// daemon on the same socket must refuse (non-zero exit) and NOT orphan the
/// first — the first stays alive and keeps answering Ping on its socket.
#[tokio::test]
#[serial]
async fn test_second_daemon_refuses_and_first_survives() {
    setup();

    // Daemon A owns the socket.
    let daemon_a = TestDaemon::start().await.expect("Failed to start daemon A");
    let socket_path = daemon_a.socket_path().to_path_buf();

    // Sanity: A answers Ping.
    {
        let mut client = TestClient::connect(&socket_path)
            .await
            .expect("connect to A");
        client.ping().await.expect("A ping before second daemon");
    }

    // Attempt a second daemon on the SAME socket. It must refuse and exit
    // non-zero.
    //
    // The bound is deliberately generous. The singleton guard's liveness probe
    // itself is ~500ms, but it runs *after* the rest of daemon startup, and a
    // 10s bound failed deterministically on a loaded 8-core host where spawn to
    // refusal measured ~15s (#4385). The poll loop below returns as soon as the
    // child exits, so a large bound costs nothing on a passing run — it only
    // stops a saturated machine from turning a liveness check into a latency
    // assertion.
    let (mut child, exited) = spawn_daemon_and_wait(&socket_path, DAEMON_REFUSAL_WAIT);
    assert!(exited, "second daemon should exit promptly after refusing to start");
    let status = child.wait().expect("wait on second daemon");
    assert!(
        !status.success(),
        "second daemon must exit non-zero when a live daemon owns the socket; got {status:?}"
    );

    // The socket must still be there and daemon A must still be answering —
    // the second daemon must NOT have unlinked/stolen it.
    assert!(socket_path.exists(), "socket must still exist after the second daemon refused");
    let mut client = TestClient::connect(&socket_path)
        .await
        .expect("connect to A after second daemon refused");
    client
        .ping()
        .await
        .expect("A must still answer Ping — it was not orphaned");
}

/// Regression: a stale socket file (a plain file left behind by a crashed
/// daemon, nothing listening) must still be reclaimed — a fresh daemon starts
/// normally and binds it.
#[tokio::test]
#[serial]
async fn test_stale_socket_is_reclaimed() {
    setup();

    let temp_dir = tempfile::TempDir::new().expect("temp dir");
    let socket_path = temp_dir.path().join("daemon.sock");
    // Simulate a crashed daemon's leftover: a regular file at the socket path.
    std::fs::write(&socket_path, b"").expect("write stale socket file");

    // Deliberately still short: this only has to catch an *early exit*, and a
    // longer wait would be paid on every (passing) run. The daemon's remaining
    // startup latency — which grew once CI began running tests process-per-test
    // (#4385) — is absorbed by `TestClient::connect`'s retry budget below.
    let (mut child, exited_early) = spawn_daemon_and_wait(&socket_path, Duration::from_secs(2));
    // It should still be running (successfully bound), not exited.
    assert!(
        !exited_early,
        "daemon should start normally against a stale socket file, not exit early"
    );

    // Give it a moment and connect — it should be a live daemon now.
    let mut client = TestClient::connect(&socket_path)
        .await
        .expect("connect to daemon that reclaimed a stale socket");
    client
        .ping()
        .await
        .expect("daemon that reclaimed a stale socket must answer Ping");

    // Teardown.
    let _ = child.kill();
    let _ = child.wait();
}
