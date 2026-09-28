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
// HOW THIS IS PROVEN (#9092). This test used to inject a fixed sleep into the
// pass and then assert that bind / ping / pidfile / heartbeat all landed
// within *half* that sleep — a wall-clock fraction, which is a race, not a
// proof: a loaded CI runner can easily spend seconds of scheduling jitter
// getting a daemon started, and the test flaked on `main` exactly that way.
//
// Instead, the pass now blocks on a filesystem rendezvous
// (`daemon_startup_reconciliation::TEST_STARTUP_GATE_DIR_ENV`) that only this
// test releases:
//
//   1. the daemon's startup-pass thread creates `startup-pass.entered` and
//      then blocks — the pass is now *in flight* and cannot finish;
//   2. the test observes socket bind, a Ping answer, the pidfile and the
//      heartbeat, asserting after each that `startup-pass.completed` does not
//      exist yet;
//   3. the test creates `startup-pass.release`, and only then does the pass
//      run and write `startup-pass.completed`.
//
// Every observation in step 2 therefore provably happened while the pass was
// still running, at any host speed — no timing margin anywhere. And the
// #7974 regression still fails deterministically: if bind/pidfile/heartbeat
// went back to waiting for the pass, the pass would be waiting for a release
// the test never sends, so the step-2 poll loops would time out on
// `OBSERVE_WAIT` and panic.
//
// expect/unwrap are acceptable here since tests should panic on failure.
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]

mod common;

use common::{daemon_bin, isolate_daemon_state, TestClient};
use loom_daemon::daemon_startup_reconciliation::{
    TEST_GATE_COMPLETED_FILE, TEST_GATE_ENTERED_FILE, TEST_GATE_RELEASE_FILE,
    TEST_STARTUP_GATE_DIR_ENV,
};
use serial_test::serial;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Liveness bound for observing socket bind / pidfile claim / heartbeat
/// write, and for the gate markers. Deliberately generous (mirrors the
/// singleton-guard suite's `DAEMON_BIND_WAIT`) — every poll loop below
/// returns the moment its condition is met, so a large ceiling costs a
/// passing run nothing, and nothing here compares elapsed time against a
/// *fraction* of anything (#9092). Exceeding it means the observation never
/// happened while the pass was in flight, which is the #7974 regression.
const OBSERVE_WAIT: Duration = Duration::from_secs(30);

/// Kills the spawned daemon if the test panics before it exits on its own.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The rendezvous directory handed to the daemon: its startup pass blocks in
/// here until [`Gate::release`] is called, and records its own progress with
/// the `entered` / `completed` markers.
struct Gate(PathBuf);

impl Gate {
    fn new(dir: PathBuf) -> Self {
        std::fs::create_dir_all(&dir).expect("create startup-pass gate dir");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    /// Wait until the daemon's startup-pass thread reports that it has begun
    /// (and is therefore now blocked on our release).
    fn wait_until_pass_in_flight(&self, child: &mut Child) {
        let entered = self.0.join(TEST_GATE_ENTERED_FILE);
        let start = Instant::now();
        while !entered.exists() {
            if let Some(status) = child.try_wait().expect("try_wait failed") {
                panic!("daemon exited before entering the startup pass (status {status:?})");
            }
            if start.elapsed() > OBSERVE_WAIT {
                panic!(
                    "daemon never entered its startup reconciliation pass within {}s (no \
                     {TEST_GATE_ENTERED_FILE} in {}) — the test gate never engaged, so nothing \
                     below would prove anything",
                    OBSERVE_WAIT.as_secs(),
                    self.0.display()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// True once the startup passes have actually finished.
    fn pass_completed(&self) -> bool {
        self.0.join(TEST_GATE_COMPLETED_FILE).exists()
    }

    /// Assert the startup pass is *still running*, i.e. whatever readiness
    /// `observation` names was reached while the pass was in flight — the
    /// entire #7974 claim. This can only trip if the gate stopped gating
    /// (e.g. the env contract drifted), which would otherwise silently turn
    /// this test into a vacuous pass.
    fn assert_pass_still_in_flight(&self, observation: &str) {
        assert!(
            !self.pass_completed(),
            "{observation} was only observed after the startup reconciliation pass had already \
             completed ({TEST_GATE_COMPLETED_FILE} exists) — the pass is supposed to still be \
             blocked on {TEST_GATE_RELEASE_FILE} at this point, so this test can no longer \
             prove readiness happens *during* the pass (#7974/#9092)"
        );
    }

    /// Let the blocked startup pass proceed.
    fn release(&self) {
        std::fs::write(self.0.join(TEST_GATE_RELEASE_FILE), b"go\n")
            .expect("write startup-pass gate release marker");
    }

    /// Wait for the released pass to actually finish.
    fn wait_for_completion(&self) {
        let start = Instant::now();
        while !self.pass_completed() {
            if start.elapsed() > OBSERVE_WAIT {
                panic!(
                    "startup reconciliation pass never completed within {}s of being released \
                     (no {TEST_GATE_COMPLETED_FILE} in {})",
                    OBSERVE_WAIT.as_secs(),
                    self.0.display()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Build a `loom-daemon` command whose startup reconciliation passes block on
/// `gate_dir` until this test releases them, standing in for "a slow pass".
///
/// Reconciliation itself (`LOOM_STALE_CLAIM_RECONCILE` /
/// `LOOM_QUARANTINE_RECONCILE`) is disabled so the pass never shells out to a
/// real `gh` — the gate alone stands in for the slowness, keeping this test
/// hermetic and independent of any real forge/network state. The work finder
/// / epic supervisor / role runner are all disabled too: this test is about
/// socket/pidfile/heartbeat *readiness* ordering, not dispatch.
fn daemon_command(
    socket_path: &Path,
    workspace: &Path,
    pid_file: &Path,
    gate_dir: &Path,
) -> Command {
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
        .env(TEST_STARTUP_GATE_DIR_ENV, gate_dir)
        .env("LOOM_DAEMON_HEARTBEAT", "1")
        .env("LOOM_DAEMON_HEARTBEAT_INTERVAL_SECS", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// Poll until `socket_path` is a bound Unix socket (not merely present as a
/// regular file), panicking if `child` exits first or `OBSERVE_WAIT` elapses.
fn wait_for_bind(child: &mut Child, socket_path: &Path) {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("try_wait failed") {
            panic!("daemon exited before binding its socket (status {status:?})");
        }
        let bound = std::fs::metadata(socket_path)
            .map(|m| m.file_type().is_socket())
            .unwrap_or(false);
        if bound {
            return;
        }
        if start.elapsed() > OBSERVE_WAIT {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "daemon did not bind its socket within {}s while the startup reconciliation pass \
                 was still in flight — the bind must not wait for the pass (#7974)",
                OBSERVE_WAIT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Poll `pid_file` until it names `expected`, panicking once `OBSERVE_WAIT`
/// elapses.
fn wait_for_recorded_pid(pid_file: &Path, expected: u32) {
    let start = Instant::now();
    loop {
        let observed = std::fs::read_to_string(pid_file)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok());
        if observed == Some(expected) {
            return;
        }
        if start.elapsed() > OBSERVE_WAIT {
            panic!(
                "pid file {} never recorded pid {expected} within {}s while the startup \
                 reconciliation pass was still in flight (last observed: {observed:?}) — the \
                 pidfile claim must not wait for the pass (#7974)",
                pid_file.display(),
                OBSERVE_WAIT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Poll for the heartbeat file (`<socket dir>/daemon.heartbeat`, mirroring
/// `daemon_heartbeat::resolve_heartbeat_path`'s `LOOM_SOCKET_PATH`-derived
/// resolution) to exist.
fn wait_for_heartbeat_file(socket_path: &Path) {
    let heartbeat_path = socket_path
        .parent()
        .expect("socket path has a parent dir")
        .join("daemon.heartbeat");
    let start = Instant::now();
    loop {
        if heartbeat_path.exists() {
            return;
        }
        if start.elapsed() > OBSERVE_WAIT {
            panic!(
                "heartbeat file {} never appeared within {}s while the startup reconciliation \
                 pass was still in flight — the heartbeat must not wait for the pass (#7974)",
                heartbeat_path.display(),
                OBSERVE_WAIT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// THE #7974 REGRESSION: the socket must bind, the pidfile must be claimed,
/// and the heartbeat must start ticking WHILE the startup reconciliation pass
/// is still running. Before the fix, none of them happened until the
/// (then-synchronous) pass returned — so with the pass held open by this
/// test's gate, none of them would ever happen at all.
#[tokio::test]
#[serial]
async fn test_socket_pidfile_and_heartbeat_are_ready_during_slow_startup_reconciliation() {
    let temp_dir = tempfile::TempDir::new().expect("temp dir");
    let socket_path = temp_dir.path().join("daemon.sock");
    let pid_file = temp_dir.path().join(".daemon.pid");
    let gate = Gate::new(temp_dir.path().join("startup-gate"));

    let mut child = ChildGuard(
        daemon_command(&socket_path, temp_dir.path(), &pid_file, gate.path())
            .spawn()
            .expect("spawn daemon"),
    );
    let pid = child.0.id();

    // Everything below happens while the startup pass is blocked in the gate,
    // so no assertion needs a timing margin: the pass physically cannot
    // complete until this test releases it.
    gate.wait_until_pass_in_flight(&mut child.0);

    wait_for_bind(&mut child.0, &socket_path);
    gate.assert_pass_still_in_flight("socket bind");

    // AC1: any IPC call answers while the pass is still in flight.
    let mut client = TestClient::connect(&socket_path)
        .await
        .expect("connect while reconciliation pass is still running");
    client
        .ping()
        .await
        .expect("Ping must answer while the startup reconciliation pass is still in flight");
    gate.assert_pass_still_in_flight("a Ping answer");

    // AC2 (pidfile): claimed without waiting for the pass.
    wait_for_recorded_pid(&pid_file, pid);
    gate.assert_pass_still_in_flight("the pidfile claim");

    // AC2 (heartbeat): the first heartbeat write also lands without waiting
    // for the pass — `tokio::time::interval` fires immediately on its first
    // tick, so this appears almost as fast as the bind itself.
    wait_for_heartbeat_file(&socket_path);
    gate.assert_pass_still_in_flight("the first heartbeat write");

    // Sanity: once the pass is released and actually finishes, the daemon is
    // still healthy and answering — the change did not just move the hang
    // elsewhere.
    gate.release();
    gate.wait_for_completion();
    client
        .ping()
        .await
        .expect("daemon must still answer Ping after the startup reconciliation pass finishes");
}
