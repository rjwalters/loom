//! On-disk star-time cache shared by `forge starred` and `pr-queue` (#9975
//! review).
//!
//! `forge starred` and `pr-queue` are short-lived CLI processes, so the work
//! finder's in-process [`crate::work_finder::operator_priority::StarredAtCache`]
//! cannot help them: without this cache every call re-read every starred
//! item's timeline (measured: 88 `gh` calls / 62 s per `pr-queue --role
//! judge` on a repo with 85 stars, against 1 call before).
//!
//! # Validator: the item's `updated_at`, not a TTL
//!
//! An entry records the `updated_at` the item's listing row carried when its
//! timeline was read. Applying or removing a label, and posting the loom-ui
//! star-intent comment, both bump `updated_at`, so an unchanged `updated_at`
//! means the timeline cannot carry a newer star. The validator is data the
//! caller already fetched (a `304` listing included), so a hit costs no forge
//! call, and there is no staleness window: an un-star and re-star between two
//! calls still changes `updated_at`. Unrelated activity (a comment, a push)
//! also changes it; that costs one re-read of that item only.
//!
//! An unknown star time (the read failed or found no event) is retried after
//! [`STARRED_AT_RETRY`], exactly like the daemon's cache, so a forge outage or
//! the rate-limit breaker never turns into a read per call. A row without an
//! `updated_at` is never served from the cache.
//!
//! Stored in the shared listing-cache directory
//! ([`crate::forge_etag_store::disk_cache_dir`]): private (`0700`), keyed by
//! repo, host and credential scope like every other entry there, written
//! atomically. Concurrent writers race last-writer-wins, which can only cost
//! a re-read, never a wrong answer.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::forge_etag_store as store;
use crate::work_finder::operator_priority::{StarredAtSource, STARRED_AT_RETRY};

/// Schema tag stored in the [`store::DiskEntry`] `etag` slot, so a future
/// shape change reads an old file as empty rather than misparsing it.
const SCHEMA: &str = "starred-at-v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    /// The listing row's `updated_at` when the timeline was read.
    updated_at: String,
    /// The resolved star time, `None` when unknown.
    at: Option<String>,
    /// Unix seconds of the read (for the unknown-value retry).
    fetched: i64,
}

/// Star times keyed by item number, validated by `updated_at`.
#[derive(Debug, Default)]
pub struct StarTimeCache {
    path: Option<PathBuf>,
    entries: BTreeMap<u32, Entry>,
    dirty: bool,
    /// Timeline reads this process made through the cache (tests, logs).
    reads: usize,
}

impl StarTimeCache {
    /// An empty cache that is never persisted (tests).
    #[must_use]
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// The persisted cache for `target` reached from `root`. A missing,
    /// unreadable or foreign-schema file is an empty cache.
    #[must_use]
    pub(crate) fn open(root: &Path, target: &store::Target) -> Self {
        let key = store::cache_key(Some(root), target, "loom:starred-at");
        let path = store::entry_path_with_prefix(&store::disk_cache_dir(), "starat-", &key);
        let entries = store::read_disk_entry(&path)
            .filter(|e| e.etag == SCHEMA)
            .and_then(|e| serde_json::from_str(&e.body).ok())
            .unwrap_or_default();
        Self {
            path: Some(path),
            entries,
            ..Self::default()
        }
    }

    /// `number`'s star time: from the cache when its entry was read at this
    /// same `updated_at` (and is known, or unknown but recent), else from
    /// `source`, recording the answer.
    pub fn star_at(
        &mut self,
        number: u32,
        updated_at: Option<&str>,
        source: &mut dyn StarredAtSource,
        now: i64,
    ) -> Option<String> {
        let retry = i64::try_from(STARRED_AT_RETRY.as_secs()).unwrap_or(i64::MAX);
        if let (Some(u), Some(e)) = (updated_at, self.entries.get(&number)) {
            if e.updated_at == u && (e.at.is_some() || now.saturating_sub(e.fetched) < retry) {
                return e.at.clone();
            }
        }
        self.reads += 1;
        let at = source.starred_at(number).unwrap_or_else(|e| {
            log::debug!(
                "forge starred: star-time read for #{number} failed ({e}); using createdAt"
            );
            None
        });
        match updated_at {
            Some(u) => {
                self.entries.insert(
                    number,
                    Entry {
                        updated_at: u.to_string(),
                        at: at.clone(),
                        fetched: now,
                    },
                );
            }
            None => {
                self.entries.remove(&number);
            }
        }
        self.dirty = true;
        at
    }

    /// Drop every entry whose item is no longer starred, so the file only
    /// ever holds the current stars.
    pub fn retain_starred(&mut self, starred: &HashSet<u32>) {
        let before = self.entries.len();
        self.entries.retain(|n, _| starred.contains(n));
        self.dirty |= self.entries.len() != before;
    }

    /// Persist when anything changed. Best-effort: a failed write only means
    /// the next call reads again.
    pub fn save(&self) {
        let (Some(path), true) = (&self.path, self.dirty) else {
            return;
        };
        if let Ok(body) = serde_json::to_string(&self.entries) {
            store::write_disk_entry(
                path,
                &store::DiskEntry {
                    etag: SCHEMA.to_string(),
                    body,
                },
            );
        }
    }

    /// Timeline reads made through this cache since it was opened.
    #[must_use]
    pub fn reads(&self) -> usize {
        self.reads
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{anyhow, Result};

    #[derive(Default)]
    struct Counting {
        calls: Vec<u32>,
        fail: bool,
    }
    impl StarredAtSource for Counting {
        fn starred_at(&mut self, issue: u32) -> Result<Option<String>> {
            self.calls.push(issue);
            if self.fail {
                return Err(anyhow!("breaker"));
            }
            Ok(Some(format!("2026-09-{issue:02}T00:00:00Z")))
        }
    }

    /// The `updated_at` validator: unchanged is a hit (no read), changed is a
    /// re-read, absent is never cached.
    #[test]
    fn an_unchanged_updated_at_is_served_without_a_read() {
        let mut c = StarTimeCache::in_memory();
        let mut src = Counting::default();
        let u1 = Some("2026-10-01T00:00:00Z");
        assert_eq!(c.star_at(7, u1, &mut src, 0).as_deref(), Some("2026-09-07T00:00:00Z"));
        assert_eq!(c.star_at(7, u1, &mut src, 99_999).as_deref(), Some("2026-09-07T00:00:00Z"));
        assert_eq!(src.calls, vec![7]);
        c.star_at(7, Some("2026-10-02T00:00:00Z"), &mut src, 0);
        assert_eq!(src.calls, vec![7, 7]);
        c.star_at(8, None, &mut src, 0);
        c.star_at(8, None, &mut src, 0);
        assert_eq!(src.calls, vec![7, 7, 8, 8]);
        assert_eq!(c.reads(), 4);
    }

    /// An unknown answer is retried only after `STARRED_AT_RETRY`, so an
    /// outage never becomes a read per call.
    #[test]
    fn an_unknown_star_time_is_retried_only_after_the_retry_window() {
        let mut c = StarTimeCache::in_memory();
        let mut src = Counting {
            fail: true,
            ..Counting::default()
        };
        let u = Some("2026-10-01T00:00:00Z");
        let retry = i64::try_from(STARRED_AT_RETRY.as_secs()).unwrap();
        assert_eq!(c.star_at(3, u, &mut src, 0), None);
        assert_eq!(c.star_at(3, u, &mut src, retry - 1), None);
        assert_eq!(src.calls.len(), 1);
        src.fail = false;
        assert!(c.star_at(3, u, &mut src, retry).is_some());
        assert_eq!(src.calls.len(), 2);
    }

    #[test]
    fn the_cache_round_trips_through_disk_and_prunes_unstarred_items() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache").join("starat-x.json");
        let mut src = Counting::default();
        let u = Some("2026-10-01T00:00:00Z");
        let mut c = StarTimeCache {
            path: Some(path.clone()),
            ..StarTimeCache::default()
        };
        c.star_at(1, u, &mut src, 0);
        c.star_at(2, u, &mut src, 0);
        c.retain_starred(&HashSet::from([2]));
        c.save();
        let entry = store::read_disk_entry(&path).unwrap();
        assert_eq!(entry.etag, SCHEMA);
        let entries: BTreeMap<u32, Entry> = serde_json::from_str(&entry.body).unwrap();
        assert_eq!(entries.keys().copied().collect::<Vec<_>>(), vec![2]);
    }
}
