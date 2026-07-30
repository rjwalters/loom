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
use std::io::BufRead;
use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn setup() {
    cleanup_all_loom_sessions();
}

/// How long to wait for a second daemon to notice the incumbent and exit.
///
/// See the call site for why this is sized as a liveness bound rather than a
/// latency assertion.
const DAEMON_REFUSAL_WAIT: Duration = Duration::from_secs(60);

/// How long to wait for a daemon to reclaim a stale socket file and bind it.
///
/// Same liveness-bound reasoning as [`DAEMON_REFUSAL_WAIT`] (and the same size
/// as `common`'s socket timeout): the poll loop returns as soon as the socket
/// appears, so a generous bound costs a passing run nothing.
const DAEMON_BIND_WAIT: Duration = Duration::from_secs(60);

/// How long a refusing daemon may take to *end its process* after printing the
/// refusal to stderr.
///
/// This is the #4531 regression bound, and it IS a latency assertion — but on a
/// quantity with no legitimate reason to vary with load. Once the guard has
/// decided to refuse, all that remains is writing one line and calling
/// `std::process::exit`. Before #4531 the refusal instead returned `Err` out of
/// `#[tokio::main]`, whose generated wrapper drops the `Runtime` before
/// `Termination` prints anything — and `Runtime::drop` blocks until every
/// in-flight `spawn_blocking` task finishes, which measured ~10s on a host with
/// real work configured. The startup latency that genuinely does vary with load
/// is excluded by measuring from the refusal line rather than from spawn.
const REFUSAL_TO_EXIT_BUDGET: Duration = Duration::from_secs(5);

/// Substring identifying the singleton guard's refusal on stderr.
const REFUSAL_MARKER: &str = "refusing to start";

/// Build a `loom-daemon` command for a raw (non-`TestDaemon`) spawn, pointed at
/// `socket_path` and confined to `workspace`.
///
/// Mirrors `TestDaemon::start`'s fail-closed environment (#4573). It is not
/// cosmetic here: without it a daemon spawned by these tests inherits this
/// repo's real `.loom/config.json` (`autonomous.roleRunner.enabled: true`) and
/// spends ~10s of startup doing live forge work *before* it ever reaches the
/// singleton guard. That is both a side-effect hazard for a daemon meant to be
/// inert and the main reason these tests were load-sensitive (#4531).
fn daemon_command(socket_path: &Path, workspace: &Path) -> Command {
    let mut cmd = Command::new(daemon_bin());
    cmd.env("LOOM_SOCKET_PATH", socket_path)
        .env("RUST_LOG", "debug")
        .env("LOOM_NO_RESTORE", "1")
        .env("LOOM_ROLE_RUNNER", "0")
        .env("LOOM_WORK_FINDER", "0")
        .env("LOOM_EPIC_SUPERVISOR", "0")
        .env("LOOM_WORKSPACE", workspace)
        .env("LOOM_WORKSPACES_PATH", workspace.join("workspaces.json"))
        .env("LOOM_WORKTREE_ROOT", workspace.join("worktrees"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// What a refused start looked like from the outside.
struct RefusedStart {
    /// When the child was first observed to have exited (`None` ⇒ still running
    /// when the wait budget elapsed).
    exit_at: Option<Instant>,
    /// When the refusal line first appeared on the child's stderr.
    refusal_at: Option<Instant>,
    /// Everything the child wrote to stderr (collected to EOF).
    stderr: String,
}

/// Spawn a raw `loom-daemon` process pointed at `socket_path`, wait up to `wait`
/// for it to exit, and report what happened.
///
/// Stderr is drained on a helper thread rather than after the fact so the moment
/// the refusal is *printed* can be compared with the moment the process actually
/// *ends* — the two events #4531 pulled ~10s apart.
fn spawn_daemon_and_wait(
    socket_path: &Path,
    workspace: &Path,
    wait: Duration,
) -> (Child, RefusedStart) {
    let mut child = daemon_command(socket_path, workspace)
        .spawn()
        .expect("Failed to spawn second daemon");

    // (refusal timestamp, accumulated stderr)
    let observed: Arc<Mutex<(Option<Instant>, String)>> =
        Arc::new(Mutex::new((None, String::new())));
    let reader_handle = child.stderr.take().map(|stderr| {
        let observed = Arc::clone(&observed);
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stderr)
                .lines()
                .map_while(Result::ok)
            {
                let now = Instant::now();
                let mut guard = observed.lock().expect("stderr mutex poisoned");
                if guard.0.is_none() && line.contains(REFUSAL_MARKER) {
                    guard.0 = Some(now);
                }
                guard.1.push_str(&line);
                guard.1.push('\n');
            }
        })
    });

    let start = Instant::now();
    let mut exit_at = None;
    loop {
        match child.try_wait().expect("try_wait failed") {
            Some(_status) => {
                exit_at = Some(Instant::now());
                break;
            }
            None => {
                if start.elapsed() > wait {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    // The write end closes when the child exits, so the reader thread ends on
    // EOF. If the child is still running (the failure case) the pipe never
    // closes — leave the thread detached rather than hanging the test here; the
    // caller asserts on `exit_at` immediately and kills the child.
    if exit_at.is_some() {
        if let Some(handle) = reader_handle {
            let _ = handle.join();
        }
    }

    let (refusal_at, stderr) = {
        let guard = observed.lock().expect("stderr mutex poisoned");
        (guard.0, guard.1.clone())
    };

    (
        child,
        RefusedStart {
            exit_at,
            refusal_at,
            stderr,
        },
    )
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
    // The spawn-to-exit bound is deliberately generous. The singleton guard's
    // liveness probe itself is ~500ms, but it runs *after* the rest of daemon
    // startup, and a 10s bound failed deterministically on a loaded 8-core host
    // where spawn to refusal measured ~15s (#4385). The poll loop returns as
    // soon as the child exits, so a large bound costs nothing on a passing run —
    // it only stops a saturated machine from turning a liveness check into a
    // latency assertion. The one genuine latency assertion (#4531) is made
    // separately below, measured from the refusal line rather than from spawn.
    let workspace = tempfile::TempDir::new().expect("temp dir for second daemon");
    let (mut child, refused) =
        spawn_daemon_and_wait(&socket_path, workspace.path(), DAEMON_REFUSAL_WAIT);
    let Some(exit_at) = refused.exit_at else {
        let _ = child.kill();
        let _ = child.wait();
        panic!(
            "second daemon should exit promptly after refusing to start; stderr so far:\n{}",
            refused.stderr
        );
    };
    let status = child.wait().expect("wait on second daemon");
    assert!(
        !status.success(),
        "second daemon must exit non-zero when a live daemon owns the socket; got {status:?}"
    );

    // The refusal must reach stderr — the operator-facing half of the contract,
    // and the half an early `std::process::exit` could truncate.
    assert!(
        refused.stderr.contains(REFUSAL_MARKER),
        "second daemon must print the refusal to stderr; got:\n{}",
        refused.stderr
    );

    // …and having printed it, the process must actually end (#4531). Before the
    // fix it printed and then sat in `Runtime::drop` for ~10s.
    let refusal_at = refused
        .refusal_at
        .expect("refusal timestamp (the message was found on stderr)");
    let lag = exit_at.saturating_duration_since(refusal_at);
    assert!(
        lag < REFUSAL_TO_EXIT_BUDGET,
        "second daemon printed its refusal but took {lag:?} to exit (budget {REFUSAL_TO_EXIT_BUDGET:?}) — \
         the refusal path must end the process, not unwind through a runtime shutdown that blocks"
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

    let mut child = daemon_command(&socket_path, temp_dir.path())
        .spawn()
        .expect("Failed to spawn daemon");

    // Poll until the leftover regular file has been replaced by a real socket,
    // instead of sleeping a fixed budget and hoping startup fits inside it. The
    // old form waited a flat 2s purely to prove the daemon had NOT exited early;
    // on a loaded host that is less than the daemon needs to bind (#4531), so
    // the actual bind was silently left to `TestClient::connect`'s retry budget.
    // This loop asserts both properties directly and returns the moment the
    // socket exists.
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("try_wait failed") {
            panic!(
                "daemon should start normally against a stale socket file, not exit early (got {status:?})"
            );
        }
        let bound = std::fs::metadata(&socket_path)
            .map(|m| m.file_type().is_socket())
            .unwrap_or(false);
        if bound {
            break;
        }
        if start.elapsed() > DAEMON_BIND_WAIT {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "daemon did not reclaim the stale socket within {}s",
                DAEMON_BIND_WAIT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    // It should be a live daemon now.
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
