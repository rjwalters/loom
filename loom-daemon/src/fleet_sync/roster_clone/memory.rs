//! Per-repo clone failure memory (#11218): ordering and backoff.
//!
//! Without it a record that always fails (a repo the credential cannot read,
//! one that always outlives the timeout) would be tried first on every pass
//! and use up the per-pass cap, so a missing clone behind it would never be
//! tried. So a pass tries repos that have never failed first, then the ones
//! that failed least recently, and skips a repo still inside its backoff
//! window ([`backoff`]: doubling from [`BACKOFF_BASE`] up to [`BACKOFF_MAX`]).
//!
//! In process memory only: a restart forgets it, which costs one extra try
//! per failing repo.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Backoff after the first failure.
pub const BACKOFF_BASE: Duration = Duration::from_secs(5 * 60);
/// Longest backoff.
pub const BACKOFF_MAX: Duration = Duration::from_secs(2 * 60 * 60);

/// How long a repo that has failed `count` times in a row waits.
#[must_use]
pub fn backoff(count: u32) -> Duration {
    let doublings = count.saturating_sub(1).min(16);
    BACKOFF_BASE
        .saturating_mul(1u32 << doublings)
        .min(BACKOFF_MAX)
}

#[derive(Debug, Clone, Copy)]
struct Failure {
    count: u32,
    at: Instant,
}

/// Failures by clone path.
#[derive(Debug, Default)]
pub struct CloneMemory {
    failures: HashMap<PathBuf, Failure>,
}

impl CloneMemory {
    /// Sort key: never-failed repos first (`None`), then least recently
    /// failed.
    #[must_use]
    pub fn order_key(&self, path: &Path) -> (bool, Option<Instant>) {
        let at = self.failures.get(path).map(|f| f.at);
        (at.is_some(), at)
    }

    /// `Some((failures, remaining))` while `path` is inside its backoff.
    #[must_use]
    pub fn waiting(&self, path: &Path, now: Instant) -> Option<(u32, Duration)> {
        let f = self.failures.get(path)?;
        let until = f.at + backoff(f.count);
        (now < until).then(|| (f.count, until - now))
    }

    /// Record a clone that failed at `now`.
    pub fn failed(&mut self, path: &Path, now: Instant) {
        let count = self.failures.get(path).map_or(0, |f| f.count) + 1;
        self.failures
            .insert(path.to_path_buf(), Failure { count, at: now });
    }

    /// Forget `path` (it cloned, or is no longer attempted).
    pub fn forget(&mut self, path: &Path) {
        self.failures.remove(path);
    }
}

/// The daemon's process-wide memory.
pub fn global() -> &'static Mutex<CloneMemory> {
    static CELL: OnceLock<Mutex<CloneMemory>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(CloneMemory::default()))
}
