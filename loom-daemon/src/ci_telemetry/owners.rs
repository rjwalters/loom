//! Several forge **owners** — organizations *and* user accounts — per poll
//! cycle (Issue #9188).
//!
//! Before #9188 the poller captured exactly one org (`org`, discovered with
//! `GET /orgs/{org}/repos`), so a user-owned repo such as `rjwalters/loom` was
//! never captured and no story trace for it could carry a `loom.ci.run` span.
//!
//! # Resolution (`owners` vs the deprecated `org` alias)
//!
//! Precedence is **env > config > default** across tiers, and within one tier
//! `owners` wins over `org`:
//!
//! 1. `LOOM_CI_TELEMETRY_OWNERS` (comma-separated), else
//!    `LOOM_CI_TELEMETRY_ORG` (deprecated single-owner alias);
//! 2. config `autonomous.ciTelemetry.owners` (a string list), else config
//!    `org` (deprecated alias);
//! 3. the default, `["2amlogic"]`.
//!
//! An owner named through the `org` alias (env or config, or the CLI `--org`)
//! or the default is a **declared organization**: the key has always meant
//! "an org" and was always discovered through `/orgs/`, so it is not probed —
//! an `org`-only config makes exactly the requests it made before #9188.
//!
//! # Kind resolution
//!
//! Every other owner's kind comes from `GET /users/{owner}` → `type`
//! (`Organization` | `User`), cached per process ([`global_kind_cache`]) — one
//! request per owner per daemon lifetime. A failed or unrecognised probe is
//! never guessed: that owner is skipped this cycle (named in the cycle's
//! errors) and probed again next cycle.
//!
//! # Discovery
//!
//! Organizations list `orgs/{o}/repos?per_page=100&type=all`; users list
//! `users/{u}/repos?per_page=100&type=owner` (public repos — the endpoint does
//! not list a user's private repos). Both are paginated and ETag-cached in the
//! one shared discovery cache, whose entries are keyed by request path, so each
//! owner's pages are replaced independently of every other owner's.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use super::api::{ApiError, GithubApi};
use super::records::RepoJson;
use super::state;

/// What `GET /users/{owner}` says an owner is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OwnerKind {
    Organization,
    User,
}

impl OwnerKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            OwnerKind::Organization => "organization",
            OwnerKind::User => "user",
        }
    }

    /// The first discovery page for `login`.
    #[must_use]
    pub fn discovery_path(self, login: &str) -> String {
        match self {
            OwnerKind::Organization => format!("orgs/{login}/repos?per_page=100&type=all"),
            OwnerKind::User => format!("users/{login}/repos?per_page=100&type=owner"),
        }
    }
}

/// One owner to poll.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Owner {
    pub login: String,
    /// `Some(Organization)` for an owner named through the `org` alias or the
    /// default — never probed. `None` means "probe `GET /users/{login}`".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub declared: Option<OwnerKind>,
}

impl Owner {
    /// A declared organization (the `org` alias, `--org`, or the default).
    #[must_use]
    pub fn org(login: &str) -> Self {
        Owner {
            login: login.to_string(),
            declared: Some(OwnerKind::Organization),
        }
    }

    /// An owner from `owners`, whose kind is probed.
    #[must_use]
    pub fn probed(login: &str) -> Self {
        Owner {
            login: login.to_string(),
            declared: None,
        }
    }
}

/// Split a comma-separated or listed set of logins: trimmed, empties dropped,
/// de-duplicated case-insensitively (first spelling wins).
#[must_use]
pub fn parse_logins<'a>(values: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut seen = HashSet::new();
    values
        .into_iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|login| !login.is_empty() && seen.insert(login.to_ascii_lowercase()))
        .map(str::to_string)
        .collect()
}

/// Which tier and key produced the resolved owners (for `status` and the
/// deprecation warning).
pub const SOURCE_ENV_OWNERS: &str = "env LOOM_CI_TELEMETRY_OWNERS";
pub const SOURCE_ENV_ORG: &str = "env LOOM_CI_TELEMETRY_ORG (deprecated alias)";
pub const SOURCE_CONFIG_OWNERS: &str = "config autonomous.ciTelemetry.owners";
pub const SOURCE_CONFIG_ORG: &str = "config autonomous.ciTelemetry.org (deprecated alias)";
pub const SOURCE_DEFAULT: &str = "default";

/// Resolve the owner list: env > config > default, `owners` over `org` within
/// a tier. An empty `owners` value at a tier counts as unset there.
#[must_use]
pub fn resolve_owners(
    env_owners: Option<&str>,
    env_org: Option<&str>,
    config_owners: Option<&[String]>,
    config_org: Option<&str>,
    default_org: &str,
) -> (Vec<Owner>, &'static str) {
    let probed = |logins: Vec<String>| logins.iter().map(|l| Owner::probed(l)).collect();
    if let Some(logins) = env_owners
        .map(|v| parse_logins([v]))
        .filter(|l| !l.is_empty())
    {
        return (probed(logins), SOURCE_ENV_OWNERS);
    }
    if let Some(org) = env_org.map(str::trim).filter(|o| !o.is_empty()) {
        return (vec![Owner::org(org)], SOURCE_ENV_ORG);
    }
    if let Some(logins) = config_owners
        .map(|list| parse_logins(list.iter().map(String::as_str)))
        .filter(|l| !l.is_empty())
    {
        return (probed(logins), SOURCE_CONFIG_OWNERS);
    }
    if let Some(org) = config_org.map(str::trim).filter(|o| !o.is_empty()) {
        return (vec![Owner::org(org)], SOURCE_CONFIG_ORG);
    }
    (vec![Owner::org(default_org)], SOURCE_DEFAULT)
}

/// Resolved owner kinds, keyed by lowercased login.
pub type KindCache = Mutex<HashMap<String, OwnerKind>>;

/// The process-wide kind cache: one `GET /users/{owner}` per owner per daemon
/// lifetime. Only successes are cached, so a failed probe retries next cycle.
#[must_use]
pub fn global_kind_cache() -> Arc<KindCache> {
    static CACHE: OnceLock<Arc<KindCache>> = OnceLock::new();
    CACHE.get_or_init(Arc::default).clone()
}

/// A GitHub login: `[A-Za-z0-9-]`, 1–39 characters. Checked before a login is
/// interpolated into a request path.
fn valid_login(login: &str) -> bool {
    !login.is_empty()
        && login.len() <= 39
        && login.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

#[derive(Deserialize)]
struct UserJson {
    #[serde(rename = "type")]
    kind: String,
}

/// `owner`'s kind: declared, cached, or probed via `GET /users/{login}`.
/// Never guessed — any failure is returned for the caller to skip the owner.
pub fn resolve_kind(
    api: &dyn GithubApi,
    owner: &Owner,
    cache: &KindCache,
    requests: &mut usize,
) -> Result<OwnerKind, ApiError> {
    if let Some(kind) = owner.declared {
        return Ok(kind);
    }
    let key = owner.login.to_ascii_lowercase();
    if let Some(kind) = cache.lock().ok().and_then(|c| c.get(&key).copied()) {
        return Ok(kind);
    }
    let path = format!("users/{}", owner.login);
    if !valid_login(&owner.login) {
        return Err(ApiError::Parse {
            path,
            detail: "not a valid GitHub login".to_string(),
        });
    }
    *requests += 1;
    let response = api.get(&path, None)?;
    let parsed: UserJson = serde_json::from_str(&response.body).map_err(|e| ApiError::Parse {
        path: path.clone(),
        detail: e.to_string(),
    })?;
    let kind = match parsed.kind.as_str() {
        "Organization" => OwnerKind::Organization,
        "User" => OwnerKind::User,
        other => {
            return Err(ApiError::Parse {
                path,
                detail: format!("unsupported owner type {other:?} (expected Organization or User)"),
            })
        }
    };
    if let Ok(mut cache) = cache.lock() {
        cache.insert(key, kind);
    }
    Ok(kind)
}

/// One owner's outcome in the last cycle, as `status` reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerStatus {
    pub owner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<OwnerKind>,
    /// Discovered repos eligible for polling (not archived, not excluded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repos: Option<usize>,
    /// Why the owner was skipped this cycle (kind probe or discovery failed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Discover `login`'s repos, paginated, with the persisted per-page ETag
/// cache: a `304` serves the cached page (and its cached `next`) at zero
/// rate-limit cost — the `forge_listing` ETag mechanism, never a raw
/// re-listing. Only this owner's cached pages are replaced; every other
/// owner's entries are kept.
pub fn discover(
    api: &dyn GithubApi,
    login: &str,
    kind: OwnerKind,
    dir: &Path,
    requests: &mut usize,
) -> Result<Vec<RepoJson>, ApiError> {
    let mut cache = state::load_discovery_cache(dir);
    let first = kind.discovery_path(login);
    let base = first.split('?').next().unwrap_or_default().to_string();
    let mut refreshed = state::DiscoveryCache::new();
    let mut repos = Vec::new();
    let mut visited = HashSet::new();
    let mut next = Some(first);
    while let Some(path) = next.take() {
        if !visited.insert(path.clone()) {
            break;
        }
        let cached = cache.get(&path).cloned();
        *requests += 1;
        let mut response = api.get(&path, cached.as_ref().map(|c| c.etag.as_str()))?;
        let page = match (response.status, cached) {
            (304, Some(page)) => page,
            (304, None) => {
                // Unreachable in practice (a 304 only answers our ETag);
                // re-fetch unconditionally rather than trust an empty body.
                *requests += 1;
                response = api.get(&path, None)?;
                cached_page(&response)
            }
            _ => cached_page(&response),
        };
        let rows: Vec<RepoJson> =
            serde_json::from_str(&page.body).map_err(|e| ApiError::Parse {
                path: path.clone(),
                detail: e.to_string(),
            })?;
        repos.extend(rows);
        next = page.next.clone();
        if !page.etag.is_empty() {
            refreshed.insert(path, page);
        }
    }
    cache.retain(|path, _| path.split('?').next() != Some(base.as_str()));
    cache.extend(refreshed);
    if let Err(error) = state::save_discovery_cache(dir, &cache) {
        log::warn!("ci_telemetry: could not persist the discovery ETag cache: {error}");
    }
    Ok(repos)
}

fn cached_page(response: &super::api::ApiResponse) -> state::CachedPage {
    state::CachedPage {
        etag: response.etag.clone().unwrap_or_default(),
        body: response.body.clone(),
        next: response.next.clone(),
    }
}
