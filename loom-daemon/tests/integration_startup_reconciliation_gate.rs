// Integration test for Issue #7974: the daemon must bind its IPC socket,
// claim its pidfile, and start its heartbeat WITHOUT waiting for the
// synchronous startup claim-reconciliation pass to finish.
//
// Before the fix, `run_daemon` ran the startup claim-reconciliation pass (and
// the co-located stranded-quarantine pass) as a single blocking call before
// any of the above happened — so a slow pass (unbounded `gh` fan-out under
// rate limiting, see the issue) left the daemon alive but completely
// unobservable: no socket, no heartbeat, no pidfile. The watchdog's startup
// grace is measured from process age, so a pass slower than that grace made
// a healthy (if slow) daemon get probed and reported as wedged.
//
// `LOOM_TEST_STARTUP_RECONCILE_DELAY_MS`
// (`daemon_startup_reconciliation::TEST_STARTUP_DELAY_MS_ENV`) stubs a slow
// pass deterministically, without needing a real multi-workspace `gh`
// fan-out to actually run long.
//
// expect/unwrap are acceptable here since tests should panic on failure.
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]

mod common;

use common::{daemon_bin, isolate_daemon_state, TestClient};
use loom_daemon::daemon_startup_reconciliation::TEST_STARTUP_DELAY_MS_ENV;
use serial_test::serial;
use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How long the stubbed startup reconciliation pass sleeps before the (fast,
/// disabled) reconciliation logic itself runs. Long enough that any of the
/// assertions below firing before it elapses is not a timing fluke, short
/// enough that a passing test does not sit around waiting.
const STUB_DELAY: Duration = Duration::from_millis(4000);

/// Liveness bound for observing socket bind / pidfile claim / heartbeat
/// write. Deliberately generous (mirrors the singleton-guard suite's
/// `DAEMON_BIND_WAIT`) — every poll loop below returns the moment its
/// condition is met, so a large ceiling costs a passing run nothing. What
/// actually proves the fix is the elapsed-time-vs-`STUB_DELAY` comparison
/// made once the condition is observed, not this ceiling.
const OBSERVE_WAIT: Duration = Duration::from_secs(30);

/// Kills the spawned daemon if the test panics before it exits on its own.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Build a `loom-daemon` command wired to inject a `STUB_DELAY`-long sleep
/// immediately before the (otherwise-disabled) startup reconciliation passes
/// run.
///
/// Reconciliation itself (`LOOM_STALE_CLAIM_RECONCILE` /
/// `LOOM_QUARANTINE_RECONCILE`) is disabled so the pass never shells out to a
/// real `gh` — the injected sleep alone stands in for "a slow pass", keeping
/// this test hermetic and independent of any real forge/network state. The
/// work finder / epic supervisor / role runner are all disabled too: this
/// test is about socket/pidfile/heartbeat *readiness* timing, not dispatch.
fn daemon_command(socket_path: &Path, workspace: &Path, pid_file: &Path) -> Command {
    let mut cmd = Command::new(daemon_bin());
    isolate_daemon_state(&mut cmd, workspace);
    cmd.env("LOOM_SOCKET_PATH", socket_path)
        .env("LOOM_PID_FILE", pid_file)
        .env("RUST_LOG", "info")
        .env("LOOM_NO_RESTORE", "1")
        .env("LOOM_ROLE_RUNNER", "0")
        .env("LOOM_WORK_FINDER", "0")
        .env("LOOM_EPIC_SUPERVISOR", "0")
        .env("LOOM_WORKSPACE", workspace)
        .env("LOOM_WORKSPACES_PATH", workspace.join("workspaces.json"))
        .env("LOOM_WORKTREE_ROOT", workspace.join("worktrees"))
        .env("LOOM_STALE_CLAIM_RECONCILE", "0")
        .env("LOOM_QUARANTINE_RECONCILE", "0")
        .env(TEST_STARTUP_DELAY_MS_ENV, STUB_DELAY.as_millis().to_string())
        .env("LOOM_DAEMON_HEARTBEAT", "1")
        .env("LOOM_DAEMON_HEARTBEAT_INTERVAL_SECS", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// Poll until `socket_path` is a bound Unix socket (not merely present as a
/// regular file), panicking if `child` exits first or `OBSERVE_WAIT` elapses.
fn wait_for_bind(child: &mut Child, socket_path: &Path) -> Duration {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("try_wait failed") {
            panic!("daemon exited before binding its socket (status {status:?})");
        }
        let bound = std::fs::metadata(socket_path)
            .map(|m| m.file_type().is_socket())
            .unwrap_or(false);
        if bound {
            return start.elapsed();
        }
        if start.elapsed() > OBSERVE_WAIT {
            let _ = child.kill();
            let _ = child.wait();
            panic!("daemon did not bind its socket within {}s", OBSERVE_WAIT.as_secs());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Poll `pid_file` until it names `expected`, returning the elapsed time when
/// observed (or panicking once `OBSERVE_WAIT` elapses).
fn wait_for_recorded_pid(pid_file: &Path, expected: u32, start: Instant) -> Duration {
    loop {
        let observed = std::fs::read_to_string(pid_file)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok());
        if observed == Some(expected) {
            return start.elapsed();
        }
        if start.elapsed() > OBSERVE_WAIT {
            panic!(
                "pid file {} never recorded pid {expected} within {}s (last observed: {observed:?})",
                pid_file.display(),
                OBSERVE_WAIT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Poll for the heartbeat file (`<socket dir>/daemon.heartbeat`, mirroring
/// `daemon_heartbeat::resolve_heartbeat_path`'s `LOOM_SOCKET_PATH`-derived
/// resolution) to exist, returning the elapsed time when observed.
fn wait_for_heartbeat_file(socket_path: &Path, start: Instant) -> Duration {
    let heartbeat_path = socket_path
        .parent()
        .expect("socket path has a parent dir")
        .join("daemon.heartbeat");
    loop {
        if heartbeat_path.exists() {
            return start.elapsed();
        }
        if start.elapsed() > OBSERVE_WAIT {
            panic!(
                "heartbeat file {} never appeared within {}s",
                heartbeat_path.display(),
                OBSERVE_WAIT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// THE #7974 REGRESSION: the socket must bind, the pidfile must be claimed,
/// and the heartbeat must start ticking WHILE the stubbed startup
/// reconciliation pass is still sleeping — all three well before
/// `STUB_DELAY` elapses. Before the fix, none of them happened until the
/// (then-synchronous) pass returned.
#[tokio::test]
#[serial]
async fn test_socket_pidfile_and_heartbeat_are_ready_during_slow_startup_reconciliation() {
    let temp_dir = tempfile::TempDir::new().expect("temp dir");
    let socket_path = temp_dir.path().join("daemon.sock");
    let pid_file = temp_dir.path().join(".daemon.pid");

    let mut child = ChildGuard(
        daemon_command(&socket_path, temp_dir.path(), &pid_file)
            .spawn()
            .expect("spawn daemon"),
    );
    let pid = child.0.id();

    // A margin below the full stub delay: comparing against a fraction of it
    // (rather than the whole thing) tolerates ordinary scheduling jitter on a
    // loaded CI host while still failing loudly if any of these regress to
    // waiting for the full pass.
    let must_be_ready_within = STUB_DELAY / 2;

    let bind_elapsed = wait_for_bind(&mut child.0, &socket_path);
    assert!(
        bind_elapsed < must_be_ready_within,
        "socket bind took {bind_elapsed:?}, expected well under the {STUB_DELAY:?} stubbed \
         reconciliation delay — the bind must not wait for the startup pass (#7974)"
    );

    // AC1: any IPC call answers while the stubbed pass is still sleeping.
    let ping_start = Instant::now();
    let mut client = TestClient::connect(&socket_path)
        .await
        .expect("connect while reconciliation pass is still running");
    client
        .ping()
        .await
        .expect("Ping must answer while the startup reconciliation pass is still in flight");
    assert!(
        ping_start.elapsed() < must_be_ready_within,
        "Ping did not answer promptly — it must not be blocked behind the startup \
         reconciliation pass (#7974)"
    );

    // AC2 (pidfile): claimed immediately after the bind, well before the
    // pass completes.
    let pid_elapsed = wait_for_recorded_pid(&pid_file, pid, Instant::now());
    assert!(
        pid_elapsed < must_be_ready_within,
        "pid file was not claimed until {pid_elapsed:?} after the bind was already confirmed — \
         expected it well before the {STUB_DELAY:?} stubbed reconciliation delay (#7974)"
    );

    // AC2 (heartbeat): the first heartbeat write also happens before the
    // pass completes — `tokio::time::interval` fires immediately on its
    // first tick, so this should appear almost as fast as the bind itself.
    let heartbeat_elapsed = wait_for_heartbeat_file(&socket_path, Instant::now());
    assert!(
        heartbeat_elapsed < must_be_ready_within,
        "heartbeat file did not appear until {heartbeat_elapsed:?} — expected well before the \
         {STUB_DELAY:?} stubbed reconciliation delay (#7974)"
    );

    // Sanity: once the stubbed pass actually finishes, the daemon is still
    // healthy and answering — the change did not just move the hang
    // elsewhere.
    tokio::time::sleep(STUB_DELAY).await;
    client
        .ping()
        .await
        .expect("daemon must still answer Ping after the startup reconciliation pass finishes");
}
