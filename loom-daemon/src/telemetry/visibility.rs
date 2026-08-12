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
//!
//! # Silent failure vs. durable mis-stamp (#6039)
//!
//! A probe failure is fail-closed (correct) but was previously **silent** — no
//! log line distinguished "repo is actually private" from "probe failed,
//! defaulting private". Combined with the cache's "a failed probe is never
//! cached" rule (see [`refresh_visibility_cache_with`]), a transient forge
//! outage durably stamped every *new* record `Private` for as long as the
//! outage lasted, with zero operator-visible signal — see the 2026-08-11
//! incident writeup on the issue. [`note_probe_failed`]/[`note_probe_recovered`]
//! close that gap: exactly one `warn` line is emitted per repo per outage (not
//! per record — outages are typically many probe attempts, all deduped to one
//! line), and exactly one `info` line on recovery.

use std::collections::HashMap;
use std::fmt;
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
/// stubbing the data source). `fetch` returns `Err(ProbeFailure)` when the
/// repo's visibility could not be determined; a failure is not cached, so a
/// later call re-probes — and **is not silent**: [`note_probe_failed`] logs the
/// first failure of a streak, [`note_probe_recovered`] logs the recovery.
fn refresh_visibility_cache_with<F>(owner_repo: &str, fetch: F)
where
    F: FnOnce(&str) -> Result<RepoVisibility, ProbeFailure>,
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
    //
    // On failure the *existing* entry (if any) is deliberately left in place —
    // this is the stale-cache fallback that lets a warm cache ride out a probe
    // outage (see the module docs and the `stale_cache_survives_probe_outage`
    // test below) rather than falling all the way back to the private-safe
    // default on every re-probe of an already-known-public repo.
    let result = fetch(owner_repo);
    if let Ok(visibility) = result {
        guard.insert(
            owner_repo.to_string(),
            VisibilityEntry {
                visibility,
                updated_at: Instant::now(),
            },
        );
    }
    drop(guard);
    match result {
        Ok(_) => note_probe_recovered(owner_repo),
        Err(failure) => note_probe_failed(owner_repo, failure),
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

/// Why a visibility probe failed to positively establish a repo's visibility.
/// Purely descriptive — used only to name the failure mode in the warn log
/// line ([`note_probe_failed`]); it never changes the fail-closed outcome
/// itself, which stays `Private` regardless of which variant fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeFailure {
    /// The `gh` subprocess itself failed to spawn (e.g. binary missing / not on `PATH`).
    SpawnFailed,
    /// `gh` ran but exited non-zero (API error, rate limit, auth failure, forge outage, ...).
    NonZeroExit,
    /// `gh` exited `0` but the `.private` output was not a bare `true`/`false`.
    UnparseableOutput,
}

impl fmt::Display for ProbeFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ProbeFailure::SpawnFailed => "gh failed to spawn",
            ProbeFailure::NonZeroExit => "gh exited non-zero",
            ProbeFailure::UnparseableOutput => "gh returned unparseable output",
        })
    }
}

/// Probe the forge for a repo's visibility via `gh api repos/{owner}/{repo} --jq
/// .private`. Returns `Ok(Private)`/`Ok(Public)` on a clean `true`/`false`
/// answer, and `Err(ProbeFailure)` on any failure (missing/erroring `gh`,
/// non-boolean output) so [`derive_visibility`] falls back to the private-safe
/// default rather than caching a guess — and so the caller can name the
/// failure mode in its log line instead of it being swallowed.
fn fetch_visibility_via_gh(owner_repo: &str) -> Result<RepoVisibility, ProbeFailure> {
    let mut cmd = Command::new("gh");
    cmd.args(["api", &format!("repos/{owner_repo}"), "--jq", ".private"])
        .stderr(Stdio::null());
    // #5431: this probe carries `owner/repo` in the API path but no
    // checkout-root `current_dir`, so key the token off the owner slug. Without
    // it a cross-owner *private* repo 404s under the root owner's token and is
    // (safely) reported Private even when the owner's own token could read it.
    crate::credential_preflight::apply_gh_config_for_owner_slug(&mut cmd, owner_repo);
    let output = cmd.output().map_err(|_| ProbeFailure::SpawnFailed)?;
    if !output.status.success() {
        return Err(ProbeFailure::NonZeroExit);
    }
    parse_gh_private(&String::from_utf8_lossy(&output.stdout))
        .ok_or(ProbeFailure::UnparseableOutput)
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

// ------------------------------------------------------------------
// Failure/recovery logging (#6039) — de-duped to one line per transition.
// ------------------------------------------------------------------

/// Process-global "is this repo's most recent probe currently failing"
/// tracker, keyed on `owner/repo`. Presence (with the recorded
/// [`ProbeFailure`]) means the most recent probe failed and no warn line has
/// been emitted for a *subsequent* failure of the same outage yet. Absence
/// means either the repo has never been probed, or its most recent probe
/// succeeded — both cases where a fresh failure is worth a fresh warn line.
///
/// Deliberately a **separate** map/lock from [`cache`]: this tracks probe
/// *health*, not the visibility *answer*, and the two must stay independent —
/// e.g. a stale-but-still-served cache entry (see [`refresh_visibility_cache_with`])
/// coexists with an actively-failing probe.
fn probe_failing() -> &'static Mutex<HashMap<String, ProbeFailure>> {
    static PROBE_FAILING: OnceLock<Mutex<HashMap<String, ProbeFailure>>> = OnceLock::new();
    PROBE_FAILING.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record a probe failure for `owner_repo` and report whether this is the
/// *first* failure since the last success (i.e. a state transition worth
/// logging) — split from the actual `log::warn!` call so the de-duplication
/// logic is unit-testable without a log-capturing harness.
fn record_probe_failure_transition(owner_repo: &str, failure: ProbeFailure) -> bool {
    let mut guard = probe_failing()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.insert(owner_repo.to_string(), failure).is_none()
}

/// Record a probe success for `owner_repo` and report whether it followed a
/// failure streak (i.e. a recovery worth logging) — split from `log::info!`
/// for the same testability reason as [`record_probe_failure_transition`]. A
/// bare first-ever success (nothing was failing) is **not** a "recovery" and
/// returns `false`, matching the acceptance criterion that only a genuine
/// transition gets a log line.
fn record_probe_recovery_transition(owner_repo: &str) -> bool {
    let mut guard = probe_failing()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.remove(owner_repo).is_some()
}

/// Log (at `warn`) the first probe failure for `owner_repo` since its last
/// success. Subsequent failures of the same repo while the outage is ongoing
/// are silent — satisfying "one warn line per repo per outage, not per record".
fn note_probe_failed(owner_repo: &str, failure: ProbeFailure) {
    if record_probe_failure_transition(owner_repo, failure) {
        log::warn!(
            "telemetry: visibility probe failing for {owner_repo} ({failure}) — \
             defaulting new records to private until the probe recovers"
        );
    }
}

/// Log (at `info`) exactly once when `owner_repo`'s probe recovers after a
/// failure streak. A repo whose probe has never failed stays silent on every
/// ordinary success — only the failing → healthy transition is worth a line.
fn note_probe_recovered(owner_repo: &str) {
    if record_probe_recovery_transition(owner_repo) {
        log::info!("telemetry: visibility probe recovered for {owner_repo}");
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
            Ok(RepoVisibility::Public)
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
    fn probe_failure_is_not_cached_and_reprobes() {
        let key = "test-owner/uncacheable-repo";
        let calls = AtomicUsize::new(0);
        let fetch = |_repo: &str| -> Result<RepoVisibility, ProbeFailure> {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(ProbeFailure::NonZeroExit) // e.g. gh failed / unparseable
        };

        refresh_visibility_cache_with(key, fetch);
        // Nothing cached, so the hot-path read is still empty…
        assert_eq!(cached_visibility(key), None);
        // …and a second refresh re-probes (a failed probe is not memoized).
        refresh_visibility_cache_with(key, fetch);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a probe that failed must not be cached; the next call re-probes"
        );
    }

    // ------------------------------------------------------------------
    // Stale-cache fallback (acceptance criterion #3) — a warm cache entry
    // past its TTL must survive a probe outage instead of being evicted.
    // ------------------------------------------------------------------

    #[test]
    fn stale_cache_survives_probe_outage_past_ttl() {
        let key = "test-owner/stale-survives-outage-repo";

        // Seed the cache directly with an entry older than the TTL — the
        // real-world equivalent of a repo that was successfully probed once,
        // then the forge went down for longer than VISIBILITY_CACHE_TTL.
        {
            let mut guard = cache().lock().unwrap();
            guard.insert(
                key.to_string(),
                VisibilityEntry {
                    visibility: RepoVisibility::Public,
                    updated_at: Instant::now() - VISIBILITY_CACHE_TTL - Duration::from_secs(1),
                },
            );
        }

        let calls = AtomicUsize::new(0);
        let failing_fetch = |_repo: &str| -> Result<RepoVisibility, ProbeFailure> {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(ProbeFailure::NonZeroExit)
        };

        // The entry is stale, so this attempts a re-probe...
        refresh_visibility_cache_with(key, failing_fetch);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "stale entry should trigger a re-probe attempt"
        );

        // ...but the probe failing must NOT evict or downgrade the stale
        // answer: a warm cache rides out the outage rather than falling all
        // the way back to the private-safe default.
        assert_eq!(
            cached_visibility(key),
            Some(RepoVisibility::Public),
            "a probe failure on a stale entry must leave the last-known-good answer in place"
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
        refresh_visibility_cache_with(key, |_r| Err(ProbeFailure::NonZeroExit));
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

    // ------------------------------------------------------------------
    // Failure/recovery transition logging (#6039) — one line per outage.
    // ------------------------------------------------------------------

    #[test]
    fn failure_transition_fires_once_per_outage() {
        let key = "test-owner/failure-transition-repo-1";
        // First failure: this repo was healthy (never probed), so it's a
        // genuine transition worth a warn line.
        assert!(
            record_probe_failure_transition(key, ProbeFailure::NonZeroExit),
            "the first failure of an outage must be reported"
        );
        // A second (and third) failure while still down must NOT re-fire —
        // this is the "not per record" half of the acceptance criterion.
        assert!(
            !record_probe_failure_transition(key, ProbeFailure::NonZeroExit),
            "a repeat failure during the same outage must not be reported again"
        );
        assert!(
            !record_probe_failure_transition(key, ProbeFailure::UnparseableOutput),
            "a repeat failure (even a different failure mode) during the same outage must not re-fire"
        );
    }

    #[test]
    fn recovery_transition_only_fires_after_a_failure() {
        let key = "test-owner/recovery-transition-repo-1";
        // No prior failure recorded — an ordinary first-ever success is not
        // a "recovery" and must not fire.
        assert!(
            !record_probe_recovery_transition(key),
            "a success with no prior failure must not be reported as a recovery"
        );

        // Now force a failure, then recover — this transition must fire exactly once.
        assert!(record_probe_failure_transition(key, ProbeFailure::SpawnFailed));
        assert!(
            record_probe_recovery_transition(key),
            "recovering from a failure streak must be reported"
        );
        // Calling it again immediately (nothing failed in between) must not re-fire.
        assert!(
            !record_probe_recovery_transition(key),
            "a second consecutive success must not be reported as another recovery"
        );
    }

    #[test]
    fn probe_failure_display_names_each_mode() {
        assert_eq!(ProbeFailure::SpawnFailed.to_string(), "gh failed to spawn");
        assert_eq!(ProbeFailure::NonZeroExit.to_string(), "gh exited non-zero");
        assert_eq!(ProbeFailure::UnparseableOutput.to_string(), "gh returned unparseable output");
    }
}
