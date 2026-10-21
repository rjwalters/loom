//! Judge is not served a PR whose CI is still running at its head (#11971).
//!
//! Judge's own CI pre-check releases such a PR at once ("CI is still
//! running"), so serving it only buys a claim, a spawn and a release: after
//! the 0.19.1160 roll about two thirds of Judge runs with a PR were exactly
//! that. `pr-queue --role judge` and the Judge spawn gate therefore skip a
//! candidate while its head has CI pending, and serve it once CI concludes.
//!
//! "Pending" is the no-verdict backstop's rule ([`backstop::pending_exempts`],
//! #11867/#11920): a check run or legacy status not yet concluded that
//! started inside [`backstop::PENDING_EXEMPT_HOURS`]. A run stuck queued past
//! that cap no longer holds the PR back, so a head with no runner is still
//! reviewed. An unreadable read counts as not pending: the skip is a
//! best-effort saving, never a reason to starve a PR.
//!
//! Cost stays bounded: the listing reads only rows that are otherwise about
//! to be served ([`CI_SCREENED_ROWS`]), the reads are conditional (an
//! unchanged page is a free `304`), and a long-lived process (the daemon's
//! spawn gate) keeps each head's answer for [`CACHE_TTL`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::judge_backstop as backstop;

/// How many CI-clear rows `pr-queue --role judge` screens before serving the
/// rest unread. A Judge pass claims the first row it can, so only the head
/// of the queue is worth a read; a row further down that is pending is still
/// caught by Judge's own pre-check.
pub(super) const CI_SCREENED_ROWS: usize = 3;

/// How long one head's answer is reused in-process. Short: CI that concludes
/// should be served within a tick or two, and a new push is a new head (a
/// new key) anyway.
pub(super) const CACHE_TTL: Duration = Duration::from_secs(60);

/// Whether `sha` (a PR head) has CI pending young enough to hold it from
/// Judge, read through `get(kind, path)` with `path` relative to `repos/{o}/{r}/`:
/// every check-run page up to [`backstop::MAX_CHECK_RUN_PAGES`], then the
/// legacy commit statuses. Stops at the first read that is pending, so the
/// usual cost is one call; an unreadable read counts as not pending.
pub fn ci_pending(
    sha: &str,
    now: DateTime<Utc>,
    mut get: impl FnMut(backstop::CiRead, &str) -> Option<Value>,
) -> bool {
    backstop::ci_state(sha, now, &mut get) == backstop::CiState::Young
}

type Cache = Mutex<HashMap<(PathBuf, String), (bool, Instant)>>;

fn cache() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(Cache::default)
}

/// `read()`'s answer for head `sha` of the repository at `root`, reused for
/// [`CACHE_TTL`]. Expired entries are dropped on every miss.
pub(super) fn cached(root: &Path, sha: &str, read: impl FnOnce() -> bool) -> bool {
    let key = (root.to_path_buf(), sha.to_string());
    let now = Instant::now();
    let hit = cache().lock().ok().and_then(|c| c.get(&key).copied());
    if let Some((pending, at)) = hit {
        if now.duration_since(at) < CACHE_TTL {
            return pending;
        }
    }
    let pending = read();
    if let Ok(mut c) = cache().lock() {
        c.retain(|_, (_, at)| now.duration_since(*at) < CACHE_TTL);
        c.insert(key, (pending, now));
    }
    pending
}

/// The cached answer for head `sha` of the repository at `root`, when one is
/// still fresh; never reads. Lets a budgeted caller (the step draw's Judge
/// hold, #12042) spend its reads on heads it has no answer for.
pub fn cached_answer(root: &Path, sha: &str) -> Option<bool> {
    let key = (root.to_path_buf(), sha.to_string());
    let hit = cache().lock().ok().and_then(|c| c.get(&key).copied())?;
    (hit.1.elapsed() < CACHE_TTL).then_some(hit.0)
}

/// Forget every cached answer (tests that change CI under one head).
#[cfg(test)]
pub(super) fn clear_cache() {
    if let Ok(mut c) = cache().lock() {
        c.clear();
    }
}
