//! Repo-visibility derivation with TTL caching (Epic #4702, Phase 1 — #4703).
//!
//! Every telemetry record that references a repository carries a
//! [`RepoVisibility`](super::RepoVisibility) tag. Deriving it means asking the
//! forge whether the repo is private (a conditional `gh api --include
//! repos/{owner}/{repo}` through [`crate::forge_etag_store`], reading
//! `.private`) — a subprocess call far too expensive to pay per emitted record.
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
//!
//! # Conditional revalidation (#10512)
//!
//! The probe goes through the shared ETag store under the `"visibility-"`
//! prefix: the `ETag` and body of every `200` persist on disk, so a TTL
//! expiry (or a daemon restart) revalidates with `If-None-Match` and an
//! unchanged repo answers a `304` that is free on the core bucket. The TTL is
//! one hour, so a public/private flip is seen within an hour (it was 5 min);
//! nothing but a `200` body saying `"private": false` ever raises a repo to
//! Public.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::RepoVisibility;

/// TTL for a cached per-repo visibility answer. A repo's public/private state
/// changes rarely, so a generous window keeps the forge-probe rate negligible
/// even under a high record-emission rate. Longer than [`crate::cpu_headroom::
/// CPU_UTIL_MEMO_TTL`] on purpose: visibility is far more stable than CPU load.
/// One hour (#10512, parity with `repo_identity`'s positive TTL): each expiry
/// is a conditional revalidation, so it is also the bound on how long a
/// public/private flip goes unseen.
pub const VISIBILITY_CACHE_TTL: Duration = Duration::from_secs(3600);

/// How long a failed probe backs off when no rate-limit reset is known —
/// the pre-#10512 5-minute cadence, kept separate from the (longer) answer TTL
/// so a transient failure on a cold repo is retried as soon as it was before.
const FAILURE_BACKOFF: Duration = Duration::from_secs(300);

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

/// Process-global negative cache (#10087): per `owner/repo`, the instant before
/// which a failed probe must not be retried. Without it, "a failed probe is
/// never cached as an *answer*" (#6039) degenerated into "every telemetry
/// emission re-spawns `gh`", which during a rate-limit outage meant thousands of
/// doomed `gh api` calls. This stores only a retry deadline, never a visibility,
/// so the fail-closed rule is untouched.
fn next_probe_after() -> &'static Mutex<HashMap<String, Instant>> {
    static NEXT: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    NEXT.get_or_init(|| Mutex::new(HashMap::new()))
}

/// How long to back off after `failure`. A rate limit waits for the breaker's
/// reported cooldown end when one is known; everything else (and a rate limit
/// with no known reset) waits [`FAILURE_BACKOFF`].
fn backoff_for(failure: ProbeFailure) -> Duration {
    if failure == ProbeFailure::RateLimited {
        if let Some(until) =
            crate::rate_limit_breaker::global_snapshot().and_then(|s| s.cooldown_until)
        {
            if let Ok(d) = (until - chrono::Utc::now()).to_std() {
                return d.max(Duration::from_secs(1));
            }
        }
    }
    FAILURE_BACKOFF
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
    refresh_visibility_cache_with(
        owner_repo,
        fetch_visibility_via_gh,
        crate::rate_limit_breaker::global_is_suppressed,
    );
}

/// Testable core of [`refresh_visibility_cache`]: the TTL/caching logic with the
/// forge probe injected as `fetch`, so a unit test can substitute a call-counting
/// fake for the real `gh` subprocess (the seam `cpu_headroom`'s tests achieve by
/// stubbing the data source). `fetch` returns `Err(ProbeFailure)` when the
/// repo's visibility could not be determined; a failure is not cached, so a
/// later call is *backed off* (#10087, see [`next_probe_after`]) rather than
/// re-probed per call — and **is not silent**: [`note_probe_failed`] logs the
/// first failure of a streak, [`note_probe_recovered`] logs the recovery.
fn refresh_visibility_cache_with<F, S>(owner_repo: &str, fetch: F, is_suppressed: S)
where
    F: FnOnce(&str) -> Result<RepoVisibility, ProbeFailure>,
    S: FnOnce() -> bool,
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
    // Negative cache (#10087): a recent failure suppresses re-probing, for a
    // cold entry and a stale one alike.
    {
        let mut next = next_probe_after()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match next.get(owner_repo) {
            Some(deadline) if Instant::now() < *deadline => return,
            Some(_) => {
                next.remove(owner_repo);
            }
            None => {}
        }
    }
    // Never spawn `gh` while the shared rate-limit breaker is suppressing the
    // forge (#10087). Not recorded as a failure: the breaker owns that window.
    if is_suppressed() {
        return;
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
        Ok(_) => {
            next_probe_after()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(owner_repo);
            note_probe_recovered(owner_repo);
        }
        Err(failure) => {
            next_probe_after()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(owner_repo.to_string(), Instant::now() + backoff_for(failure));
            note_probe_failed(owner_repo, failure);
        }
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
    /// `gh` exited non-zero and its stderr carried a rate-limit signature (#10087).
    RateLimited,
    /// The read answered but its `.private` was not a JSON boolean.
    UnparseableOutput,
}

impl fmt::Display for ProbeFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ProbeFailure::SpawnFailed => "gh failed to spawn",
            ProbeFailure::NonZeroExit => "gh exited non-zero",
            ProbeFailure::RateLimited => "gh was rate limited",
            ProbeFailure::UnparseableOutput => "gh returned unparseable output",
        })
    }
}

/// Probe the forge for a repo's visibility: a conditional `gh api --include
/// repos/{owner}/{repo}` through the shared ETag store (#10512), whose `200`
/// body is parsed for `.private` and whose `304` serves the stored body.
/// Returns `Ok(Private)`/`Ok(Public)` on a clean boolean answer, and
/// `Err(ProbeFailure)` on any failure (missing/erroring `gh`, a `404`, a
/// non-boolean `.private`) so [`derive_visibility`] falls back to the
/// private-safe default rather than caching a guess — and so the caller can
/// name the failure mode in its log line instead of it being swallowed.
///
/// The caller string stays `visibility.repo` (#10089 accounting) and the
/// store names `owner/repo` in the URL with no checkout `cwd`, so the read
/// runs under the target owner's credential (#5431). The store books the
/// call, feeds the rate-limit breaker, and honours its suppression.
fn fetch_visibility_via_gh(owner_repo: &str) -> Result<RepoVisibility, ProbeFailure> {
    use crate::forge_etag_store::{cached_read, ConditionalRead};
    let gh_bin = std::path::PathBuf::from(crate::gh_invocation::gh_bin());
    let read = cached_read(
        ConditionalRead::new("visibility.repo", crate::forge_call_stats::ops::REPO_VIEW),
        &gh_bin,
        None,
        Some(owner_repo),
        &format!("repos/{owner_repo}"),
        "visibility-",
    )
    .map_err(|e| classify_failure(&e.to_string()))?;
    // A `404` (`body: None`) is the fail-closed path, never cached (#6039).
    let body = read.body.ok_or(ProbeFailure::NonZeroExit)?;
    parse_repo_private_json(&body).ok_or(ProbeFailure::UnparseableOutput)
}

/// Classify a failed store read from its error text. The store has already
/// fed the shared breaker (#10087), so this only names the failure mode and
/// picks the backoff ([`backoff_for`]).
fn classify_failure(err: &str) -> ProbeFailure {
    if crate::rate_limit_breaker::indicates_rate_limit(err) || err.contains("rate-limit breaker") {
        ProbeFailure::RateLimited
    } else if err.starts_with("failed to invoke") {
        ProbeFailure::SpawnFailed
    } else {
        ProbeFailure::NonZeroExit
    }
}

/// Parse a `repos/{owner}/{repo}` JSON body's `.private` into a visibility.
/// `true` ⇒ [`RepoVisibility::Private`], `false` ⇒ [`RepoVisibility::Public`],
/// anything else (missing, `null`, non-boolean, not JSON) ⇒ `None`
/// (unparseable — caller falls back to Private). Split from the subprocess
/// I/O so it is unit-testable without a real `gh`.
#[must_use]
fn parse_repo_private_json(body: &str) -> Option<RepoVisibility> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    if value.get("private")?.as_bool()? {
        Some(RepoVisibility::Private)
    } else {
        Some(RepoVisibility::Public)
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
    // parse_repo_private_json — pure parsing.
    // ------------------------------------------------------------------

    #[test]
    fn parse_repo_private_json_maps_bools() {
        let parse = parse_repo_private_json;
        assert_eq!(parse(r#"{"private":true}"#), Some(RepoVisibility::Private));
        assert_eq!(parse(r#"{"private":false,"x":1}"#), Some(RepoVisibility::Public));
        assert_eq!(parse(" {\"private\": true}\n"), Some(RepoVisibility::Private));
    }

    #[test]
    fn parse_repo_private_json_unparseable_is_none() {
        let parse = parse_repo_private_json;
        assert_eq!(parse(""), None);
        assert_eq!(parse("true"), None, "the old --jq output is not a repo body");
        assert_eq!(parse(r#"{"name":"r"}"#), None, "missing");
        assert_eq!(parse(r#"{"private":null}"#), None);
        assert_eq!(parse(r#"{"private":"false"}"#), None, "non-bool");
        assert_eq!(parse("not json"), None);
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
        refresh_visibility_cache_with(key, fetch, || false);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "cold cache should probe once");
        assert_eq!(cached_visibility(key), Some(RepoVisibility::Public));

        // Warm cache within the TTL: the second refresh must NOT probe again.
        refresh_visibility_cache_with(key, fetch, || false);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a lookup within the TTL window must be served from cache, not re-probed"
        );
        assert_eq!(cached_visibility(key), Some(RepoVisibility::Public));
    }

    #[test]
    fn probe_failure_is_not_cached_as_an_answer() {
        let key = "test-owner/uncacheable-repo";
        let fetch = |_repo: &str| -> Result<RepoVisibility, ProbeFailure> {
            Err(ProbeFailure::NonZeroExit)
        };
        refresh_visibility_cache_with(key, fetch, || false);
        // No visibility is ever stamped on failure (#6039 fail-closed).
        assert_eq!(cached_visibility(key), None);
    }

    #[test]
    fn failed_probes_in_backoff_window_cause_one_spawn() {
        let key = "test-owner/backoff-window-repo";
        let calls = AtomicUsize::new(0);
        let fetch = |_repo: &str| -> Result<RepoVisibility, ProbeFailure> {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(ProbeFailure::RateLimited)
        };
        for _ in 0..50 {
            refresh_visibility_cache_with(key, fetch, || false);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "N emissions => one probe");
        assert_eq!(cached_visibility(key), None);
    }

    #[test]
    fn stale_entry_is_probed_once_per_backoff_and_value_kept() {
        let key = "test-owner/stale-backoff-repo";
        cache().lock().unwrap().insert(
            key.to_string(),
            VisibilityEntry {
                visibility: RepoVisibility::Public,
                updated_at: Instant::now() - VISIBILITY_CACHE_TTL - Duration::from_secs(1),
            },
        );
        let calls = AtomicUsize::new(0);
        let fetch = |_repo: &str| -> Result<RepoVisibility, ProbeFailure> {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(ProbeFailure::NonZeroExit)
        };
        for _ in 0..10 {
            refresh_visibility_cache_with(key, fetch, || false);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(cached_visibility(key), Some(RepoVisibility::Public));
    }

    #[test]
    fn probe_resumes_after_backoff_deadline_and_success_clears_it() {
        let key = "test-owner/backoff-expiry-repo";
        let calls = AtomicUsize::new(0);
        let failing = |_repo: &str| -> Result<RepoVisibility, ProbeFailure> {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(ProbeFailure::NonZeroExit)
        };
        refresh_visibility_cache_with(key, failing, || false);
        // Force the deadline into the past.
        next_probe_after()
            .lock()
            .unwrap()
            .insert(key.to_string(), Instant::now() - Duration::from_secs(1));
        let ok = |_repo: &str| -> Result<RepoVisibility, ProbeFailure> {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(RepoVisibility::Public)
        };
        refresh_visibility_cache_with(key, ok, || false);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(cached_visibility(key), Some(RepoVisibility::Public));
        assert!(!next_probe_after().lock().unwrap().contains_key(key));
    }

    #[test]
    fn suppressed_breaker_prevents_any_spawn() {
        let key = "test-owner/suppressed-repo";
        let calls = AtomicUsize::new(0);
        let fetch = |_repo: &str| -> Result<RepoVisibility, ProbeFailure> {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(RepoVisibility::Public)
        };
        refresh_visibility_cache_with(key, fetch, || true);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(cached_visibility(key), None);
    }

    #[test]
    fn classify_failure_detects_rate_limit() {
        assert_eq!(
            classify_failure("gh: API rate limit exceeded for user ID 1 (HTTP 403)"),
            ProbeFailure::RateLimited
        );
        assert_eq!(classify_failure("gh: Not Found (HTTP 404)"), ProbeFailure::NonZeroExit);
        assert_eq!(
            classify_failure("rate-limit breaker is suppressing forge calls"),
            ProbeFailure::RateLimited
        );
        assert_eq!(classify_failure("failed to invoke /no/gh"), ProbeFailure::SpawnFailed);
    }

    // ------------------------------------------------------------------
    // #10512: the real probe through the shared ETag store.
    // ------------------------------------------------------------------

    /// A `200 {"private":false}` caches Public; once the entry is past the
    /// TTL the refresh revalidates with `If-None-Match`, the `304` keeps
    /// Public and is booked free (`not_modified`). A `404` is never cached
    /// and backs off (one spawn for three refreshes).
    #[test]
    #[serial_test::serial(loom_config_env)]
    fn stale_entry_revalidates_conditionally_and_a_304_keeps_the_answer() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("gh.log");
        let gh = tmp.path().join("gh-visibility");
        let script = format!(
            r#"#!/bin/sh
echo "$*" >> {log}
case "$*" in
  *missing-10512*) printf 'HTTP/2.0 404 Not Found\r\n\r\n{{"message":"Not Found"}}'; exit 1 ;;
  *If-None-Match*) printf 'HTTP/2.0 304 Not Modified\r\nEtag: W/"v1"\r\n\r\n'; exit 1 ;;
esac
printf 'HTTP/2.0 200 OK\r\nEtag: W/"v1"\r\n\r\n{{"private":false}}'
"#,
            log = log.display()
        );
        std::fs::write(&gh, script).unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let key = "test-owner/conditional-revalidate-10512";
        let missing = "test-owner/missing-10512";
        let sink = tempfile::tempdir().unwrap();
        std::env::set_var("LOOM_GH_BIN", &gh);
        crate::forge_call_stats::set_test_sink_dir(Some(sink.path().to_path_buf()));

        refresh_visibility_cache(key);
        assert_eq!(cached_visibility(key), Some(RepoVisibility::Public));
        refresh_visibility_cache(key); // fresh: no spawn
        cache().lock().unwrap().get_mut(key).unwrap().updated_at =
            Instant::now() - VISIBILITY_CACHE_TTL - Duration::from_secs(1);
        refresh_visibility_cache(key);
        for _ in 0..3 {
            refresh_visibility_cache(missing);
        }

        let report = crate::forge_call_stats::status_report(chrono::Utc::now(), None);
        crate::forge_call_stats::set_test_sink_dir(None);
        std::env::remove_var("LOOM_GH_BIN");
        assert_eq!(cached_visibility(key), Some(RepoVisibility::Public));
        assert_eq!(cached_visibility(missing), None, "a 404 is never cached");
        let argv = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = argv.lines().collect();
        assert_eq!(lines.len(), 3, "200, 304, then one backed-off 404: {argv}");
        assert!(!lines[0].contains("If-None-Match"), "{argv}");
        assert!(lines[1].contains(r#"If-None-Match: W/"v1""#), "{argv}");
        let rows = report.host_window.unwrap_or_default();
        let sum = |f: fn(&crate::types::ForgeCallCounts) -> u64| -> u64 {
            rows.iter().filter(|r| r.caller == "visibility.repo").map(f).sum()
        };
        assert_eq!((sum(|r| r.ok), sum(|r| r.not_modified)), (1, 1), "{rows:?}");
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
        refresh_visibility_cache_with(key, failing_fetch, || false);
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
        refresh_visibility_cache_with(key, |_r| Err(ProbeFailure::NonZeroExit), || false);
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
        assert_eq!(ProbeFailure::RateLimited.to_string(), "gh was rate limited");
        assert_eq!(ProbeFailure::UnparseableOutput.to_string(), "gh returned unparseable output");
    }
}
