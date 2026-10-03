//! Per-key cross-process single-flight (#9783 step 5): concurrent same-key
//! requests coalesce on an exclusive lock file; a crashed writer's lock is
//! reclaimed after the pid proves dead or the lease ages out.

use anyhow::{Context, Result};
use std::path::PathBuf;

use super::store::ArtifactStore;

/// How long a lock may sit before it is considered abandoned even if its pid
/// cannot be probed (e.g. pid recycling across containers).
const LEASE_MAX_SECS: u64 = 600;

pub struct SingleFlight {
    path: PathBuf,
}

impl SingleFlight {
    /// Acquire the per-key lock. Blocks by retrying briefly, then fails with
    /// a clear "another fetch holds this key" error — callers re-run later;
    /// the in-progress artifact appears once the holder finishes.
    pub fn acquire(store: &ArtifactStore, key: &str) -> Result<Self> {
        let dir = store.root().join(&key[..2]);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating shard dir {}", dir.display()))?;
        let path = dir.join(format!("{key}.lock"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    use std::io::Write;
                    writeln!(f, "{} {}", std::process::id(), chrono::Utc::now().to_rfc3339())?;
                    return Ok(Self { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if Self::lock_is_stale(&path) {
                        // Crashed/abandoned holder: reclaim.
                        std::fs::remove_file(&path).ok();
                        continue;
                    }
                    if std::time::Instant::now() >= deadline {
                        anyhow::bail!(
                            "another fetch holds the single-flight lock for key {} (waited 30s)",
                            key
                        );
                    }
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("creating lock {}", path.display()));
                }
            }
        }
    }

    /// A lock is stale when its recorded pid is provably dead OR the lease
    /// age exceeds [`LEASE_MAX_SECS`]. An unreadable lock counts as stale —
    /// fail-open here is safe because the artifact write itself is atomic.
    fn lock_is_stale(path: &PathBuf) -> bool {
        let Ok(body) = std::fs::read_to_string(path) else {
            return true;
        };
        let mut parts = body.split_whitespace();
        let pid: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let stamped: String = parts.next().unwrap_or_default().to_string();
        if pid == 0 {
            return true;
        }
        #[cfg(unix)]
        {
            let alive = unsafe { libc::kill(pid as i32, 0) } == 0;
            if !alive {
                return true;
            }
        }
        // Pid alive: still stale if the lease aged out (a long-lived pid
        // recycled onto another process must not pin the lock forever).
        match chrono::DateTime::parse_from_rfc3339(&stamped) {
            Ok(t) => {
                let age = chrono::Utc::now().signed_duration_since(t);
                age.num_seconds() > LEASE_MAX_SECS as i64
            }
            Err(_) => true,
        }
    }
}

impl Drop for SingleFlight {
    fn drop(&mut self) {
        std::fs::remove_file(&self.path).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_cache::store::ArtifactStore;

    const K: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    #[test]
    fn exclusive_then_released() {
        let dir = tempfile::tempdir().unwrap();
        let store = ArtifactStore::at(dir.path().join("s"));
        {
            let _g1 = SingleFlight::acquire(&store, K).unwrap();
            // Second acquisition while held must fail (not deadlock).
            assert!(SingleFlight::acquire(&store, K).is_err());
        }
        // Released on drop.
        assert!(SingleFlight::acquire(&store, K).is_ok());
    }

    #[test]
    fn dead_pid_lock_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let store = ArtifactStore::at(dir.path().join("s"));
        let lock = store.root().join(&K[..2]).join(format!("{K}.lock"));
        std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
        // A pid that cannot exist (high, unspawned) reads as dead.
        std::fs::write(&lock, "4194304 2026-01-01T00:00:00Z").unwrap();
        assert!(SingleFlight::acquire(&store, K).is_ok(), "dead holder must be reclaimed");
    }
}
