//! Process-wide cache of rate-limit budget probes (#12204).
//!
//! Before this, every caller that wanted the remaining budget asked GitHub:
//! the 60 s gauge tick, every `check-stale-blocked` run and every breaker
//! trip each paid `GET /rate_limit` plus a standalone GraphQL `rateLimit`
//! query — about 4.2k calls an hour across a 6-worker fleet, almost all on
//! the writer App. A reading does not change meaning until its window
//! resets, so one probe per credential per window is enough:
//!
//! - A successful probe is held per credential ([`CacheKey`]) until the
//!   earlier of its `core` and `graphql` resets. Inside that window
//!   [`super::forge::probe_budget_ctx`] answers from here with no forge call.
//! - A failed probe is remembered for [`NEGATIVE_TTL_SECS`], so a wedged
//!   `gh` or a credential that cannot read `/rate_limit` is not re-asked on
//!   every tick.
//! - The breaker's trip path ([`super::forge::refresh_budget_ctx`]) is the
//!   one caller that always asks: a 403/429 is exactly when the cached
//!   reading has stopped being true, and the trip needs the real reset.
//!   Its answer replaces the cached one.
//!
//! Between probes, the remaining counts are refreshed for free from the
//! `x-ratelimit-*` headers the bucket book already collects off ordinary
//! responses (see `super::forge`), so a cached reading does not sit at its
//! window-start value all hour.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock, PoisonError};

use chrono::{DateTime, Duration, Utc};

use super::BudgetSnapshot;

/// How long a failed probe suppresses the next one for the same credential.
pub const NEGATIVE_TTL_SECS: i64 = 300;

/// Most credentials held (a host has a handful of credential directories).
const MAX_ENTRIES: usize = 256;

/// The credential a probe read: the effective `GH_CONFIG_DIR` the `gh`
/// child ran under (`None` = the process's own, unset) and the `gh` program
/// it ran (an injected stub or launcher). Holds paths only, never a token.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub config_dir: Option<PathBuf>,
    pub program: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum Entry {
    Budget(BudgetSnapshot),
    Failed { retry_at: DateTime<Utc> },
}

/// What the cache knows about a credential at an instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup {
    /// A reading whose window is still open.
    Hit(BudgetSnapshot),
    /// A recent probe failed; do not ask again yet.
    RecentlyFailed,
    /// Nothing usable: a probe may be made.
    Miss,
}

fn cache() -> &'static Mutex<HashMap<CacheKey, Entry>> {
    static CACHE: OnceLock<Mutex<HashMap<CacheKey, Entry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// When `b` stops being believed: the earlier of its two resets.
#[must_use]
pub fn valid_until(b: &BudgetSnapshot) -> DateTime<Utc> {
    b.core_reset.min(b.graphql_reset)
}

/// The cached state of `key` at `now` (pure read, no forge call).
#[must_use]
pub fn lookup(key: &CacheKey, now: DateTime<Utc>) -> Lookup {
    let map = cache().lock().unwrap_or_else(PoisonError::into_inner);
    match map.get(key) {
        Some(Entry::Budget(b)) if valid_until(b) > now => Lookup::Hit(*b),
        Some(Entry::Failed { retry_at }) if *retry_at > now => Lookup::RecentlyFailed,
        _ => Lookup::Miss,
    }
}

/// Record a probe's outcome for `key` at `now`: a reading is held until its
/// reset, a failure for [`NEGATIVE_TTL_SECS`]. A reading whose window has
/// already closed is not held (it would never be served).
pub fn store(key: CacheKey, outcome: Option<BudgetSnapshot>, now: DateTime<Utc>) {
    let entry = match outcome {
        Some(b) if valid_until(&b) > now => Entry::Budget(b),
        Some(_) => return,
        None => Entry::Failed {
            retry_at: now + Duration::seconds(NEGATIVE_TTL_SECS),
        },
    };
    let mut map = cache().lock().unwrap_or_else(PoisonError::into_inner);
    if map.len() >= MAX_ENTRIES && !map.contains_key(&key) {
        map.retain(|_, e| match e {
            Entry::Budget(b) => valid_until(b) > now,
            Entry::Failed { retry_at } => *retry_at > now,
        });
        if map.len() >= MAX_ENTRIES {
            return;
        }
    }
    map.insert(key, entry);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_785_352_000 + secs, 0).unwrap()
    }

    fn key(name: &str) -> CacheKey {
        CacheKey {
            config_dir: Some(PathBuf::from(format!("/nonexistent/budget-cache-test/{name}"))),
            program: None,
        }
    }

    fn snap(core_reset: i64, graphql_reset: i64) -> BudgetSnapshot {
        BudgetSnapshot {
            core_remaining: 4000,
            core_reset: t(core_reset),
            graphql_remaining: 3000,
            graphql_reset: t(graphql_reset),
            core_used: Some(1000),
            graphql_used: Some(2000),
            probed_at: t(0),
        }
    }

    #[test]
    fn a_reading_is_served_until_its_earlier_reset_then_misses() {
        let k = key("until-reset");
        assert_eq!(lookup(&k, t(0)), Lookup::Miss);
        store(k.clone(), Some(snap(3600, 1800)), t(0));
        assert_eq!(lookup(&k, t(1)), Lookup::Hit(snap(3600, 1800)));
        assert_eq!(lookup(&k, t(1799)), Lookup::Hit(snap(3600, 1800)));
        assert_eq!(lookup(&k, t(1800)), Lookup::Miss, "graphql window closed");
    }

    #[test]
    fn a_failure_is_remembered_for_the_negative_ttl() {
        let k = key("negative");
        store(k.clone(), None, t(0));
        assert_eq!(lookup(&k, t(NEGATIVE_TTL_SECS - 1)), Lookup::RecentlyFailed);
        assert_eq!(lookup(&k, t(NEGATIVE_TTL_SECS)), Lookup::Miss);
    }

    #[test]
    fn an_already_reset_reading_is_not_held() {
        let k = key("stale");
        store(k.clone(), Some(snap(-5, 100)), t(0));
        assert_eq!(lookup(&k, t(0)), Lookup::Miss);
    }

    #[test]
    fn credentials_are_cached_independently() {
        let (a, b) = (key("cred-a"), key("cred-b"));
        store(a.clone(), Some(snap(3600, 3600)), t(0));
        assert!(matches!(lookup(&a, t(1)), Lookup::Hit(_)));
        assert_eq!(lookup(&b, t(1)), Lookup::Miss);
        let prog = CacheKey {
            program: Some("/stub/gh".into()),
            ..a.clone()
        };
        assert_eq!(lookup(&prog, t(1)), Lookup::Miss, "another gh program is another key");
    }
}
