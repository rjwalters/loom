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

use common::{cleanup_test_sessions, daemon_bin, isolate_daemon_state, TestClient, TestDaemon};
use serial_test::serial;
use std::io::BufRead;
use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Scoped to this binary's `TEST_PREFIX` (issue #4622) — a host-wide kill was
/// never actually needed here: this suite tests socket/singleton behavior and
/// never creates a named terminal/tmux session at all, so there is nothing for
/// the broad cleanup to catch that the scoped helper would miss.
fn setup() {
    cleanup_test_sessions();
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
    // The refusal path is reached before any workspace work, but a daemon that
    // does NOT refuse (the failure these tests detect) would otherwise come up
    // pointed at the operator's real machine-level state — sweep journal, watch
    // registry, cwd-derived default workspace — so confine all of it to the
    // per-test fixture first (#4556). Complements the #4573 vars below, which
    // pin the workspace registry and worktree root.
    isolate_daemon_state(&mut cmd, workspace);
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
    spawn_and_wait(daemon_command(socket_path, workspace), wait)
}

/// [`spawn_daemon_and_wait`]'s core, taking an already-built [`Command`] so a
/// caller can add env the standard fixture does not set (#4774's
/// `LOOM_PID_FILE`) without duplicating the stderr-timing machinery.
fn spawn_and_wait(mut cmd: Command, wait: Duration) -> (Child, RefusedStart) {
    let mut child = cmd.spawn().expect("Failed to spawn second daemon");

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

// ============================================================================
// Issue #4774 — the daemon owns its pid file write
// ============================================================================
//
// These live in the singleton-guard suite on purpose: #4774's whole design
// hinges on *where* the write sits relative to the guard. The pid file is
// claimed immediately after `UnixListener::bind` succeeds, which makes the two
// properties below structural rather than incidental — a daemon that binds has
// necessarily passed the guard, and a daemon the guard refuses never reaches
// the write at all. Testing them anywhere else would test the claim function
// instead of the invariant.

/// How long to wait for a pid file to name an expected pid. Same liveness-bound
/// reasoning as [`DAEMON_BIND_WAIT`] — the poll returns the moment the file
/// agrees, so a generous ceiling costs a passing run nothing.
const PID_FILE_WAIT: Duration = Duration::from_secs(60);

/// The pid recorded in `pid_file`, or `None` when it is missing/unparseable.
fn recorded_pid(pid_file: &Path) -> Option<u32> {
    std::fs::read_to_string(pid_file)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
}

/// Poll until `socket_path` is a *bound socket*, panicking if `child` exits
/// first (a daemon that refused when it should have started) or the bind budget
/// elapses. Mirrors `test_stale_socket_is_reclaimed`'s loop.
fn wait_for_bind(child: &mut Child, socket_path: &Path, what: &str) {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("try_wait failed") {
            panic!("{what} should have started and bound the socket, but exited ({status:?})");
        }
        let bound = std::fs::metadata(socket_path)
            .map(|m| m.file_type().is_socket())
            .unwrap_or(false);
        if bound {
            return;
        }
        if start.elapsed() > DAEMON_BIND_WAIT {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{what} did not bind the socket within {}s", DAEMON_BIND_WAIT.as_secs());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Poll until `pid_file` records `expected`, returning whatever it actually held
/// when the budget elapsed. The bind and the pid-file claim are separate
/// statements in `IpcServer::run`, so the socket can exist a beat before the
/// file is rewritten — this waits for the second event rather than racing it.
fn wait_for_recorded_pid(pid_file: &Path, expected: u32) -> Option<u32> {
    let start = Instant::now();
    loop {
        let observed = recorded_pid(pid_file);
        if observed == Some(expected) || start.elapsed() > PID_FILE_WAIT {
            return observed;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// AC4 regression: a **supervisor-style relaunch** — the same binary re-spawned
/// with the same baked-in env, with `loom-daemon-start.sh` nowhere in the
/// picture — must leave a pid file naming the *new* process.
///
/// This is the exact 2026-07-31 incident reduced to two processes. Before
/// #4774 the pid file was written only by the start script at provisioning
/// time, so every launchd `KeepAlive` respawn, `systemd Restart=`, restart
/// primitive, and self-update roll left it naming a pid that no longer existed
/// — and every liveness cross-check that consulted it inherited the lie.
#[tokio::test]
#[serial]
async fn test_supervisor_style_relaunch_rewrites_the_pid_file() {
    setup();

    let temp_dir = tempfile::TempDir::new().expect("temp dir");
    let socket_path = temp_dir.path().join("daemon.sock");
    let pid_file = temp_dir.path().join(".daemon.pid");

    // Seed the incident's shape: a pid file left behind by an earlier
    // generation, naming a pid this daemon will not have.
    std::fs::write(&pid_file, "13724\n").expect("seed stale pid file");

    // --- Generation 1: the daemon the start script provisioned. ---
    let mut first = daemon_command(&socket_path, temp_dir.path())
        .env("LOOM_PID_FILE", &pid_file)
        .spawn()
        .expect("spawn first daemon");
    wait_for_bind(&mut first, &socket_path, "first daemon");
    let first_pid = first.id();
    assert_eq!(
        wait_for_recorded_pid(&pid_file, first_pid),
        Some(first_pid),
        "a daemon that binds the socket must claim the pid file for itself, replacing the \
         seeded stale pid 13724"
    );

    // --- The supervisor's trigger: an ungraceful death. --------------------
    // `kill -9` specifically, not a graceful `Shutdown`: it is what a crash,
    // an OOM kill, and the `RestartDaemon` primitive's teardown all look like
    // to the file, and it leaves the socket behind as a stale regular entry
    // for generation 2 to reclaim.
    let _ = first.kill();
    let _ = first.wait();
    assert_eq!(
        recorded_pid(&pid_file),
        Some(first_pid),
        "a killed daemon does not get to clean up — the file still names it, which is exactly \
         the state the relaunch must correct"
    );

    // --- Generation 2: the relaunch. --------------------------------------
    // Same binary, same socket, same `LOOM_PID_FILE`, no start script — a
    // faithful stand-in for `KeepAlive`/`Restart=` re-exec'ing the plist/unit
    // command with its render-time environment.
    let mut second = daemon_command(&socket_path, temp_dir.path())
        .env("LOOM_PID_FILE", &pid_file)
        .spawn()
        .expect("spawn relaunched daemon");
    wait_for_bind(&mut second, &socket_path, "relaunched daemon");
    let second_pid = second.id();
    assert_ne!(
        second_pid, first_pid,
        "the relaunch must be a genuinely new process for this test to mean anything"
    );

    let observed = wait_for_recorded_pid(&pid_file, second_pid);
    // Teardown before asserting, so a failure does not also leak a daemon.
    let _ = second.kill();
    let _ = second.wait();

    assert_eq!(
        observed,
        Some(second_pid),
        "THE #4774 REGRESSION: after a supervisor-style relaunch the pid file must name the new \
         daemon ({second_pid}), not the dead one ({first_pid}). A stale value here means the \
         daemon is once again relying on `loom-daemon-start.sh` to write a file no relaunch \
         path re-runs it to write."
    );
}

/// The AC edge case, and the reason the claim sits *after* the bind rather than
/// beside #4331's marker healing: a daemon the singleton guard **refuses** must
/// leave the live incumbent's pid file completely untouched.
///
/// Written as a hazard test, not a happy path. Hoisting the claim to
/// `daemon_service.rs`'s pre-guard startup block — the obvious place, since
/// that is where the autonomy-desired marker heals — would make the refused
/// process stomp the incumbent's correct pid with its own doomed one on the way
/// out, converting the guard's "refuse without disturbing the incumbent"
/// contract into active corruption.
#[tokio::test]
#[serial]
async fn test_a_refused_daemon_does_not_overwrite_the_incumbents_pid_file() {
    setup();

    let temp_dir = tempfile::TempDir::new().expect("temp dir");
    let socket_path = temp_dir.path().join("daemon.sock");
    let pid_file = temp_dir.path().join(".daemon.pid");

    // The incumbent binds the socket and claims the pid file.
    let mut incumbent = daemon_command(&socket_path, temp_dir.path())
        .env("LOOM_PID_FILE", &pid_file)
        .spawn()
        .expect("spawn incumbent daemon");
    wait_for_bind(&mut incumbent, &socket_path, "incumbent daemon");
    let incumbent_pid = incumbent.id();
    assert_eq!(
        wait_for_recorded_pid(&pid_file, incumbent_pid),
        Some(incumbent_pid),
        "incumbent must claim the pid file before the interloper is introduced"
    );

    // A second daemon on the SAME socket AND the same pid file — the strongest
    // form of the hazard, since a pre-guard write would have nothing to miss.
    let interloper_ws = tempfile::TempDir::new().expect("temp dir for interloper");
    let mut cmd = daemon_command(&socket_path, interloper_ws.path());
    cmd.env("LOOM_PID_FILE", &pid_file);
    let (mut child, refused) = spawn_and_wait(cmd, DAEMON_REFUSAL_WAIT);

    let exited = refused.exit_at.is_some();
    if !exited {
        let _ = child.kill();
    }
    let _ = child.wait();

    // Read the file back BEFORE tearing the incumbent down.
    let after_refusal = recorded_pid(&pid_file);
    let _ = incumbent.kill();
    let _ = incumbent.wait();

    assert!(
        exited,
        "the interloper should have been refused and exited; stderr:\n{}",
        refused.stderr
    );
    assert!(
        refused.stderr.contains(REFUSAL_MARKER),
        "the interloper should have printed the singleton refusal; stderr:\n{}",
        refused.stderr
    );
    assert_eq!(
        after_refusal,
        Some(incumbent_pid),
        "a REFUSED daemon must never touch the pid file — it still belongs to the live \
         incumbent ({incumbent_pid}). Seeing the refused process's pid here means the claim was \
         hoisted ahead of the singleton guard (#4774)."
    );
}
