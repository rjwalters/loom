//! ETag-cached REST issue listing (Issue #4428).
//!
//! The daemon's hot polling loops (work-finder every 60s per workspace, epic
//! supervisor every 300s, claim reconciliation every 600s) re-fetch issue
//! lists that change rarely relative to their cadence. They previously went
//! through `gh issue list` — **GraphQL**, which has no conditional-request
//! mechanism, so every poll burned shared budget even when nothing changed
//! (the 2026-07-29 exhaustion, #4429/#4432).
//!
//! The REST issues endpoint supports conditional requests: a `GET` with
//! `If-None-Match: <etag>` that matches returns **304 Not Modified at zero
//! rate-limit cost** (verified live: `x-ratelimit-remaining` unchanged across
//! a 304). This module wraps `gh api` — keeping the daemon's zero-HTTP-client
//! house style (`forge_cmd`) — with a process-lifetime ETag cache:
//!
//! - First fetch: `200` → parse, cache `(etag, issues)`, return.
//! - Subsequent fetches: send `If-None-Match`; `304` → serve the cached
//!   parse (free); `200` → something changed, re-cache, return.
//!
//! One deliberate semantic difference from `gh issue list`: REST
//! `/repos/{owner}/{repo}/issues` returns **pull requests too** (marked with
//! a `pull_request` key). [`RestIssue::is_pull_request`] carries the marker
//! and every converted call site filters on it, so consumers see the same
//! issue-only (or PR-only) sets as before.
//!
//! Scope: single page, `per_page=100` (the old `gh issue list --limit`
//! ceilings were 100–200; a repo with >100 simultaneously-labeled items is
//! far outside normal operation). A full page logs a truncation warning
//! rather than silently capping.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use anyhow::{anyhow, Context, Result};

use crate::forge_call_stats::RateLimitHeaders;
use crate::forge_etag_store as store;

/// One page's worth — matches GitHub's REST maximum and brackets the old
/// `gh issue list --limit` values (100–200) used by the converted call sites.
pub const PER_PAGE: usize = 100;

/// An issue (or PR — see [`Self::is_pull_request`]) as returned by the REST
/// `/repos/{owner}/{repo}/issues` listing, reduced to the union of fields the
/// daemon's polling loops actually consume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestIssue {
    pub number: u32,
    /// Issue/PR title. Added for the agent-facing cached-listing surface
    /// (#5056), which projects it into `gh issue list --json title` parity.
    pub title: Option<String>,
    /// Label names (flattened from the REST label objects).
    pub labels: Vec<String>,
    /// RFC-3339 creation timestamp. Lexicographic order == chronological.
    pub created_at: Option<String>,
    /// RFC-3339 last-update timestamp.
    pub updated_at: Option<String>,
    /// RFC-3339 close timestamp (`null` while open). Projected into
    /// `gh … --json closedAt` parity for the #5056 cached surface.
    pub closed_at: Option<String>,
    /// `"open"` / `"closed"` (REST lowercase; compare case-insensitively —
    /// GraphQL-era consumers saw `"OPEN"`).
    pub state: String,
    pub body: Option<String>,
    /// Author login (`user.login` from REST), for `gh … --json author` parity.
    pub author: Option<String>,
    /// Present when the row is actually a pull request (REST issue listings
    /// include PRs). Call sites filter on this to keep pre-#4428 semantics.
    pub is_pull_request: bool,
}

/// List open/closed issues carrying `label`, via the ETag cache.
///
/// - `cwd`: the workspace root `gh` resolves `{owner}/{repo}` placeholders
///   from (its `git remote`). `None` uses the daemon's own cwd.
/// - `repo_override`: an explicit `owner/repo` (from a `--repo`-style caller
///   knob); when absent, the `LOOM_REPO` env var is honored — the same
///   precedence the previous `gh issue list` call sites applied.
/// - `state`: `"open"`, `"closed"`, or `"all"` (REST accepts all three).
///
/// Errors carry the `gh` stderr tail so [`crate::rate_limit_breaker`]'s
/// classifier sees rate-limit text unchanged. Calls are attributed to the
/// generic `forge_listing` caller; daemon loops use [`list_issues_cached_as`].
pub fn list_issues_cached(
    gh_bin: &Path,
    cwd: Option<&Path>,
    repo_override: Option<&str>,
    label: &str,
    state: &str,
) -> Result<Vec<RestIssue>> {
    list_issues_cached_as("forge_listing", gh_bin, cwd, repo_override, label, state)
}

/// [`list_issues_cached`], with every forge call recorded against `caller` in
/// [`crate::forge_call_stats`] (#9251).
pub fn list_issues_cached_as(
    caller: &'static str,
    gh_bin: &Path,
    cwd: Option<&Path>,
    repo_override: Option<&str>,
    label: &str,
    state: &str,
) -> Result<Vec<RestIssue>> {
    match list_issues_cached_once(caller, gh_bin, cwd, repo_override, label, state) {
        Ok(issues) => Ok(issues),
        Err(e) => {
            // #6171: a 404 from a *registered* workspace (a `Some(cwd)` — an
            // ad hoc call with no checkout root has nothing to refresh) can
            // mean the per-owner GitHub App installation token was minted
            // before this repo was registered with the daemon (or before it
            // was added to the org's installation) — see
            // `credential_preflight`'s "Hot-apply for a newly registered
            // workspace" module docs. Force exactly one fresh mint + retry
            // before treating this as a real scan failure; a repeat 404
            // after the refresh is real (AC2).
            if let Some(root) = cwd {
                if is_404_error(&e.to_string())
                    && crate::credential_preflight::force_refresh_owner_credential(root)
                {
                    log::info!(
                        "forge_listing: retrying {} in {} after a forced per-owner credential \
                         refresh (#6171)",
                        build_issues_url(repo_override, label, state),
                        root.display()
                    );
                    return list_issues_cached_once(
                        caller,
                        gh_bin,
                        cwd,
                        repo_override,
                        label,
                        state,
                    );
                }
            }
            Err(e)
        }
    }
}

/// True when `error_message` (the `Display` of a [`list_issues_cached_once`]
/// error, which carries the raw `gh` stderr tail) indicates the request
/// failed with an HTTP 404 — the signature of a per-owner GitHub App
/// installation token whose mint predates a just-registered repo (#6171).
/// Deliberately string-matched rather than a numeric field: callers here only
/// ever see `gh`'s own formatted diagnostic (`gh: Not Found (HTTP 404)`), not
/// a structured status code.
fn is_404_error(error_message: &str) -> bool {
    error_message.contains("HTTP 404")
}

/// One unconditional attempt at the ETag-cached REST listing — the pre-#6171
/// body of [`list_issues_cached`], split out so the public function can retry
/// it exactly once after a forced credential refresh.
///
/// Keyed by resolved identity ([`store::daemon_cache_key`], #9252) and backed
/// by the shared disk store, so an ETag survives a daemon restart. Unlike the
/// agent path this trusts every `200` (no #7451 shrink guard): each claim
/// shrinks the `loom:issue` listing, and re-serving the larger prior listing
/// could re-offer a just-claimed issue.
fn list_issues_cached_once(
    caller: &'static str,
    gh_bin: &Path,
    cwd: Option<&Path>,
    repo_override: Option<&str>,
    label: &str,
    state: &str,
) -> Result<Vec<RestIssue>> {
    let env_repo = std::env::var("LOOM_REPO").ok();
    let repo = repo_override.or(env_repo.as_deref());
    let url = build_issues_url(repo, label, state);
    let cache_key = store::daemon_cache_key(cwd, repo, &url);
    let disk_path = store::daemon_store_dir().map(|d| store::entry_path_in(&d, &cache_key));
    let cached_etag = cached_etag(&cache_key, disk_path.as_deref());

    let (status, response, stderr) =
        store::fetch_conditional(caller, gh_bin, cwd, &url, cached_etag.as_deref())?;

    match response {
        Some(ref r) if r.status == 304 => {
            // Free cache hit (a 304 does not count against the rate limit).
            if let Ok(guard) = cache().lock() {
                if let Some(entry) = guard.get(&cache_key) {
                    log::debug!("forge_listing: 304 cache hit for {url}");
                    return Ok(entry.issues.clone());
                }
            }
            // A 304 can only happen because WE sent the etag, so the entry
            // existing is an invariant; if it is somehow gone, drop the etag
            // path and error so the next call re-fetches unconditionally.
            if let Ok(mut guard) = cache().lock() {
                guard.remove(&cache_key);
            }
            if let Some(path) = &disk_path {
                let _ = std::fs::remove_file(path);
            }
            Err(anyhow!(
                "forge_listing: 304 for {url} but the cache entry vanished; will re-fetch"
            ))
        }
        Some(ref r) if r.status == 200 && status.success() => {
            let issues = parse_rest_issues(&r.body)
                .with_context(|| format!("parse REST issues JSON from {url}"))?;
            if issues.len() >= PER_PAGE {
                log::warn!(
                    "forge_listing: {url} returned a full page ({PER_PAGE}); the listing may be \
                     truncated — items beyond the first page are not seen this poll"
                );
            }
            if let Some(etag) = r.etag.clone() {
                if let Some(path) = &disk_path {
                    let entry = store::DiskEntry {
                        etag: etag.clone(),
                        body: r.body.clone(),
                    };
                    store::write_disk_entry(path, &entry);
                }
                if let Ok(mut guard) = cache().lock() {
                    let issues = issues.clone();
                    guard.insert(cache_key, CacheEntry { etag, issues });
                }
            }
            Ok(issues)
        }
        _ => Err(anyhow!(
            "gh api {url} failed{}: {stderr}",
            cwd.map(|d| format!(" in {}", d.display()))
                .unwrap_or_default(),
        )),
    }
}

/// The ETag to present for `key`: the in-memory hot layer, else a read-through
/// of the disk entry at `disk` (parsed once and promoted into memory, so a
/// steady-state `304` never re-reads or re-parses the file).
fn cached_etag(key: &str, disk: Option<&Path>) -> Option<String> {
    if let Some(etag) = cache().lock().ok()?.get(key).map(|e| e.etag.clone()) {
        return Some(etag);
    }
    let entry = store::read_disk_entry(disk?)?;
    let issues = parse_rest_issues(&entry.body).ok()?;
    let etag = entry.etag;
    if let Ok(mut guard) = cache().lock() {
        guard.insert(
            key.to_string(),
            CacheEntry {
                etag: etag.clone(),
                issues,
            },
        );
    }
    Some(etag)
}

/// Build the REST listing URL. With no explicit repo, gh's
/// `{owner}/{repo}` placeholders resolve from the `cwd` repo's remote.
#[must_use]
pub fn build_issues_url(repo: Option<&str>, label: &str, state: &str) -> String {
    let repo_path = repo.unwrap_or("{owner}/{repo}");
    format!("repos/{repo_path}/issues?labels={label}&state={state}&per_page={PER_PAGE}")
}

/// A parsed `gh api --include` response: the status from the first
/// `HTTP/x.y NNN` line, the `ETag` header, and the body (everything past the
/// first blank line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub etag: Option<String>,
    pub body: String,
    /// Free `x-ratelimit-*` headers (#9251), sent on `200`s and `304`s alike.
    pub ratelimit: RateLimitHeaders,
}

/// Parse `gh api --include` output. Returns `None` when the first line is not
/// an HTTP status line (e.g. gh failed before issuing the request).
#[must_use]
pub fn parse_http_response(raw: &str) -> Option<HttpResponse> {
    let mut lines = raw.lines();
    let status_line = lines.next()?;
    let mut parts = status_line.split_whitespace();
    let proto = parts.next()?;
    if !proto.starts_with("HTTP/") {
        return None;
    }
    let status: u16 = parts.next()?.parse().ok()?;

    let mut etag = None;
    let mut ratelimit = RateLimitHeaders::default();
    for line in lines {
        let trimmed = line.trim_end_matches('\r');
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.eq_ignore_ascii_case("etag") {
                etag = Some(value.trim().to_string());
            } else {
                ratelimit.absorb(name, value);
            }
        }
    }
    // Locate the header/body boundary directly on the raw text rather than
    // reconstructing the header block's length line-by-line: `str::lines()`
    // strips a `\r\n` terminator as ONE line ending, so a per-line
    // `len() + 1` sum undercounts every CRLF-terminated header by a byte
    // and corrupts the body slice on any real (~20-header) response — the
    // #4443-review bug. Searching for the blank-line separator is
    // terminator-width-agnostic; the earliest match wins so a CRLF header
    // block is never mis-split by a later LF-only sequence in the body.
    let body = ["\r\n\r\n", "\n\n"]
        .iter()
        .filter_map(|sep| raw.find(sep).map(|idx| (idx, idx + sep.len())))
        .min_by_key(|&(idx, _)| idx)
        .map(|(_, body_start)| raw[body_start..].to_string())
        .unwrap_or_default();
    Some(HttpResponse {
        status,
        etag,
        body,
        ratelimit,
    })
}

/// Parse a REST issues-listing body into [`RestIssue`]s.
#[must_use = "parse failures must surface as listing errors"]
pub fn parse_rest_issues(body: &str) -> Result<Vec<RestIssue>> {
    #[derive(serde::Deserialize)]
    struct RawLabel {
        name: String,
    }
    #[derive(serde::Deserialize)]
    struct RawUser {
        #[serde(default)]
        login: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct RawIssue {
        number: u32,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        labels: Vec<RawLabel>,
        #[serde(default)]
        created_at: Option<String>,
        #[serde(default)]
        updated_at: Option<String>,
        #[serde(default)]
        closed_at: Option<String>,
        #[serde(default)]
        state: String,
        #[serde(default)]
        body: Option<String>,
        #[serde(default)]
        user: Option<RawUser>,
        #[serde(default)]
        pull_request: Option<serde_json::Value>,
    }
    let rows: Vec<RawIssue> = serde_json::from_str(body.trim())?;
    Ok(rows
        .into_iter()
        .map(|r| RestIssue {
            number: r.number,
            title: r.title,
            labels: r.labels.into_iter().map(|l| l.name).collect(),
            created_at: r.created_at,
            updated_at: r.updated_at,
            closed_at: r.closed_at,
            state: r.state,
            body: r.body,
            author: r.user.and_then(|u| u.login),
            is_pull_request: r.pull_request.is_some(),
        })
        .collect())
}

// ============================================================================
// Process-lifetime cache
// ============================================================================

#[derive(Debug, Clone)]
struct CacheEntry {
    etag: String,
    issues: Vec<RestIssue>,
}

/// Process-global hot layer of the ETag cache, keyed by resolved identity
/// ([`store::daemon_cache_key`]: repo + host + `gh` credential — since #5401
/// credentials are per-root, not per-process). Backed by the shared disk
/// store (#9252) so a daemon restart does not re-pay a `200` per listing.
fn cache() -> &'static Mutex<HashMap<String, CacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

// ============================================================================
// Disk-persistent cache (Issue #5056 — agent-facing cached listing surface)
// ============================================================================
//
// The [`cache`] above serves the daemon's own long-running polling loops,
// with an in-memory hot layer over the disk store. Agent role prompts, by
// contrast, run each `gh issue list` as a **fresh, short-lived process** — an
// in-process cache would be born empty and die after one fetch, so it could
// never serve the "second and subsequent readers" the way the daemon does. To give agents the same zero-cost-on-`304` win, the
// ETag and the last-good body are persisted to a small per-host on-disk store,
// keyed the same way. A second CLI invocation (even in a different process, for
// a different label) reads the prior ETag, sends `If-None-Match`, and — when
// nothing changed — gets a **free** `304` and serves the body from disk.
//
// This is deliberately the *same* ETag/REST/304 mechanism as the in-process
// cache, not a second cache with different semantics: it reuses
// [`build_issues_url`], [`parse_http_response`], and [`parse_rest_issues`], and
// since #9252 the same key and store ([`crate::forge_etag_store`]) as the
// daemon's own cache, which is now durable too. The one semantic difference is
// the #7451 shrink guard below, which only this agent path applies.

/// Result of a disk-cached listing: the parsed rows plus whether the single
/// REST page was full (so the caller can decline rather than silently serve a
/// truncated set — see [`PER_PAGE`]).
#[derive(Debug, Clone)]
pub struct CachedListing {
    pub issues: Vec<RestIssue>,
    /// `true` when the response filled a whole page and may be truncated.
    pub truncated: bool,
}

/// List issues carrying `label` (comma-joined AND when multiple) in `state`,
/// via the **disk-persistent** ETag cache — the agent-facing analogue of
/// [`list_issues_cached`].
///
/// Semantics match [`list_issues_cached`] (conditional `GET`, `304` served
/// free, `200` re-cached) but the ETag/body survive process exit, so the
/// second short-lived CLI process on a host pays zero rate-limit cost when the
/// queue is unchanged. Returns [`CachedListing`] so the caller can decline on a
/// possibly-truncated full page rather than serve a partial set.
///
/// # Shrink guard (#7451)
///
/// A single `200` response whose parsed item count is *lower* than what is
/// already cached for this exact `(repo, label, state)` key is corroborated
/// with one extra unconditional re-fetch before it is trusted and persisted.
/// This exists because a bare "trust every 200" policy, combined with the
/// disk entry being durable and shared across every short-lived CLI caller on
/// the host, turns a single transient/inconsistent read from GitHub's issues
/// listing endpoint (observed live as a clean alternation between the correct
/// result set and an empty one on consecutive `--label loom:epic --state
/// open` calls with no intervening label mutation — #7451) into a durably
/// wrong answer served for free to *every* caller via the next `304`, with no
/// error and no way to tell from the output alone. A shrink that the
/// immediate re-fetch confirms (e.g. an issue was genuinely un-labeled or
/// closed between reads) still goes through normally — this only filters a
/// single-request disagreement, it never blocks a corroborated change.
pub fn list_issues_cached_persistent(
    gh_bin: &Path,
    cwd: Option<&Path>,
    repo_override: Option<&str>,
    label: &str,
    state: &str,
) -> Result<CachedListing> {
    list_issues_cached_persistent_as("persistent_listing", gh_bin, cwd, repo_override, label, state)
}

/// [`list_issues_cached_persistent`], with every forge call recorded against
/// `caller` in [`crate::forge_call_stats`] (#9251).
pub fn list_issues_cached_persistent_as(
    caller: &'static str,
    gh_bin: &Path,
    cwd: Option<&Path>,
    repo_override: Option<&str>,
    label: &str,
    state: &str,
) -> Result<CachedListing> {
    let env_repo = std::env::var("LOOM_REPO").ok();
    let repo = repo_override.or(env_repo.as_deref());
    let url = build_issues_url(repo, label, state);
    // #9252: the same resolved-identity key the daemon's listing cache uses,
    // so the two share one on-disk entry per (repo, host, credential, query).
    let entry_path = store::disk_cache_path(&store::cache_key(cwd, repo, &url));
    let prior = store::read_disk_entry(&entry_path);
    let prior_etag = prior.as_ref().map(|e| e.etag.as_str());

    let (status, response, stderr) =
        store::fetch_conditional(caller, gh_bin, cwd, &url, prior_etag)?;

    match response {
        Some(ref r) if r.status == 304 => match prior {
            Some(e) => {
                let issues = parse_rest_issues(&e.body)
                    .with_context(|| format!("parse cached REST issues for {url}"))?;
                let truncated = issues.len() >= PER_PAGE;
                Ok(CachedListing { issues, truncated })
            }
            None => {
                // A 304 can only occur because we sent an ETag; if the entry
                // vanished under us, drop it and error so the next call
                // re-fetches unconditionally.
                let _ = std::fs::remove_file(&entry_path);
                Err(anyhow!(
                    "forge_listing: 304 for {url} but the on-disk entry vanished; will re-fetch"
                ))
            }
        },
        Some(ref r) if r.status == 200 && status.success() => {
            let issues = parse_rest_issues(&r.body)
                .with_context(|| format!("parse REST issues JSON from {url}"))?;
            let truncated = issues.len() >= PER_PAGE;

            // Shrink guard: does this response disagree — downward — with
            // what we already had cached? If so, corroborate with one
            // unconditional re-fetch before accepting it (see doc comment).
            if let Some(prior_issues) = prior.as_ref().and_then(|p| parse_rest_issues(&p.body).ok())
            {
                if issues.len() < prior_issues.len() {
                    let confirmed = matches!(
                        store::fetch_conditional(caller, gh_bin, cwd, &url, None),
                        Ok((confirm_status, Some(ref cr), _))
                            if confirm_status.success()
                                && cr.status == 200
                                && parse_rest_issues(&cr.body)
                                    .map(|v| v.len())
                                    .unwrap_or(usize::MAX)
                                    == issues.len()
                    );
                    if !confirmed {
                        log::warn!(
                            "forge_listing: {url} shrank from {} to {} item(s) on a single \
                             read; an immediate re-fetch did not agree — keeping the prior \
                             cached listing rather than trusting a possibly-transient read \
                             (#7451)",
                            prior_issues.len(),
                            issues.len()
                        );
                        let prior_truncated = prior_issues.len() >= PER_PAGE;
                        return Ok(CachedListing {
                            issues: prior_issues,
                            truncated: prior_truncated,
                        });
                    }
                }
            }

            if let Some(etag) = r.etag.clone() {
                store::write_disk_entry(
                    &entry_path,
                    &store::DiskEntry {
                        etag,
                        body: r.body.clone(),
                    },
                );
            }
            Ok(CachedListing { issues, truncated })
        }
        _ => Err(anyhow!(
            "gh api {url} failed{}: {}",
            cwd.map(|d| format!(" in {}", d.display()))
                .unwrap_or_default(),
            stderr
        )),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "forge_listing_tests.rs"]
mod tests;
