//! The fleet admin roster (`fleet/admins.json`), read from the fleet store.
//!
//! Comment trust (`crate::comment_trust`) treats the logins listed here as
//! trusted authors in every fleet repo, so an admin whose org membership is
//! private (`CONTRIBUTOR` to the fleet's App) is still believed (#10303).
//!
//! File contract: `{"admins": ["turian", "rjwalters"]}`. Entries are user
//! accounts; an App-spelled entry (`x[bot]`, `app/x`) is ignored.
//!
//! **Fails closed.** Store unset, fetch failure, missing file, malformed JSON
//! or a wrong type all yield an empty roster, never an error that widens
//! trust, and the reason is carried in [`Admins::state`] so the trust decision
//! can explain itself. Resolution is cached process-wide per store for a TTL
//! (no network call per comment), a stale snapshot older than [`MAX_AGE`] is
//! treated as unavailable, and each failure cause is logged once.
//!
//! The roster is read only from the configured store ref, never from the
//! repository whose comments are being judged.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::fetch::{self, Freshness, Policy};
use super::ADMINS_PATH;

/// Config key for the roster cache TTL, in seconds.
pub const TTL_KEY: &str = "forge.fleetAdminsTtlSecs";
/// Default roster cache TTL.
pub const DEFAULT_TTL_SECS: u64 = 300;
/// A roster served from a cache older than this is treated as unavailable.
pub const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// A resolved roster and how it was obtained.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Admins {
    /// The admin user logins (empty when unavailable).
    pub logins: Vec<String>,
    /// Human description for the sources-consulted line.
    pub state: String,
}

impl Admins {
    /// An unavailable roster: trusts nobody, says why.
    #[must_use]
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            logins: Vec::new(),
            state: format!("unavailable: {}", reason.into()),
        }
    }

    /// Whether the roster loaded.
    #[must_use]
    pub fn is_loaded(&self) -> bool {
        !self.state.starts_with("unavailable")
    }
}

/// Parse `fleet/admins.json`. Fails closed on any shape problem.
pub fn parse(text: &str) -> Result<Vec<String>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("malformed JSON: {e}"))?;
    let arr = v
        .get("admins")
        .and_then(Value::as_array)
        .ok_or_else(|| "`admins` is missing or not an array".to_string())?;
    Ok(arr
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && !crate::comment_trust::is_app_login(s))
        .map(str::to_string)
        .collect())
}

/// A process-wide TTL cache of resolved rosters, keyed by store.
#[derive(Default)]
pub struct Cache {
    map: Mutex<HashMap<String, (Instant, Admins)>>,
}

impl Cache {
    /// The cached roster for `key` if younger than `ttl`, else `load()`'s.
    pub fn get_or_load(&self, key: &str, ttl: Duration, load: impl FnOnce() -> Admins) -> Admins {
        let mut map = self
            .map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((at, admins)) = map.get(key) {
            if at.elapsed() < ttl {
                return admins.clone();
            }
        }
        let admins = load();
        map.insert(key.to_string(), (Instant::now(), admins.clone()));
        admins
    }
}

fn global() -> &'static Cache {
    static C: OnceLock<Cache> = OnceLock::new();
    C.get_or_init(Cache::default)
}

fn warn_once(cause: &str) {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(Mutex::default);
    if seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(cause.to_string())
    {
        log::warn!("comment_trust: fleet admin roster {cause}; trust is not widened (#10303)");
    }
}

/// Turn a loaded snapshot into a roster (fail closed).
#[must_use]
pub fn from_loaded(loaded: &fetch::Loaded, now: chrono::DateTime<chrono::Utc>) -> Admins {
    let m = &loaded.snapshot.manifest;
    if matches!(loaded.freshness, Freshness::Cached { .. }) {
        let age = (now - m.confirmed_at).to_std().unwrap_or_default();
        if age > MAX_AGE {
            return Admins::unavailable(format!(
                "cached snapshot of {} is older than 24h and the forge was not reachable",
                m.repo
            ));
        }
    }
    let text = match loaded.snapshot.text(ADMINS_PATH) {
        Ok(Some(t)) => t,
        Ok(None) => {
            return Admins::unavailable(format!("{} has no {ADMINS_PATH}", m.repo));
        }
        Err(e) => return Admins::unavailable(format!("{e:#}")),
    };
    match parse(&text) {
        Ok(logins) => Admins {
            state: format!(
                "loaded {} from {} @ {}",
                logins.len(),
                m.repo,
                loaded.snapshot.short_commit()
            ),
            logins,
        },
        Err(e) => Admins::unavailable(format!("{ADMINS_PATH}: {e}")),
    }
}

/// The roster for the workspace at `root`, cached for the configured TTL.
#[must_use]
pub fn resolve(root: &std::path::Path) -> Admins {
    let effective = crate::config_resolver::resolve_effective_config(root);
    let location = match super::resolve_location(&effective, &|k| std::env::var(k).ok()) {
        Ok(Some(l)) => l,
        Ok(None) => return Admins::unavailable("no fleet store configured (fleet.repo)"),
        Err(e) => {
            let reason = format!("{e:#}");
            warn_once(&reason);
            return Admins::unavailable(reason);
        }
    };
    let ttl = crate::config_resolver::get_path(&effective, TTL_KEY)
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TTL_SECS);
    let key = format!("{}@{}", location.repo, location.reference);
    global().get_or_load(&key, Duration::from_secs(ttl), || {
        let admins = load_live(root, &location);
        if !admins.is_loaded() {
            warn_once(&admins.state);
        }
        admins
    })
}

fn load_live(root: &std::path::Path, location: &super::StoreLocation) -> Admins {
    let cache_dir = match super::default_cache_dir(location) {
        Ok(d) => d,
        Err(e) => return Admins::unavailable(format!("{e:#}")),
    };
    let transport = super::gh::GhTransport::new(root, &location.repo);
    let now = chrono::Utc::now();
    match fetch::load(&transport, &cache_dir, location, Policy::AllowStale, now) {
        Ok(loaded) => from_loaded(&loaded, now),
        Err(e) => Admins::unavailable(format!("{e:#}")),
    }
}

#[cfg(test)]
#[path = "tests/admins_tests.rs"]
mod tests;
