//! The cycle lock is released when it is dropped, even while a child that
//! another thread forked still holds a copy of its descriptor (#11066).
//!
//! `flock` locks belong to the open file description, and a forked child
//! shares the parent's descriptions until it execs (`O_CLOEXEC` closes them
//! only at exec). Releasing by `close` alone leaves the lock held for as long
//! as any such child sits between fork and exec. In the test binary that
//! child is some other test's `gh` stub. In the daemon it is any `gh` spawn.
//! The lock then reads as held by a cycle that has already finished:
//! `run_cycle` returns `Busy`, and `note_captain_gate` and the export pass
//! skip their writes. Those were the parallel-only failures #11066 reported
//! (`a_stale_page_2_...`, `a_no_captain_tick_...`). The fix unlocks
//! explicitly (`LOCK_UN`), which releases the description for every copy.

use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::Command;

use super::*;

/// Fork a child that signals on `ready` once forked, then lingers before
/// exec. Until it execs it holds a copy of every descriptor the parent had
/// open at the fork, including a held cycle lock.
fn spawn_lingering_child(ready: UnixStream) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let fd = ready.as_raw_fd();
        let mut command = Command::new("true");
        // SAFETY: the hook runs in the forked child before exec and calls
        // only async-signal-safe functions (`write`, `nanosleep`).
        unsafe {
            command.pre_exec(move || {
                libc::write(fd, b"x".as_ptr().cast(), 1);
                let pause = libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 300_000_000,
                };
                libc::nanosleep(&pause, std::ptr::null_mut());
                Ok(())
            });
        }
        command.status().unwrap();
    })
}

#[test]
fn a_dropped_lock_is_free_while_a_forked_child_still_holds_its_descriptor() {
    let dir = TempDir::new().unwrap();
    let lock_dir = state_dir(dir.path());
    let held = state::CycleLock::try_acquire(&lock_dir).unwrap().unwrap();

    let (ready, mut forked) = UnixStream::pair().unwrap();
    let child = spawn_lingering_child(ready);
    let mut byte = [0_u8; 1];
    forked.read_exact(&mut byte).unwrap();

    // The child is now between fork and exec with a copy of the lock's
    // descriptor. Dropping ours must still release the lock.
    drop(held);
    let again = state::CycleLock::try_acquire(&lock_dir).unwrap();
    assert!(
        again.is_some(),
        "a finished holder's lock must not stay held by a forked child's copy"
    );
    drop(again);
    child.join().unwrap();
}

#[test]
fn a_cycle_right_after_another_is_not_busy_while_a_forked_child_lingers() {
    let dir = TempDir::new().unwrap();
    let lock_dir = state_dir(dir.path());
    let held = state::CycleLock::try_acquire(&lock_dir).unwrap().unwrap();

    let (ready, mut forked) = UnixStream::pair().unwrap();
    let child = spawn_lingering_child(ready);
    let mut byte = [0_u8; 1];
    forked.read_exact(&mut byte).unwrap();
    drop(held);

    let report = run_cycle(&ctx(dir.path()), &FixtureApi::new());
    assert!(report.is_ok(), "{:?}", report.err());
    child.join().unwrap();
}
