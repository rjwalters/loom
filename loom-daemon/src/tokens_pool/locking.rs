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

/// RAII guard for an `mkdir`-based lock. Acquires on construction (blocking,
/// with polling + stale-lock cleanup) and releases (`rmdir`) on drop.
pub struct MkdirLock {
    lock_path: PathBuf,
    acquired: bool,
}

impl MkdirLock {
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
                    if let Ok(meta) = std::fs::metadata(lock_path) {
                        if let Ok(modified) = meta.modified() {
                            if let Ok(age) = SystemTime::now().duration_since(modified) {
                                if age > stale_threshold {
                                    let _ = std::fs::remove_dir(lock_path);
                                }
                            }
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
