//! Version-keyed reuse of the reconciliation pass's per-PR reads (#10089).
//!
//! Every tick (default 10 min, per registered workspace) the pass family
//! re-read the same per-PR facts for every candidate, although almost none
//! of them change between ticks: a claimed PR's label timeline and activity
//! comments, a verdict PR's comment scan, an open PR's changed files. Each
//! is a paginated REST walk or a GraphQL view — the largest steady spend the
//! daemon makes against the shared pool.
//!
//! Each read is now keyed on a **version the cheap listing already carries**:
//!
//! - `updatedAt` — GitHub bumps it on every new comment, label event and
//!   push, so a PR whose `updatedAt` is unchanged has no new timeline event
//!   or comment to read;
//! - `headRefOid` (+ base branch) — a PR's changed files are a function of
//!   its head commit and base.
//!
//! Only a **found** answer is stored ([`ReadCache::get_or`] never caches a
//! `None`): a failed read is indistinguishable from an absent one at most of
//! these sites, and caching a failure could hide a live claim's heartbeat.
//! An entry is also dropped after its cache's expiry regardless — a backstop
//! for anything that changes without bumping the version (an edited comment).
//!
//! The two claim caches expire after [`CLAIM_MAX_AGE`] rather than
//! [`MAX_AGE`]: `updatedAt` has 1 s granularity and can lag the timeline /
//! comments endpoints, so a label event or heartbeat landing in the same
//! second as a cached read (or a listing ahead of the timeline read) can
//! leave a stale answer under the new key. For the claim heartbeat a stale,
//! older timestamp could make a live claim look inactive, so that window is
//! held to about one tick.
//!
//! Kill switch: [`READ_CACHE_ENV`] (`0`/`false`/`off`/`no` disables).

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

/// Kill switch for every cache in this module. Defaults ON.
pub const READ_CACHE_ENV: &str = "LOOM_RECONCILE_READ_CACHE";

/// The age past which an entry is re-read even with an unchanged version.
pub const MAX_AGE: Duration = Duration::from_secs(3600);

/// The shorter expiry for the claim-label timeline / claim-activity caches,
/// whose stale answer can misjudge a live claim (see the module docs). 15 min
/// still spans one default 10-min tick, so an unchanged claim is re-read at
/// most every other pass.
pub const CLAIM_MAX_AGE: Duration = Duration::from_secs(15 * 60);

/// Entries per cache before expired ones are swept (and, failing that, the
/// cache is cleared) — bounds memory by PR count, never by tick count.
const MAX_ENTRIES: usize = 4096;

/// A claimed PR's newest `labeled <claim label>` instant.
pub(super) static CLAIM_LABELED: ReadCache<DateTime<Utc>> = ReadCache::new(CLAIM_MAX_AGE);
/// A claimed PR's newest trusted claim-activity comment since that label.
pub(super) static CLAIM_ACTIVITY: ReadCache<DateTime<Utc>> = ReadCache::new(CLAIM_MAX_AGE);
/// A verdict PR's comment scan: `(latest marker sha, already recorded)`.
/// Both halves are computed AFTER trusted-author filtering, so a change to
/// the comment-trust configuration is not seen here until the entry expires.
pub(super) static VERDICT_SCAN: ReadCache<(Option<String>, bool)> = ReadCache::new(MAX_AGE);
/// An open PR's changed paths. Keyed on the base branch NAME, not its SHA —
/// an approximation: a base that advances without the head moving keeps the
/// cached set until [`MAX_AGE`] (the PR's own diff rarely changes from that).
pub(super) static CHANGED_FILES: ReadCache<BTreeSet<String>> = ReadCache::new(MAX_AGE);

/// A closed-over compare of two commit SHAs (`tree_unchanged`): whether the
/// trees are byte-identical. A function of the two SHAs alone, so it never
/// goes stale; keyed with no version stamp (see [`key_of`]).
pub(crate) static TREE_COMPARE: ReadCache<bool> = ReadCache::new(MAX_AGE);
/// A `loom:blocked` issue's quarantine scan, keyed on the issue's
/// `updatedAt`: `(trusted marker comment present, newest marker comment,
/// newest `labeled loom:blocked`)`. Stored only when every leg answered.
pub(crate) static QUARANTINE_SCAN: ReadCache<QuarantineScan> = ReadCache::new(CLAIM_MAX_AGE);

/// See [`QUARANTINE_SCAN`].
pub(crate) type QuarantineScan = (bool, Option<DateTime<Utc>>, Option<DateTime<Utc>>);

/// One process-wide cache of found answers, keyed by [`key`].
pub(crate) struct ReadCache<V> {
    entries: Mutex<Option<HashMap<String, (Instant, V)>>>,
    /// The age past which an entry is re-read even with an unchanged version.
    max_age: Duration,
}

impl<V: Clone> ReadCache<V> {
    const fn new(max_age: Duration) -> Self {
        Self {
            entries: Mutex::new(None),
            max_age,
        }
    }

    /// The fresh cached answer for `key`, else `fetch()` — stored when it
    /// found something. A `None` key (no version, or caching off) always
    /// fetches.
    pub(crate) fn get_or(
        &self,
        key: Option<String>,
        fetch: impl FnOnce() -> Option<V>,
    ) -> Option<V> {
        let Some(key) = key else {
            return fetch();
        };
        let now = Instant::now();
        if let Ok(mut guard) = self.entries.lock() {
            let map = guard.get_or_insert_with(HashMap::new);
            match map.get(&key) {
                Some((at, v)) if now.duration_since(*at) < self.max_age => return Some(v.clone()),
                Some(_) => {
                    map.remove(&key);
                }
                None => {}
            }
        }
        let value = fetch()?;
        if let Ok(mut guard) = self.entries.lock() {
            let map = guard.get_or_insert_with(HashMap::new);
            if map.len() >= MAX_ENTRIES {
                let max_age = self.max_age;
                map.retain(|_, (at, _)| now.duration_since(*at) < max_age);
                if map.len() >= MAX_ENTRIES {
                    map.clear();
                }
            }
            map.insert(key, (now, value.clone()));
        }
        Some(value)
    }
}

/// The cache key for PR `number` in `root`: `what` names the fact (a label,
/// a base branch), `version` is the listing's version stamp. `None` — never
/// cached — without a version or with caching off.
pub(crate) fn key(root: &Path, number: u32, what: &str, version: Option<&str>) -> Option<String> {
    let version = version.filter(|v| !v.is_empty())?;
    enabled().then(|| format!("{}\u{1f}{number}\u{1f}{what}\u{1f}{version}", root.display()))
}

/// A cache key from free-form `parts` (joined unambiguously); `None` when
/// caching is off. For facts keyed on immutable inputs rather than a listing
/// version stamp.
pub(crate) fn key_of(parts: &[&str]) -> Option<String> {
    enabled().then(|| parts.join("\u{1f}"))
}

#[cfg(not(test))]
fn enabled() -> bool {
    !matches!(
        std::env::var(READ_CACHE_ENV)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "0" | "false" | "off" | "no"
    )
}

/// Test builds: off unless the current test thread opts in, so the many
/// fake-`gh` tests that re-run a pass against changed stub output are not
/// served a previous test's answer.
#[cfg(test)]
fn enabled() -> bool {
    TEST_ENABLED.with(std::cell::Cell::get)
}

#[cfg(test)]
thread_local! {
    static TEST_ENABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Turn THIS test thread's caching on or off.
#[cfg(test)]
pub(crate) fn set_test_enabled(on: bool) {
    TEST_ENABLED.with(|c| c.set(on));
}

#[cfg(test)]
#[path = "read_cache_tests.rs"]
mod tests;
