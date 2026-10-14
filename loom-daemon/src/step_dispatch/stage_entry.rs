//! Stage-entry time (#11894 §4.3, #11897): when an item entered the stage
//! its label names, for the item draw's `age` term.
//!
//! [`StageEntryProvider::entry`] answers `(repo, number, label)` with a time
//! and the [`Source`] that gave it, trying three sources in order:
//!
//! 1. **`mirror`**: the row's `labels_at`, which only a GitHub state mirror
//!    listing carries ([`crate::forge_listing::RestIssue::labels_at`]). Free.
//! 2. **`events`**: the label's **latest** `labeled` event in
//!    `GET repos/{repo}/issues/{n}/events`, read from the end
//!    ([`crate::observability::ops::stage_events`], #11924: page 1 for the
//!    `Link` header, then back from the last page; GitHub lists events
//!    oldest first, so page 1 alone would give an earlier application). It
//!    shares the stage-dwell sampler's page walk, not its in-process cache:
//!    that one is the captain's, budgeted for a 5-minute sample and skipped
//!    on dispatchers. Its reads are counted under their own caller,
//!    [`EVENTS_CALLER`]. This provider keeps its own **persisted** cache,
//!    keyed `(repo, number, label)`, and its own per-tick read budget
//!    ([`DEFAULT_EVENTS_BUDGET`]), where **each page is one read**. One read
//!    caches the times of every label the item carries. A read that finds no
//!    event for the label (it lies further back than the pages read) is
//!    cached as "not found" for [`NOT_FOUND_TTL_SECS`]; a failed or shed read
//!    is not cached at all.
//! 3. **`created`**: the item's `created_at`, when nothing better is known.
//!    For the backlog this is the common answer until the mirror's
//!    `labels_at` fills in (#11894 §4.3; the creation-age ruling).
//!
//! A cached time stays right only while the label stays on: a label removed
//! and re-applied later has a newer entry time. Two guards keep the cache
//! tied to the label's **current** application (#11924):
//!
//! - [`StageEntryProvider::retain_present`] drops every cached
//!   `(number, label)` a complete listing no longer shows;
//! - each entry remembers the item's `updated_at` when it was read. A
//!   removal and re-application between two listings moves `updated_at`
//!   (as any other change to the item does), so an entry whose item has
//!   moved on is re-read, at most once per [`REVALIDATE_SECS`], whenever the
//!   budget allows. First-time reads come first: a re-read never spends the
//!   half of the budget kept for them, nor any budget in a tick after one
//!   where a first-time read went unserved. Until a re-read happens the old
//!   time is served; it is still closer than `created`. A re-read that does
//!   not see the label keeps it too.
//!
//! What the events cannot tell: adding a label an item already carries
//! records no `labeled` event (and leaves the label's time unchanged on
//! GitHub), so a "re-application" that never took the label off is not a
//! new entry and the original time is the right answer.
//!
//! Each answer is counted in `loom.stage_entry.lookups{repo, kind}` (`kind`
//! is the source), so the coverage by source is visible before the draw
//! depends on it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::forge_listing::RestIssue;
use crate::observability::ops::stage_events::{self, LabelTimes};

/// Events reads one tick may spend, across every repo.
pub const DEFAULT_EVENTS_BUDGET: usize = 8;

/// How long an events read that found no `labeled` event for a label is
/// believed before the label is read again.
pub const NOT_FOUND_TTL_SECS: i64 = 6 * 3600;

/// How long a found time is served without a re-read after its item's
/// `updated_at` moved on: bounds the reads an often-updated item (a PR in
/// review) can cost, at the price of serving a superseded time that long.
pub const REVALIDATE_SECS: i64 = 10 * 60;

/// The `caller` this provider's events reads are counted under in
/// `loom.forge.calls`, apart from stage-dwell's.
pub const EVENTS_CALLER: &str = "stage_entry.events";

/// Most cached entries kept; past it the oldest reads are dropped first.
pub const MAX_ENTRIES: usize = 20_000;

/// The on-disk schema; a file of another schema reads as empty.
const SCHEMA: u32 = 1;

/// Which source answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Source {
    /// The mirror's `labels_at`.
    Mirror,
    /// The issue's `labeled` events (cached).
    Events,
    /// The item's creation time.
    Created,
}

impl Source {
    /// The `source` label, and the `age_source` the draw records.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mirror => "mirror",
            Self::Events => "events",
            Self::Created => "created",
        }
    }
}

/// When an item entered its stage, and how that is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageEntry {
    pub at: DateTime<Utc>,
    pub source: Source,
}

/// The per-item events read, injected so the provider is testable offline.
pub trait EventsSource {
    /// The latest `labeled` time of `label` on item `number`, and of every
    /// other label the pages read vouch for, in at most `max_pages` page
    /// reads; `times` is `None` when the read failed or was shed.
    fn label_times(
        &mut self,
        root: &Path,
        slug: &str,
        number: u32,
        label: &str,
        max_pages: usize,
    ) -> LabelTimes;
}

/// The live read: the shared events page walk (`gh api` in the repo's own
/// checkout, deferrable), counted under [`EVENTS_CALLER`].
#[derive(Debug, Clone, Copy, Default)]
pub struct GhEventsReader;

impl EventsSource for GhEventsReader {
    fn label_times(
        &mut self,
        root: &Path,
        slug: &str,
        number: u32,
        label: &str,
        max_pages: usize,
    ) -> LabelTimes {
        stage_events::read(EVENTS_CALLER, root, slug, number, label, max_pages)
    }
}

type Key = (String, u32, String);

/// One cached events answer.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cached {
    /// The label's latest `labeled` time; `None`: read, not found.
    at: Option<DateTime<Utc>>,
    /// When the events were read.
    read_at: DateTime<Utc>,
    /// The item's `updated_at` as listed when the events were read.
    updated_at: Option<String>,
}

/// The cache file.
#[derive(Debug, Default, Serialize, Deserialize)]
struct CacheFile {
    schema: u32,
    entries: Vec<CacheRow>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CacheRow {
    repo: String,
    number: u32,
    label: String,
    #[serde(default)]
    at: Option<DateTime<Utc>>,
    read_at: DateTime<Utc>,
    /// Absent in a file written before #11924: such an entry is re-read
    /// once its item's `updated_at` is known.
    #[serde(default)]
    updated_at: Option<String>,
}

/// `$HOME/.loom/step-dispatch/stage-entry.json`; `None` under `cfg(test)`
/// (tests share one process `HOME`).
#[cfg(not(test))]
#[must_use]
pub fn default_cache_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join(".loom")
            .join("step-dispatch")
            .join("stage-entry.json")
    })
}

#[cfg(test)]
#[must_use]
pub fn default_cache_path() -> Option<PathBuf> {
    None
}

/// The stage-entry provider: its events cache and this tick's read budget.
#[derive(Debug)]
pub struct StageEntryProvider {
    cache: HashMap<Key, Cached>,
    path: Option<PathBuf>,
    budget: usize,
    left: usize,
    /// A first-time read (an item with no time yet) found the budget spent
    /// this tick / last tick: re-reads then wait (#11924).
    starved: bool,
    starved_last: bool,
    dirty: bool,
}

impl StageEntryProvider {
    /// A provider whose cache lives at `path` (`None`: in memory only),
    /// loaded now, spending at most `budget` events reads per tick. A
    /// missing, unreadable, malformed or other-schema file reads as empty.
    #[must_use]
    pub fn load(path: Option<PathBuf>, budget: usize) -> Self {
        let file = path
            .as_deref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice::<CacheFile>(&b).ok())
            .filter(|f| f.schema == SCHEMA)
            .unwrap_or_default();
        let cache = file
            .entries
            .into_iter()
            .map(|r| {
                let cached = Cached {
                    at: r.at,
                    read_at: r.read_at,
                    updated_at: r.updated_at,
                };
                ((r.repo, r.number, r.label), cached)
            })
            .collect();
        Self {
            cache,
            path,
            budget,
            left: budget,
            starved: false,
            starved_last: false,
            dirty: false,
        }
    }

    /// Start a tick: the events budget is whole again.
    pub fn begin_tick(&mut self) {
        self.left = self.budget;
        self.starved_last = std::mem::take(&mut self.starved);
    }

    /// Pages a re-read of an already-known time may spend now. First-time
    /// reads come first: re-reads never touch the half of the budget kept
    /// for them, and wait out any tick (this one or the last) in which a
    /// first-time read went unserved, so a busy item cannot starve one that
    /// has no time yet.
    fn reread_allowance(&self) -> usize {
        if self.starved || self.starved_last {
            return 0;
        }
        self.left.saturating_sub(self.budget / 2)
    }

    /// Events reads left this tick.
    #[must_use]
    pub fn budget_left(&self) -> usize {
        self.left
    }

    /// When `item` (of repo `slug`, checked out at `root`) entered the stage
    /// `label` names, and how that is known; `None` when no source knows,
    /// not even a creation time. Counted in `loom.stage_entry.lookups`.
    pub fn entry(
        &mut self,
        events: &mut impl EventsSource,
        root: &Path,
        slug: &str,
        item: &RestIssue,
        label: &str,
        now: DateTime<Utc>,
    ) -> Option<StageEntry> {
        let found = self.lookup(events, root, slug, item, label, now);
        crate::observability::ops::stage_entry::record_lookup(slug, found.map(|e| e.source));
        found
    }

    fn lookup(
        &mut self,
        events: &mut impl EventsSource,
        root: &Path,
        slug: &str,
        item: &RestIssue,
        label: &str,
        now: DateTime<Utc>,
    ) -> Option<StageEntry> {
        if let Some(at) = mirror_time(item, label) {
            return Some(StageEntry {
                at,
                source: Source::Mirror,
            });
        }
        if let Some(at) = self.events_time(events, root, slug, item, label, now) {
            return Some(StageEntry {
                at,
                source: Source::Events,
            });
        }
        item.created_at
            .as_deref()
            .and_then(parse_time)
            .map(|at| StageEntry {
                at,
                source: Source::Created,
            })
    }

    /// The cached events time, else (budget permitting) one events read
    /// that caches every carried label's time. A found time whose item has
    /// been updated since it was read is re-read when the budget allows (at
    /// most once per [`REVALIDATE_SECS`]) and first-time reads leave room
    /// ([`Self::reread_allowance`]), else served as it stands. A re-read
    /// that does not see the label keeps the known time.
    fn events_time(
        &mut self,
        events: &mut impl EventsSource,
        root: &Path,
        slug: &str,
        item: &RestIssue,
        label: &str,
        now: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        let wanted = key(slug, item.number, label);
        let mut stale_time = None;
        if let Some(cached) = self.cache.get(&wanted) {
            let age = now - cached.read_at;
            let fresh = match cached.at {
                Some(_) => {
                    item.updated_at.is_none()
                        || cached.updated_at == item.updated_at
                        || age < Duration::seconds(REVALIDATE_SECS)
                }
                None => age < Duration::seconds(NOT_FOUND_TTL_SECS),
            };
            if fresh {
                return cached.at;
            }
            stale_time = cached.at;
        }
        let allowance = if stale_time.is_some() {
            self.reread_allowance()
        } else {
            self.left
        };
        if allowance == 0 {
            self.starved |= stale_time.is_none();
            return stale_time;
        }
        let read = events.label_times(root, slug, item.number, label, allowance);
        self.left = self.left.saturating_sub(read.pages.max(1));
        let Some(times) = read.times else {
            return stale_time;
        };
        let carried: HashSet<String> = item.labels.iter().map(|l| l.to_ascii_lowercase()).collect();
        for (name, at) in &times {
            if carried.contains(&name.to_ascii_lowercase()) {
                let cached = Cached {
                    at: Some(*at),
                    read_at: now,
                    updated_at: item.updated_at.clone(),
                };
                self.cache.insert(key(slug, item.number, name), cached);
            }
        }
        // A re-read that did not reach the label (it lies further back than
        // the pages read) keeps the time already known rather than
        // forgetting it as "not found".
        let found = times
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(label))
            .map(|(_, at)| *at);
        // A contiguous walk ending at the last page that misses the label
        // leaves the cached time as its latest application.
        let found = found.or(stale_time);
        self.cache.insert(
            wanted,
            Cached {
                at: found,
                read_at: now,
                updated_at: item.updated_at.clone(),
            },
        );
        self.dirty = true;
        found
    }

    /// Drop every cached `(number, label)` of `slug` that `rows`, a
    /// **complete** listing of its open items, does not show: the item
    /// closed, or the label came off (a later re-application is a new
    /// entry time). Never call it with a partial listing.
    pub fn retain_present(&mut self, slug: &str, rows: &[RestIssue]) {
        let slug = slug.to_ascii_lowercase();
        let present: HashSet<(u32, String)> = rows
            .iter()
            .flat_map(|r| r.labels.iter().map(|l| (r.number, l.to_ascii_lowercase())))
            .collect();
        let before = self.cache.len();
        self.cache.retain(|(repo, number, label), _| {
            *repo != slug || present.contains(&(*number, label.clone()))
        });
        self.dirty |= self.cache.len() != before;
    }

    /// Persist the cache when it changed, atomically, keeping the
    /// [`MAX_ENTRIES`] most recent reads. A provider with no path keeps its
    /// cache in memory only.
    ///
    /// # Errors
    ///
    /// Any I/O failure; the cache stays dirty, so the next flush retries.
    pub fn flush(&mut self) -> std::io::Result<()> {
        let Some(path) = self.path.clone() else {
            self.dirty = false;
            return Ok(());
        };
        if !self.dirty {
            return Ok(());
        }
        self.evict();
        let mut entries: Vec<CacheRow> = self
            .cache
            .iter()
            .map(|((repo, number, label), c)| CacheRow {
                repo: repo.clone(),
                number: *number,
                label: label.clone(),
                at: c.at,
                read_at: c.read_at,
                updated_at: c.updated_at.clone(),
            })
            .collect();
        entries.sort_by(|a, b| (&a.repo, a.number, &a.label).cmp(&(&b.repo, b.number, &b.label)));
        let file = CacheFile {
            schema: SCHEMA,
            entries,
        };
        let body = serde_json::to_vec(&file).map_err(std::io::Error::other)?;
        crate::roll_pause::write_atomic(&path, &body)?;
        self.dirty = false;
        Ok(())
    }

    /// Keep the [`MAX_ENTRIES`] most recently read entries.
    fn evict(&mut self) {
        if self.cache.len() <= MAX_ENTRIES {
            return;
        }
        let mut reads: Vec<DateTime<Utc>> = self.cache.values().map(|c| c.read_at).collect();
        reads.sort_unstable_by(|a, b| b.cmp(a));
        let cutoff = reads[MAX_ENTRIES - 1];
        self.cache.retain(|_, c| c.read_at >= cutoff);
        // Ties at the cutoff can leave a few over; drop them in key order.
        if self.cache.len() > MAX_ENTRIES {
            let mut at_cutoff: Vec<Key> = self
                .cache
                .iter()
                .filter(|(_, c)| c.read_at == cutoff)
                .map(|(k, _)| k.clone())
                .collect();
            at_cutoff.sort();
            let excess = self.cache.len() - MAX_ENTRIES;
            for k in at_cutoff.into_iter().take(excess) {
                self.cache.remove(&k);
            }
        }
    }

    /// Cached entries (for tests and status).
    #[must_use]
    pub fn cached_len(&self) -> usize {
        self.cache.len()
    }
}

/// The cache key: repo and label folded to lowercase, as GitHub compares them.
fn key(slug: &str, number: u32, label: &str) -> Key {
    (slug.to_ascii_lowercase(), number, label.to_ascii_lowercase())
}

/// The mirror's applied time for `label` on `item`, when it sent one and the
/// item still carries the label.
fn mirror_time(item: &RestIssue, label: &str) -> Option<DateTime<Utc>> {
    if !item.labels.iter().any(|l| l.eq_ignore_ascii_case(label)) {
        return None;
    }
    item.labels_at
        .as_ref()?
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(label))
        .and_then(|(_, at)| parse_time(at))
}

fn parse_time(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw.trim())
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "stage_entry_tests.rs"]
mod tests;
