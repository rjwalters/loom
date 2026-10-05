//! ETag-cached REST open-PR listing and per-PR mergeability read (#10349).
//!
//! The claim-reconciliation pass family used to list PRs with `gh pr list`
//! — **GraphQL**, which has no conditional-request mechanism, so every tick
//! of every workspace billed the shared GraphQL budget even when nothing had
//! changed (five listings per workspace per pass). This module is the PR
//! analogue of [`crate::forge_listing`]'s issue listing:
//!
//! - `GET repos/{o}/{r}/pulls?state=open` carries everything those listings
//!   read — number, draft, created/updated timestamps, labels, head ref, head
//!   sha, base ref — except `mergeable`, which GitHub only returns on a single
//!   PR's `GET repos/{o}/{r}/pulls/{n}` ([`pull_mergeable_cached_as`]).
//! - Every request is conditional on the ETag of the last `200` for the same
//!   resolved key ([`store::daemon_cache_key`], #9252): an unchanged page or
//!   PR is a `304`, free against the REST rate limit. The ETag and body live
//!   in the shared disk store (an ETag survives a daemon restart) behind an
//!   in-memory hot layer, exactly like [`crate::forge_listing`].
//!
//! The REST pulls listing has no label filter, so callers filter client-side
//! — the same shape the review-conflict / merge-sequence passes already used.
//!
//! Like the daemon's issue listing this trusts every `200` (no #7451 shrink
//! guard): a just-relabelled PR must drop out of the next pass's view.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{anyhow, Context, Result};

use crate::forge_call_stats::{ops, ForgeOp};
use crate::forge_etag_store as store;

/// Rows per page — GitHub's REST maximum.
pub const PER_PAGE: usize = 100;

/// An open PR as returned by `GET repos/{o}/{r}/pulls`, reduced to the fields
/// the daemon's reconciliation passes consume.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RestPull {
    pub number: u32,
    /// `"open"` / `"closed"` (REST lowercase).
    pub state: String,
    pub draft: bool,
    pub title: Option<String>,
    /// RFC-3339 creation timestamp. Lexicographic order == chronological.
    pub created_at: Option<String>,
    /// RFC-3339 last-update timestamp.
    pub updated_at: Option<String>,
    /// Author login (`user.login`).
    pub author: Option<String>,
    /// Label names (flattened from the REST label objects).
    pub labels: Vec<String>,
    /// `head.ref` — the PR's branch name.
    pub head_ref: Option<String>,
    /// `head.sha` — the PR's current head commit.
    pub head_sha: Option<String>,
    /// `base.ref` — the branch the PR targets.
    pub base_ref: Option<String>,
}

impl RestPull {
    /// Does this PR carry `label`?
    #[must_use]
    pub fn has_label(&self, label: &str) -> bool {
        self.labels.iter().any(|l| l == label)
    }
}

/// The page-`page` URL of the open-PR listing, newest first — the same order
/// `gh pr list` returns, so a capped listing keeps the newest PRs.
#[must_use]
pub fn build_pulls_url(repo: Option<&str>, page: usize) -> String {
    let repo_path = repo.unwrap_or("{owner}/{repo}");
    format!(
        "repos/{repo_path}/pulls?state=open&sort=created&direction=desc&per_page={PER_PAGE}\
         &page={page}"
    )
}

/// `GET pulls?state=open` has no inventory row yet (the inventory's PR
/// discovery row is by-head only) — the same marking `pr_planning`'s queue
/// listing uses (#9831).
const PR_LIST_OPEN: ForgeOp = ForgeOp::uninventoried("open-PR listing has no inventory row");

/// Every open PR of the repo `cwd` resolves to (or `repo_override` /
/// `LOOM_REPO`), up to `max_pages` pages of [`PER_PAGE`], newest first. Each
/// page is its own conditional read; paging stops at the first short page.
/// A capped listing whose last page is full logs a truncation warning.
///
/// Every forge call is recorded against `caller` in
/// [`crate::forge_call_stats`] (#9251). Errors carry the `gh` stderr tail so
/// [`crate::rate_limit_breaker`]'s classifier sees rate-limit text unchanged.
pub fn list_open_pulls_cached_as(
    caller: &'static str,
    gh_bin: &Path,
    cwd: Option<&Path>,
    repo_override: Option<&str>,
    max_pages: usize,
) -> Result<Vec<RestPull>> {
    let target = resolve(cwd, repo_override);
    let mut rows = Vec::new();
    for page in 1..=max_pages.max(1) {
        let url = build_pulls_url(target.repo.as_deref(), page);
        let site = store::ConditionalRead::new(caller, PR_LIST_OPEN);
        let body = conditional_get(site, gh_bin, cwd, &target, &url)?;
        let batch =
            parse_rest_pulls(&body).with_context(|| format!("parse REST pulls JSON from {url}"))?;
        let full = batch.len() >= PER_PAGE;
        rows.extend(batch);
        if !full {
            return Ok(rows);
        }
    }
    log::warn!(
        "forge_pull_listing: the open-PR listing filled {max_pages} page(s) of {PER_PAGE}; PRs \
         beyond them are not seen this poll"
    );
    Ok(rows)
}

/// One PR's REST `mergeable` (`true` / `false`, or `None` while GitHub is
/// still computing it — "no information"), via a conditional
/// `GET repos/{o}/{r}/pulls/{number}`. The ETag covers the whole body, so a
/// recomputed mergeability (e.g. after the base branch moved, which does not
/// bump `updated_at`) is a fresh `200`, never a stale `304`.
pub fn pull_mergeable_cached_as(
    caller: &'static str,
    gh_bin: &Path,
    cwd: Option<&Path>,
    repo_override: Option<&str>,
    number: u32,
) -> Result<Option<bool>> {
    let target = resolve(cwd, repo_override);
    let repo_path = target.repo.as_deref().unwrap_or("{owner}/{repo}");
    let url = format!("repos/{repo_path}/pulls/{number}");
    let site = store::ConditionalRead::new(caller, ops::PR_VIEW_STATE);
    let body = conditional_get(site, gh_bin, cwd, &target, &url)?;
    parse_mergeable(&body).with_context(|| format!("parse REST pull JSON from {url}"))
}

/// The `mergeable` field of one `GET pulls/{n}` body (absent or `null` ⇒
/// `None`).
///
/// # Errors
/// Malformed JSON.
pub fn parse_mergeable(body: &str) -> Result<Option<bool>> {
    #[derive(serde::Deserialize)]
    struct Raw {
        #[serde(default)]
        mergeable: Option<bool>,
    }
    let raw: Raw = serde_json::from_str(body.trim())?;
    Ok(raw.mergeable)
}

/// Parse a REST pulls-listing body. Lenient: a missing `head` / `base` /
/// `labels` parses to empty values rather than failing the whole listing.
///
/// # Errors
/// Malformed JSON.
pub fn parse_rest_pulls(body: &str) -> Result<Vec<RestPull>> {
    #[derive(serde::Deserialize)]
    struct RawLabel {
        name: String,
    }
    #[derive(serde::Deserialize, Default)]
    struct RawRef {
        #[serde(default, rename = "ref")]
        ref_name: Option<String>,
        #[serde(default)]
        sha: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct RawUser {
        #[serde(default)]
        login: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct RawPull {
        number: u32,
        #[serde(default)]
        state: String,
        #[serde(default)]
        draft: Option<bool>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        created_at: Option<String>,
        #[serde(default)]
        updated_at: Option<String>,
        #[serde(default)]
        user: Option<RawUser>,
        #[serde(default)]
        labels: Vec<RawLabel>,
        #[serde(default)]
        head: Option<RawRef>,
        #[serde(default)]
        base: Option<RawRef>,
    }
    let rows: Vec<RawPull> = serde_json::from_str(body.trim())?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let head = r.head.unwrap_or_default();
            RestPull {
                number: r.number,
                state: r.state,
                draft: r.draft.unwrap_or(false),
                title: r.title,
                created_at: r.created_at,
                updated_at: r.updated_at,
                author: r.user.and_then(|u| u.login),
                labels: r.labels.into_iter().map(|l| l.name).collect(),
                head_ref: head.ref_name,
                head_sha: head.sha,
                base_ref: r.base.and_then(|b| b.ref_name),
            }
        })
        .collect())
}

fn resolve(cwd: Option<&Path>, repo_override: Option<&str>) -> store::Target {
    let env_repo = std::env::var("LOOM_REPO").ok();
    // #9252: the URL names the SAME resolved repo the key does.
    store::resolve_target(cwd, repo_override.or(env_repo.as_deref()))
}

/// One conditional `GET url`: the body of the last `200` on a `304`, else the
/// fresh `200` body (cached when it carries an ETag). Mirrors
/// `forge_listing::list_issues_cached_once`'s flow, including the rule that a
/// `304` may only serve the body stored with the ETag it validated.
fn conditional_get(
    site: store::ConditionalRead,
    gh_bin: &Path,
    cwd: Option<&Path>,
    target: &store::Target,
    url: &str,
) -> Result<String> {
    let cache_key = store::daemon_cache_key(cwd, target, url);
    let disk_path = store::daemon_store_dir().map(|d| store::entry_path_in(&d, &cache_key));
    let sent = cached_entry(&cache_key, disk_path.as_deref());
    let sent_etag = sent.as_ref().map(|e| e.etag.as_str());
    let (status, response, stderr) =
        store::fetch_conditional(site, gh_bin, cwd, target, url, sent_etag)?;
    match response {
        // gh exits 1 on a 304, so this arm precedes any exit-status check.
        Some(ref r) if r.status == 304 => {
            if let Some(entry) = sent {
                log::debug!("forge_pull_listing: 304 cache hit for {url}");
                return Ok(entry.body.as_ref().clone());
            }
            if let Ok(mut guard) = cache().lock() {
                guard.remove(&cache_key);
            }
            if let Some(path) = &disk_path {
                let _ = std::fs::remove_file(path);
            }
            Err(anyhow!("forge_pull_listing: 304 for {url} but the cache entry vanished"))
        }
        Some(ref r) if r.status == 200 && status.success() => {
            if let Some(etag) = r.etag.clone() {
                if let Some(path) = &disk_path {
                    let entry = store::DiskEntry {
                        etag: etag.clone(),
                        body: r.body.clone(),
                    };
                    store::write_disk_entry(path, &entry);
                }
                if let Ok(mut guard) = cache().lock() {
                    let body = Arc::new(r.body.clone());
                    guard.insert(cache_key, CacheEntry { etag, body });
                }
            }
            Ok(r.body.clone())
        }
        _ => Err(anyhow!(
            "gh api {url} failed{}: {stderr}",
            cwd.map(|d| format!(" in {}", d.display()))
                .unwrap_or_default(),
        )),
    }
}

#[derive(Debug, Clone)]
struct CacheEntry {
    etag: String,
    body: Arc<String>,
}

/// The `(etag, body)` pair to present for `key`: the hot layer, else a
/// read-through of the disk entry (promoted into memory).
fn cached_entry(key: &str, disk: Option<&Path>) -> Option<CacheEntry> {
    if let Some(entry) = cache().lock().ok()?.get(key).cloned() {
        return Some(entry);
    }
    let disk_entry = store::read_disk_entry(disk?)?;
    let entry = CacheEntry {
        etag: disk_entry.etag,
        body: Arc::new(disk_entry.body),
    };
    if let Ok(mut guard) = cache().lock() {
        guard.insert(key.to_string(), entry.clone());
    }
    Some(entry)
}

/// Process-global hot layer, keyed like [`crate::forge_listing`]'s.
fn cache() -> &'static Mutex<HashMap<String, CacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "forge_pull_listing_tests.rs"]
mod tests;
