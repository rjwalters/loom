//! Unit tests for [`crate::forge_listing`], extracted to a sibling file
//! (#9251/#9252) so the production module stays under its file-size ratchet.

use super::*;
use crate::forge_etag_store::{
    cache_key, disk_cache_path, read_disk_entry, repo_scope, resolve_target,
};
use std::cell::RefCell;
use std::path::PathBuf;
use std::process::Command;

thread_local! {
    /// Test hook run by `list_issues_cached_once` between the request and the
    /// response handling — lets a test mutate the cache mid-flight (#9252).
    static AFTER_SEND_HOOK: RefCell<Option<Box<dyn FnMut()>>> = const { RefCell::new(None) };
}

pub(super) fn run_after_send_hook() {
    AFTER_SEND_HOOK.with(|h| {
        if let Some(f) = h.borrow_mut().as_mut() {
            f();
        }
    });
}

pub(super) fn set_after_send_hook(hook: Option<Box<dyn FnMut()>>) {
    AFTER_SEND_HOOK.with(|h| *h.borrow_mut() = hook);
}

/// The repo component of the shared key for a call from `cwd`.
fn disk_cache_repo_scope(cwd: Option<&Path>, repo: Option<&str>) -> String {
    repo_scope(cwd, &resolve_target(cwd, repo))
}

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

/// Regression for the #4443 review finding: a response with GitHub's real
/// ~20-header block (CRLF terminators) must split headers/body exactly.
/// The old line-length reconstruction undercounted each `\r\n` header by
/// one byte, so the "body" started inside the header block (e.g.
/// `"58\r\nX-Xss-Protection: 0\r\n\r\n[{…"`) and every real 200 fetch
/// failed to parse — masked in the small fixtures above because their
/// few stray bytes were pure whitespace that `trim()` swallowed.
#[test]
fn parses_realistic_multi_header_crlf_response() {
    let headers = [
        "HTTP/2.0 200 OK",
        "Access-Control-Allow-Origin: *",
        "Access-Control-Expose-Headers: ETag, Link, Location, Retry-After, X-GitHub-OTP, \
         X-RateLimit-Limit, X-RateLimit-Remaining, X-RateLimit-Used, X-RateLimit-Resource, \
         X-RateLimit-Reset, X-OAuth-Scopes, X-Accepted-OAuth-Scopes, X-Poll-Interval, \
         X-GitHub-Media-Type, X-GitHub-SSO, X-GitHub-Request-Id, Deprecation, Sunset, Warning",
        "Cache-Control: private, max-age=60, s-maxage=60",
        "Content-Security-Policy: default-src 'none'",
        "Content-Type: application/json; charset=utf-8",
        "Etag: W/\"6289abc123def\"",
        "Referrer-Policy: origin-when-cross-origin, strict-origin-when-cross-origin",
        "Server: github.com",
        "Strict-Transport-Security: max-age=31536000; includeSubdomains; preload",
        "Vary: Accept, Authorization, Cookie, X-GitHub-OTP",
        "X-Accepted-Oauth-Scopes: repo",
        "X-Content-Type-Options: nosniff",
        "X-Frame-Options: deny",
        "X-Github-Api-Version-Selected: 2022-11-28",
        "X-Github-Media-Type: github.v3; format=json",
        "X-Github-Request-Id: E5E5:1234:ABCDEF:FEDCBA:66A7B8C9",
        "X-Oauth-Scopes: gist, read:org, repo, workflow",
        "X-Ratelimit-Limit: 5000",
        "X-Ratelimit-Remaining: 4034",
        "X-Ratelimit-Reset: 1785356436",
        "X-Ratelimit-Resource: core",
        "X-Ratelimit-Used: 966",
        "Content-Length: 58",
        "X-Xss-Protection: 0",
    ]
    .join("\r\n");
    let body_json = r#"[{"number": 4441, "state": "open", "labels": [{"name": "loom:issue"}]}]"#;
    let raw = format!("{headers}\r\n\r\n{body_json}");

    let r = parse_http_response(&raw).unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(r.etag.as_deref(), Some("W/\"6289abc123def\""));
    assert_eq!(
        r.body, body_json,
        "the body must be EXACTLY the JSON payload — no header-tail bytes"
    );
    let issues = parse_rest_issues(&r.body).unwrap();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].number, 4441);
}

/// The boundary search must not be confused by an LF-only pair occurring
/// AFTER the CRLF header/body boundary (e.g. inside a JSON string value).
#[test]
fn crlf_boundary_wins_over_later_lf_pair_in_body() {
    let raw = "HTTP/2.0 200 OK\r\nEtag: \"x\"\r\n\r\n[{\"number\": 1, \"state\": \"open\", \
               \"labels\": [], \"body\": \"line1\\n\\nline2\"}]";
    let r = parse_http_response(raw).unwrap();
    assert!(r.body.starts_with("[{"));
    assert_eq!(parse_rest_issues(&r.body).unwrap()[0].number, 1);
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

/// The disk-persistent variant must behave like the in-process one across
/// *separate* short-lived processes: the first call (200) writes the ETag +
/// body to disk; a second call — which for the persistent path has no
/// in-memory state, exactly as a fresh agent CLI process would — presents
/// that ETag, gets a free 304, and reconstructs the identical listing from
/// the on-disk body.
///
/// `#[serial_test::serial]`: mutates the process-global
/// `LOOM_LISTING_CACHE_DIR` env var, which every disk-cache test in this
/// module shares — unserialized, a concurrently-running test's `set_var`
/// can be observed mid-test and point this one at the wrong tempdir.
#[test]
#[serial_test::serial]
fn disk_cache_persists_etag_and_serves_304_across_processes() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    std::env::set_var("LOOM_LISTING_CACHE_DIR", cache.path());
    let gh = write_fake_gh(dir.path());
    let repo = format!("test/disk-flow-{}", std::process::id());

    // Round 1: 200 → parsed + written to disk (not truncated: 1 row).
    let first =
        list_issues_cached_persistent(&gh, Some(dir.path()), Some(&repo), "loom:issue", "open")
            .unwrap();
    assert_eq!(first.issues.len(), 1);
    assert_eq!(first.issues[0].number, 7);
    assert!(!first.truncated);
    // The entry file now exists on disk (durable across process exit).
    assert!(std::fs::read_dir(cache.path()).unwrap().count() >= 1);

    // Round 2: the fake gh answers 304 to the presented If-None-Match; the
    // listing must come back identical, reconstructed from the disk body.
    let second =
        list_issues_cached_persistent(&gh, Some(dir.path()), Some(&repo), "loom:issue", "open")
            .unwrap();
    assert_eq!(second.issues, first.issues);
    std::env::remove_var("LOOM_LISTING_CACHE_DIR");
}

// ========================================================================
// Shrink guard against a single-read alternation (#7451)
// ========================================================================

/// A fake `gh` that, keyed purely on invocation ORDER (not the presented
/// `If-None-Match`), models exactly the #7451 repro: a correct 3-item
/// response, then — on the very next call — a single transient,
/// inconsistent `200` with ZERO items (simulating GitHub's issues-listing
/// endpoint occasionally disagreeing with itself on one request under
/// concurrent label churn, with no real state change), then a re-fetch
/// that reverts to the correct 3-item answer, then `304`s forever after
/// (steady state once the disk entry settles).
fn write_fake_gh_transient_shrink(dir: &Path, calls_log: &Path) -> PathBuf {
    let path = dir.join("fake-gh-shrink.sh");
    let body = format!(
        r#"#!/bin/sh
echo x >> {calls}
n=$(wc -l < {calls} | tr -d ' ')
three='[{{"number": 1, "state": "open", "labels": [{{"name": "loom:epic"}}]}}, {{"number": 2, "state": "open", "labels": [{{"name": "loom:epic"}}]}}, {{"number": 3, "state": "open", "labels": [{{"name": "loom:epic"}}]}}]'
case "$n" in
  1)
printf 'HTTP/2.0 200 OK\r\nEtag: W/"gen1"\r\n\r\n'
printf '%s\n' "$three"
;;
  2)
# The transient, single-request-only disagreement: fewer items, a new
# etag, despite no real label mutation having happened.
printf 'HTTP/2.0 200 OK\r\nEtag: W/"gen2-flaky"\r\n\r\n'
printf '[]\n'
;;
  3)
# The shrink guard's unconditional re-fetch: reverts to the truth.
printf 'HTTP/2.0 200 OK\r\nEtag: W/"gen1"\r\n\r\n'
printf '%s\n' "$three"
;;
  *)
# Steady state: whatever we hold (gen1, never overwritten by gen2-flaky)
# validates as unchanged.
printf 'HTTP/2.0 304 Not Modified\r\n\r\n'
echo 'gh: Not Modified (HTTP 304)' 1>&2
exit 1
;;
esac
"#,
        calls = calls_log.display()
    );
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

/// Regression for #7451: a single transient/inconsistent empty read must
/// never be trusted and durably cached — repeated calls across the
/// alternation window (and after it settles) must all keep returning the
/// correct 3-item listing, never the flaky empty one, and the on-disk
/// entry must never be overwritten with the disputed `gen2-flaky` etag.
#[test]
#[serial_test::serial]
fn a_transient_single_read_shrink_never_reaches_the_caller_or_the_disk() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    std::env::set_var("LOOM_LISTING_CACHE_DIR", cache.path());
    let calls_log = dir.path().join("calls.log");
    let gh = write_fake_gh_transient_shrink(dir.path(), &calls_log);
    let repo = format!("test/shrink-guard-{}", std::process::id());

    // Round 1: establishes the correct 3-item cache entry (gen1).
    let first =
        list_issues_cached_persistent(&gh, Some(dir.path()), Some(&repo), "loom:epic", "open")
            .unwrap();
    assert_eq!(first.issues.len(), 3);

    // Round 2: the underlying `gh` answers with the transient empty
    // read (invocation #2) — the guard must issue its own corroborating
    // re-fetch (invocation #3, which reverts to gen1/3-items) and return
    // the CORRECT listing to the caller, never the flaky `[]`.
    let second =
        list_issues_cached_persistent(&gh, Some(dir.path()), Some(&repo), "loom:epic", "open")
            .unwrap();
    assert_eq!(
        second.issues.len(),
        3,
        "a single-request shrink must never reach the caller unconfirmed"
    );

    // The on-disk entry must still hold the ORIGINAL gen1 etag/body — the
    // disputed gen2-flaky response must never have been persisted.
    let on_disk = read_disk_entry(&disk_cache_path(&cache_key(
        Some(dir.path()),
        &resolve_target(Some(dir.path()), Some(&repo)),
        &build_issues_url(Some(&repo), "loom:epic", "open"),
    )))
    .unwrap();
    assert_eq!(on_disk.etag, "W/\"gen1\"");

    // Round 3 onward: the fake gh now only ever answers 304 (steady
    // state) — every further call in the "alternation window" from the
    // bug report must keep returning 3, never flip back to empty.
    for _ in 0..4 {
        let round =
            list_issues_cached_persistent(&gh, Some(dir.path()), Some(&repo), "loom:epic", "open")
                .unwrap();
        assert_eq!(round.issues.len(), 3, "no post-settle alternation back to empty");
    }

    std::env::remove_var("LOOM_LISTING_CACHE_DIR");
}

/// A genuine shrink — corroborated by the re-fetch — must still go
/// through: the guard only filters a single-request disagreement, it
/// never blocks a real, confirmed content change.
#[test]
#[serial_test::serial]
fn a_corroborated_shrink_is_accepted_and_persisted() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    std::env::set_var("LOOM_LISTING_CACHE_DIR", cache.path());
    let path = dir.path().join("fake-gh-real-shrink.sh");
    let calls_log = dir.path().join("calls.log");
    std::fs::write(
        &path,
        format!(
            r#"#!/bin/sh
echo x >> {calls}
n=$(wc -l < {calls} | tr -d ' ')
case "$n" in
  1)
printf 'HTTP/2.0 200 OK\r\nEtag: W/"gen1"\r\n\r\n'
printf '[{{"number": 1, "state": "open", "labels": [{{"name": "loom:epic"}}]}}]\n'
;;
  *)
# Every later call (the shrink itself AND the guard's own re-fetch) sees
# the SAME genuinely-empty state — a real, corroborated content change.
printf 'HTTP/2.0 200 OK\r\nEtag: W/"gen2"\r\n\r\n'
printf '[]\n'
;;
esac
"#,
            calls = calls_log.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
    }
    let repo = format!("test/real-shrink-{}", std::process::id());

    let first =
        list_issues_cached_persistent(&path, Some(dir.path()), Some(&repo), "loom:epic", "open")
            .unwrap();
    assert_eq!(first.issues.len(), 1);

    let second =
        list_issues_cached_persistent(&path, Some(dir.path()), Some(&repo), "loom:epic", "open")
            .unwrap();
    assert_eq!(
        second.issues.len(),
        0,
        "a re-fetch-corroborated real shrink must be accepted, not suppressed"
    );

    std::env::remove_var("LOOM_LISTING_CACHE_DIR");
}

// ========================================================================
// Disk-persistent cache key repo scoping (#7275)
// ========================================================================

/// `git init` + an `origin` remote pointing at `owner_repo`, so
/// [`crate::credential_preflight::nwo_from_git_remote`] resolves it.
fn init_git_repo_with_remote(dir: &Path, owner_repo: &str) {
    assert!(Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir)
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .args([
            "remote",
            "add",
            "origin",
            &format!("https://github.com/{owner_repo}.git")
        ])
        .current_dir(dir)
        .status()
        .unwrap()
        .success());
}

#[test]
fn disk_cache_repo_scope_prefers_explicit_repo_over_cwd() {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo_with_remote(dir.path(), "fixture-owner/from-remote");
    assert_eq!(
        disk_cache_repo_scope(Some(dir.path()), Some("fixture-owner/explicit")),
        "fixture-owner/explicit"
    );
}

#[test]
fn disk_cache_repo_scope_resolves_distinct_git_remotes() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    init_git_repo_with_remote(dir_a.path(), "fixture-owner/repo-a");
    init_git_repo_with_remote(dir_b.path(), "fixture-owner/repo-b");

    let scope_a = disk_cache_repo_scope(Some(dir_a.path()), None);
    let scope_b = disk_cache_repo_scope(Some(dir_b.path()), None);
    assert_eq!(scope_a, "fixture-owner/repo-a");
    assert_eq!(scope_b, "fixture-owner/repo-b");
    assert_ne!(scope_a, scope_b);
}

#[test]
fn disk_cache_repo_scope_falls_back_to_raw_cwd_for_a_non_git_dir() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(disk_cache_repo_scope(Some(dir.path()), None), dir.path().display().to_string());
}

#[test]
fn disk_cache_repo_scope_is_empty_with_no_cwd_and_no_repo() {
    // The pre-#7275 collision precondition: with neither a cwd nor a
    // resolved repo, there is no repo-identifying signal to scope by at
    // all — this is exactly why `default_fetcher` hardcoding `cwd: None`
    // was the actual defect (fixed by forwarding the real process cwd).
    assert_eq!(disk_cache_repo_scope(None, None), "");
}

/// A fake `gh` whose response depends on which fixture repo directory it
/// was invoked in (mirroring how real `gh` resolves `{owner}/{repo}`
/// placeholders from the inherited process cwd's git remote) — so a
/// single shared binary can serve two disjoint fixture repos. The first
/// call for a repo gets `200` + a repo-specific ETag/body; presenting that
/// same ETag again gets `304`.
fn write_fake_gh_for_repo(dir: &Path, repo_dirname: &str, etag: &str, body: &str) -> PathBuf {
    let path = dir.join(format!("fake-gh-{repo_dirname}.sh"));
    let script = format!(
        r#"#!/bin/sh
here="$(pwd)"
case "$here" in
  */{repo_dirname})
case "$*" in
  *'If-None-Match: {etag}'*)
    printf 'HTTP/2.0 304 Not Modified\r\n\r\n'
    echo 'gh: Not Modified (HTTP 304)' 1>&2
    exit 1
    ;;
  *)
    printf 'HTTP/2.0 200 OK\r\nEtag: {etag}\r\n\r\n'
    printf '%s\n' '{body}'
    ;;
esac
;;
  *)
echo "unexpected cwd for {repo_dirname} fixture: $here" 1>&2
exit 1
;;
esac
"#
    );
    std::fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
    }
    path
}

/// Regression for #7275: two different fixture repos, each with its own
/// `origin` remote and disjoint issue data, querying the IDENTICAL
/// `(label, state)` combo through [`list_issues_cached_persistent`] and
/// sharing ONE on-disk cache directory (exactly the "multi-repo fleet
/// host" scenario from the bug report) must never read or serve the
/// other's listing — each gets its own file, keyed by its resolved
/// `owner/repo`, not a placeholder-only string that collapses across
/// repos.
#[test]
#[serial_test::serial]
fn two_repos_sharing_a_disk_cache_never_cross_contaminate() {
    let base = tempfile::tempdir().unwrap();
    let repo_a = base.path().join("repo-a");
    let repo_b = base.path().join("repo-b");
    std::fs::create_dir_all(&repo_a).unwrap();
    std::fs::create_dir_all(&repo_b).unwrap();
    init_git_repo_with_remote(&repo_a, "fixture-owner/repo-a");
    init_git_repo_with_remote(&repo_b, "fixture-owner/repo-b");

    let gh_a = write_fake_gh_for_repo(
        base.path(),
        "repo-a",
        "W/\"repo-a-etag\"",
        r#"[{"number": 100, "state": "open", "labels": [{"name": "loom:issue"}]}]"#,
    );
    let gh_b = write_fake_gh_for_repo(
        base.path(),
        "repo-b",
        "W/\"repo-b-etag\"",
        r#"[{"number": 200, "state": "open", "labels": [{"name": "loom:issue"}]}]"#,
    );

    let cache = tempfile::tempdir().unwrap();
    std::env::set_var("LOOM_LISTING_CACHE_DIR", cache.path());

    // Both queries share the identical (label, state) combo AND, with no
    // `--repo` override, the identical UNRESOLVED URL — the exact
    // precondition the bug report describes.
    let listing_a =
        list_issues_cached_persistent(&gh_a, Some(&repo_a), None, "loom:issue", "open").unwrap();
    let listing_b =
        list_issues_cached_persistent(&gh_b, Some(&repo_b), None, "loom:issue", "open").unwrap();

    assert_eq!(listing_a.issues.len(), 1);
    assert_eq!(listing_a.issues[0].number, 100);
    assert_eq!(listing_b.issues.len(), 1);
    assert_eq!(listing_b.issues[0].number, 200);

    // Two distinct on-disk entries — never one shared file.
    assert_eq!(
        std::fs::read_dir(cache.path()).unwrap().count(),
        2,
        "each repo must get its own disk cache entry, never a shared one"
    );

    // Re-querying each repo presents its OWN etag and gets a 304,
    // reconstructing its OWN (never the other repo's) cached body.
    let listing_a_again =
        list_issues_cached_persistent(&gh_a, Some(&repo_a), None, "loom:issue", "open").unwrap();
    let listing_b_again =
        list_issues_cached_persistent(&gh_b, Some(&repo_b), None, "loom:issue", "open").unwrap();
    assert_eq!(listing_a_again.issues, listing_a.issues);
    assert_eq!(listing_b_again.issues, listing_b.issues);

    std::env::remove_var("LOOM_LISTING_CACHE_DIR");
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
    let err =
        list_issues_cached(&path, Some(dir.path()), Some(&repo), "loom:issue", "open").unwrap_err();
    assert!(crate::rate_limit_breaker::indicates_rate_limit(&err.to_string()));
}

// ========================================================================
// Forced-refresh retry on a registered-workspace 404 (#6171)
// ========================================================================

#[test]
fn is_404_error_matches_only_an_http_404() {
    assert!(is_404_error(
        "gh api repos/x/y/issues failed in /path: gh: Not Found (HTTP 404)"
    ));
    assert!(!is_404_error("gh: API rate limit exceeded for user ID 1 (HTTP 403)"));
    assert!(!is_404_error("could not invoke gh: No such file or directory"));
    assert!(!is_404_error(""));
}

/// A fake `gh` that always answers a 404 (mirroring the exact repro text
/// from #6171: `gh: Not Found (HTTP 404)`), recording one line per
/// invocation to `calls_log` so a test can assert how many times it ran.
fn write_fake_gh_always_404(dir: &Path, calls_log: &Path) -> PathBuf {
    let path = dir.join("fake-gh-404.sh");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\necho x >> {}\necho 'gh: Not Found (HTTP 404)' 1>&2\nexit 1\n",
            calls_log.display()
        ),
    )
    .unwrap();
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
fn a_404_with_no_git_remote_never_retries_and_returns_the_original_error() {
    // #6171's recovery path needs `nwo_from_git_remote(cwd)` to resolve an
    // owner/repo before it can do anything — a `cwd` that isn't even a git
    // checkout (the common case for a `LOOM_REPO`-only invocation with a
    // scratch `cwd`) must behave byte-identically to any other failure:
    // exactly one `gh` invocation, the original error surfaced unchanged.
    // This holds regardless of whether some OTHER daemon process has ever
    // registered a primary workspace root, so it carries no cross-test
    // ordering hazard.
    let dir = tempfile::tempdir().unwrap();
    let calls_log = dir.path().join("calls.log");
    let gh = write_fake_gh_always_404(dir.path(), &calls_log);
    let repo = format!("test/404-no-remote-{}", std::process::id());

    let err =
        list_issues_cached(&gh, Some(dir.path()), Some(&repo), "loom:issue", "open").unwrap_err();
    assert!(err.to_string().contains("HTTP 404"));
    assert_eq!(
        std::fs::read_to_string(&calls_log).unwrap().lines().count(),
        1,
        "no retry without a resolvable owner/repo to refresh a credential for"
    );
}

#[test]
fn a_404_with_cwd_none_never_attempts_a_retry() {
    // The retry path is scoped to a *registered workspace* — a `None`
    // cwd (no checkout root to refresh a credential for) must skip it
    // entirely, exactly like the no-remote case above.
    let dir = tempfile::tempdir().unwrap();
    let calls_log = dir.path().join("calls.log");
    let gh = write_fake_gh_always_404(dir.path(), &calls_log);
    let repo = format!("test/404-cwd-none-{}", std::process::id());

    let err = list_issues_cached(&gh, None, Some(&repo), "loom:issue", "open").unwrap_err();
    assert!(err.to_string().contains("HTTP 404"));
    assert_eq!(std::fs::read_to_string(&calls_log).unwrap().lines().count(), 1);
}

#[test]
fn a_non_404_failure_never_attempts_a_retry() {
    // A rate-limit 403 (or any other failure) must not trigger the #6171
    // recovery path at all — it is scoped to 404s specifically (AC2).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fake-gh-403.sh");
    let calls_log = dir.path().join("calls.log");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\necho x >> {}\necho 'gh: API rate limit exceeded for user ID 1 (HTTP \
             403)' 1>&2\nexit 1\n",
            calls_log.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
    }
    let repo = format!("test/403-{}", std::process::id());
    let err =
        list_issues_cached(&path, Some(dir.path()), Some(&repo), "loom:issue", "open").unwrap_err();
    assert!(err.to_string().contains("HTTP 403"));
    assert_eq!(std::fs::read_to_string(&calls_log).unwrap().lines().count(), 1);
}

#[test]
fn a_registered_workspace_404_forces_one_refresh_and_retries_successfully() {
    // #6171 end-to-end (mirrors the reported repro): a per-owner
    // credential minted before a workspace was registered 404s on the
    // first scan; a forced mint + one retry recovers within the SAME
    // call — no restart, no second tick required (AC1/AC2).
    //
    // `register_primary_workspace_root` is a set-once `OnceLock` with
    // exactly one production call site (`daemon_service.rs`, never
    // exercised by `cargo test`) and no other test in this crate ever
    // calls it — safe to register here for the lifetime of the test
    // binary; every OTHER test's "no retry" assertions above hold
    // regardless of this global's state (they short-circuit earlier, on
    // a missing git remote or a `None` cwd).
    let workspace_root = tempfile::tempdir().unwrap();
    let script_dir = workspace_root.path().join(".loom/scripts/lib");
    std::fs::create_dir_all(&script_dir).unwrap();
    let app_script = script_dir.join("github-app-token.sh");
    std::fs::write(
        &app_script,
        "#!/bin/sh\necho '{\"status\":\"ok\",\"token\":\"ghs_retry\",\"installation_id\":\"1\",\"app_id\":\"2\",\"expires_at\":\"2099-01-01T00:00:00Z\"}'\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&app_script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&app_script, perms).unwrap();
    }
    crate::credential_preflight::register_primary_workspace_root(workspace_root.path());

    let repo_root = tempfile::tempdir().unwrap();
    assert!(Command::new("git")
        .args(["init", "-q"])
        .current_dir(repo_root.path())
        .status()
        .unwrap()
        .success());
    let owner_repo = format!("test-owner-{}/retry-repo", std::process::id());
    assert!(Command::new("git")
        .args([
            "remote",
            "add",
            "origin",
            &format!("https://github.com/{owner_repo}.git")
        ])
        .current_dir(repo_root.path())
        .status()
        .unwrap()
        .success());

    let calls_log = repo_root.path().join("calls.log");
    let fake_gh = repo_root.path().join("fake-gh-then-recovers.sh");
    std::fs::write(
        &fake_gh,
        format!(
            "#!/bin/sh\necho x >> {calls}\nn=$(wc -l < {calls})\nif [ \"$n\" -eq 1 ]; then\n  \
             echo 'gh: Not Found (HTTP 404)' 1>&2\n  exit 1\nelse\n  printf 'HTTP/2.0 200 \
             OK\\r\\n\\r\\n'\n  printf '[{{\"number\": 99, \"state\": \"open\", \"labels\": \
             [{{\"name\": \"loom:issue\"}}]}}]\\n'\nfi\n",
            calls = calls_log.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();
    }

    let issues = list_issues_cached(
        &fake_gh,
        Some(repo_root.path()),
        Some(&owner_repo),
        "loom:issue",
        "open",
    )
    .expect("the second attempt, after the forced refresh, must succeed");
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].number, 99);

    let calls = std::fs::read_to_string(&calls_log).unwrap();
    assert_eq!(
        calls.lines().count(),
        2,
        "exactly one retry after the forced refresh, never a loop: {calls:?}"
    );
}

// Resolved-identity key + disk persistence for the daemon cache (#9252) and
// per-caller accounting of both outcomes (#9251).
#[path = "forge_listing_persist_tests.rs"]
mod persist;

// ===== list_issues_cached_all_as (#10389) =====

/// A `gh` stub that logs each call's argv to `calls.log`. A `&page=N` request
/// fails when `fail<N>` exists, else answers `200` with `pages.json`. The
/// plain URL (page 1) answers `304` to `If-None-Match: W/"<etag1>"` (`p1`
/// unless `etag1` exists), else `200` + that ETag with `p1.json`. When
/// `shift` exists, the `&page=2` request first swaps page 1 for `p1b.json`
/// under ETag `p1b` (a mid-walk change).
fn paging_stub(dir: &Path) -> PathBuf {
    let path = dir.join("fake-gh-pages.sh");
    std::fs::write(
        &path,
        format!(
            r#"#!/bin/sh
d={dir}
echo "$*" >> "$d/calls.log"
case "$*" in
  *'&page='*)
    n=$(echo "$*" | sed 's/.*&page=\([0-9]*\).*/\1/')
    if [ -f "$d/fail$n" ]; then echo 'gh: Server Error (HTTP 502)' 1>&2; exit 1; fi
    if [ "$n" = 2 ] && [ -f "$d/shift" ]; then cp "$d/p1b.json" "$d/p1.json"; echo p1b > "$d/etag1"; fi
    printf 'HTTP/2.0 200 OK\r\nEtag: W/"pn"\r\n\r\n'
    cat "$d/pages.json" ;;
  *)
    e=p1; [ -f "$d/etag1" ] && e=$(cat "$d/etag1")
    case "$*" in
      *"If-None-Match: W/\"$e\""*)
        printf 'HTTP/2.0 304 Not Modified\r\n\r\n'
        echo 'gh: Not Modified (HTTP 304)' 1>&2
        exit 1 ;;
    esac
    printf 'HTTP/2.0 200 OK\r\nEtag: W/"%s"\r\n\r\n' "$e"
    cat "$d/p1.json" ;;
esac
"#,
            dir = dir.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

fn page_json(numbers: std::ops::Range<u32>) -> String {
    let rows: Vec<String> = numbers
        .map(|n| format!(r#"{{"number": {n}, "state": "open", "labels": []}}"#))
        .collect();
    format!("[{}]\n", rows.join(","))
}

fn calls(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("calls.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// More than one page of starred items (rjwalters/loom has 105): the walk
/// returns all of them, and its page 1 is the single-page listing's own
/// cache entry, revalidated with that entry's ETag.
#[test]
fn the_paged_listing_reads_past_page_one_through_the_same_cache_entry() {
    let dir = tempfile::tempdir().unwrap();
    let repo = format!("test-owner/paged-{}", std::process::id());
    std::fs::write(dir.path().join("p1.json"), page_json(1..101)).unwrap();
    std::fs::write(dir.path().join("pages.json"), page_json(101..106)).unwrap();
    let gh = paging_stub(dir.path());
    let label = "loom:operator-priority";

    // The single-page listing sees only page 1 and caches it under W/"p1".
    let single = list_issues_cached_as("t", &gh, Some(dir.path()), Some(&repo), label, "open");
    assert_eq!(single.unwrap().len(), PER_PAGE);

    let all =
        list_issues_cached_all_as("t", &gh, Some(dir.path()), Some(&repo), label, "open").unwrap();
    let numbers: Vec<u32> = all.iter().map(|i| i.number).collect();
    assert_eq!(numbers, (1..106).collect::<Vec<_>>());

    let calls = calls(dir.path());
    // Single-listing read, page 1, page 2, then the page 1 revalidation.
    assert_eq!(calls.len(), 4, "{calls:?}");
    assert!(!calls[3].contains("&page="), "{calls:?}");
    assert!(calls[3].contains(r#"If-None-Match: W/"p1""#), "{calls:?}");
    let plain = build_issues_url(Some(&repo), label, "open");
    // Page 1: the unchanged URL, presenting the single listing's ETag (so the
    // same cache key), answered from the cache by a 304.
    assert!(calls[1].contains(&plain), "{calls:?}");
    assert!(!calls[1].contains("&page="), "{calls:?}");
    assert!(calls[1].contains(r#"If-None-Match: W/"p1""#), "{calls:?}");
    assert!(calls[2].contains(&format!("{plain}&page=2")), "{calls:?}");
}

/// An incomplete walk is an error, never a partial set: a later page
/// failing, or every page up to [`MAX_PAGES`] full.
#[test]
fn an_incomplete_paged_listing_is_an_error_never_a_partial_set() {
    let dir = tempfile::tempdir().unwrap();
    let repo = format!("test-owner/paged-incomplete-{}", std::process::id());
    std::fs::write(dir.path().join("p1.json"), page_json(1..101)).unwrap();
    std::fs::write(dir.path().join("pages.json"), page_json(101..201)).unwrap();
    let gh = paging_stub(dir.path());
    let walk = || list_issues_cached_all_as("t", &gh, Some(dir.path()), Some(&repo), "x", "open");

    std::fs::write(dir.path().join("fail3"), "").unwrap();
    let err = walk().unwrap_err();
    assert!(format!("{err:#}").contains("HTTP 502"), "{err:#}");

    std::fs::remove_file(dir.path().join("fail3")).unwrap();
    let before = calls(dir.path()).len();
    let err = walk().unwrap_err();
    assert!(format!("{err:#}").contains("incomplete"), "{err:#}");
    assert_eq!(calls(dir.path()).len() - before, MAX_PAGES as usize);

    // A short page 1 is the whole set, in one request.
    std::fs::write(dir.path().join("p1.json"), page_json(1..4)).unwrap();
    let other = format!("{repo}-short");
    let all = list_issues_cached_all_as("t", &gh, Some(dir.path()), Some(&other), "x", "open");
    assert_eq!(all.unwrap().len(), 3);
}

/// A stable multi-page walk returns everything; the only extra request is the
/// conditional re-read of page 1 (a `304`).
#[test]
fn a_stable_multi_page_walk_revalidates_earlier_pages_and_returns_all() {
    let dir = tempfile::tempdir().unwrap();
    let repo = format!("test-owner/paged-stable-{}", std::process::id());
    std::fs::write(dir.path().join("p1.json"), page_json(1..101)).unwrap();
    std::fs::write(dir.path().join("pages.json"), page_json(101..106)).unwrap();
    let gh = paging_stub(dir.path());
    // Prime page 1's ETag so the revalidation is a 304.
    list_issues_cached_as("t", &gh, Some(dir.path()), Some(&repo), "x", "open").unwrap();
    let before = calls(dir.path()).len();
    let all =
        list_issues_cached_all_as("t", &gh, Some(dir.path()), Some(&repo), "x", "open").unwrap();
    assert_eq!(all.len(), 105);
    let c = calls(dir.path());
    assert_eq!(c.len() - before, 3, "page 1, page 2, page 1 again: {c:?}");
    assert!(c[c.len() - 1].contains(r#"If-None-Match: W/"p1""#), "{c:?}");
}

/// Page 1 changing between the page 1 and page 2 reads (an item unstarred, so
/// the old first item of page 2 shifts up) is an error, not a set missing it.
#[test]
fn a_page_that_changes_mid_walk_is_an_error_not_a_set_missing_the_shifted_item() {
    let dir = tempfile::tempdir().unwrap();
    let repo = format!("test-owner/paged-shift-{}", std::process::id());
    std::fs::write(dir.path().join("p1.json"), page_json(1..101)).unwrap();
    // Item 1 leaves page 1 and item 101 moves up into it.
    std::fs::write(dir.path().join("p1b.json"), page_json(2..102)).unwrap();
    std::fs::write(dir.path().join("pages.json"), page_json(102..106)).unwrap();
    std::fs::write(dir.path().join("shift"), "").unwrap();
    let gh = paging_stub(dir.path());
    let err = list_issues_cached_all_as("t", &gh, Some(dir.path()), Some(&repo), "x", "open")
        .expect_err("a walk whose page 1 moved mid-walk is inconsistent");
    assert!(format!("{err:#}").contains("changed mid-walk"), "{err:#}");
}

/// A single-page walk makes exactly one request: no revalidation.
#[test]
fn a_single_page_walk_makes_no_extra_request() {
    let dir = tempfile::tempdir().unwrap();
    let repo = format!("test-owner/paged-single-{}", std::process::id());
    std::fs::write(dir.path().join("p1.json"), page_json(1..4)).unwrap();
    let gh = paging_stub(dir.path());
    let all =
        list_issues_cached_all_as("t", &gh, Some(dir.path()), Some(&repo), "x", "open").unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(calls(dir.path()).len(), 1);
}

// ===== list_issues_cached_paged_as (#11139) =====

/// The partial walk: a mid-walk change hands back the fresher rows, each item
/// once, marked incomplete, where the all-or-nothing walk errors.
#[test]
fn a_paged_walk_that_moves_mid_walk_returns_the_fresher_rows_marked_incomplete() {
    let dir = tempfile::tempdir().unwrap();
    let repo = format!("test-owner/paged-partial-shift-{}", std::process::id());
    std::fs::write(dir.path().join("p1.json"), page_json(1..101)).unwrap();
    std::fs::write(dir.path().join("p1b.json"), page_json(2..102)).unwrap();
    std::fs::write(dir.path().join("pages.json"), page_json(101..106)).unwrap();
    std::fs::write(dir.path().join("shift"), "").unwrap();
    let gh = paging_stub(dir.path());
    let walk = list_issues_cached_paged_as("t", &gh, Some(dir.path()), Some(&repo), "x", "open")
        .expect("page 1 was read");
    assert!(!walk.complete());
    let reason = format!("{:#}", walk.incomplete.as_ref().unwrap());
    assert!(reason.contains("changed mid-walk"), "{reason}");
    let numbers: Vec<u32> = walk.rows.iter().map(|i| i.number).collect();
    assert_eq!(numbers, (2..106).collect::<Vec<_>>(), "#1 left; #101 listed once");
}

/// The partial walk: a later page failing or the page cap keeps what was
/// read, marked incomplete; page 1 failing is still an error; a whole walk
/// is complete and costs what [`list_issues_cached_all_as`]'s does.
#[test]
fn a_paged_walk_falls_short_with_its_rows_and_a_reason() {
    let dir = tempfile::tempdir().unwrap();
    let repo = format!("test-owner/paged-partial-{}", std::process::id());
    std::fs::write(dir.path().join("p1.json"), page_json(1..101)).unwrap();
    std::fs::write(dir.path().join("pages.json"), page_json(101..201)).unwrap();
    let gh = paging_stub(dir.path());
    let walk = || list_issues_cached_paged_as("t", &gh, Some(dir.path()), Some(&repo), "x", "open");

    std::fs::write(dir.path().join("fail3"), "").unwrap();
    let partial = walk().unwrap();
    assert_eq!(partial.rows.len(), 200);
    assert!(format!("{:#}", partial.incomplete.unwrap()).contains("HTTP 502"));

    std::fs::remove_file(dir.path().join("fail3")).unwrap();
    let capped = walk().unwrap();
    // Pages 2..=MAX_PAGES all serve the same rows: each item listed once.
    assert_eq!(capped.rows.len(), 200);
    assert!(format!("{:#}", capped.incomplete.unwrap()).contains("incomplete"));

    std::fs::write(dir.path().join("pages.json"), page_json(101..106)).unwrap();
    let before = calls(dir.path()).len();
    let whole = walk().unwrap();
    assert!(whole.complete());
    assert_eq!(whole.rows.len(), 105);
    assert_eq!(calls(dir.path()).len() - before, 3, "page 1, page 2, page 1 again");

    let other = format!("{repo}-p1-fails");
    std::fs::write(dir.path().join("p1.json"), "not json").unwrap();
    assert!(
        list_issues_cached_paged_as("t", &gh, Some(dir.path()), Some(&other), "x", "open").is_err()
    );
}
