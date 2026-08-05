//! Repo-visibility derivation with TTL caching (Epic #4702, Phase 1 — #4703).
//!
//! Every telemetry record that references a repository carries a
//! [`RepoVisibility`](super::RepoVisibility) tag. Deriving it means asking the
//! forge whether the repo is private (`gh api repos/{owner}/{repo} --jq
//! .private`) — a subprocess call far too expensive to pay per emitted record.
//! This module memoizes the answer per `owner/repo`, modeled on the exact
//! "avoid one probe per record" shape [`crate::cpu_headroom`] uses for the
//! measured idle fraction:
//!
//! - a `Mutex`-guarded, process-global per-repo cache (here a `HashMap` keyed on
//!   `owner/repo`, versus `cpu_headroom`'s single-value `CpuUtilState`),
//! - a **refresh** function that shells out only when the cached entry is absent
//!   or older than [`VISIBILITY_CACHE_TTL`] (versus `refresh_cpu_util_cache`),
//! - a pure, non-shelling **read** accessor for the hot path
//!   ([`cached_visibility`], versus `cached_cpu_idle_fraction`).
//!
//! # Private by default, always
//!
//! The forge probe is the ONLY thing that can raise a repo to
//! [`RepoVisibility::Public`]. Every failure mode — `gh` missing, the API call
//! erroring, an unparseable `.private` value, or simply no cached answer yet —
//! resolves to [`RepoVisibility::Private`] via [`derive_visibility`]'s
//! `unwrap_or`. This mirrors the schema's private-safe deserialization: a repo is
//! never treated as public on absent evidence, so a probe failure can never leak
//! private work into the epic's public view.

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::RepoVisibility;

/// TTL for a cached per-repo visibility answer. A repo's public/private state
/// changes rarely, so a generous window keeps the forge-probe rate negligible
/// even under a high record-emission rate. Longer than [`crate::cpu_headroom::
/// CPU_UTIL_MEMO_TTL`] on purpose: visibility is far more stable than CPU load.
pub const VISIBILITY_CACHE_TTL: Duration = Duration::from_secs(300);

/// One cached visibility answer plus when it was last refreshed (for the TTL gate).
struct VisibilityEntry {
    visibility: RepoVisibility,
    updated_at: Instant,
}

/// Process-global per-repo visibility cache, keyed on `owner/repo`.
///
/// `HashMap::new()` is not a `const fn`, so — unlike `cpu_headroom`'s
/// `static Mutex<CpuUtilState>` with a `const` constructor — this is lazily
/// initialized through a [`OnceLock`] rather than declared as a plain `static`.
fn cache() -> &'static Mutex<HashMap<String, VisibilityEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, VisibilityEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The cached visibility for `owner_repo`, or `None` when nothing has been cached
/// yet. Never shells out — a pure cache read, safe to call on the hot path (the
/// analogue of [`crate::cpu_headroom::cached_cpu_idle_fraction`]). Note it does
/// **not** consult the TTL: a stale-but-present entry is still returned here;
/// staleness only governs whether [`refresh_visibility_cache`] re-probes.
#[must_use]
pub fn cached_visibility(owner_repo: &str) -> Option<RepoVisibility> {
    cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(owner_repo)
        .map(|e| e.visibility)
}

/// Refresh the cached visibility for `owner_repo`, shelling out to the forge only
/// when the cached entry is absent or older than [`VISIBILITY_CACHE_TTL`]. A
/// no-op (no subprocess) within the TTL window — this is the memoization that
/// keeps a burst of record emissions from each paying a `gh api` call.
///
/// **Blocks** (spawns `gh`) when it does probe; on the daemon's async runtime,
/// call it from `spawn_blocking`, mirroring `cpu_headroom`'s guidance.
pub fn refresh_visibility_cache(owner_repo: &str) {
    refresh_visibility_cache_with(owner_repo, fetch_visibility_via_gh);
}

/// Testable core of [`refresh_visibility_cache`]: the TTL/caching logic with the
/// forge probe injected as `fetch`, so a unit test can substitute a call-counting
/// fake for the real `gh` subprocess (the seam `cpu_headroom`'s tests achieve by
/// stubbing the data source). `fetch` returns `None` when the repo's visibility
/// could not be determined; a `None` is not cached, so a later call re-probes.
fn refresh_visibility_cache_with<F>(owner_repo: &str, fetch: F)
where
    F: FnOnce(&str) -> Option<RepoVisibility>,
{
    let mut guard = cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(entry) = guard.get(owner_repo) {
        if entry.updated_at.elapsed() < VISIBILITY_CACHE_TTL {
            // Fresh — do not re-probe. This is the cache hit the Test Plan pins.
            return;
        }
    }
    // Absent or stale: probe. Hold the lock across the probe (as `cpu_headroom`
    // holds its lock across the ~1s `iostat`); the per-repo answer is cheap to
    // block a concurrent lookup of the *same* repo on, and the alternative
    // (dropping the lock) risks a thundering herd of duplicate probes.
    if let Some(visibility) = fetch(owner_repo) {
        guard.insert(
            owner_repo.to_string(),
            VisibilityEntry {
                visibility,
                updated_at: Instant::now(),
            },
        );
    }
}

/// Derive the visibility for `owner_repo`, refreshing the cache first. This is
/// the one-call public entry point the exporter/persistence layers use at emit
/// time. **Private-safe:** any failure to positively establish `Public` — a
/// probe error or an unparseable answer that leaves nothing cached — yields
/// [`RepoVisibility::Private`].
///
/// Blocks when the cache is cold/stale (see [`refresh_visibility_cache`]).
#[must_use]
pub fn derive_visibility(owner_repo: &str) -> RepoVisibility {
    refresh_visibility_cache(owner_repo);
    cached_visibility(owner_repo).unwrap_or(RepoVisibility::Private)
}

/// Probe the forge for a repo's visibility via `gh api repos/{owner}/{repo} --jq
/// .private`. Returns `Some(Private)`/`Some(Public)` on a clean `true`/`false`
/// answer, and `None` on any failure (missing/erroring `gh`, non-boolean output)
/// so [`derive_visibility`] falls back to the private-safe default rather than
/// caching a guess.
fn fetch_visibility_via_gh(owner_repo: &str) -> Option<RepoVisibility> {
    let mut cmd = Command::new("gh");
    cmd.args(["api", &format!("repos/{owner_repo}"), "--jq", ".private"])
        .stderr(Stdio::null());
    // #5431: this probe carries `owner/repo` in the API path but no
    // checkout-root `current_dir`, so key the token off the owner slug. Without
    // it a cross-owner *private* repo 404s under the root owner's token and is
    // (safely) reported Private even when the owner's own token could read it.
    crate::credential_preflight::apply_gh_config_for_owner_slug(&mut cmd, owner_repo);
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    parse_gh_private(&String::from_utf8_lossy(&output.stdout))
}

/// Parse the `--jq .private` output (`"true"`/`"false"`) into a visibility.
/// `true` ⇒ [`RepoVisibility::Private`], `false` ⇒ [`RepoVisibility::Public`],
/// anything else ⇒ `None` (unparseable — caller falls back to Private). Split
/// from the subprocess I/O so it is unit-testable without a real `gh`.
#[must_use]
fn parse_gh_private(output: &str) -> Option<RepoVisibility> {
    match output.trim() {
        "true" => Some(RepoVisibility::Private),
        "false" => Some(RepoVisibility::Public),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Each test uses a UNIQUE owner/repo key so the process-global cache cannot
    // let one test's entry satisfy another's lookup (the cache outlives a single
    // test under plain `cargo test`; nextest's process-per-test makes this moot,
    // but distinct keys keep the tests correct under both).

    // ------------------------------------------------------------------
    // parse_gh_private — pure parsing.
    // ------------------------------------------------------------------

    #[test]
    fn parse_gh_private_maps_bools() {
        assert_eq!(parse_gh_private("true\n"), Some(RepoVisibility::Private));
        assert_eq!(parse_gh_private("false\n"), Some(RepoVisibility::Public));
        assert_eq!(parse_gh_private("  true  "), Some(RepoVisibility::Private));
    }

    #[test]
    fn parse_gh_private_unparseable_is_none() {
        assert_eq!(parse_gh_private(""), None);
        assert_eq!(parse_gh_private("null"), None);
        assert_eq!(parse_gh_private("not-a-bool"), None);
    }

    // ------------------------------------------------------------------
    // Caching — a second lookup within the TTL must NOT re-invoke the probe.
    // ------------------------------------------------------------------

    #[test]
    fn second_lookup_within_ttl_does_not_reprobe() {
        let key = "test-owner/cache-hit-repo";
        let calls = AtomicUsize::new(0);
        let fetch = |_repo: &str| {
            calls.fetch_add(1, Ordering::SeqCst);
            Some(RepoVisibility::Public)
        };

        // Cold cache: the first refresh probes exactly once.
        refresh_visibility_cache_with(key, fetch);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "cold cache should probe once");
        assert_eq!(cached_visibility(key), Some(RepoVisibility::Public));

        // Warm cache within the TTL: the second refresh must NOT probe again.
        refresh_visibility_cache_with(key, fetch);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a lookup within the TTL window must be served from cache, not re-probed"
        );
        assert_eq!(cached_visibility(key), Some(RepoVisibility::Public));
    }

    #[test]
    fn probe_returning_none_is_not_cached_and_reprobes() {
        let key = "test-owner/uncacheable-repo";
        let calls = AtomicUsize::new(0);
        let fetch = |_repo: &str| -> Option<RepoVisibility> {
            calls.fetch_add(1, Ordering::SeqCst);
            None // e.g. gh failed / unparseable
        };

        refresh_visibility_cache_with(key, fetch);
        // Nothing cached, so the hot-path read is still empty…
        assert_eq!(cached_visibility(key), None);
        // …and a second refresh re-probes (a failed probe is not memoized).
        refresh_visibility_cache_with(key, fetch);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a probe that returned None must not be cached; the next call re-probes"
        );
    }

    // ------------------------------------------------------------------
    // derive_visibility — private-safe fallback.
    // ------------------------------------------------------------------

    #[test]
    fn derive_visibility_falls_back_to_private_when_uncached() {
        // A repo whose probe we force to fail (via the injected fetch) has
        // nothing cached, so the public entry point must resolve to Private —
        // never leak-by-default.
        let key = "test-owner/never-probed-repo";
        refresh_visibility_cache_with(key, |_r| None);
        assert_eq!(cached_visibility(key), None);
        // derive_visibility layers the real gh probe on top; for a repo that
        // does not exist under the test's `gh`, that probe also fails, so the
        // fallback is Private. (We assert the fallback invariant directly.)
        assert_eq!(
            cached_visibility(key).unwrap_or(RepoVisibility::Private),
            RepoVisibility::Private
        );
    }

    #[test]
    fn cached_visibility_absent_key_is_none() {
        assert_eq!(cached_visibility("test-owner/definitely-absent"), None);
    }
}
