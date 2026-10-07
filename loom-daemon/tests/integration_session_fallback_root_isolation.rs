// Integration guard (#10661): a test daemon never writes the session
// reconcile loop's fallback-root record under the real `~/.loom`.
//
// The full daemon records its fallback root at startup
// (`session_reconcile::spawn_from_config`) so that `accounts session` CLI
// invocations can read and lift holds in it. A test daemon that wrote the
// real `~/.loom/session-reconcile-fallback-root.json` would overwrite the
// host daemon's record with a temp dir that is deleted moments later.
// `common::isolate_daemon_state` turns the loop off and redirects the record;
// this test turns the loop back ON (the worst case) and checks both halves:
// the record lands in the fixture, and the real one's presence and mtime are
// unchanged.
//
// expect/unwrap are acceptable here since tests should panic on failure.
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]

#[allow(dead_code)]
mod common;

use common::{daemon_bin, isolate_daemon_state, RealFallbackRecordGuard};
use serial_test::serial;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const RECORD_WAIT: Duration = Duration::from_secs(90);

#[test]
#[serial]
fn a_test_daemon_writes_its_fallback_root_record_only_inside_its_fixture() {
    let _real_record = RealFallbackRecordGuard::arm();
    let fixture = tempfile::tempdir().unwrap();
    let workspace = fixture.path().canonicalize().unwrap();
    let mut cmd = Command::new(daemon_bin());
    isolate_daemon_state(&mut cmd, &workspace);
    cmd.env("LOOM_SOCKET_PATH", workspace.join("daemon.sock"))
        .env("LOOM_NO_RESTORE", "1")
        .env("LOOM_ROLE_RUNNER", "0")
        .env("LOOM_WORK_FINDER", "0")
        .env("LOOM_EPIC_SUPERVISOR", "0")
        .env("LOOM_WORKSPACE", &workspace)
        .env("LOOM_WORKTREE_ROOT", workspace.join("worktrees"))
        // No Codex accounts here: the enabled pass makes zero docker calls.
        .env("LOOM_CODEX_PROFILE_ROOT", workspace.join("profiles"))
        // The worst case: the loop on, so the record IS written.
        .env("LOOM_SESSION_RECONCILE", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    std::fs::create_dir_all(workspace.join("profiles")).unwrap();
    let mut child = cmd.spawn().expect("spawn daemon");

    let record = workspace.join("session-reconcile-fallback-root.json");
    let started = Instant::now();
    while !record.exists() && started.elapsed() < RECORD_WAIT {
        if let Ok(Some(status)) = child.try_wait() {
            panic!("the daemon exited before recording its fallback root: {status}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();

    let body = std::fs::read_to_string(&record).unwrap_or_else(|e| {
        panic!("no fallback-root record in the fixture after {RECORD_WAIT:?}: {e}")
    });
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["root"].as_str(), Some(workspace.to_str().unwrap()), "{body}");
}
