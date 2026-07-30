//! `mkdir`-based file lock, ported from `loom_tools.tokens._locking`.
//!
//! Several token state files (`.bad_tokens`, `.allowlist`, `.failure_counts`,
//! the rotation cursor) are shared across concurrent bash and Python (and now
//! Rust) writers. Coordination is via a sibling `*.lock` directory created
//! with `mkdir` (POSIX-atomic). `flock` is intentionally not used — it is
//! unavailable on stock macOS.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

const LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(100);
const STALE_LOCK_THRESHOLD: Duration = Duration::from_secs(30);

/// Whether the lock directory at `lock_path` is older than `stale_threshold`
/// (its mtime is its creation time — nothing refreshes it). An unreadable
/// mtime is conservatively **not** stale: never reap a lock we cannot age.
fn is_stale(lock_path: &Path, stale_threshold: Duration) -> bool {
    std::fs::metadata(lock_path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age > stale_threshold)
}

/// RAII guard for an `mkdir`-based lock. Acquires on construction (blocking,
/// with polling + stale-lock cleanup) and releases (`rmdir`) on drop.
pub struct MkdirLock {
    lock_path: PathBuf,
    acquired: bool,
}

impl MkdirLock {
    /// **Non-blocking** single attempt at `lock_path`, reaping a lock whose
    /// mtime is older than `stale_threshold` before retrying once.
    ///
    /// Distinguishes the three outcomes a caller with its own wait policy needs
    /// (the machine-wide build slot, [`crate::build_slot`], polls several slot
    /// paths per round and must not block on any single one):
    ///
    /// - `Ok(Some(lock))` — acquired; released on drop like [`Self::acquire`].
    /// - `Ok(None)` — currently held by someone else (and not stale).
    /// - `Err(_)` — the lock path is **unusable** (missing parent, no write
    ///   permission, …). A caller that must never block should treat this as
    ///   "no locking available" and degrade open rather than spin.
    ///
    /// # Errors
    /// Returns an error when `mkdir` fails for any reason other than
    /// `AlreadyExists`.
    pub fn try_acquire(
        lock_path: &Path,
        stale_threshold: Duration,
    ) -> Result<Option<Self>, String> {
        match std::fs::create_dir(lock_path) {
            Ok(()) => Ok(Some(Self {
                lock_path: lock_path.to_path_buf(),
                acquired: true,
            })),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if !is_stale(lock_path, stale_threshold) {
                    return Ok(None);
                }
                // Reap the stale lock and make exactly one more attempt: a
                // racing peer may have reaped and re-taken it first, which is
                // an ordinary `Ok(None)`, not an error.
                let _ = std::fs::remove_dir(lock_path);
                match std::fs::create_dir(lock_path) {
                    Ok(()) => Ok(Some(Self {
                        lock_path: lock_path.to_path_buf(),
                        acquired: true,
                    })),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
                    Err(e) => Err(format!("could not create lock dir: {e}")),
                }
            }
            Err(e) => Err(format!("could not create lock dir: {e}")),
        }
    }

    /// The path this lock occupies (for telemetry).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.lock_path
    }

    /// Acquire the lock at `lock_path`, blocking up to the default timeout
    /// (5s, polling every 100ms, reaping locks stale for >30s).
    ///
    /// # Errors
    /// Returns an error if the lock could not be acquired within the
    /// timeout.
    pub fn acquire(lock_path: &Path) -> Result<Self, String> {
        Self::acquire_with(lock_path, LOCK_TIMEOUT, LOCK_POLL_INTERVAL, STALE_LOCK_THRESHOLD)
    }

    /// Same as [`Self::acquire`] with explicit timing parameters (used by
    /// tests to avoid a 5s wall-clock wait).
    ///
    /// # Errors
    /// Returns an error if the lock could not be acquired within `timeout`.
    pub fn acquire_with(
        lock_path: &Path,
        timeout: Duration,
        poll_interval: Duration,
        stale_threshold: Duration,
    ) -> Result<Self, String> {
        let deadline = Instant::now() + timeout;
        loop {
            match std::fs::create_dir(lock_path) {
                Ok(()) => {
                    return Ok(Self {
                        lock_path: lock_path.to_path_buf(),
                        acquired: true,
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // Stale-lock cleanup.
                    if lock_path.exists() {
                        if is_stale(lock_path, stale_threshold) {
                            let _ = std::fs::remove_dir(lock_path);
                        }
                    } else {
                        // Lock vanished between checks; loop and retry immediately.
                        continue;
                    }
                }
                Err(e) => return Err(format!("could not create lock dir: {e}")),
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(poll_interval);
        }
        Err(format!(
            "Could not acquire lock at {} within {:?}",
            lock_path.display(),
            timeout
        ))
    }
}

impl Drop for MkdirLock {
    fn drop(&mut self) {
        if self.acquired {
            let _ = std::fs::remove_dir(&self.lock_path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_and_release_creates_then_removes_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join(".x.lock");
        {
            let _lock = MkdirLock::acquire(&lock_path).unwrap();
            assert!(lock_path.is_dir());
        }
        assert!(!lock_path.exists());
    }

    #[test]
    fn acquire_times_out_when_held() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join(".x.lock");
        let _held = MkdirLock::acquire(&lock_path).unwrap();
        let result = MkdirLock::acquire_with(
            &lock_path,
            Duration::from_millis(150),
            Duration::from_millis(20),
            Duration::from_secs(30),
        );
        assert!(result.is_err());
    }

    #[test]
    fn try_acquire_distinguishes_free_held_and_unusable() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join(".x.lock");
        let stale = Duration::from_secs(30);

        // Free -> acquired.
        let held = MkdirLock::try_acquire(&lock_path, stale).unwrap();
        assert!(held.is_some());
        assert!(lock_path.is_dir());

        // Held (not stale) -> Ok(None), never an error and never a block.
        assert!(MkdirLock::try_acquire(&lock_path, stale).unwrap().is_none());

        // Released on drop -> acquirable again.
        drop(held);
        assert!(MkdirLock::try_acquire(&lock_path, stale).unwrap().is_some());

        // Unusable path (parent does not exist) -> Err, so a non-blocking
        // caller can degrade open instead of spinning.
        let missing = tmp.path().join("no-such-dir").join("x.lock");
        assert!(MkdirLock::try_acquire(&missing, stale).is_err());
    }

    #[test]
    fn try_acquire_reaps_a_stale_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join(".x.lock");
        std::fs::create_dir(&lock_path).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        let acquired = MkdirLock::try_acquire(&lock_path, Duration::from_millis(10)).unwrap();
        assert!(acquired.is_some(), "a lock older than the stale threshold must be reaped");
    }

    #[test]
    fn acquire_reaps_stale_lock() {
        // std has no portable set_mtime without a new crate, so simulate
        // staleness by using a tiny stale_threshold and letting the lock's
        // real mtime (creation time) age past it via a short sleep.
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join(".x.lock");
        std::fs::create_dir(&lock_path).unwrap();
        std::thread::sleep(Duration::from_millis(30));

        let result = MkdirLock::acquire_with(
            &lock_path,
            Duration::from_millis(500),
            Duration::from_millis(20),
            Duration::from_millis(10),
        );
        assert!(result.is_ok());
    }
}
