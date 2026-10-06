//! Installation-snapshot coverage (W8): fail-private on every unknown,
//! conditional revalidation, pagination, the user-credential fallback,
//! host-wide sharing through the store, and the call ledger row.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::*;
use crate::forge_repo_facts::state;
use crate::forge_repo_facts::test_support::Env;

/// A fake `gh` serving `installation/repositories` (and a per-repo read).
/// Files in its dir steer it: `mode` (`ok` / `fail` / `user` / `failpage2`
/// — only page 2 fails / `limited` — a primary rate limit whose reset is the
/// `reset` file / `limited_bare` — the same with no rate-limit headers /
/// `forbidden` — a `403` that is neither), `page1` / `page2` (the listing
/// bodies), `notmodified` (answer a conditional read `304`), `repo` (the
/// per-repo body).
pub(crate) struct Listing {
    pub(crate) dir: PathBuf,
    pub(crate) gh: PathBuf,
}

impl Listing {
    pub(crate) fn new(parent: &Path) -> Self {
        let dir = parent.join("listing");
        std::fs::create_dir_all(&dir).unwrap();
        let gh = dir.join("gh");
        let script = format!(
            r#"#!/bin/sh
D='{d}'
echo "$* | CFG=${{GH_CONFIG_DIR-unset}} | TOKEN=${{GH_TOKEN-unset}}" >> "$D/log"
mode=$(cat "$D/mode" 2>/dev/null || echo ok)
case "$*" in
  *"installation/repositories"*)
    case "$mode" in
      fail) echo "gh: Server Error (HTTP 502)" >&2; printf 'HTTP/2.0 502 Bad Gateway\r\n\r\n{{}}'; exit 1 ;;
      user) printf 'HTTP/2.0 403 Forbidden\r\n\r\n{{"message":"You must authenticate with an installation access token in order to list repositories for an installation."}}'; exit 1 ;;
      limited) echo "gh: API rate limit exceeded for installation ID 1 (HTTP 403)" >&2; printf 'HTTP/2.0 403 Forbidden\r\nX-Ratelimit-Resource: core\r\nX-Ratelimit-Remaining: 0\r\nX-Ratelimit-Reset: %s\r\n\r\n{{"message":"API rate limit exceeded for installation ID 1."}}' "$(cat "$D/reset")"; exit 1 ;;
      limited_bare) echo "gh: API rate limit exceeded for installation ID 1 (HTTP 403)" >&2; printf 'HTTP/2.0 403 Forbidden\r\n\r\n{{}}'; exit 1 ;;
      forbidden) echo "gh: Resource not accessible by integration (HTTP 403)" >&2; printf 'HTTP/2.0 403 Forbidden\r\nX-Ratelimit-Remaining: 4000\r\n\r\n{{"message":"Resource not accessible by integration"}}'; exit 1 ;;
      failpage2) case "$*" in *"page=2"*) echo "gh: Server Error (HTTP 502)" >&2; printf 'HTTP/2.0 502 Bad Gateway\r\n\r\n{{}}'; exit 1 ;; esac ;;
    esac
    case "$*" in *"page=2"*) p=2 ;; *) p=1 ;; esac
    case "$*" in
      *If-None-Match*) if [ -f "$D/notmodified" ]; then printf 'HTTP/2.0 304 Not Modified\r\nEtag: W/"p%s"\r\n\r\n' "$p"; exit 1; fi ;;
    esac
    printf 'HTTP/2.0 200 OK\r\nEtag: W/"p%s"\r\n\r\n' "$p"
    cat "$D/page$p"
    exit 0 ;;
  *"repos/"*)
    cat "$D/repo" 2>/dev/null
    exit 0 ;;
esac
echo "unexpected: $*" >&2
exit 1
"#,
            d = dir.display()
        );
        std::fs::write(&gh, script).unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir, gh }
    }

    pub(crate) fn set(&self, file: &str, value: &str) {
        std::fs::write(self.dir.join(file), value).unwrap();
    }

    pub(crate) fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    pub(crate) fn listing_calls(&self) -> usize {
        self.calls()
            .iter()
            .filter(|c| c.contains("installation/repositories"))
            .count()
    }
}

/// A listing page body of `(id, full_name, private)` rows.
pub(crate) fn page(total: u64, rows: &[(u64, &str, bool)]) -> String {
    let repos: Vec<String> = rows
        .iter()
        .map(|(id, name, private)| {
            format!(r#"{{"id":{id},"full_name":"{name}","private":{private},"name":"x"}}"#)
        })
        .collect();
    format!(r#"{{"total_count":{total},"repositories":[{}]}}"#, repos.join(","))
}

pub(crate) fn writer(env: &Env) -> Credential {
    Credential::writer(Some(env.tmp.path().join("cfg-writer")))
}

pub(crate) fn listed(name: &str, id: u64, private: bool) -> Answer {
    Answer::Listed(Some(RepoEntry {
        id,
        full_name: name.to_string(),
        private,
    }))
}

#[test]
fn a_fresh_snapshot_answers_every_repo_from_one_call() {
    let env = Env::new(&[]);
    let fake = Listing::new(env.tmp.path());
    fake.set("page1", &page(2, &[(1, "Acme/Pub", false), (2, "acme/secret", true)]));
    let cred = writer(&env);
    assert_eq!(lookup(&fake.gh, &cred, "acme/pub"), listed("Acme/Pub", 1, false));
    assert_eq!(lookup(&fake.gh, &cred, "ACME/SECRET"), listed("acme/secret", 2, true));
    assert_eq!(lookup(&fake.gh, &cred, "acme/other"), Answer::Listed(None), "absent");
    assert_eq!(fake.listing_calls(), 1, "{:?}", fake.calls());
    // The writer keeps its credential; the listing ran under it.
    assert!(fake.calls()[0].contains("cfg-writer"), "{:?}", fake.calls());
}

#[test]
fn a_failed_fetch_is_unavailable_and_backs_off() {
    let env = Env::new(&[]);
    let fake = Listing::new(env.tmp.path());
    fake.set("mode", "fail");
    let cred = writer(&env);
    for _ in 0..5 {
        assert_eq!(lookup(&fake.gh, &cred, "acme/pub"), Answer::Unavailable);
    }
    assert_eq!(fake.listing_calls(), 1, "one call per backoff window");
    state::advance_test_clock(SNAPSHOT_FAILURE_BACKOFF_SECS + 1);
    fake.set("mode", "ok");
    fake.set("page1", &page(1, &[(1, "acme/pub", false)]));
    assert_eq!(lookup(&fake.gh, &cred, "acme/pub"), listed("acme/pub", 1, false));
}

#[test]
fn a_stale_snapshot_is_never_served() {
    let env = Env::new(&[]);
    let fake = Listing::new(env.tmp.path());
    fake.set("page1", &page(1, &[(1, "acme/pub", false)]));
    let cred = writer(&env);
    assert_eq!(lookup(&fake.gh, &cred, "acme/pub"), listed("acme/pub", 1, false));
    state::advance_test_clock(ttl_secs() + 1);
    fake.set("mode", "fail");
    assert_eq!(
        lookup(&fake.gh, &cred, "acme/pub"),
        Answer::Unavailable,
        "a snapshot past its TTL whose revalidation failed must not answer"
    );
    // ...and still not while the failure backs off.
    assert_eq!(lookup(&fake.gh, &cred, "acme/pub"), Answer::Unavailable);
    assert_eq!(fake.listing_calls(), 2);
}

#[test]
fn a_304_revalidation_keeps_the_data_and_is_booked_not_modified() {
    let env = Env::new(&[]);
    let fake = Listing::new(env.tmp.path());
    fake.set("page1", &page(1, &[(9, "acme/pub", false)]));
    let cred = writer(&env);
    let sink = tempfile::tempdir().unwrap();
    crate::forge_call_stats::set_test_sink_dir(Some(sink.path().to_path_buf()));
    assert_eq!(lookup(&fake.gh, &cred, "acme/pub"), listed("acme/pub", 9, false));
    state::advance_test_clock(ttl_secs() + 1);
    fake.set("notmodified", "");
    fake.set("page1", "garbage: a 304 must not re-read the body");
    assert_eq!(lookup(&fake.gh, &cred, "acme/pub"), listed("acme/pub", 9, false));
    // Fresh again after the 304: no third call.
    assert_eq!(lookup(&fake.gh, &cred, "acme/pub"), listed("acme/pub", 9, false));
    let report = crate::forge_call_stats::status_report(chrono::Utc::now(), None);
    crate::forge_call_stats::set_test_sink_dir(None);
    let calls = fake.calls();
    assert_eq!(fake.listing_calls(), 2, "{calls:?}");
    assert!(!calls[0].contains("If-None-Match"), "{calls:?}");
    assert!(calls[1].contains(r#"If-None-Match: W/"p1""#), "{calls:?}");
    let rows = report.host_window.unwrap_or_default();
    let sum = |f: fn(&crate::types::ForgeCallCounts) -> u64| -> u64 {
        rows.iter()
            .filter(|r| r.caller == SNAPSHOT_CALLER)
            .map(f)
            .sum()
    };
    assert_eq!((sum(|r| r.ok), sum(|r| r.not_modified)), (1, 1), "{rows:?}");
}

#[test]
fn every_page_is_read_and_revalidated_with_its_own_validator() {
    let env = Env::new(&[]);
    let fake = Listing::new(env.tmp.path());
    let first: Vec<(u64, String, bool)> =
        (1..=100).map(|i| (i, format!("acme/r{i}"), true)).collect();
    let rows: Vec<(u64, &str, bool)> = first.iter().map(|(i, n, p)| (*i, n.as_str(), *p)).collect();
    fake.set("page1", &page(101, &rows));
    fake.set("page2", &page(101, &[(101, "acme/last", false)]));
    let cred = writer(&env);
    assert_eq!(lookup(&fake.gh, &cred, "acme/last"), listed("acme/last", 101, false));
    assert_eq!(lookup(&fake.gh, &cred, "acme/r7"), listed("acme/r7", 7, true));
    assert_eq!(fake.listing_calls(), 2);
    // A public → private flip on page 2 alone is seen at the next
    // revalidation: page 2 carries its own validator.
    state::advance_test_clock(ttl_secs() + 1);
    fake.set("page2", &page(101, &[(101, "acme/last", true)]));
    assert_eq!(lookup(&fake.gh, &cred, "acme/last"), listed("acme/last", 101, true));
    let calls = fake.calls();
    assert!(calls[2].contains("page=1") && calls[2].contains(r#"W/"p1""#), "{calls:?}");
    assert!(calls[3].contains("page=2") && calls[3].contains(r#"W/"p2""#), "{calls:?}");
}

#[test]
fn a_user_credential_is_per_repo_and_remembered() {
    let env = Env::new(&[]);
    let fake = Listing::new(env.tmp.path());
    fake.set("mode", "user");
    let cred = writer(&env);
    assert_eq!(lookup(&fake.gh, &cred, "acme/pub"), Answer::PerRepo);
    assert_eq!(lookup(&fake.gh, &cred, "acme/pub"), Answer::PerRepo);
    assert_eq!(fake.listing_calls(), 1, "the refusal is remembered for the TTL");
    // A failed hourly recheck keeps a known user token on its per-repo path.
    state::advance_test_clock(ttl_secs() + 1);
    fake.set("mode", "fail");
    assert_eq!(lookup(&fake.gh, &cred, "acme/pub"), Answer::PerRepo);
}

/// A rate-limited `403` is a failure, never "this is a user token". Checked
/// on the classifier alone: driving it through `lookup` would feed the
/// process-wide rate-limit breaker that other tests share.
#[test]
fn a_rate_limited_refusal_is_not_mistaken_for_a_user_token() {
    let limited = parse_http_response(
        "HTTP/2.0 403 Forbidden\r\nX-Ratelimit-Remaining: 0\r\n\r\n{\"message\":\"x\"}",
    )
    .unwrap();
    assert!(is_rate_limited(&limited, ""));
    let secondary =
        parse_http_response("HTTP/2.0 403 Forbidden\r\nRetry-After: 60\r\n\r\n{}").unwrap();
    assert!(is_rate_limited(&secondary, ""));
    let refused = parse_http_response(
        "HTTP/2.0 403 Forbidden\r\nX-Ratelimit-Remaining: 4000\r\n\r\n{\"message\":\"You must authenticate with an installation access token\"}",
    )
    .unwrap();
    assert!(!is_rate_limited(&refused, "gh: Forbidden (HTTP 403)"));
    assert!(is_rate_limited(&refused, "gh: API rate limit exceeded (HTTP 403)"));
}

#[test]
fn the_snapshot_is_shared_through_the_store_by_every_process() {
    let env = Env::new(&[]);
    let store_dir = env.tmp.path().join("store");
    crate::forge_etag_store::set_test_daemon_store_dir(Some(store_dir.clone()));
    let fake = Listing::new(env.tmp.path());
    fake.set("page1", &page(1, &[(1, "acme/pub", false)]));
    let cred = writer(&env);
    assert_eq!(lookup(&fake.gh, &cred, "acme/pub"), listed("acme/pub", 1, false));
    // A second process: empty memory, the same store.
    state::set_test_enabled(true);
    assert_eq!(lookup(&fake.gh, &cred, "acme/pub"), listed("acme/pub", 1, false));
    assert_eq!(fake.listing_calls(), 1, "served from the shared store");
    let file = std::fs::read_dir(&store_dir)
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().starts_with("instsnap-"))
        .expect("an instsnap- entry");
    let mode = std::fs::metadata(file.path()).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    crate::forge_etag_store::set_test_daemon_store_dir(None);
}

#[test]
fn credentials_never_share_a_snapshot() {
    let env = Env::new(&[]);
    let fake = Listing::new(env.tmp.path());
    fake.set("page1", &page(1, &[(1, "acme/pub", false)]));
    let a = Credential::writer(Some(env.tmp.path().join("cfg-a")));
    let b = Credential::reader(env.tmp.path().join("cfg-b"), "1", "acme/pub");
    assert_ne!(a.key(), b.key());
    let _ = lookup(&fake.gh, &a, "acme/pub");
    let _ = lookup(&fake.gh, &b, "acme/pub");
    assert_eq!(fake.listing_calls(), 2);
    // A reader's listing never runs under an env token.
    let reader_call = &fake.calls()[1];
    assert!(reader_call.contains("TOKEN=unset"), "{reader_call}");
}

#[test]
fn lookup_repo_with_falls_through_credentials() {
    let env = Env::new(&[]);
    let reader_fake = Listing::new(&env.tmp.path().join("r"));
    let writer_fake = Listing::new(&env.tmp.path().join("w"));
    reader_fake.set("page1", &page(1, &[(1, "acme/one", false)]));
    writer_fake.set("page1", &page(1, &[(2, "acme/two", true)]));
    let reader = Credential::reader(env.tmp.path().join("cfg-r"), "1", "acme/one");
    let w = writer(&env);
    // Two gh stubs stand in for two credentials: look each up once so its
    // snapshot is filed under its own key, then ask through both.
    let _ = lookup(&reader_fake.gh, &reader, "acme/one");
    let _ = lookup(&writer_fake.gh, &w, "acme/two");
    let both = [reader, w];
    assert_eq!(
        lookup_repo_with(&reader_fake.gh, &both, "acme/two"),
        listed("acme/two", 2, true)
    );
    assert_eq!(lookup_repo_with(&reader_fake.gh, &both, "acme/none"), Answer::Listed(None));
}

#[test]
fn the_kill_switches_disable_every_answer() {
    let env = Env::new(&[("LOOM_INSTALLATION_SNAPSHOT", "0")]);
    let fake = Listing::new(env.tmp.path());
    assert_eq!(lookup(&fake.gh, &writer(&env), "acme/pub"), Answer::Disabled);
    drop(env);
    let env = Env::new(&[("LOOM_REPO_FACTS", "0")]);
    assert_eq!(lookup(&fake.gh, &writer(&env), "acme/pub"), Answer::Disabled);
    assert_eq!(fake.listing_calls(), 0);
}

#[test]
fn parse_page_drops_malformed_rows() {
    let body = r#"{"total_count":5,"repositories":[
        {"id":1,"full_name":"a/ok","private":false},
        {"id":0,"full_name":"a/zero","private":false},
        {"id":2,"full_name":"noslash","private":false},
        {"id":3,"full_name":"a/b/c","private":false},
        {"id":4,"full_name":"a/nobool","private":"false"}
    ]}"#;
    let (repos, total) = parse_page(body).unwrap();
    assert_eq!(total, 5);
    assert_eq!(repos.len(), 1, "{repos:?}");
    assert_eq!(repos[0].full_name, "a/ok");
    assert!(parse_page(r#"{"repositories":[]}"#).is_none(), "no total_count");
    assert!(parse_page("not json").is_none());
}

/// The listing is booked under its own inventoried operation
/// (`repo.list-for-installation`), never `unknown`: the constant the fetch
/// names is an active row whose declared caller is this module.
#[test]
fn repo_list_for_installation_is_the_inventoried_operation() {
    let op = crate::forge_call_stats::ops::REPO_LIST_FOR_INSTALLATION;
    assert_eq!(op.id(), Some("repo.list-for-installation"));
    assert!(crate::forge_call_stats::ops::ALL_INVENTORIED.contains(&op));
    let inv = crate::forge_inventory::load_embedded().unwrap();
    let row = inv
        .operations
        .iter()
        .find(|o| Some(o.id.as_str()) == op.id())
        .expect("a row in defaults/forge/operations/*.toml");
    assert!(row.is_active());
    assert!(
        row.callers
            .iter()
            .any(|c| c.path == "loom-daemon/src/forge_repo_facts/installation.rs"),
        "{:?}",
        row.callers
    );
}
