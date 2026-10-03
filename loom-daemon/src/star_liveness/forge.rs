//! The forge surface the liveness pass and the intent applier use, as a
//! trait so every decision is testable against a fake ([`StarForge`]), and
//! its production implementation over REST `gh api` ([`GhStarForge`]).
//!
//! Every read (label listing, single issue, comment pages, issue search) is a
//! conditional GET through the shared ETag store ([`crate::forge_etag_store`]):
//! an unchanged answer is a free `304`, and the request runs under the repo's
//! reader App installation credential (writer fallback) exactly like the work
//! finder's own reads (#9949). Writes (and the one-off `GET /user`) go through
//! the single [`GhStarForge::api`] helper under the per-owner App credential
//! (`credential_preflight`). Every call honors the rate-limit breaker.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};

use crate::forge_etag_store as store;
use crate::forge_listing::RestIssue;

/// One issue comment: its body, when it was posted and last edited, and who
/// wrote it (for [`super::trust`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct ForgeComment {
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub created_at: Option<String>,
    /// The forge-assigned edit time (a lease renewal PATCHes its comment).
    #[serde(default)]
    pub updated_at: Option<String>,
    /// `user.login`.
    #[serde(default)]
    pub author: Option<String>,
    /// `author_association` (`OWNER`, `MEMBER`, `COLLABORATOR`, …).
    #[serde(default)]
    pub author_association: Option<String>,
}

/// One issue-search hit and who filed it (for [`super::trust`]: an outsider
/// quoting a refusal phrase must not be taken for the incident).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub issue: RestIssue,
    /// `author_association` of the issue's author.
    pub author_association: Option<String>,
}

/// First retry delay after a failed own-login lookup; doubles per failure.
pub const LOGIN_RETRY_BASE: Duration = Duration::from_secs(60);
/// Longest delay between own-login retries.
pub const LOGIN_RETRY_MAX: Duration = Duration::from_secs(3600);

/// The daemon's own forge login, looked up lazily: a success is kept for the
/// process, a failure (transient, or an App token the forge cannot name) is
/// retried with exponential backoff instead of being cached forever.
#[derive(Debug, Default, Clone)]
pub struct LoginLookup {
    known: Option<String>,
    failures: u32,
    retry_at: Option<Instant>,
}

impl LoginLookup {
    /// The login, calling `fetch` only when none is known and no backoff is
    /// pending.
    pub fn get(&mut self, now: Instant, fetch: impl FnOnce() -> Option<String>) -> Option<String> {
        if self.known.is_some() || self.retry_at.is_some_and(|at| now < at) {
            return self.known.clone();
        }
        match fetch()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            Some(login) => {
                self.known = Some(login);
                self.failures = 0;
                self.retry_at = None;
            }
            None => {
                let delay = LOGIN_RETRY_BASE
                    .saturating_mul(1 << self.failures.min(10))
                    .min(LOGIN_RETRY_MAX);
                self.failures = self.failures.saturating_add(1);
                self.retry_at = Some(now + delay);
            }
        }
        self.known.clone()
    }
}

/// What the liveness pass and the intent applier need from one repo.
pub trait StarForge {
    /// Open issues **and PRs** carrying `label`.
    ///
    /// # Errors
    /// The listing failed.
    fn list_open(&mut self, label: &str) -> Result<Vec<RestIssue>>;

    /// One issue or PR by number (`None` when it does not exist).
    ///
    /// # Errors
    /// The read failed.
    fn issue(&mut self, number: u32) -> Result<Option<RestIssue>>;

    /// Every comment on an issue or PR, oldest first.
    ///
    /// # Errors
    /// The read failed.
    fn comments(&mut self, number: u32) -> Result<Vec<ForgeComment>>;

    /// Open **issues** (not PRs) whose title or body contains `phrase`, for
    /// finding the incident behind a merge refusal, with each author's
    /// association. Callers re-check the phrase and the author: a forge
    /// search is fuzzy, and anyone can file an issue.
    ///
    /// # Errors
    /// The search failed.
    fn search_open_issues(&mut self, phrase: &str) -> Result<Vec<SearchHit>>;

    /// This daemon's own forge login, when the forge can name it (an App
    /// installation token cannot). Used only to trust its own markers.
    fn self_login(&mut self) -> Option<String> {
        None
    }

    /// Add `label` (a no-op when already present).
    ///
    /// # Errors
    /// The write failed.
    fn add_label(&mut self, number: u32, label: &str) -> Result<()>;

    /// Remove `label` (a no-op when absent).
    ///
    /// # Errors
    /// The write failed.
    fn remove_label(&mut self, number: u32, label: &str) -> Result<()>;

    /// Post a comment.
    ///
    /// # Errors
    /// The write failed.
    fn post_comment(&mut self, number: u32, body: &str) -> Result<()>;
}

/// [`StarForge`] over `gh api`, for the repo checked out at `root` whose
/// forge slug is `slug`.
pub struct GhStarForge {
    pub gh_bin: PathBuf,
    pub root: PathBuf,
    pub slug: String,
}

impl GhStarForge {
    #[must_use]
    pub fn new(root: &Path, slug: &str) -> Self {
        Self {
            gh_bin: PathBuf::from("gh"),
            root: root.to_path_buf(),
            slug: slug.to_string(),
        }
    }

    fn api(&self, args: &[&str], context: &str) -> Result<String> {
        if crate::rate_limit_breaker::global_is_suppressed() {
            return Err(anyhow!("rate-limit breaker is suppressing forge calls"));
        }
        let mut cmd = Command::new(&self.gh_bin);
        cmd.arg("api").args(args).current_dir(&self.root);
        crate::credential_preflight::apply_gh_config_for_cwd(&mut cmd, Some(&self.root));
        let out = cmd.output()?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            crate::rate_limit_breaker::global_observe_failure(&stderr, "star_liveness");
            return Err(anyhow!("gh api {context} failed: {stderr}"));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn issue_path(&self, number: u32) -> String {
        format!("repos/{}/issues/{number}", self.slug)
    }

    /// Conditional GET of `url` (a REST path): `Ok(Some(body))` on a `200` or
    /// a `304` served from the stored body, `Ok(None)` on a `404`.
    fn cached_get(&self, url: &str) -> Result<Option<String>> {
        if crate::rate_limit_breaker::global_is_suppressed() {
            return Err(anyhow!("rate-limit breaker is suppressing forge calls"));
        }
        let cwd = Some(self.root.as_path());
        let target = store::resolve_target(cwd, Some(&self.slug));
        let key = store::daemon_cache_key(cwd, &target, url);
        let disk =
            store::daemon_store_dir().map(|d| store::entry_path_with_prefix(&d, "star-", &key));
        let sent = mem_cache()
            .lock()
            .ok()
            .and_then(|m| m.get(&key).cloned())
            .or_else(|| {
                let e = store::read_disk_entry(disk.as_deref()?)?;
                Some(Arc::new(e))
            });
        let sent_etag = sent.as_ref().map(|e| e.etag.as_str());
        let (status, response, stderr) =
            store::fetch_conditional("star_liveness", &self.gh_bin, cwd, &target, url, sent_etag)?;
        match response {
            Some(r) if r.status == 304 => match sent {
                Some(e) => Ok(Some(e.body.clone())),
                None => {
                    // A 304 with nothing sent is anomalous: drop and re-fetch.
                    if let Ok(mut m) = mem_cache().lock() {
                        m.remove(&key);
                    }
                    if let Some(p) = &disk {
                        let _ = std::fs::remove_file(p);
                    }
                    Err(anyhow!("gh api {url}: 304 but the cache entry vanished"))
                }
            },
            Some(r) if r.status == 200 && status.success() => {
                if let Some(etag) = r.etag.clone() {
                    let entry = store::DiskEntry {
                        etag,
                        body: r.body.clone(),
                    };
                    if let Some(p) = &disk {
                        store::write_disk_entry(p, &entry);
                    }
                    if let Ok(mut m) = mem_cache().lock() {
                        m.insert(key, Arc::new(entry));
                    }
                }
                Ok(Some(r.body))
            }
            Some(r) if r.status == 404 => Ok(None),
            _ => {
                crate::rate_limit_breaker::global_observe_failure(&stderr, "star_liveness");
                Err(anyhow!("gh api {url} failed: {stderr}"))
            }
        }
    }
}

fn mem_cache() -> &'static Mutex<HashMap<String, Arc<store::DiskEntry>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<store::DiskEntry>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Percent-encode a query value (unreserved characters pass through).
fn url_encode(s: &str) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Comment pages are fetched until one is short; a hard cap bounds a runaway.
const COMMENT_PAGE: usize = 100;
const COMMENT_MAX_PAGES: usize = 50;

#[derive(serde::Deserialize)]
struct RawUser {
    login: Option<String>,
}

#[derive(serde::Deserialize)]
struct RawComment {
    #[serde(default)]
    body: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
    user: Option<RawUser>,
    author_association: Option<String>,
}

impl StarForge for GhStarForge {
    fn list_open(&mut self, label: &str) -> Result<Vec<RestIssue>> {
        crate::forge_listing::list_issues_cached_as(
            "star_liveness",
            &self.gh_bin,
            Some(&self.root),
            Some(&self.slug),
            label,
            "open",
        )
    }

    fn issue(&mut self, number: u32) -> Result<Option<RestIssue>> {
        let Some(body) = self.cached_get(&self.issue_path(number))? else {
            return Ok(None);
        };
        Ok(crate::forge_listing::parse_rest_issues(&format!("[{body}]"))?
            .into_iter()
            .next())
    }

    fn comments(&mut self, number: u32) -> Result<Vec<ForgeComment>> {
        let mut all = Vec::new();
        for page in 1..=COMMENT_MAX_PAGES {
            let url =
                format!("{}/comments?per_page={COMMENT_PAGE}&page={page}", self.issue_path(number));
            let Some(body) = self.cached_get(&url)? else {
                return Err(anyhow!("gh api {url} failed: HTTP 404"));
            };
            let raw: Vec<RawComment> = serde_json::from_str(&body)
                .map_err(|e| anyhow!("parse comments page {page} of {url}: {e}"))?;
            let n = raw.len();
            all.extend(raw.into_iter().map(|c| ForgeComment {
                body: c.body.unwrap_or_default(),
                created_at: c.created_at,
                updated_at: c.updated_at,
                author: c.user.and_then(|u| u.login),
                author_association: c.author_association,
            }));
            if n < COMMENT_PAGE {
                break;
            }
        }
        Ok(all)
    }

    fn search_open_issues(&mut self, phrase: &str) -> Result<Vec<SearchHit>> {
        let clean: String = phrase.chars().filter(|c| *c != '"').collect();
        let q = format!("\"{clean}\" repo:{} is:issue is:open in:title,body", self.slug);
        let url = format!("search/issues?q={}&per_page=20", url_encode(&q));
        let Some(body) = self.cached_get(&url)? else {
            return Ok(Vec::new());
        };
        let items = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("items").cloned())
            .unwrap_or(serde_json::Value::Array(Vec::new()));
        let out = items.to_string();
        let assoc: HashMap<u64, String> = items
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| {
                let n = v.get("number")?.as_u64()?;
                Some((n, v.get("author_association")?.as_str()?.to_string()))
            })
            .collect();
        Ok(crate::forge_listing::parse_rest_issues(&out)?
            .into_iter()
            .filter(|i| !i.is_pull_request && i.state.eq_ignore_ascii_case("open"))
            .map(|issue| SearchHit {
                author_association: assoc.get(&u64::from(issue.number)).cloned(),
                issue,
            })
            .collect())
    }

    fn self_login(&mut self) -> Option<String> {
        static CACHE: OnceLock<Mutex<HashMap<PathBuf, LoginLookup>>> = OnceLock::new();
        // Registers the configured fleet App with `trust` for this process.
        let app = super::trust::configured_app_slug(&self.root);
        let mut cache = CACHE
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let lookup = cache.entry(self.root.clone()).or_default();
        // An App installation token cannot name itself (`GET /user` is a
        // 403): the configured App slug is then this daemon's identity.
        lookup
            .get(Instant::now(), || self.api(&["user", "--jq", ".login"], "user").ok())
            .or_else(|| app.map(|a| format!("{a}[bot]")))
    }

    fn add_label(&mut self, number: u32, label: &str) -> Result<()> {
        let path = format!("{}/labels", self.issue_path(number));
        let field = format!("labels[]={label}");
        self.api(&["-X", "POST", &path, "-f", &field], &path)
            .map(|_| ())
    }

    fn remove_label(&mut self, number: u32, label: &str) -> Result<()> {
        let path = format!("{}/labels/{label}", self.issue_path(number));
        match self.api(&["-X", "DELETE", &path], &path) {
            Err(e) if e.to_string().contains("HTTP 404") => Ok(()),
            other => other.map(|_| ()),
        }
    }

    fn post_comment(&mut self, number: u32, body: &str) -> Result<()> {
        let path = format!("{}/comments", self.issue_path(number));
        let field = format!("body={body}");
        self.api(&["-X", "POST", &path, "-f", &field], &path)
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_failed_login_lookup_is_retried_with_backoff_and_a_success_is_kept() {
        let mut l = LoginLookup::default();
        let t0 = Instant::now();
        let mut calls = 0;
        let mut fetch = |v: Option<&str>| {
            calls += 1;
            v.map(str::to_string)
        };
        assert_eq!(l.get(t0, || fetch(None)), None);
        assert_eq!(l.get(t0 + Duration::from_secs(30), || fetch(None)), None);
        l.get(t0 + LOGIN_RETRY_BASE, || fetch(None));
        // The second failure doubled the delay: no call at +2 base.
        l.get(t0 + LOGIN_RETRY_BASE * 2, || fetch(Some("early")));
        let got = l.get(t0 + LOGIN_RETRY_BASE * 3, || fetch(Some(" robb-bot ")));
        assert_eq!(got.as_deref(), Some("robb-bot"));
        let got = l.get(t0 + LOGIN_RETRY_MAX * 9, || fetch(None));
        assert_eq!(got.as_deref(), Some("robb-bot"), "a success is kept");
        assert_eq!(calls, 3, "one call per elapsed backoff, never a permanent failure");
        let mut capped = LoginLookup {
            failures: 40,
            ..LoginLookup::default()
        };
        capped.get(t0, || None);
        assert_eq!(capped.retry_at, Some(t0 + LOGIN_RETRY_MAX));
    }

    /// Fake `gh`: 200 + ETag unconditionally; 304 (exit 1, like real gh) when
    /// `If-None-Match: W/"v1"` is presented. Every argv is logged.
    #[cfg(unix)]
    fn fake_gh(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-gh.sh");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\ncase \"$*\" in\n  *'If-None-Match: W/\"v1\"'*)\n    printf 'HTTP/2.0 304 Not Modified\\r\\n\\r\\n'; echo 'gh: Not Modified (HTTP 304)' 1>&2; exit 1;;\n  *)\n    printf 'HTTP/2.0 200 OK\\r\\nEtag: W/\"v1\"\\r\\n\\r\\n'; printf '%s' '{body}';;\nesac\n",
            log = dir.join("calls.log").display()
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    fn forge_with(dir: &Path, cache: &Path, body: &str, slug: &str) -> GhStarForge {
        store::set_test_daemon_store_dir(Some(cache.to_path_buf()));
        GhStarForge {
            gh_bin: fake_gh(dir, body),
            root: dir.to_path_buf(),
            slug: slug.to_string(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn issue_read_sends_if_none_match_and_serves_the_304_from_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let body = r#"{"number":7,"title":"t","state":"open","labels":[],"body":"b"}"#;
        let mut f = forge_with(dir.path(), cache.path(), body, "o/star-issue");
        let first = f.issue(7).unwrap().unwrap();
        let second = f.issue(7).unwrap().unwrap();
        assert_eq!(first.number, 7);
        assert_eq!(second.number, 7, "the 304 serves the stored body");
        let log = std::fs::read_to_string(dir.path().join("calls.log")).unwrap();
        let calls: Vec<&str> = log.lines().collect();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].contains("repos/o/star-issue/issues/7"));
        assert!(!calls[0].contains("If-None-Match"));
        assert!(calls[1].contains("If-None-Match: W/\"v1\""));
        assert!(!calls.iter().any(|c| c.contains("--paginate")));
    }

    #[cfg(unix)]
    #[test]
    fn comments_read_is_conditional_and_maps_the_author() {
        let dir = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let body = r#"[{"body":"hi","created_at":"c","updated_at":"u","user":{"login":"bob"},"author_association":"MEMBER"}]"#;
        let mut f = forge_with(dir.path(), cache.path(), body, "o/star-comments");
        for _ in 0..2 {
            let got = f.comments(3).unwrap();
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].author.as_deref(), Some("bob"));
            assert_eq!(got[0].author_association.as_deref(), Some("MEMBER"));
        }
        let log = std::fs::read_to_string(dir.path().join("calls.log")).unwrap();
        assert_eq!(log.lines().count(), 2, "one short page per read");
        assert!(log.lines().nth(1).unwrap().contains("If-None-Match"));
    }

    #[cfg(unix)]
    #[test]
    fn search_read_is_conditional_filters_prs_and_keeps_association() {
        let dir = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let body = r#"{"items":[{"number":5,"title":"x","state":"open","labels":[],"author_association":"OWNER"},{"number":6,"title":"pr","state":"open","labels":[],"pull_request":{},"author_association":"OWNER"}]}"#;
        let mut f = forge_with(dir.path(), cache.path(), body, "o/star-search");
        for _ in 0..2 {
            let hits = f.search_open_issues("some \"phrase\"").unwrap();
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].issue.number, 5);
            assert_eq!(hits[0].author_association.as_deref(), Some("OWNER"));
        }
        let log = std::fs::read_to_string(dir.path().join("calls.log")).unwrap();
        assert!(log
            .lines()
            .next()
            .unwrap()
            .contains("search/issues?q=%22some%20phrase%22"));
        assert!(log.lines().nth(1).unwrap().contains("If-None-Match"));
    }

    #[test]
    fn url_encode_escapes_reserved_characters() {
        assert_eq!(url_encode("a b:c\"/d"), "a%20b%3Ac%22%2Fd");
        assert_eq!(url_encode("A-z_0.9~"), "A-z_0.9~");
    }

    #[test]
    fn no_raw_gh_spawn_outside_the_shared_helper() {
        let src = include_str!("forge.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        assert_eq!(prod.matches("Command::new(").count(), 1, "only GhStarForge::api spawns");
    }
}
