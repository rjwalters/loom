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
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

use anyhow::{anyhow, Context, Result};

/// One page's worth — matches GitHub's REST maximum and brackets the old
/// `gh issue list --limit` values (100–200) used by the converted call sites.
pub const PER_PAGE: usize = 100;

/// An issue (or PR — see [`Self::is_pull_request`]) as returned by the REST
/// `/repos/{owner}/{repo}/issues` listing, reduced to the union of fields the
/// daemon's polling loops actually consume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestIssue {
    pub number: u32,
    /// Label names (flattened from the REST label objects).
    pub labels: Vec<String>,
    /// RFC-3339 creation timestamp. Lexicographic order == chronological.
    pub created_at: Option<String>,
    /// RFC-3339 last-update timestamp.
    pub updated_at: Option<String>,
    /// `"open"` / `"closed"` (REST lowercase; compare case-insensitively —
    /// GraphQL-era consumers saw `"OPEN"`).
    pub state: String,
    pub body: Option<String>,
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
/// classifier sees rate-limit text unchanged.
pub fn list_issues_cached(
    gh_bin: &Path,
    cwd: Option<&Path>,
    repo_override: Option<&str>,
    label: &str,
    state: &str,
) -> Result<Vec<RestIssue>> {
    let env_repo = std::env::var("LOOM_REPO").ok();
    let repo = repo_override.or(env_repo.as_deref());
    let url = build_issues_url(repo, label, state);
    // The cache key must include the resolution context: with the
    // `{owner}/{repo}` placeholder form, the SAME url string resolves to
    // DIFFERENT repos depending on `cwd`.
    let cache_key = format!(
        "{}|{url}",
        cwd.map(Path::display)
            .map_or_else(String::new, |d| d.to_string())
    );

    let cached_etag = cache()
        .lock()
        .ok()
        .and_then(|c| c.get(&cache_key).map(|e| e.etag.clone()));

    let mut cmd = Command::new(gh_bin);
    cmd.arg("api").arg("--include").arg(&url);
    if let Some(ref etag) = cached_etag {
        cmd.arg("-H").arg(format!("If-None-Match: {etag}"));
    }
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = cmd
        .output()
        .with_context(|| format!("failed to invoke {}", gh_bin.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let response = parse_http_response(&stdout);

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
            Err(anyhow!(
                "forge_listing: 304 for {url} but the cache entry vanished; will re-fetch"
            ))
        }
        Some(ref r) if r.status == 200 && out.status.success() => {
            let issues = parse_rest_issues(&r.body)
                .with_context(|| format!("parse REST issues JSON from {url}"))?;
            if issues.len() >= PER_PAGE {
                log::warn!(
                    "forge_listing: {url} returned a full page ({PER_PAGE}); the listing may be \
                     truncated — items beyond the first page are not seen this poll"
                );
            }
            if let (Some(etag), Ok(mut guard)) = (r.etag.clone(), cache().lock()) {
                guard.insert(
                    cache_key,
                    CacheEntry {
                        etag,
                        issues: issues.clone(),
                    },
                );
            }
            Ok(issues)
        }
        _ => Err(anyhow!(
            "gh api {url} failed{}: {}",
            cwd.map(|d| format!(" in {}", d.display()))
                .unwrap_or_default(),
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
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
    let mut header_len = status_line.len() + 1;
    for line in lines {
        header_len += line.len() + 1;
        let trimmed = line.trim_end_matches('\r');
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.eq_ignore_ascii_case("etag") {
                etag = Some(value.trim().to_string());
            }
        }
    }
    let body = raw
        .get(header_len.min(raw.len())..)
        .unwrap_or("")
        .to_string();
    Some(HttpResponse { status, etag, body })
}

/// Parse a REST issues-listing body into [`RestIssue`]s.
#[must_use = "parse failures must surface as listing errors"]
pub fn parse_rest_issues(body: &str) -> Result<Vec<RestIssue>> {
    #[derive(serde::Deserialize)]
    struct RawLabel {
        name: String,
    }
    #[derive(serde::Deserialize)]
    struct RawIssue {
        number: u32,
        #[serde(default)]
        labels: Vec<RawLabel>,
        #[serde(default)]
        created_at: Option<String>,
        #[serde(default)]
        updated_at: Option<String>,
        #[serde(default)]
        state: String,
        #[serde(default)]
        body: Option<String>,
        #[serde(default)]
        pull_request: Option<serde_json::Value>,
    }
    let rows: Vec<RawIssue> = serde_json::from_str(body.trim())?;
    Ok(rows
        .into_iter()
        .map(|r| RestIssue {
            number: r.number,
            labels: r.labels.into_iter().map(|l| l.name).collect(),
            created_at: r.created_at,
            updated_at: r.updated_at,
            state: r.state,
            body: r.body,
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

/// Process-global ETag cache: one entry per (cwd, url). Process-lifetime is
/// the right scope — a daemon restart re-fetches each listing exactly once,
/// and the cache can never serve across identities (one gh credential per
/// daemon process).
fn cache() -> &'static Mutex<HashMap<String, CacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ===== parse_http_response =====

    const OK_RESPONSE: &str = "HTTP/2.0 200 OK\r\n\
        Etag: W/\"abc123\"\r\n\
        X-Ratelimit-Remaining: 4034\r\n\
        \r\n\
        [{\"number\": 7}]";

    #[test]
    fn parses_200_with_etag_and_body() {
        let r = parse_http_response(OK_RESPONSE).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.etag.as_deref(), Some("W/\"abc123\""));
        assert_eq!(r.body.trim(), "[{\"number\": 7}]");
    }

    #[test]
    fn parses_304_without_body() {
        let raw = "HTTP/2.0 304 Not Modified\r\nCache-Control: private, max-age=60\r\n\r\n";
        let r = parse_http_response(raw).unwrap();
        assert_eq!(r.status, 304);
        assert!(r.body.trim().is_empty());
    }

    #[test]
    fn rejects_non_http_output() {
        assert!(parse_http_response("gh: command not found").is_none());
        assert!(parse_http_response("").is_none());
    }

    #[test]
    fn header_parse_survives_lf_only_lines() {
        let raw = "HTTP/1.1 200 OK\nEtag: \"x\"\n\n[]";
        let r = parse_http_response(raw).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.etag.as_deref(), Some("\"x\""));
        assert_eq!(r.body, "[]");
    }

    // ===== parse_rest_issues =====

    #[test]
    fn parses_issues_and_marks_pull_requests() {
        let body = r#"[
            {"number": 42, "state": "open",
             "labels": [{"name": "loom:issue"}, {"name": "tier:goal-supporting"}],
             "created_at": "2026-07-29T00:00:00Z", "updated_at": "2026-07-29T01:00:00Z",
             "body": "the body"},
            {"number": 43, "state": "open", "labels": [],
             "pull_request": {"url": "https://api.github.com/repos/o/r/pulls/43"}}
        ]"#;
        let issues = parse_rest_issues(body).unwrap();
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].number, 42);
        assert_eq!(issues[0].labels, vec!["loom:issue", "tier:goal-supporting"]);
        assert!(!issues[0].is_pull_request);
        assert_eq!(issues[0].state, "open");
        assert!(issues[1].is_pull_request);
    }

    #[test]
    fn rejects_malformed_bodies() {
        assert!(parse_rest_issues("not json").is_err());
        assert!(parse_rest_issues("{\"not\": \"an array\"}").is_err());
    }

    // ===== build_issues_url =====

    #[test]
    fn url_uses_placeholders_without_override_and_repo_with() {
        assert_eq!(
            build_issues_url(None, "loom:issue", "open"),
            "repos/{owner}/{repo}/issues?labels=loom:issue&state=open&per_page=100"
        );
        assert_eq!(
            build_issues_url(Some("rjwalters/loom"), "loom:epic-phase", "all"),
            "repos/rjwalters/loom/issues?labels=loom:epic-phase&state=all&per_page=100"
        );
    }

    // ===== end-to-end cache flow with a fake gh =====

    /// A fake `gh` that returns 200+ETag+body on the first call and, when the
    /// caller presents that ETag, 304 + exit 1 (mirroring real gh) after.
    fn write_fake_gh(dir: &Path) -> PathBuf {
        let path = dir.join("fake-gh.sh");
        let body = r#"#!/bin/sh
case "$*" in
  *'If-None-Match: W/"round1"'*)
    printf 'HTTP/2.0 304 Not Modified\r\n\r\n'
    echo 'gh: Not Modified (HTTP 304)' 1>&2
    exit 1
    ;;
  *)
    printf 'HTTP/2.0 200 OK\r\nEtag: W/"round1"\r\n\r\n'
    printf '[{"number": 7, "state": "open", "labels": [{"name": "loom:issue"}]}]\n'
    ;;
esac
"#;
        std::fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        path
    }

    #[test]
    fn caches_etag_and_serves_304_from_cache() {
        let dir = tempfile::tempdir().unwrap();
        let gh = write_fake_gh(dir.path());
        // A unique repo string keys this test's cache slot so parallel tests
        // (and reruns in one process) never collide.
        let repo = format!("test/etag-flow-{}", std::process::id());

        // Round 1: 200 → parsed + cached.
        let first =
            list_issues_cached(&gh, Some(dir.path()), Some(&repo), "loom:issue", "open").unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].number, 7);

        // Round 2: fake gh sees our If-None-Match and answers 304 + exit 1;
        // the listing must come back identical, straight from the cache.
        let second =
            list_issues_cached(&gh, Some(dir.path()), Some(&repo), "loom:issue", "open").unwrap();
        assert_eq!(second, first);
    }

    #[test]
    fn errors_carry_stderr_for_the_rate_limit_classifier() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fake-gh.sh");
        std::fs::write(
            &path,
            "#!/bin/sh\necho 'gh: API rate limit exceeded for user ID 1 (HTTP 403)' 1>&2\nexit 1\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        let repo = format!("test/limit-{}", std::process::id());
        let err = list_issues_cached(&path, Some(dir.path()), Some(&repo), "loom:issue", "open")
            .unwrap_err();
        assert!(crate::rate_limit_breaker::indicates_rate_limit(&err.to_string()));
    }
}
