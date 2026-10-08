//! `loom-daemon agent-resume capture-codex` must not hold a private Codex
//! account's lease (#10830, PR #10864).
//!
//! A private-workspace dispatch hands `spawn-codex.sh` the account lease as an
//! inherited descriptor (`LOOM_PRIVATE_LEASE_FD`) with close-on-exec cleared.
//! The script backgrounds this watcher, which inherits the descriptor and can
//! outlive the script by one poll. The lease is a `flock`, shared by every
//! copy of the descriptor, so a watcher that kept its copy kept the account
//! busy after the run ended: the next dispatch got "account session is busy".
//!
//! Driven through the real binary, because the property is about what the
//! process holds after exec, not about a function.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The descriptor number the dispatcher uses.
const LEASE_FD: i32 = 198;

/// Whether a fresh open of `path` can take the account lock right now.
fn lock_is_free(path: &Path) -> bool {
    let probe = std::fs::File::open(path).unwrap();
    // SAFETY: `probe` is an open file for the whole call.
    unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// Start a watcher that inherits a held lease on `lock` as [`LEASE_FD`], the
/// way a child of `spawn-codex.sh` does. It watches this test process and a
/// capture file that never gets a session id, so it runs until it is killed.
/// On return the watcher's copy is the only one left.
fn watcher_holding_the_lease(dir: &Path, lock: &Path, name_the_fd: bool) -> Child {
    let owner = std::fs::File::create(lock).unwrap();
    let fd = owner.as_raw_fd();
    // SAFETY: `owner` is an open file for the whole call.
    assert_eq!(unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) }, 0);
    let mut command = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
    command
        .args(["agent-resume", "capture-codex", "--timeout-secs", "120"])
        .arg("--stderr-file")
        .arg(dir.join("no-capture"))
        .arg("--handle-file")
        .arg(dir.join("handle.json"))
        .arg("--watch-pid")
        .arg(std::process::id().to_string())
        .current_dir(dir)
        .env_remove("LOOM_PRIVATE_LEASE_FD")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if name_the_fd {
        command.env("LOOM_PRIVATE_LEASE_FD", LEASE_FD.to_string());
    }
    // SAFETY: only async-signal-safe calls between fork and exec. This is the
    // dispatcher's own hand-off (`Selection::apply`).
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(fd, LEASE_FD) < 0 || libc::fcntl(LEASE_FD, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().unwrap();
    drop(owner);
    child
}

fn stop(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn a_running_capture_watcher_does_not_hold_the_account_lease() {
    let tmp = tempfile::tempdir().unwrap();

    // Control: a process that inherits the descriptor and is not told about it
    // keeps the account busy for as long as it lives. This is what the watcher
    // did, and it shows the hand-off below really passes the lease on.
    let control_lock = tmp.path().join("control.lock");
    let mut control = watcher_holding_the_lease(tmp.path(), &control_lock, false);
    std::thread::sleep(Duration::from_millis(500));
    assert!(control.try_wait().unwrap().is_none(), "the control watcher is still running");
    assert!(!lock_is_free(&control_lock), "an inherited lease copy keeps the account busy");
    stop(control);
    assert!(lock_is_free(&control_lock));

    // The watcher is told which descriptor is the lease and closes it, so the
    // account is free while the watcher is still running.
    let lock = tmp.path().join("account.lock");
    let mut watcher = watcher_holding_the_lease(tmp.path(), &lock, true);
    let deadline = Instant::now() + Duration::from_secs(60);
    while !lock_is_free(&lock) {
        assert!(Instant::now() < deadline, "the watcher still holds the account lease");
        assert!(watcher.try_wait().unwrap().is_none(), "the watcher exited before releasing");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        watcher.try_wait().unwrap().is_none(),
        "the lease was freed by the close, not by exit"
    );
    stop(watcher);
}
