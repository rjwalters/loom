//! Per-container advisory lock between a Codex dispatch and anything that
//! stops the session container (issue #10364 Part B; the epic's "never
//! interrupt work" rule).
//!
//! `docker top` cannot see a dispatch that is only starting: `session-exec
//! host` inspects the container, runs a short `protocol` exec, then starts
//! the `worker` exec, and between those nothing runs inside the container.
//! A drift teardown that found it "idle" in such a gap would stop it under
//! the dispatch.
//!
//! * `session-exec host` holds the lock **shared** from before its first
//!   inspect until its worker exec has exited ([`shared`]). Any number of
//!   dispatches share it.
//! * The reconciler's drift teardown — and an operator `accounts session
//!   stop` without `--force` — takes it **exclusive, non-blocking**
//!   immediately before `docker stop` ([`try_exclusive`]) and keeps it until
//!   the container is gone (or recreated). [`Exclusive::Busy`] means a
//!   dispatch is in flight: defer.
//!
//! It is a kernel `flock` on `<lock dir>/<container>.lock`, so it is
//! released when the holder exits, however it exits; there is nothing to
//! reap. The existing `docker top` check stays as the second line (it covers
//! an operator's interactive `attach`, which takes no lock).
//!
//! The files persist (one empty file per container; deleting a `flock`ed
//! file would split later holders onto a new inode). The lock only works when
//! the daemon and its dispatches resolve the same [`lock_dir`], i.e. the same
//! `$HOME` or `LOOM_SESSION_LOCK_DIR`; otherwise `docker top` alone protects
//! a starting dispatch.
//!
//! **Fail-safe both ways.** If the lock file cannot be created or opened, a
//! dispatch proceeds as it always did ([`shared`] returns `None`), and a
//! teardown gets [`Exclusive::Unknown`], which the reconciler treats as
//! busy: it never stops a container when the lock state is unknown.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Overrides the lock directory (tests; a host whose dispatches and daemon
/// do not share a home directory).
pub const LOCK_DIR_ENV: &str = "LOOM_SESSION_LOCK_DIR";

/// How long a dispatch waits for a teardown in progress before proceeding
/// without the lock (its own preflight then reports the container down).
pub const DISPATCH_WAIT: Duration = Duration::from_secs(30);

/// The directory the lock files live in: [`LOCK_DIR_ENV`], else
/// `~/.loom/session-locks`. `None` when neither resolves.
#[must_use]
pub fn lock_dir() -> Option<PathBuf> {
    match std::env::var_os(LOCK_DIR_ENV) {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => dirs::home_dir().map(|home| home.join(".loom").join("session-locks")),
    }
}

/// A held lock; released on drop (and by the kernel if the holder dies).
#[derive(Debug)]
pub struct DispatchLock(#[allow(dead_code)] File);

/// The result of one non-blocking exclusive attempt.
#[derive(Debug)]
pub enum Exclusive {
    /// No dispatch holds the lock; none can start until this is dropped.
    Acquired(DispatchLock),
    /// A dispatch (or another teardown) holds it.
    Busy,
    /// The lock could not be created, opened or queried.
    Unknown(String),
}

fn open(dir: &Path, container: &str) -> std::io::Result<File> {
    std::fs::create_dir_all(dir)?;
    File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(format!("{container}.lock")))
}

fn flock(file: &File, operation: libc::c_int) -> std::io::Result<()> {
    // SAFETY: `flock` on a descriptor this function's caller owns.
    if unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Take `container`'s lock shared, waiting up to `wait` for an exclusive
/// holder to finish. `None` when the lock is unusable or still held
/// exclusively after `wait`: the dispatch then proceeds unlocked, as before
/// this lock existed.
#[must_use]
pub fn shared(dir: &Path, container: &str, wait: Duration) -> Option<DispatchLock> {
    let file = open(dir, container).ok()?;
    let deadline = Instant::now() + wait;
    loop {
        match flock(&file, libc::LOCK_SH) {
            Ok(()) => return Some(DispatchLock(file)),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return None,
        }
    }
}

/// [`shared`] in the default [`lock_dir`], for a dispatch.
#[must_use]
pub fn shared_for_dispatch(container: &str) -> Option<DispatchLock> {
    shared(&lock_dir()?, container, DISPATCH_WAIT)
}

/// One non-blocking attempt at `container`'s lock, exclusive. `dir` is
/// `None` when no lock directory resolves.
#[must_use]
pub fn try_exclusive(dir: Option<&Path>, container: &str) -> Exclusive {
    let Some(dir) = dir else {
        return Exclusive::Unknown("no session lock directory".into());
    };
    let file = match open(dir, container) {
        Ok(file) => file,
        Err(error) => return Exclusive::Unknown(format!("{}: {error}", dir.display())),
    };
    match flock(&file, libc::LOCK_EX) {
        Ok(()) => Exclusive::Acquired(DispatchLock(file)),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Exclusive::Busy,
        Err(error) => Exclusive::Unknown(format!("flock: {error}")),
    }
}

/// [`try_exclusive`], retried every 100 ms while [`Exclusive::Busy`] for up
/// to `wait` (#10661: the reconciler undoing its own start for a hold that
/// a concurrent operator `stop` wrote, which holds this lock until it
/// returns).
#[must_use]
pub fn exclusive_within(dir: Option<&Path>, container: &str, wait: Duration) -> Exclusive {
    let deadline = Instant::now() + wait;
    loop {
        match try_exclusive(dir, container) {
            Exclusive::Busy if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            other => return other,
        }
    }
}

/// The lock an operator `accounts session stop` (without `--force`) takes
/// for the account's `container`. `Ok(None)` when the lock is
/// unusable: the operator's stop then proceeds on the `docker top` check
/// alone, as it did before this lock existed (the stop is deliberate; only
/// the unattended reconciler refuses on an unknown lock).
///
/// # Errors
/// When a dispatch holds the lock: the stop is refused as busy.
pub fn for_operator_stop(container: &str) -> anyhow::Result<Option<DispatchLock>> {
    match try_exclusive(lock_dir().as_deref(), container) {
        Exclusive::Acquired(lock) => Ok(Some(lock)),
        Exclusive::Unknown(why) => {
            eprintln!(
                "note: the dispatch lock for {container} is unusable ({why}); checking for an \
                 in-flight exec with `docker top` only, which cannot see a dispatch that is just \
                 starting"
            );
            Ok(None)
        }
        Exclusive::Busy => anyhow::bail!(
            "{container} has a dispatch starting or running; refusing to stop without \
             --force (the #5119 restart-safety contract). Retry once it finishes, or pass \
             --force to override."
        ),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const C: &str = "loom-codex-session-a";

    #[test]
    fn dispatches_share_and_a_teardown_waits_for_all_of_them() {
        let dir = tempfile::tempdir().unwrap();
        let first = shared(dir.path(), C, Duration::ZERO).unwrap();
        let second = shared(dir.path(), C, Duration::ZERO).unwrap();
        assert!(matches!(try_exclusive(Some(dir.path()), C), Exclusive::Busy));
        drop(first);
        assert!(matches!(try_exclusive(Some(dir.path()), C), Exclusive::Busy));
        drop(second);
        assert!(matches!(try_exclusive(Some(dir.path()), C), Exclusive::Acquired(_)));
    }

    #[test]
    fn a_teardown_keeps_dispatches_out_until_it_is_done() {
        let dir = tempfile::tempdir().unwrap();
        let Exclusive::Acquired(teardown) = try_exclusive(Some(dir.path()), C) else {
            panic!("not acquired")
        };
        assert!(shared(dir.path(), C, Duration::from_millis(150)).is_none());
        // Another container is unaffected.
        assert!(shared(dir.path(), "loom-codex-session-b", Duration::ZERO).is_some());
        drop(teardown);
        assert!(shared(dir.path(), C, Duration::ZERO).is_some());
    }

    #[test]
    fn exclusive_within_waits_for_a_holder_that_lets_go_and_gives_up_on_one_that_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let held = shared(dir.path(), C, Duration::ZERO).unwrap();
        let path = dir.path().to_path_buf();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(held);
        });
        let got = exclusive_within(Some(&path), C, Duration::from_secs(10));
        assert!(matches!(got, Exclusive::Acquired(_)), "{got:?}");
        release.join().unwrap();
        drop(got);
        let _stuck = shared(dir.path(), C, Duration::ZERO).unwrap();
        let started = Instant::now();
        let got = exclusive_within(Some(dir.path()), C, Duration::from_millis(300));
        assert!(matches!(got, Exclusive::Busy), "{got:?}");
        assert!(started.elapsed() >= Duration::from_millis(300));
    }

    #[test]
    fn an_unusable_lock_lets_dispatch_proceed_and_makes_a_teardown_defer() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, "x").unwrap();
        assert!(shared(&file, C, Duration::ZERO).is_none());
        assert!(matches!(try_exclusive(Some(&file), C), Exclusive::Unknown(_)));
        assert!(matches!(try_exclusive(None, C), Exclusive::Unknown(_)));
    }
}
