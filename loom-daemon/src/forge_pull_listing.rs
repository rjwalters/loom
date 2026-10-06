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
//!   PR's `GET repos/{o}/{r}/pulls/{n}` ([`pull_state_cached_as`], which pairs it
//!   with that response's `head.sha`).
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
//!
//! The listing uses a fixed set of keys (one per page), but the per-PR reads
//! add keys per distinct PR ever checked. Those entries store only a reduced
//! body — `{"mergeable", "head_sha"}` under the `pull-` file prefix, the changed-file
//! names under `files-` ([`pull_files_cached_as`], #10382; never the patches)
//! — and both layers drop them [`PULL_ENTRY_MAX_AGE`] after their last `200`,
//! the same bound as `forge_cached_view`'s `prune_stale`.
//!
//! [`open_pulls_for_head_as`] is the by-head lookup (`pulls?head=owner:branch`,
//! #10382) — a plain REST read: its callers are rare, and a per-branch ETag
//! key would grow the cache without bound.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Context, Result};

use crate::forge_call_stats::{ops, ForgeOp};
use crate::forge_etag_store as store;

/// Rows per page — GitHub's REST maximum.
pub const PER_PAGE: usize = 100;

/// Per-PR mergeable entries whose last `200` is older than this are pruned
/// (disk and memory) on the next per-PR `200`. Without it both layers grow
/// by one entry per distinct PR ever read; an evicted entry costs one `200`.
pub const PULL_ENTRY_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Disk filename prefix of the per-PR entries (the listing uses `listing-`).
const PULL_PREFIX: &str = "pull-";

/// Disk filename prefix of the per-PR changed-file pages (#10382).
const FILES_PREFIX: &str = "files-";

/// `GET pulls/{n}/files` stops at 3000 files — 30 pages of [`PER_PAGE`]. A walk
/// that fills every page cannot tell "exactly 3000" from "truncated", so it
/// is an error: a silently truncated set could hide an overlap.
pub const MAX_FILE_PAGES: usize = 30;

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
    /// The PR description (#10514: the open-PR guard's `Closes #N` filter).
    pub body: Option<String>,
    /// `author_association` (`OWNER` / `MEMBER` / `NONE` …), for the H14 rule.
    pub author_association: Option<String>,
    /// `user.type == "Bot"` — an App author.
    pub author_is_bot: bool,
    /// `head.repo.full_name` — `None` when GitHub omits it (a deleted fork).
    pub head_repo: Option<String>,
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

/// `GET pulls?state=open` — the `pr.list-open` inventory row (#10382).
const PR_LIST_OPEN: ForgeOp = ops::PR_LIST_OPEN;

/// Every open PR of the repo `cwd` resolves to (or `repo_override` /
/// `LOOM_REPO`), up to `max_pages` pages of [`PER_PAGE`], newest first, each
/// PR number once. Each page is its own conditional read; paging stops at
/// the first short page.
///
/// Pages are read one after another, not atomically (#10382, the
/// [`crate::forge_listing::list_issues_cached_all_as`] rule): after a
/// multi-page walk every earlier page is revalidated (a free `304` when
/// nothing moved), and any change is an error rather than a set that may
/// have lost or doubled a PR across a page boundary. Rows are also deduped
/// by number (first occurrence wins). A single-page walk makes no extra read.
///
/// Every forge call is recorded against `caller` in
/// [`crate::forge_call_stats`] (#9251). Errors carry the `gh` stderr tail so
/// [`crate::rate_limit_breaker`]'s classifier sees rate-limit text unchanged.
///
/// # Errors
/// A page failed (after the #6171 one-time credential-refresh retry on a
/// 404), `max_pages` full pages were read (the listing is incomplete — the
/// `pr.list-open` row is `complete-required`), or the listing moved mid-walk.
pub fn list_open_pulls_cached_as(
    caller: &'static str,
    gh_bin: &Path,
    cwd: Option<&Path>,
    repo_override: Option<&str>,
    max_pages: usize,
) -> Result<Vec<RestPull>> {
    list_open_pulls_cached_within(caller, gh_bin, cwd, repo_override, max_pages, None)
}

/// [`list_open_pulls_cached_as`] with every page read bounded by `timeout`
/// (`None` = the conditional-read default). The open-PR dispatch guard reads
/// it under its `reap_gh_timeout` bound (#10514), which a wedged `gh` must not
/// outlast.
///
/// # Errors
/// As [`list_open_pulls_cached_as`]; a timed-out page is an error.
pub fn list_open_pulls_cached_within(
    caller: &'static str,
    gh_bin: &Path,
    cwd: Option<&Path>,
    repo_override: Option<&str>,
    max_pages: usize,
    timeout: Option<Duration>,
) -> Result<Vec<RestPull>> {
    let refresh = crate::credential_preflight::force_refresh_owner_credential;
    let site = store::ConditionalRead::new(caller, PR_LIST_OPEN).within(timeout);
    list_open_pulls_at(site, gh_bin, cwd, repo_override, max_pages, &refresh)
}

/// [`list_open_pulls_cached_as`] with the #6171 credential refresh injected
/// (tests cannot drive the process-global primary-workspace mint).
#[cfg(test)]
fn list_open_pulls_with_refresh(
    caller: &'static str,
    gh_bin: &Path,
    cwd: Option<&Path>,
    repo_override: Option<&str>,
    max_pages: usize,
    refresh: &dyn Fn(&Path) -> bool,
) -> Result<Vec<RestPull>> {
    let site = store::ConditionalRead::new(caller, PR_LIST_OPEN);
    list_open_pulls_at(site, gh_bin, cwd, repo_override, max_pages, refresh)
}

/// The listing walk behind [`list_open_pulls_cached_within`], each page read
/// as `site`.
fn list_open_pulls_at(
    site: store::ConditionalRead,
    gh_bin: &Path,
    cwd: Option<&Path>,
    repo_override: Option<&str>,
    max_pages: usize,
    refresh: &dyn Fn(&Path) -> bool,
) -> Result<Vec<RestPull>> {
    let target = resolve(cwd, repo_override);
    let read = |page: usize| -> Result<Vec<RestPull>> {
        let url = build_pulls_url(target.repo.as_deref(), page);
        let get = || {
            conditional_get(site, gh_bin, cwd, &target, &url, Kind::Listing)
        };
        let body = retry_404_once(cwd, &url, refresh, get)?;
        parse_rest_pulls(&body).with_context(|| format!("parse REST pulls JSON from {url}"))
    };
    let max_pages = max_pages.max(1);
    let mut pages: Vec<Vec<RestPull>> = Vec::new();
    for page in 1..=max_pages {
        let rows = read(page)?;
        let full = rows.len() >= PER_PAGE;
        pages.push(rows);
        if !full {
            for (i, earlier) in pages[..pages.len() - 1].iter().enumerate() {
                if read(i + 1)? != *earlier {
                    return Err(anyhow!(
                        "forge_pull_listing: the open-PR listing changed mid-walk (page {} \
                         moved); the set is not a consistent snapshot",
                        i + 1
                    ));
                }
            }
            let mut seen = std::collections::HashSet::new();
            return Ok(pages
                .into_iter()
                .flatten()
                .filter(|r| seen.insert(r.number))
                .collect());
        }
    }
    Err(anyhow!(
        "forge_pull_listing: more than {} open PRs; the listing is incomplete",
        max_pages * PER_PAGE
    ))
}

/// `get()`, retried exactly once after a forced credential refresh when a
/// registered workspace (`Some(cwd)`) sees an HTTP 404 (#6171 — a per-owner
/// App token minted before the repo was registered). Any other failure, a
/// `None` cwd, or a refresh that does nothing returns the first error.
fn retry_404_once(
    cwd: Option<&Path>,
    url: &str,
    refresh: &dyn Fn(&Path) -> bool,
    get: impl Fn() -> Result<String>,
) -> Result<String> {
    let err = match get() {
        Ok(body) => return Ok(body),
        Err(e) => e,
    };
    if let Some(root) = cwd {
        if crate::forge_listing::is_404_error(&err.to_string()) && refresh(root) {
            log::info!(
                "forge_pull_listing: retrying {url} in {} after a forced per-owner credential \
                 refresh (#6171)",
                root.display()
            );
            return get();
        }
    }
    Err(err)
}

/// One PR's state from a single-PR read, `mergeable` paired with the
/// `head.sha` GitHub computed it for (#10382): a listing row's head may be
/// older or newer than the per-PR read, so a verdict must name this one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PullState {
    /// `true` / `false`, or `None` while GitHub is still computing it.
    pub mergeable: Option<bool>,
    /// `head.sha` of the same response.
    pub head_sha: Option<String>,
}

/// One PR's [`PullState`] via a conditional `GET repos/{o}/{r}/pulls/{number}`.
/// The ETag covers the whole body, so a recomputed mergeability (e.g. after
/// the base branch moved, which does not bump `updated_at`) or a pushed head
/// is a fresh `200`, never a stale `304`.
pub fn pull_state_cached_as(
    caller: &'static str,
    gh_bin: &Path,
    cwd: Option<&Path>,
    repo_override: Option<&str>,
    number: u32,
) -> Result<PullState> {
    let target = resolve(cwd, repo_override);
    let repo_path = target.repo.as_deref().unwrap_or("{owner}/{repo}");
    let url = format!("repos/{repo_path}/pulls/{number}");
    let site = store::ConditionalRead::new(caller, ops::PR_VIEW_STATE);
    let body = conditional_get(site, gh_bin, cwd, &target, &url, Kind::Pull)?;
    parse_pull_state(&body).with_context(|| format!("parse REST pull JSON from {url}"))
}

/// The changed-file paths of PR `number`, via conditional
/// `GET repos/{o}/{r}/pulls/{number}/files` pages (#10382: was the GraphQL
/// `gh pr view --json files`). Each page is its own ETag'd read, so an
/// unchanged PR re-reads as free `304`s; paging stops at the first short page.
///
/// # Errors
/// Any failed or unparseable page, or a walk that fills all
/// [`MAX_FILE_PAGES`] (GitHub's cap — the set may be truncated).
pub fn pull_files_cached_as(
    caller: &'static str,
    gh_bin: &Path,
    cwd: Option<&Path>,
    repo_override: Option<&str>,
    number: u32,
) -> Result<BTreeSet<String>> {
    let target = resolve(cwd, repo_override);
    let repo_path = target.repo.as_deref().unwrap_or("{owner}/{repo}");
    let mut files = BTreeSet::new();
    for page in 1..=MAX_FILE_PAGES {
        let url = format!("repos/{repo_path}/pulls/{number}/files?per_page={PER_PAGE}&page={page}");
        let site = store::ConditionalRead::new(caller, ops::PR_DIFF_AND_FILES);
        let body = conditional_get(site, gh_bin, cwd, &target, &url, Kind::Files)?;
        let batch =
            parse_files(&body).with_context(|| format!("parse REST files JSON from {url}"))?;
        let full = batch.len() >= PER_PAGE;
        files.extend(batch);
        if !full {
            return Ok(files);
        }
    }
    Err(anyhow!(
        "forge_pull_listing: PR #{number} fills all {MAX_FILE_PAGES} pages of changed files \
         (GitHub's cap); the file set may be truncated"
    ))
}

/// The file names of one `GET pulls/{n}/files` page — the REST objects'
/// `filename`, or the reduced name array a cached entry stores.
///
/// # Errors
/// Malformed JSON.
pub fn parse_files(body: &str) -> Result<Vec<String>> {
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Row {
        Name(String),
        File { filename: String },
    }
    let rows: Vec<Row> = serde_json::from_str(body.trim())?;
    Ok(rows
        .into_iter()
        .map(|r| match r {
            Row::Name(n) | Row::File { filename: n } => n,
        })
        .collect())
}

/// The open PRs whose head is `branch` in the repo `cwd` resolves to (or
/// `repo_override` / `LOOM_REPO`), via `GET pulls?head=owner:branch&state=open`
/// (#10382: was the GraphQL `gh pr list --head`). Rows are re-filtered on
/// `head.ref == branch` and `state == "open"`: GitHub ignores a `head` it
/// cannot resolve and returns every open PR, which must not read as a match.
///
/// # Errors
/// A failed or unparseable read — distinct from `Ok(vec![])` ("confirmed no
/// open PR"), which an inconclusive read must never collapse into (#7863).
pub fn open_pulls_for_head_as(
    caller: &'static str,
    gh_bin: &Path,
    cwd: Option<&Path>,
    repo_override: Option<&str>,
    branch: &str,
) -> Result<Vec<u32>> {
    let target = resolve(cwd, repo_override);
    let repo_path = target.repo.as_deref().unwrap_or("{owner}/{repo}");
    let owner = repo_path.split('/').next().unwrap_or("{owner}");
    // `head=` first: a distinct shape from the open listing's `pulls?state=open`.
    let url =
        format!("repos/{repo_path}/pulls?head={owner}:{branch}&state=open&per_page={PER_PAGE}");
    let site = store::ConditionalRead::new(caller, ops::PR_LIST_BY_HEAD);
    let (status, response, stderr) =
        store::fetch_conditional(site, gh_bin, cwd, &target, &url, None)?;
    match response {
        Some(r) if r.status == 200 && status.success() => Ok(parse_rest_pulls(&r.body)
            .with_context(|| format!("parse REST pulls JSON from {url}"))?
            .into_iter()
            .filter(|p| p.state == "open" && p.head_ref.as_deref() == Some(branch))
            .map(|p| p.number)
            .collect()),
        _ => Err(anyhow!("gh api {url} failed: {stderr}")),
    }
}

/// The [`PullState`] of one `GET pulls/{n}` body — or of the reduced
/// `{"mergeable", "head_sha"}` body a cached entry stores. An absent or
/// `null` field is `None`.
///
/// # Errors
/// Malformed JSON.
pub fn parse_pull_state(body: &str) -> Result<PullState> {
    #[derive(serde::Deserialize)]
    struct RawHead {
        #[serde(default)]
        sha: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct Raw {
        #[serde(default)]
        mergeable: Option<bool>,
        #[serde(default)]
        head: Option<RawHead>,
        #[serde(default)]
        head_sha: Option<String>,
    }
    let raw: Raw = serde_json::from_str(body.trim())?;
    Ok(PullState {
        mergeable: raw.mergeable,
        head_sha: raw.head.and_then(|h| h.sha).or(raw.head_sha),
    })
}

/// Is `body` a per-PR entry in the current reduced shape? Entries written
/// before #10382 hold only `{"mergeable"}`: presenting their ETag would `304`
/// forever with no head, so they are treated as a cache miss.
fn is_current_pull_entry(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .is_some_and(|v| v.get("head_sha").is_some())
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
    #[derive(serde::Deserialize)]
    struct RawRepo {
        #[serde(default)]
        full_name: Option<String>,
    }
    #[derive(serde::Deserialize, Default)]
    struct RawRef {
        #[serde(default, rename = "ref")]
        ref_name: Option<String>,
        #[serde(default)]
        sha: Option<String>,
        #[serde(default)]
        repo: Option<RawRepo>,
    }
    #[derive(serde::Deserialize)]
    struct RawUser {
        #[serde(default)]
        login: Option<String>,
        #[serde(default, rename = "type")]
        kind: Option<String>,
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
        #[serde(default)]
        body: Option<String>,
        #[serde(default)]
        author_association: Option<String>,
    }
    let rows: Vec<RawPull> = serde_json::from_str(body.trim())?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let head = r.head.unwrap_or_default();
            let author_is_bot = r
                .user
                .as_ref()
                .is_some_and(|u| u.kind.as_deref() == Some("Bot"));
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
                body: r.body,
                author_association: r.author_association,
                author_is_bot,
                head_repo: head.repo.and_then(|repo| repo.full_name),
            }
        })
        .collect())
}

fn resolve(cwd: Option<&Path>, repo_override: Option<&str>) -> store::Target {
    let env_repo = std::env::var("LOOM_REPO").ok();
    // #9252: the URL names the SAME resolved repo the key does.
    store::resolve_target(cwd, repo_override.or(env_repo.as_deref()))
}

/// Which cache an entry belongs to: the bounded-key listing pages, or the
/// per-PR reads (reduced body, age-pruned) — mergeability or changed files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Listing,
    Pull,
    Files,
}

/// The disk prefixes of the age-pruned per-PR entries.
const PER_PR_PREFIXES: [&str; 2] = [PULL_PREFIX, FILES_PREFIX];

fn is_per_pr_file(name: &str) -> bool {
    PER_PR_PREFIXES.iter().any(|p| name.starts_with(p))
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
    kind: Kind,
) -> Result<String> {
    let cache_key = store::daemon_cache_key(cwd, target, url);
    let disk_dir = store::daemon_store_dir();
    let disk_path = disk_dir.as_deref().map(|d| match kind {
        Kind::Listing => store::entry_path_in(d, &cache_key),
        Kind::Pull => store::entry_path_with_prefix(d, PULL_PREFIX, &cache_key),
        Kind::Files => store::entry_path_with_prefix(d, FILES_PREFIX, &cache_key),
    });
    let sent = cached_entry(&cache_key, disk_path.as_deref())
        .filter(|e| kind != Kind::Pull || is_current_pull_entry(&e.body));
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
                // A per-PR entry keeps only what a 304 must reproduce.
                let stored = match kind {
                    Kind::Listing => Some(r.body.clone()),
                    Kind::Pull => parse_pull_state(&r.body).ok().map(|s| {
                        serde_json::json!({ "mergeable": s.mergeable, "head_sha": s.head_sha })
                            .to_string()
                    }),
                    Kind::Files => parse_files(&r.body)
                        .ok()
                        .map(|f| serde_json::json!(f).to_string()),
                };
                if let Some(stored) = stored {
                    if let Some(path) = &disk_path {
                        let entry = store::DiskEntry {
                            etag: etag.clone(),
                            body: stored.clone(),
                        };
                        store::write_disk_entry(path, &entry);
                    }
                    if let Ok(mut guard) = cache().lock() {
                        let entry = CacheEntry {
                            etag,
                            body: Arc::new(stored),
                            pull_written: (kind != Kind::Listing).then(SystemTime::now),
                        };
                        guard.insert(cache_key, entry);
                    }
                }
                if kind != Kind::Listing {
                    prune_stale_pulls(disk_dir.as_deref(), SystemTime::now());
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
    /// When a per-PR (`pull-` / `files-`) entry's last `200` was stored (`None` for listing pages,
    /// which are never pruned).
    pull_written: Option<SystemTime>,
}

/// Drop per-PR entries whose last `200` is more than [`PULL_ENTRY_MAX_AGE`]
/// before `now`: hot-layer entries by their stored time, disk entries (only
/// `pull-` / `files-` files — listing pages share the directory) by mtime.
fn prune_stale_pulls(disk_dir: Option<&Path>, now: SystemTime) {
    let stale = |t: SystemTime| now.duration_since(t).is_ok_and(|a| a > PULL_ENTRY_MAX_AGE);
    if let Ok(mut guard) = cache().lock() {
        guard.retain(|_, e| !e.pull_written.is_some_and(stale));
    }
    let Some(Ok(rd)) = disk_dir.map(std::fs::read_dir) else {
        return;
    };
    for entry in rd.filter_map(std::result::Result::ok) {
        let name = entry.file_name();
        let is_pull = name
            .to_str()
            .is_some_and(|n| is_per_pr_file(n) && n.ends_with(".json"));
        let old = || entry.metadata().and_then(|m| m.modified()).is_ok_and(stale);
        if is_pull && old() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The `(etag, body)` pair to present for `key`: the hot layer, else a
/// read-through of the disk entry (promoted into memory).
fn cached_entry(key: &str, disk: Option<&Path>) -> Option<CacheEntry> {
    if let Some(entry) = cache().lock().ok()?.get(key).cloned() {
        return Some(entry);
    }
    let disk = disk?;
    let disk_entry = store::read_disk_entry(disk)?;
    let is_pull = disk
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(is_per_pr_file);
    // A promoted per-PR entry keeps its disk age, so promotion never resets it.
    let written = std::fs::metadata(disk).and_then(|m| m.modified()).ok();
    let entry = CacheEntry {
        etag: disk_entry.etag,
        body: Arc::new(disk_entry.body),
        pull_written: if is_pull {
            written.or_else(|| Some(SystemTime::now()))
        } else {
            None
        },
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
