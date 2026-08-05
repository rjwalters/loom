// End-to-end integration test for the exact incident chain in Issue #5390
// ("auto-update drain exits 0 for a launchd relaunch that never comes").
//
// #5390's curator enhancement verified that the two halves of this incident
// are each ALREADY tested in isolation:
//   - the internal auto-update path's `exit(EXIT_RESTART)` contract is pinned
//     by `integration_drain_then_exit.rs` (the `then_exit: true` sibling of
//     the request this test issues with `then_exit: false`), and
//   - the watchdog's bounded kickstart self-heal for "job loaded, not
//     running, last exit status 0" is pinned by
//     `test-loom-daemon-watchdog.sh`'s `exit-0-and-down` case and
//     `test-loom-daemon-update.sh` test 35.
// Nothing tied them together end-to-end: a real daemon actually taking the
// internal drain-then-`exit(0)` path, a supervisor that (like the real
// `robb-studio` incident) never relaunches it on its own, and a watchdog tick
// that observes exactly that reality and recovers it with a genuinely NEW
// process. That chain is what this test drives.
//
// WHY SYSTEMD, NOT LAUNCHD: this suite runs on Linux CI (`ubuntu-latest`).
// The watchdog script's launchd auto-remediation gate is guarded by
// `[[ "$(uname -s)" == "Darwin" ]] || USE_LAUNCHD=false` — on any non-macOS
// host it is unconditionally disabled, which is why every launchd-specific
// case in `test-loom-daemon-watchdog.sh` is wrapped in its own Darwin check
// and skips cleanly elsewhere. The systemd mirror added for #4862 carries NO
// such platform clobber (deliberately — see the comment above `USE_SYSTEMD`
// in the script), so it is the one auto-remediation path this test can
// actually execute in this repository's CI. The Rust-side drain/exit
// mechanics under test are supervisor-agnostic (`EXIT_RESTART` is the same
// `0` either way), so pinning the systemd mirror exercises the identical
// end-to-end contract the launchd incident describes.
//
// `systemctl` itself is never invoked for real — a stub is put first on
// PATH, exactly the technique `test-loom-daemon-watchdog.sh` test 10a and
// `test-loom-daemon-update.sh` test 35 already use, so this test cannot touch
// a real systemd --user manager on the host running it.
//
// expect/unwrap are acceptable here since tests should panic on failure.
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]

mod common;

use common::{daemon_bin, isolate_daemon_state, TestClient};
use loom_daemon::ipc::EXIT_RESTART;
use serial_test::serial;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Kills the spawned daemon if the test panics before it exits on its own.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Kills a bare pid (the second daemon instance, spawned indirectly by the
/// stubbed `systemctl start` — there is no `Child` handle for it, only the
/// pid it wrote to the state file) if the test panics before cleanup.
struct PidGuard(u32);

impl Drop for PidGuard {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-9", &self.0.to_string()])
            .status();
    }
}

/// Liveness bound for the drain-then-exit round trip. Mirrors
/// `integration_drain_then_exit.rs`'s `DRAIN_EXIT_WAIT`.
const DRAIN_EXIT_WAIT: Duration = Duration::from_secs(60);

/// Bound on the second daemon instance (spawned by the stubbed
/// `systemctl start`) creating its socket.
const RESPAWN_SOCKET_WAIT: Duration = Duration::from_secs(30);

/// The full incident chain: internal auto-update-shaped drain request ->
/// `exit(EXIT_RESTART)` -> simulated supervisor non-relaunch -> a watchdog
/// tick that observes "unit loaded, not running, last exit 0" and kickstarts
/// a real recovery.
#[tokio::test]
#[serial]
async fn test_drain_exit0_then_watchdog_systemd_kickstart_recovers() {
    let temp_dir = tempfile::TempDir::new().expect("temp dir");
    let socket_path = temp_dir.path().join("daemon.sock");
    let workspace_root = temp_dir.path().join("workspace");
    std::fs::create_dir_all(&workspace_root).expect("create workspace root");
    let worktree_root = temp_dir.path().join("worktrees");

    // A unit name unique to this test run — never resolved against any real
    // systemd manager, since `systemctl` itself is stubbed below.
    let unit_name = format!("loom-daemon-test-5390-{}.service", std::process::id());

    // ------------------------------------------------------------------
    // Phase 1: the internal auto-update drain path.
    //
    // `auto_update::IpcDrainTrigger::trigger()` calls
    // `ipc::handle_drain_request(..., then_exit: false)` — this is that exact
    // request shape, issued directly over the real IPC socket against a real
    // daemon binary, so the assertion below exercises the same
    // `handle_drain_request` -> `run_drain_supervisor` ->
    // `std::process::exit(EXIT_RESTART)` path the auto-update loop's rebuild
    // completion drives in production.
    // ------------------------------------------------------------------
    let mut cmd = Command::new(daemon_bin());
    isolate_daemon_state(&mut cmd, temp_dir.path());
    let mut child = ChildGuard(
        cmd.current_dir(&workspace_root)
            .env("LOOM_SOCKET_PATH", &socket_path)
            .env("LOOM_WORKSPACE", &workspace_root)
            .env("LOOM_ROLE_RUNNER", "0")
            .env("LOOM_WORK_FINDER", "0")
            .env("LOOM_EPIC_SUPERVISOR", "0")
            .env("LOOM_WORKTREE_ROOT", &worktree_root)
            .env("RUST_LOG", "debug")
            .env("LOOM_NO_RESTORE", "1")
            // The daemon's own record of how it is supervised — read by
            // `detect_supervisor()`, which `handle_drain_request` requires to
            // be `Some` before it will accept a `then_exit: false` (relaunch)
            // drain at all. `systemd` (not `launchd`) so the watchdog phase
            // below stays on the one auto-remediation path this suite can
            // actually execute on Linux CI (see the module doc comment).
            .env("LOOM_DAEMON_SUPERVISOR", "systemd")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn daemon"),
    );

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

    let original_pid = child.0.id();

    let request = serde_json::json!({
        "type": "DrainAndRestartDaemon",
        "payload": {
            "timeout_secs": 60,
            "force_after_timeout": false,
            "then_exit": false
        }
    });
    let reply = client
        .send_request(request)
        .await
        .expect("daemon replies to DrainAndRestartDaemon");
    assert_eq!(
        reply.get("type").and_then(serde_json::Value::as_str),
        Some("DaemonDrain"),
        "unexpected reply: {reply:?}"
    );
    let payload = reply.get("payload").expect("DaemonDrain payload");
    assert_eq!(
        payload.get("accepted").and_then(serde_json::Value::as_bool),
        Some(true),
        "a relaunch drain must be accepted when a supervisor is declared: {reply:?}"
    );
    assert_eq!(
        payload
            .get("supervisor")
            .and_then(serde_json::Value::as_str),
        Some("systemd"),
        "the ack must report the daemon's own declared supervisor: {reply:?}"
    );
    assert_eq!(
        payload
            .get("then_exit")
            .and_then(serde_json::Value::as_bool),
        Some(false),
        "this is a RELAUNCH drain (the auto-update roll's shape), not a teardown: {reply:?}"
    );
    drop(client);

    // The daemon must exit for a supervised relaunch: EXIT_RESTART (0). This
    // is deliberately the SAME exit code a healthy, working relaunch also
    // produces — the failure mode under test is entirely about what the
    // supervisor does (or fails to do) next, not about the daemon's own exit.
    let start = Instant::now();
    let status = loop {
        match child.0.try_wait().expect("try_wait") {
            Some(status) => break status,
            None => {
                assert!(
                    start.elapsed() < DRAIN_EXIT_WAIT,
                    "daemon did not exit after the relaunch drain with 0 in-flight"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    };
    assert_eq!(
        status.code(),
        Some(EXIT_RESTART),
        "a relaunch drain must exit {EXIT_RESTART} (EXIT_RESTART) for the supervisor to relaunch \
         it. Got {status:?}"
    );

    // ------------------------------------------------------------------
    // Phase 2: simulate "the supervisor never relaunches it" — the exact
    // #5390 failure mode. Nothing in this test process relaunches the
    // daemon, so a fresh connect must fail. This IS the simulated
    // non-relaunch: from here on, the daemon is down with no live listener,
    // precisely the state `robb-studio` was found in.
    // ------------------------------------------------------------------
    let re_listened = match TestClient::connect(&socket_path).await {
        Ok(mut c) => c.ping().await.is_ok(),
        Err(_) => false,
    };
    assert!(
        !re_listened,
        "nothing may answer on the socket yet — the supervisor has not relaunched anything"
    );

    // ------------------------------------------------------------------
    // Phase 3: a watchdog tick observes "unit loaded, not running, last exit
    // 0" (systemd's `ExecMainCode=exited` / `ExecMainStatus=0` — the #4862
    // mirror of the launchd `last exit status = 0` signature #5390
    // describes) and kickstarts. `systemctl` is stubbed so this test never
    // touches a real systemd --user manager; the stub's `start` action is
    // the one thing that is REAL: it spawns a genuine second instance of the
    // exact same `loom-daemon` binary, bound to the exact same socket.
    // ------------------------------------------------------------------
    let stub_dir = temp_dir.path().join("stub-bin");
    std::fs::create_dir_all(&stub_dir).expect("create stub bin dir");
    let state_path = temp_dir.path().join("systemctl-state"); // empty => "not running"
    std::fs::write(&state_path, "").expect("seed empty state file");
    let systemctl_log = temp_dir.path().join("systemctl.log");
    std::fs::write(&systemctl_log, "").expect("seed empty log file");
    let daemon_out_log = temp_dir.path().join("respawned-daemon.out");

    let systemctl_stub = format!(
        r#"#!/usr/bin/env bash
echo "$*" >> "{log}"
if [[ "${{1:-}}" == "--user" ]]; then shift; fi
case "${{1:-}}" in
  show)
    case "$*" in
      *"-p MainPID"*)        pid="$(cat "{state}" 2>/dev/null)"; echo "${{pid:-0}}" ;;
      *"-p LoadState"*)      echo "loaded" ;;
      *"-p ExecMainCode"*)   echo "exited" ;;
      *"-p ExecMainStatus"*) echo "0" ;;
      *)                     echo "" ;;
    esac
    ;;
  reset-failed) exit 0 ;;
  start)
    "{daemon_bin}" >>"{daemon_out}" 2>&1 &
    new_pid=$!
    disown
    echo "$new_pid" > "{state}"
    exit 0
    ;;
  *) exit 0 ;;
esac
"#,
        log = systemctl_log.display(),
        state = state_path.display(),
        daemon_bin = daemon_bin().display(),
        daemon_out = daemon_out_log.display(),
    );
    let systemctl_stub_path = stub_dir.join("systemctl");
    std::fs::write(&systemctl_stub_path, systemctl_stub).expect("write systemctl stub");
    std::fs::set_permissions(&systemctl_stub_path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod systemctl stub");

    let marker_path = temp_dir.path().join("autonomy-desired");
    let heartbeat_path = temp_dir.path().join("daemon.heartbeat");
    let mut marker_file = std::fs::File::create(&marker_path).expect("create marker");
    writeln!(
        marker_file,
        "started_at=2026-08-05T00:00:00Z\n\
         heartbeat_file={heartbeat}\n\
         heartbeat_interval_secs=60\n\
         use_launchd=false\n\
         use_systemd=true\n\
         systemd_unit={unit}\n\
         socket_path={socket}",
        heartbeat = heartbeat_path.display(),
        unit = unit_name,
        socket = socket_path.display(),
    )
    .expect("write marker contents");
    drop(marker_file);

    let watchdog_log = temp_dir.path().join("watchdog.log");
    let recovery_state = temp_dir.path().join("watchdog-recovery-state");
    let watchdog_script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../defaults/scripts/cli/loom-daemon-watchdog.sh");
    assert!(
        watchdog_script.is_file(),
        "watchdog script not found at {}",
        watchdog_script.display()
    );

    let path_var = format!("{}:{}", stub_dir.display(), std::env::var("PATH").unwrap_or_default());

    let watchdog_output = Command::new("bash")
        .arg(&watchdog_script)
        .env("PATH", path_var)
        // Same #4573/#4556 confinement as the original daemon spawn — in
        // case the stub's `start` action ever runs before this env is set on
        // its own Command (it inherits from this one).
        .env("LOOM_WORKSPACE", &workspace_root)
        .env("LOOM_WORKSPACES_PATH", temp_dir.path().join("workspaces.json"))
        .env("LOOM_SWEEPS_JOURNAL_PATH", temp_dir.path().join("sweeps.json"))
        .env("LOOM_WATCHES_PATH", temp_dir.path().join("watches.json"))
        .env(
            "LOOM_WATCH_RESULTS_LOG",
            temp_dir.path().join("watch-results.log"),
        )
        .env("LOOM_ROLE_RUNNER", "0")
        .env("LOOM_WORK_FINDER", "0")
        .env("LOOM_EPIC_SUPERVISOR", "0")
        .env("LOOM_WORKTREE_ROOT", &worktree_root)
        .env("LOOM_NO_RESTORE", "1")
        .env("LOOM_DAEMON_SUPERVISOR", "systemd")
        .env("LOOM_SOCKET_PATH", &socket_path)
        // Watchdog-specific env (mirrors `SUPERVISOR_CASE_ENV` /
        // `run_watchdog` in test-loom-daemon-watchdog.sh):
        .env("LOOM_WATCHDOG_IPC_PROBE", "0")
        .env("LOOM_PID_FILE", "")
        .env("LOOM_MACHINE_CHECKOUT", "")
        .env("LOOM_WATCHDOG_AUTO_RECOVER", "0")
        .env("LOOM_WATCHDOG_ESCALATE", "0")
        .env("LOOM_WATCHDOG_RECOVERY_STATE", &recovery_state)
        .env("LOOM_AUTONOMY_MARKER", &marker_path)
        .env("LOOM_WATCHDOG_LOG", &watchdog_log)
        .env("LOOM_DAEMON_LAUNCHD", "0")
        .env("LOOM_WATCHDOG_KICKSTART_RECHECK_ATTEMPTS", "40")
        .env("LOOM_WATCHDOG_KICKSTART_RECHECK_INTERVAL", "0.25")
        .output()
        .expect("run watchdog script");

    let watchdog_stdout = String::from_utf8_lossy(&watchdog_output.stdout).to_string();
    let watchdog_stderr = String::from_utf8_lossy(&watchdog_output.stderr).to_string();
    let watchdog_log_contents = std::fs::read_to_string(&watchdog_log).unwrap_or_default();

    assert!(
        watchdog_output.status.success(),
        "watchdog must exit 0 (auto-remediation succeeded). stdout:\n{watchdog_stdout}\n\
         stderr:\n{watchdog_stderr}\nlog:\n{watchdog_log_contents}"
    );
    assert!(
        watchdog_log_contents.to_lowercase().contains("remediat"),
        "watchdog log must record the remediation: {watchdog_log_contents}"
    );
    let systemctl_invocations = std::fs::read_to_string(&systemctl_log).unwrap_or_default();
    assert!(
        systemctl_invocations.contains("start ") || systemctl_invocations.contains("start\n"),
        "the narrow #4862 gate must invoke 'systemctl --user start': {systemctl_invocations}"
    );

    // ------------------------------------------------------------------
    // Phase 4: confirmed recovery — a genuinely NEW pid, and the daemon
    // actually answering on the socket again. This is the concrete
    // "recovered, with a new pid" the issue asks for; the watchdog's own
    // exit code and log line above are corroborating evidence, not the
    // whole proof.
    // ------------------------------------------------------------------
    let recorded_pid: u32 = std::fs::read_to_string(&state_path)
        .expect("read systemctl state file")
        .trim()
        .parse()
        .expect("state file must contain the respawned daemon's pid");
    assert_ne!(
        recorded_pid, original_pid,
        "the recovered daemon must be a NEW process, not the original one lingering"
    );
    let _new_daemon_guard = PidGuard(recorded_pid);

    let start = Instant::now();
    loop {
        if socket_path.exists() {
            if let Ok(mut c) = TestClient::connect(&socket_path).await {
                if c.ping().await.is_ok() {
                    break;
                }
            }
        }
        assert!(
            start.elapsed() < RESPAWN_SOCKET_WAIT,
            "recovered daemon (pid {recorded_pid}) never answered Ping on {}",
            socket_path.display()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
