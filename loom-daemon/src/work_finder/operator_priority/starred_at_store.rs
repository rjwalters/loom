//! The on-disk layer under [`super::StarredAtCache`]: known starred-at values
//! that survive a daemon restart or roll.
//!
//! Without it every restart re-reads the REST timeline of every starred issue
//! in every workspace at once, which shows up as a burst of
//! `work_finder.starred_at` calls on each roll. The store is strictly a
//! *restart* cache: it is consulted only when the in-process cache has no
//! entry for a starred issue, and it never outlives what the in-process cache
//! would have kept.
//!
//! - **One file per in-process key.** The file is
//!   `starred-<sha16(repo_key)>.json` in the daemon's private listing-cache
//!   dir ([`crate::forge_etag_store::daemon_store_dir`]), keyed by the same
//!   `cwd|repo` string as the in-process map ([`super::repo_key`]). Two
//!   workspaces of one repo therefore have separate files, and each file has a
//!   single writer: an eviction in one workspace can never be resurrected by
//!   another workspace's write.
//! - **Every in-process drop is mirrored.** An issue evicted from the starred
//!   set loses its disk entry, and the whole-repo drop on a nothing-starred
//!   tick deletes the file. So an unstar followed by a re-star reads the new
//!   time, exactly as it does without the store.
//! - **Reuse is narrow.** A disk entry is used only when its level-label set
//!   equals the issue's current one, its value is known (unknowns are never
//!   written, so [`super::STARRED_AT_RETRY`] still governs them), and it was
//!   seen within [`RESTART_GAP_SECS`], and the issue's listed `updated_at` is
//!   no later than the one the value was confirmed under. Label events advance
//!   `updated_at`, so an unstar and re-star made while the daemon was down
//!   (no tick saw the gap) is read, never masked; an untouched issue — most of
//!   a restart's burst — still hits. A missing or unparseable `updated_at`
//!   reads. A loom-ui intent's `requested_at` is consulted before the disk and
//!   always wins over it.
//! - **Kill switch:** `LOOM_STARRED_AT_PERSIST=0` neither reads nor writes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// The env var that turns the store off (`0`, `false`, `off`, `no`).
pub const PERSIST_ENV: &str = "LOOM_STARRED_AT_PERSIST";

/// How long after it was last seen a persisted value may be reused. Bounds
/// restart staleness to the same class as a star flipped between two ticks.
pub const RESTART_GAP_SECS: u64 = 1800;

/// `last_seen` is rewritten at most this often per file, unless an entry
/// itself changed.
pub const LAST_SEEN_WRITE_SECS: u64 = 300;

/// A store file nobody has written for this long is deleted.
pub const GC_AFTER_SECS: u64 = 7 * 24 * 3600;

/// The filename prefix inside the shared listing-cache dir.
const FILE_PREFIX: &str = "starred-";

/// One persisted starred-at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedAt {
    /// The known starred-at (RFC 3339). Unknown values are never persisted.
    pub at: String,
    /// The issue's full level-label set when the value was read (#10307).
    pub levels: BTreeSet<String>,
    /// When the in-process cache last held this entry (unix seconds).
    pub last_seen: u64,
    /// The issue's listed `updated_at` (unix seconds, the forge's clock) when
    /// the value was read or confirmed. `None` (no timestamp on the listing,
    /// or a file written before this field) never matches, so it is re-read.
    #[serde(default)]
    pub updated: Option<i64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    /// The full in-process key, so a hash collision is never served.
    repo_key: String,
    entries: BTreeMap<u32, PersistedAt>,
}

/// Whether the env value leaves persistence on (unset = on).
#[must_use]
pub fn persist_enabled(env: Option<&str>) -> bool {
    !matches!(
        env.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("0" | "false" | "off" | "no")
    )
}

/// The file for one in-process cache key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StarredAtStore {
    path: PathBuf,
    repo_key: String,
}

impl StarredAtStore {
    /// The production store for `repo_key`: the daemon store dir, unless the
    /// kill switch is set or there is no store dir (tests by default).
    #[must_use]
    pub fn for_repo(repo_key: &str) -> Option<Self> {
        Self::for_repo_with(
            repo_key,
            crate::forge_etag_store::daemon_store_dir(),
            std::env::var(PERSIST_ENV).ok().as_deref(),
        )
    }

    /// [`Self::for_repo`] with its inputs explicit.
    #[must_use]
    pub fn for_repo_with(repo_key: &str, dir: Option<PathBuf>, env: Option<&str>) -> Option<Self> {
        if !persist_enabled(env) {
            return None;
        }
        dir.map(|d| Self::in_dir(&d, repo_key))
    }

    /// The store for `repo_key` inside `dir`.
    #[must_use]
    pub fn in_dir(dir: &Path, repo_key: &str) -> Self {
        Self {
            path: dir
                .join(format!("{FILE_PREFIX}{}.json", crate::short_hash::short_sha16(repo_key))),
            repo_key: repo_key.to_string(),
        }
    }

    /// The file this store reads and writes.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The persisted entries; empty when the file is missing, unreadable,
    /// in a directory that is not private, or written for another key.
    #[must_use]
    pub fn load(&self) -> BTreeMap<u32, PersistedAt> {
        let Some(dir) = self.path.parent() else {
            return BTreeMap::new();
        };
        if !crate::forge_etag_store::private_dir(dir, false) {
            return BTreeMap::new();
        }
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|raw| serde_json::from_str::<StoreFile>(&raw).ok())
            .filter(|f| f.repo_key == self.repo_key)
            .map(|f| f.entries)
            .unwrap_or_default()
    }

    /// Replace the file with `entries` (atomically); no entries deletes it.
    pub fn save(&self, entries: &BTreeMap<u32, PersistedAt>, now_unix: u64) {
        if entries.is_empty() {
            self.clear();
            return;
        }
        let file = StoreFile {
            repo_key: self.repo_key.clone(),
            entries: entries.clone(),
        };
        let Ok(bytes) = serde_json::to_vec(&file) else {
            return;
        };
        crate::forge_etag_store::write_private_atomic(&self.path, &bytes);
        if let Some(dir) = self.path.parent() {
            maybe_gc(dir, now_unix);
        }
    }

    /// Delete the file (the whole-repo drop).
    pub fn clear(&self) {
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::debug!(
                "work_finder: could not remove starred-at store {}: {e}",
                self.path.display()
            ),
        }
    }
}

/// Run [`gc_dir`] at most once an hour per process.
fn maybe_gc(dir: &Path, now_unix: u64) {
    static LAST_GC: AtomicU64 = AtomicU64::new(0);
    let last = LAST_GC.load(Ordering::Relaxed);
    if now_unix.saturating_sub(last) < 3600 {
        return;
    }
    if LAST_GC
        .compare_exchange(last, now_unix, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        gc_dir(dir, now_unix);
    }
}

/// Delete every store file in `dir` not modified for [`GC_AFTER_SECS`]: the
/// workspace was removed, or nothing there has been starred for a week.
/// Returns how many files were removed.
pub fn gc_dir(dir: &Path, now_unix: u64) -> usize {
    let mut removed = 0;
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !(name.starts_with(FILE_PREFIX) && name.ends_with(".json")) {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        if modified.is_some_and(|m| now_unix.saturating_sub(m) > GC_AFTER_SECS)
            && std::fs::remove_file(entry.path()).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

/// The current wall clock in unix seconds.
#[must_use]
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The in-memory copy of one store file, held by the in-process cache so a
/// tick reads the file at most once per process (this process is its only
/// writer).
#[derive(Debug, Default)]
pub(super) struct DiskMirror {
    pub(super) entries: BTreeMap<u32, PersistedAt>,
    /// When `last_seen` was last written (unix seconds).
    pub(super) last_touch: Option<u64>,
}

impl DiskMirror {
    /// A persisted value usable for a starred issue whose level-label set is
    /// `levels` and whose listed `updated_at` is `updated` (unix seconds), at
    /// `now_unix`. Reused only when nothing on the issue changed since the
    /// value was confirmed: a later `updated_at`, or none at all, reads.
    pub(super) fn usable(
        &self,
        issue: u32,
        levels: &BTreeSet<String>,
        updated: Option<i64>,
        now_unix: u64,
    ) -> Option<&str> {
        self.entries
            .get(&issue)
            .filter(|e| {
                &e.levels == levels
                    && now_unix.saturating_sub(e.last_seen) <= RESTART_GAP_SECS
                    && matches!((updated, e.updated), (Some(c), Some(m)) if c <= m)
            })
            .map(|e| e.at.as_str())
    }
}

#[cfg(test)]
mod tests;
