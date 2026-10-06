use super::probe::{classify_repo_permissions, Permission, PermissionProbe};
use super::target::{gh_target, parse_remote_config, parse_remote_url};
use super::*;
use std::cell::Cell;

// ---- gh's base-repo resolution --------------------------------------------

#[test]
fn remote_urls_parse_in_every_common_shape() {
    for (url, host, nwo) in [
        ("git@github.com:acme/widgets.git", "github.com", "acme/widgets"),
        ("git@github.com-work:acme/widgets", "github.com-work", "acme/widgets"),
        ("https://github.com/acme/widgets.git", "github.com", "acme/widgets"),
        ("https://x-access-token@github.com/acme/widgets/", "github.com", "acme/widgets"),
        ("ssh://git@github.com:22/acme/widgets.git", "github.com", "acme/widgets"),
        ("https://GHE.example.com/acme/widgets", "ghe.example.com", "acme/widgets"),
    ] {
        assert_eq!(parse_remote_url(url), Some((host.to_string(), nwo.to_string())), "{url}");
    }
    for bad in [
        "/srv/git/widgets.git",
        "file:///srv/widgets",
        "https://github.com/acme",
        "",
    ] {
        assert_eq!(parse_remote_url(bad), None, "{bad}");
    }
}

fn remotes(config: &str) -> Vec<super::target::Remote> {
    parse_remote_config(config)
}

const FORK: &str = "remote.origin.url https://github.com/me/widgets.git\n\
     remote.origin.fetch +refs/heads/*:refs/remotes/origin/*\n\
     remote.upstream.url https://github.com/acme/widgets.git\n";

#[test]
fn an_upstream_remote_outranks_origin_as_gh_does() {
    let t = gh_target(&remotes(FORK), None).unwrap();
    assert_eq!(t.nwo, "acme/widgets");
    assert_eq!(t.via, "remote `upstream`");
}

#[test]
fn github_outranks_origin_and_other_names_do_not() {
    let r = remotes(
        "remote.fork.url git@github.com:me/w.git\nremote.origin.url git@github.com:acme/w.git\n",
    );
    assert_eq!(gh_target(&r, None).unwrap().nwo, "acme/w", "`fork` ranks below origin");
    let r = remotes(
        "remote.origin.url git@github.com:acme/w.git\nremote.github.url git@github.com:x/w.git\n",
    );
    assert_eq!(gh_target(&r, None).unwrap().nwo, "x/w");
}

#[test]
fn a_set_default_pin_and_gh_repo_override_the_ranking() {
    let pinned = format!("{FORK}remote.origin.gh-resolved base\n");
    assert_eq!(gh_target(&remotes(&pinned), None).unwrap().nwo, "me/widgets");
    let named = format!("{FORK}remote.upstream.gh-resolved me/widgets\n");
    assert_eq!(gh_target(&remotes(&named), None).unwrap().nwo, "me/widgets");
    let t = gh_target(&remotes(FORK), Some("github.com/other/thing")).unwrap();
    assert_eq!(t.nwo, "other/thing");
}

#[test]
fn remotes_on_another_host_are_not_candidates() {
    let r = remotes(
        "remote.origin.url git@github.com:acme/w.git\n\
         remote.upstream.url https://gitlab.com/else/w.git\n",
    );
    assert_eq!(gh_target(&r, None).unwrap().nwo, "acme/w");
    assert_eq!(gh_target(&[], None), None);
}

// ---- the decision ----------------------------------------------------------

struct FakeProbe {
    answer: Permission,
    calls: Cell<u32>,
}

impl FakeProbe {
    fn new(answer: Permission) -> Self {
        Self {
            answer,
            calls: Cell::new(0),
        }
    }
}

impl PermissionProbe for FakeProbe {
    fn permission(&self, _repo: &str) -> Permission {
        self.calls.set(self.calls.get() + 1);
        self.answer.clone()
    }

    fn cache_scope(&self) -> probe::CacheScope {
        probe::CacheScope::GitHub
    }
}

fn target(nwo: &str) -> Option<GhTarget> {
    Some(GhTarget {
        nwo: nwo.into(),
        via: "remote `upstream`".into(),
    })
}

fn run(
    t: Option<GhTarget>,
    explicit: bool,
    origin: Option<&str>,
    managed: &[&str],
    probe: &FakeProbe,
) -> Verdict {
    let m = |r: &str| managed.iter().any(|x| x.eq_ignore_ascii_case(r));
    decide(
        &Inputs {
            target: t,
            explicit,
            origin,
            managed: &m,
        },
        probe,
    )
}

#[test]
fn a_managed_writable_origin_is_allowed_case_insensitively() {
    let p = FakeProbe::new(Permission::Write);
    assert_eq!(
        run(target("Acme/Widgets"), false, Some("acme/widgets"), &["acme/widgets"], &p),
        Verdict::Allow("Acme/Widgets".into())
    );
}

#[test]
fn an_upstream_target_is_refused_before_any_probe() {
    let p = FakeProbe::new(Permission::Write);
    let v = run(target("acme/widgets"), false, Some("me/widgets"), &["acme/widgets"], &p);
    let Verdict::Deny(why) = v else {
        panic!("upstream must be refused")
    };
    assert!(why.contains("not its origin me/widgets"), "{why}");
    assert!(why.contains("gh repo set-default me/widgets"), "{why}");
    assert_eq!(p.calls.get(), 0, "no forge call for a mis-targeted checkout");
}

#[test]
fn an_unmanaged_repo_is_refused_even_with_write_access() {
    let p = FakeProbe::new(Permission::Write);
    let v = run(target("other/repo"), true, Some("acme/widgets"), &["acme/widgets"], &p);
    assert!(
        matches!(&v, Verdict::Deny(w) if w.contains("not a repository this installation manages"))
    );
    assert_eq!(p.calls.get(), 0);
}

#[test]
fn read_access_and_an_unverifiable_probe_both_fail_closed() {
    let read = FakeProbe::new(Permission::Insufficient("repository role `pull`".into()));
    let v = run(target("acme/widgets"), false, Some("acme/widgets"), &["acme/widgets"], &read);
    assert!(matches!(&v, Verdict::Deny(w) if w.contains("cannot write") && w.contains("pull")));
    let down = FakeProbe::new(Permission::Unknown("HTTP 502".into()));
    let v = run(target("acme/widgets"), false, Some("acme/widgets"), &["acme/widgets"], &down);
    assert!(matches!(&v, Verdict::Deny(w) if w.contains("could not verify")));
}

#[test]
fn no_target_or_no_origin_is_refused() {
    let p = FakeProbe::new(Permission::Write);
    assert!(!run(None, false, None, &[], &p).is_allowed());
    assert!(!run(target("acme/widgets"), false, None, &["acme/widgets"], &p).is_allowed());
}

#[test]
fn repo_permissions_classify_user_roles() {
    assert_eq!(classify_repo_permissions(r#"{"push":true}"#), Some(Permission::Write));
    assert_eq!(
        classify_repo_permissions(r#"{"maintain":true,"push":false}"#),
        Some(Permission::Write)
    );
    assert_eq!(
        classify_repo_permissions(r#"{"admin":false,"push":false,"triage":true,"pull":true}"#),
        Some(Permission::Insufficient("repository role `triage`".into()))
    );
    // An App installation token's all-false shape is not a verdict on its own.
    assert_eq!(
        classify_repo_permissions(r#"{"admin":false,"maintain":false,"pull":false,"push":false}"#),
        Some(Permission::Insufficient("repository role `none`".into()))
    );
    assert_eq!(classify_repo_permissions("not json"), None);
}

// ---- a real checkout -------------------------------------------------------

fn git(dir: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?}");
}

#[test]
fn a_fork_checkout_with_an_upstream_remote_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    git(
        dir.path(),
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/me/widgets.git",
        ],
    );
    git(
        dir.path(),
        &[
            "remote",
            "add",
            "upstream",
            "https://github.com/acme/widgets.git",
        ],
    );
    std::fs::create_dir(dir.path().join(".loom")).unwrap();
    let v = root_writable(dir.path());
    let Verdict::Deny(why) = v else {
        panic!("a write resolved through `upstream` must be refused, got {v:?}")
    };
    assert!(
        why.contains("acme/widgets") && why.contains("not its origin me/widgets"),
        "{why}"
    );
}

#[test]
#[serial_test::serial(write_scope_cache)]
fn the_cache_answers_repeat_questions_without_a_second_probe() {
    let dir = tempfile::tempdir().unwrap();
    // SAFETY-free: the variable is read only by this module's cache.
    std::env::set_var("LOOM_WRITE_SCOPE_CACHE_DIR", dir.path().join("c"));
    probe::clear_memory();
    let cached = probe::Cached {
        inner: FakeProbe::new(Permission::Write),
        key_dir: Some(dir.path().join("cred-a")),
    };
    assert_eq!(cached.permission("acme/cache-test"), Permission::Write);
    assert_eq!(cached.permission("acme/cache-test"), Permission::Write);
    assert_eq!(cached.inner.calls.get(), 1, "memory hit");
    probe::clear_memory();
    assert_eq!(cached.permission("acme/cache-test"), Permission::Write);
    assert_eq!(cached.inner.calls.get(), 1, "disk hit");
    let other = probe::Cached {
        inner: FakeProbe::new(Permission::Insufficient("x".into())),
        key_dir: Some(dir.path().join("cred-b")),
    };
    assert!(matches!(other.permission("acme/cache-test"), Permission::Insufficient(_)));
    assert_eq!(other.inner.calls.get(), 1, "a different credential probes again");
    std::env::remove_var("LOOM_WRITE_SCOPE_CACHE_DIR");
}

/// The library-test fixture is admitted by the real decision, and only
/// because its credential reports WRITE: the same registered checkout with a
/// `pull` answer is refused. Neither answer is cached across the two, since
/// each fixture has its own repository.
#[test]
#[serial_test::serial]
fn a_registered_fixture_is_admitted_only_with_write() {
    use crate::write_scope_test_support::WritableRoot;
    let dir = tempfile::tempdir().unwrap();
    let ws = WritableRoot::register(&dir.path().join("writable"));
    assert_eq!(
        root_writable_with(&dir.path().join("writable"), &ws.gh),
        Verdict::Allow(ws.repo.clone())
    );
    let ro = WritableRoot::read_only(&dir.path().join("read-only"), None);
    let v = root_writable_with(&dir.path().join("read-only"), &ro.gh);
    assert!(
        matches!(&v, Verdict::Deny(w) if w.contains("cannot write") && w.contains("pull")),
        "{v:?}"
    );
}

/// Write a disk-cache entry as if the probe had recorded it `age` ago. It is
/// the on-disk format `probe::Cached` reads; the probe itself has no seeding
/// hook.
fn seed_disk(key_dir: Option<&Path>, repo: &str, write: bool, age: std::time::Duration) {
    seed_disk_for(key_dir, &probe::CacheScope::GitHub, repo, write, age);
}

/// [`seed_disk`] under an explicit cache scope.
fn seed_disk_for(
    key_dir: Option<&Path>,
    scope: &probe::CacheScope,
    repo: &str,
    write: bool,
    age: std::time::Duration,
) {
    use std::io::Write as _;
    let dir = probe::cache_dir();
    assert!(crate::forge_etag_store::private_dir(&dir, true));
    let key = probe::cache_key(key_dir, scope, repo).expect("a keyed scope");
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .saturating_sub(age.as_secs());
    let body = serde_json::json!({"write": write, "detail": "seeded", "at": at}).to_string();
    let path = dir.join(format!("{key}.json"));
    // Re-seeding replaces the entry (the file is created exclusively).
    let _ = std::fs::remove_file(&path);
    let mut f = crate::forge_etag_store::create_private_file(&path).unwrap();
    f.write_all(body.as_bytes()).unwrap();
}

#[test]
#[serial_test::serial(write_scope_cache)]
fn an_unanswerable_reprobe_keeps_a_recent_write_but_not_an_old_one() {
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("LOOM_WRITE_SCOPE_CACHE_DIR", dir.path().join("g"));
    probe::clear_memory();
    let hour = std::time::Duration::from_secs(3600);
    let down = || probe::Cached {
        inner: FakeProbe::new(Permission::Unknown("HTTP 502".into())),
        key_dir: Some(dir.path().join("cred-g")),
    };
    // Verified two hours ago: past the TTL, inside the 24 h grace.
    seed_disk(Some(&dir.path().join("cred-g")), "acme/grace", true, 2 * hour);
    let p = down();
    assert_eq!(p.permission("acme/grace"), Permission::Write);
    assert_eq!(p.inner.calls.get(), 1, "it did re-probe");
    // Verified 25 hours ago: the outage now refuses.
    probe::clear_memory();
    seed_disk(Some(&dir.path().join("cred-g")), "acme/old", true, 25 * hour);
    assert!(matches!(down().permission("acme/old"), Permission::Unknown(_)));
    // A definitive "no" is never overridden by an earlier yes.
    probe::clear_memory();
    seed_disk(Some(&dir.path().join("cred-g")), "acme/revoked", false, 2 * hour);
    assert!(matches!(down().permission("acme/revoked"), Permission::Unknown(_)));
    std::env::remove_var("LOOM_WRITE_SCOPE_CACHE_DIR");
}

// ---- Gitea (#9699) ---------------------------------------------------------

#[test]
fn gitea_permissions_classify() {
    use probe::classify_gitea_permissions as classify;
    assert_eq!(
        classify(r#"{"permissions":{"admin":false,"push":true,"pull":true}}"#),
        Some(Permission::Write)
    );
    assert_eq!(
        classify(r#"{"permissions":{"admin":true,"push":false,"pull":true}}"#),
        Some(Permission::Write)
    );
    assert_eq!(
        classify(r#"{"permissions":{"admin":false,"push":false,"pull":true}}"#),
        Some(Permission::Insufficient("repository role `pull`".into()))
    );
    assert_eq!(
        classify(r#"{"permissions":{"admin":false,"push":false,"pull":false}}"#),
        Some(Permission::Insufficient("repository role `none`".into()))
    );
    // Not a repo object with permissions: the caller answers Unknown.
    assert_eq!(classify(r#"{"message":"Not Found"}"#), None);
    assert_eq!(classify(r#"{"permissions":null}"#), None);
    assert_eq!(classify("not json"), None);
}

/// One-request loopback Gitea stub: answers the first connection with
/// `status` + `body` and hands back the raw request bytes, so a test can
/// assert the `Authorization` header arrived (written to curl's stdin by the
/// probe, never its argv).
fn gitea_stub(
    status: &'static str,
    body: &'static str,
) -> (String, std::thread::JoinHandle<String>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 8192];
        let n = stream.read(&mut buf).unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..n]).into_owned();
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
        request
    });
    (format!("http://{addr}"), handle)
}

/// Isolate config resolution from the machine and clear the Gitea env, the
/// way `forge_cmd`'s own env tests do. Callers must hold the
/// `loom_config_env` serial group.
fn isolate_gitea_env() {
    std::env::set_var("LOOM_CONFIG_DEFAULTS_FILE", "");
    for k in ["GITEA_TOKEN", "FORGE_TOKEN", "GITEA_URL", "GITEA_USERNAME"] {
        std::env::remove_var(k);
    }
}

fn clear_gitea_env() {
    for k in ["GITEA_TOKEN", "FORGE_TOKEN", "GITEA_URL", "GITEA_USERNAME"] {
        std::env::remove_var(k);
    }
}

#[test]
#[serial_test::serial(loom_config_env)]
fn a_gitea_push_credential_is_write_through_a_stub_api() {
    isolate_gitea_env();
    let (base, request) = gitea_stub(
        "200 OK",
        r#"{"full_name":"acme/w","permissions":{"admin":false,"push":true,"pull":true}}"#,
    );
    std::env::set_var("GITEA_URL", &base);
    std::env::set_var("GITEA_TOKEN", "stub-token");
    let dir = tempfile::tempdir().unwrap();
    let probe = probe::GiteaProbe::for_root(dir.path());
    assert_eq!(probe.permission("acme/w"), Permission::Write);
    let request = request.join().unwrap();
    assert!(
        request.contains("Authorization: token stub-token"),
        "the credential must ride the request: {request}"
    );
    clear_gitea_env();
}

#[test]
#[serial_test::serial(loom_config_env)]
fn a_gitea_pull_only_credential_is_insufficient() {
    isolate_gitea_env();
    let (base, request) =
        gitea_stub("200 OK", r#"{"permissions":{"admin":false,"push":false,"pull":true}}"#);
    std::env::set_var("GITEA_URL", &base);
    std::env::set_var("GITEA_TOKEN", "stub-token");
    let dir = tempfile::tempdir().unwrap();
    let probe = probe::GiteaProbe::for_root(dir.path());
    assert_eq!(
        probe.permission("acme/w"),
        Permission::Insufficient("repository role `pull`".into())
    );
    request.join().unwrap();
    clear_gitea_env();
}

#[test]
#[serial_test::serial(loom_config_env)]
fn a_gitea_api_that_refuses_fails_closed() {
    isolate_gitea_env();
    let (base, request) = gitea_stub("404 Not Found", r#"{"message":"Not Found"}"#);
    std::env::set_var("GITEA_URL", &base);
    std::env::set_var("GITEA_TOKEN", "stub-token");
    let dir = tempfile::tempdir().unwrap();
    let probe = probe::GiteaProbe::for_root(dir.path());
    let p = probe.permission("acme/w");
    assert!(
        matches!(p, Permission::Unknown(ref why) if why.contains("404")),
        "a non-200 must be Unknown naming the status, got {p:?}"
    );
    request.join().unwrap();
    clear_gitea_env();
}

#[test]
#[serial_test::serial(loom_config_env)]
fn gitea_falls_back_to_forge_token_and_to_unknown_without_any() {
    isolate_gitea_env();
    let (base, request) =
        gitea_stub("200 OK", r#"{"permissions":{"admin":false,"push":true,"pull":true}}"#);
    std::env::set_var("GITEA_URL", &base);
    std::env::set_var("FORGE_TOKEN", "generic-token");
    let dir = tempfile::tempdir().unwrap();
    let probe = probe::GiteaProbe::for_root(dir.path());
    assert_eq!(probe.permission("acme/w"), Permission::Write);
    assert!(
        request
            .join()
            .unwrap()
            .contains("Authorization: token generic-token"),
        "FORGE_TOKEN is the generic fallback credential"
    );
    // With neither token configured the probe cannot answer — before any
    // network call — and the caller fails closed.
    clear_gitea_env();
    std::env::set_var("GITEA_URL", "http://127.0.0.1:9");
    let probe = probe::GiteaProbe::for_root(dir.path());
    let p = probe.permission("acme/w");
    assert!(
        matches!(p, Permission::Unknown(ref why) if why.contains("token is required")),
        "no credential at all must be Unknown naming why, got {p:?}"
    );
    clear_gitea_env();
}

#[test]
#[serial_test::serial(loom_config_env)]
fn gitea_env_overrides_beat_the_config_file() {
    isolate_gitea_env();
    let (base, request) =
        gitea_stub("200 OK", r#"{"permissions":{"admin":false,"push":true,"pull":true}}"#);
    let dir = tempfile::tempdir().unwrap();
    let loom = dir.path().join(".loom");
    std::fs::create_dir_all(&loom).unwrap();
    // A config that would answer from a dead port if it won.
    std::fs::write(
        loom.join("config.json"),
        r#"{"forge":{"gitea":{"url":"http://127.0.0.1:9","token":"config-token"}}}"#,
    )
    .unwrap();
    std::env::set_var("GITEA_URL", &base);
    std::env::set_var("GITEA_TOKEN", "env-token");
    let probe = probe::GiteaProbe::for_root(dir.path());
    assert_eq!(probe.permission("acme/w"), Permission::Write);
    assert!(
        request
            .join()
            .unwrap()
            .contains("Authorization: token env-token"),
        "env beats config for both url and token"
    );
    clear_gitea_env();
}

#[test]
#[serial_test::serial(loom_config_env)]
fn gitea_cache_ids_differ_by_connection() {
    isolate_gitea_env();
    let mk = |url: &str, token: &str| {
        let dir = tempfile::tempdir().unwrap();
        let loom = dir.path().join(".loom");
        std::fs::create_dir_all(&loom).unwrap();
        std::fs::write(
            loom.join("config.json"),
            format!(r#"{{"forge":{{"gitea":{{"url":"{url}","token":"{token}"}}}}}}"#),
        )
        .unwrap();
        dir
    };
    let a = mk("https://gitea.one.example.com", "tok-a");
    let b = mk("https://gitea.two.example.com", "tok-b");
    let pa = probe::GiteaProbe::for_root(a.path());
    let pa2 = probe::GiteaProbe::for_root(a.path());
    let pb = probe::GiteaProbe::for_root(b.path());
    assert_eq!(pa.cache_id(), pa2.cache_id(), "same connection, same key");
    assert_ne!(pa.cache_id(), pb.cache_id(), "two Gitea workspaces never share a cache entry");
    // An unresolvable connection has no id: it never shares a cache entry.
    let empty = tempfile::tempdir().unwrap();
    assert_eq!(probe::GiteaProbe::for_root(empty.path()).cache_id(), "");
    clear_gitea_env();
}

#[test]
#[serial_test::serial(loom_config_env, write_scope_cache)]
fn a_seeded_gitea_cache_entry_answers_through_the_same_connection_id() {
    isolate_gitea_env();
    let dir = tempfile::tempdir().unwrap();
    let loom = dir.path().join(".loom");
    std::fs::create_dir_all(&loom).unwrap();
    // The config URL is a dead port: a cache miss would probe and answer
    // Unknown, so a Write here can only come from the cache.
    std::fs::write(
        loom.join("config.json"),
        r#"{"forge":{"gitea":{"url":"http://127.0.0.1:9","token":"config-token"}}}"#,
    )
    .unwrap();
    std::env::set_var("LOOM_WRITE_SCOPE_CACHE_DIR", dir.path().join("c"));
    probe::clear_memory();
    let scope = probe::GiteaProbe::for_root(dir.path()).cache_scope();
    assert!(matches!(scope, probe::CacheScope::Gitea { .. }), "{scope:?}");
    seed_disk_for(None, &scope, "acme/g", true, std::time::Duration::from_secs(60));
    let cached = probe::Cached {
        inner: probe::GiteaProbe::for_root(dir.path()),
        key_dir: None,
    };
    assert_eq!(cached.permission("acme/g"), Permission::Write);
    std::env::remove_var("LOOM_WRITE_SCOPE_CACHE_DIR");
    clear_gitea_env();
}

/// The Judge's repro on #9817: a GitHub WRITE for the same `owner/repo`
/// slug, under the same credential dir, must never answer a Gitea probe —
/// neither inside the TTL (a direct cache hit) nor inside the 24 h grace (an
/// `Unknown` upgraded to WRITE).
#[test]
#[serial_test::serial(loom_config_env, write_scope_cache)]
fn a_github_write_never_answers_an_unresolved_gitea_probe() {
    isolate_gitea_env();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("LOOM_WRITE_SCOPE_CACHE_DIR", dir.path().join("c"));
    let cred = dir.path().join("cred");
    let unresolved = || probe::Cached {
        // No URL, no token: the connection cannot resolve.
        inner: probe::GiteaProbe::for_root(dir.path()),
        key_dir: Some(cred.clone()),
    };
    assert_eq!(unresolved().cache_scope(), probe::CacheScope::Unresolved);
    for (what, age) in [("inside the TTL", 60), ("inside the grace", 2 * 3600)] {
        probe::clear_memory();
        seed_disk(Some(&cred), "acme/w", true, std::time::Duration::from_secs(age));
        // Warm the memory side too, as a GitHub probe in this process would.
        let github = probe::Cached {
            inner: FakeProbe::new(Permission::Write),
            key_dir: Some(cred.clone()),
        };
        assert_eq!(github.permission("acme/w"), Permission::Write);
        let p = unresolved().permission("acme/w");
        assert!(
            matches!(p, Permission::Unknown(_)),
            "{what}: an unresolved Gitea connection must refuse, got {p:?}"
        );
    }
    std::env::remove_var("LOOM_WRITE_SCOPE_CACHE_DIR");
    clear_gitea_env();
}

/// A resolved Gitea connection is keyed apart from GitHub as well: a GitHub
/// WRITE for the same slug and credential dir does not satisfy it, inside the
/// TTL or the grace (its own probe, a dead port, cannot answer).
#[test]
#[serial_test::serial(loom_config_env, write_scope_cache)]
fn a_github_write_never_answers_a_resolved_gitea_probe() {
    isolate_gitea_env();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("LOOM_WRITE_SCOPE_CACHE_DIR", dir.path().join("c"));
    std::env::set_var("GITEA_URL", "http://127.0.0.1:9");
    std::env::set_var("GITEA_TOKEN", "gitea-token");
    let cred = dir.path().join("cred");
    for age in [60, 2 * 3600] {
        probe::clear_memory();
        seed_disk(Some(&cred), "acme/w", true, std::time::Duration::from_secs(age));
        let gitea = probe::Cached {
            inner: probe::GiteaProbe::for_root(dir.path()),
            key_dir: Some(cred.clone()),
        };
        assert!(matches!(gitea.cache_scope(), probe::CacheScope::Gitea { .. }));
        let p = gitea.permission("acme/w");
        assert!(
            matches!(p, Permission::Unknown(_)),
            "a GitHub entry {age}s old must not answer Gitea, got {p:?}"
        );
    }
    std::env::remove_var("LOOM_WRITE_SCOPE_CACHE_DIR");
    clear_gitea_env();
}

/// The reverse: a cached Gitea WRITE never satisfies the GitHub probe for the
/// same slug and credential dir.
#[test]
#[serial_test::serial(loom_config_env, write_scope_cache)]
fn a_gitea_write_never_answers_the_github_probe() {
    isolate_gitea_env();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("LOOM_WRITE_SCOPE_CACHE_DIR", dir.path().join("c"));
    std::env::set_var("GITEA_URL", "http://127.0.0.1:9");
    std::env::set_var("GITEA_TOKEN", "gitea-token");
    probe::clear_memory();
    let cred = dir.path().join("cred");
    let scope = probe::GiteaProbe::for_root(dir.path()).cache_scope();
    seed_disk_for(Some(&cred), &scope, "acme/w", true, std::time::Duration::from_secs(60));
    let github = probe::Cached {
        inner: FakeProbe::new(Permission::Insufficient("repository role `pull`".into())),
        key_dir: Some(cred.clone()),
    };
    assert_eq!(
        github.permission("acme/w"),
        Permission::Insufficient("repository role `pull`".into())
    );
    assert_eq!(github.inner.calls.get(), 1, "the GitHub probe had to ask");
    std::env::remove_var("LOOM_WRITE_SCOPE_CACHE_DIR");
    clear_gitea_env();
}

/// The two key spaces are disjoint by construction, and an unresolved
/// connection (or a resolved one with no id) has no key at all.
#[test]
fn cache_keys_are_namespaced_by_forge_and_host() {
    use probe::CacheScope;
    let dir = Some(Path::new("/cred"));
    let gitea = |url: &str| CacheScope::Gitea {
        base_url: url.into(),
        id: "abc".into(),
    };
    let gh = probe::cache_key(dir, &CacheScope::GitHub, "acme/w").unwrap();
    let ga = probe::cache_key(dir, &gitea("https://a.example"), "acme/w").unwrap();
    let gb = probe::cache_key(dir, &gitea("https://b.example"), "acme/w").unwrap();
    assert_ne!(gh, ga);
    assert_ne!(ga, gb, "the base URL is part of the key");
    assert_eq!(probe::cache_key(dir, &CacheScope::Unresolved, "acme/w"), None);
    let no_id = CacheScope::Gitea {
        base_url: "https://a.example".into(),
        id: String::new(),
    };
    assert_eq!(probe::cache_key(dir, &no_id, "acme/w"), None);
}

#[test]
#[serial_test::serial(loom_config_env)]
fn a_gitea_credential_with_a_line_break_does_not_resolve() {
    isolate_gitea_env();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("GITEA_URL", "http://127.0.0.1:9");
    std::env::set_var("GITEA_TOKEN", "tok\r\nX-Injected: 1");
    let gitea = probe::GiteaProbe::for_root(dir.path());
    assert_eq!(gitea.cache_scope(), probe::CacheScope::Unresolved);
    let p = gitea.permission("acme/w");
    assert!(matches!(p, Permission::Unknown(ref why) if why.contains("line break")), "{p:?}");
    clear_gitea_env();
}

// ---- structural: every daemon write path is scoped -------------------------

/// How a reviewed file's forge writes are scoped.
enum Scope {
    /// Calls `write_scope::` itself before writing.
    Gated,
    /// Reached only through the named file, which is `Gated`.
    Via(&'static str, &'static str),
    /// A CLI verb whose only caller is the named shell script, which vets
    /// the repo with `loom_write_repo` and passes it as `--repo`.
    ShellVetted(&'static str),
    /// A CLI verb whose target is `git remote get-url origin` or an explicit
    /// `--repo` from its caller, never gh's base-repo resolution; the file
    /// named must keep resolving origin first.
    OriginResolved(&'static str),
    /// An operator-run verb with an explicit, configured target, never run by
    /// an autonomous path. Not write-scoped yet; listed so the gap is visible.
    OperatorOnly(&'static str),
    /// An **autonomous** daemon write to the configured fleet store
    /// (`fleet.repo`), which is not a managed workspace repo, so
    /// `write_scope`'s managed + root-credential rule cannot vet it (it would
    /// refuse every store that is not some workspace's origin). Scoped instead
    /// by construction, and each part is asserted: the target comes from config
    /// via `fleet_store::resolve_location`, never gh base-repo resolution; the
    /// write goes only through the store's `WriteTransport` (the writer App,
    /// `GhTransport::write_raw`), never a `gh` child of its own; the file
    /// refuses the store's reviewed branch (`refuse_reviewed_branch`); and the
    /// named caller reaches the named call only under `RefreshGate::Captain`,
    /// so only the declared captain writes.
    FleetStore {
        caller: &'static str,
        call: &'static str,
        why: &'static str,
    },
    /// Matches the pattern but is not a forge write.
    NotAWrite(&'static str),
}

/// Structural guard (#9548): a daemon source file that issues a forge write
/// (a comment, a label edit, a REST mutation, a GraphQL mutation) must be
/// reviewed into this list, and a file that claims to be gated must actually
/// call `write_scope`. A new write path therefore cannot silently skip the
/// managed-repo + WRITE check.
#[test]
fn daemon_write_paths_are_scoped() {
    use Scope::{FleetStore, Gated, NotAWrite, OperatorOnly, OriginResolved, ShellVetted, Via};
    const PASS: &str = "claim_reconciliation/pass_loop.rs";
    const DISPATCH: &str = "sweep_registry/private_dispatch.rs";
    let reviewed: &[(&str, Scope)] = &[
        (PASS, Gated),
        ("claim_reconciliation.rs", Via(PASS, "reclaim + anchor passes")),
        ("claim_reconciliation/verdict_invalidation.rs", Via(PASS, "verdict pass")),
        ("claim_reconciliation/review_conflict.rs", Via(PASS, "conflict pass")),
        ("claim_reconciliation/merge_sequence.rs", Via(PASS, "merge-sequence pass")),
        ("claim_reconciliation/merge_sequence_stall.rs", Via(PASS, "merge-sequence stall escalation")),
        (
            "claim_reconciliation/merge_sequence_sticky.rs",
            Via(PASS, "merge-sequence sticky operator-release record"),
        ),
        ("claim_reconciliation/pass_loop/building_heal.rs", Via(PASS, "heal pass")),
        (
            "forge_disable_auto_merge.rs",
            Via(PASS, "verdict pass; shell guard vets its own call"),
        ),
        ("quarantine_reconciliation.rs", Gated),
        ("worktree_ops/gh.rs", Gated),
        ("star_liveness/task.rs", Gated),
        (
            "star_liveness/forge.rs",
            Via("star_liveness/task.rs", "repos pass the task's gate"),
        ),
        ("role_runner/roster.rs", Gated),
        ("dep_classify/forge.rs", OriginResolved("dep_classify/cli.rs")),
        ("cli/notify_cleared_blockers.rs", ShellVetted("merge-pr.sh")),
        (DISPATCH, Gated),
        ("work_finder/pool_preflight.rs", Gated),
        ("intake_reconcile.rs", Gated),
        (
            "sweep_registry/guards.rs",
            Via(DISPATCH, "claim flip + lease of a dispatched sweep"),
        ),
        ("sweep_registry/watchdog.rs", Via(DISPATCH, "acts on dispatched sweeps")),
        ("sweep_registry/restore_to_ready.rs", Via(DISPATCH, "acts on dispatched sweeps")),
        ("sweep_registry/quarantine.rs", Via(DISPATCH, "acts on dispatched sweeps")),
        (
            "sweep_registry/prless_retry/hold.rs",
            Via(DISPATCH, "acts on dispatched sweeps"),
        ),
        (
            "sweep_registry/outcome_journal/writeback.rs",
            Via(DISPATCH, "dispatched sweeps"),
        ),
        (
            "script_helpers/validate_phase.rs",
            Via(DISPATCH, "runs inside a dispatched sweep"),
        ),
        ("merge_pr/redate.rs", ShellVetted("merge-pr.sh")),
        (
            "forge_cmd.rs",
            Via("cli/forge_action.rs", "every writing forge verb is vetted first"),
        ),
        (
            "forge_comment.rs",
            Via("cli/forge_action.rs", "the `forge comment` verb is vetted via write_target; the internal `post_comment` sites are pre-vetted by their own callers"),
        ),
        ("cli/forge_action.rs", Gated),
        ("role_runner/launch.rs", Gated),
        ("operator_decision/cli.rs", Gated),
        (
            "fleet/drain_reset.rs",
            OperatorOnly("`fleet drain`: the operator's own worker, by name (the claim resetter, split out of fleet/drain.rs by #10089)"),
        ),
        (
            "fleet_store/propose/mod.rs",
            OperatorOnly("`fleet-config propose`: a PR against the configured store"),
        ),
        (
            "eta/fit/publish.rs",
            FleetStore {
                caller: "observability/eta_fleet_refresh.rs",
                call: "distribute_publish(root, &captain",
                why: "the captain publishes its ETA fit to `fleet.etaFitRef` every refresh cycle (#10395)",
            },
        ),
        (
            "cli/merge_pr_consolidate.rs",
            OperatorOnly("`merge-pr consolidate-prepare|abort|reconcile`: run by hand against an explicit candidate PR; #9839 is the automated caller"),
        ),
        (
            "merge_pr/consolidate/reconcile.rs",
            OperatorOnly("library half of `consolidate-reconcile`, reached only via the CLI verb above"),
        ),
        (
            "watchdog/peer_coord.rs",
            NotAWrite("follow-up on the issue this watchdog filed"),
        ),
        (
            "dep_recheck/decide.rs",
            NotAWrite("action names; writes go through dep_classify"),
        ),
        ("role_tick_telemetry.rs", NotAWrite("classifies commands, runs none")),
        (
            "gh_invocation/accounting.rs",
            NotAWrite("classifies an invocation's argv for call accounting, runs none"),
        ),
        (
            "gh_invocation/affinity.rs",
            NotAWrite("derives a read's routing key from argv; only its tests name `--method`"),
        ),
        (
            "gh_invocation/cwd_route.rs",
            NotAWrite("classifies an invocation's argv to refuse mutations a reader route, runs none"),
        ),
        (
            "gh_invocation/api_kind.rs",
            NotAWrite("classifies an invocation's argv for the github.api span attribute, runs none"),
        ),
        ("role_tick_telemetry/targets.rs", NotAWrite("classifies commands, runs none")),
        ("terminal.rs", NotAWrite("tmux flags")),
        ("fleet_store/gh.rs", NotAWrite("store reads: its one method is `--method GET`")),
        (
            "merge_group_ci/eligibility.rs",
            NotAWrite("read-only probe: its only calls are `api --method GET`"),
        ),
        ("tokens_pool/check.rs", NotAWrite("Anthropic API, not the forge")),
        ("worker_spawn/egress_proxy/server.rs", NotAWrite("HTTP method check in a proxy")),
    ];
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let writes = regex::Regex::new(
        r#""--add-label"|"--remove-label"|"comment"|"--method"|"-X"|"(POST|PATCH|PUT|DELETE)"|mutation\s*\(\$|mutation\s*\{"#,
    )
    .unwrap();
    let is_test = |rel: &str| {
        rel.ends_with("tests.rs")
            || rel.contains("/tests/")
            || rel.starts_with("tests/")
            || rel.contains("test_support")
    };
    let mut unreviewed = Vec::new();
    let mut sources: Vec<(String, String)> = Vec::new();
    let mut stack = vec![src.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let rel = path
                .strip_prefix(&src)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            if !is_test(&rel) {
                sources.push((rel.clone(), text.clone()));
            }
            if !is_test(&rel) && writes.is_match(&text) && !reviewed.iter().any(|(f, _)| *f == rel)
            {
                unreviewed.push(rel);
            }
        }
    }
    assert!(
        unreviewed.is_empty(),
        "new daemon forge-write paths must be scoped by crate::write_scope (#9548) and listed \
         here: {unreviewed:?}"
    );
    let gated = |f: &str| {
        std::fs::read_to_string(src.join(f))
            .unwrap_or_default()
            .contains("write_scope::")
    };
    let matches =
        |f: &str| writes.is_match(&std::fs::read_to_string(src.join(f)).unwrap_or_default());
    for (file, scope) in reviewed {
        match scope {
            Gated => assert!(gated(file), "{file} is listed as gated but never calls write_scope"),
            Via(parent, why) => {
                assert!(
                    gated(parent)
                        && reviewed
                            .iter()
                            .any(|(f, s)| f == parent && matches!(s, Gated)),
                    "{file} ({why}) is reached via {parent}, which must be a gated entry"
                );
                assert!(matches(file), "stale entry: {file} ({why}) no longer writes");
            }
            ShellVetted(script) => {
                let text = std::fs::read_to_string(src.join("../../defaults/scripts").join(script))
                    .unwrap_or_default();
                assert!(
                    text.contains("loom_write_repo") && matches(file),
                    "{file} relies on {script} vetting its --repo with loom_write_repo"
                );
            }
            OriginResolved(resolver) => {
                let text = std::fs::read_to_string(src.join(resolver)).unwrap_or_default();
                let origin = text.find(r#"["remote", "get-url", "origin"]"#);
                let gh = text.find(r#"["repo", "view""#);
                assert!(
                    matches(file) && origin.is_some() && gh.is_none_or(|g| origin < Some(g)),
                    "{file} relies on {resolver} resolving origin before any gh repo view"
                );
            }
            FleetStore { caller, call, why } => {
                let text = std::fs::read_to_string(src.join(file)).unwrap_or_default();
                assert!(matches(file), "stale entry: {file} ({why}) no longer writes");
                // The guard is the first statement of `publish`, not merely defined.
                let guarded = text.find("pub fn publish(").is_some_and(|f| {
                    text[f..]
                        .find(") -> Result<PublishKind> {")
                        .map(|b| text[f + b..].trim_start_matches(") -> Result<PublishKind> {"))
                        .is_some_and(|body| {
                            body.trim_start()
                                .starts_with("refuse_reviewed_branch(loc, base_ref)?;")
                        })
                });
                assert!(
                    text.contains("fleet_store::resolve_location(")
                        && text.contains("WriteTransport")
                        && guarded
                        && !text.contains("Command::new"),
                    "{file} ({why}) must take its target from fleet_store::resolve_location, \
                     write only through WriteTransport and refuse the reviewed branch"
                );
                let caller_text = std::fs::read_to_string(src.join(caller)).unwrap_or_default();
                let gate = caller_text.find("RefreshGate::Captain)");
                let at = caller_text.find(call);
                assert!(
                    gate.is_some() && at.is_some() && gate < at,
                    "{file} ({why}): {caller} must reach `{call}` only under RefreshGate::Captain"
                );
                let name = call.split('(').next().unwrap_or(call);
                let callers: Vec<&str> = sources
                    .iter()
                    .filter(|(_, t)| {
                        t.replace(&format!("fn {name}("), "")
                            .contains(&format!("{name}("))
                    })
                    .map(|(rel, _)| rel.as_str())
                    .collect();
                assert_eq!(callers, [*caller], "{file} ({why}): only {caller} may call `{name}`");
            }
            OperatorOnly(why) | NotAWrite(why) => {
                assert!(matches(file), "stale entry: {file} ({why})");
            }
        }
    }
    // The reconciliation gate sits in front of every pass it covers.
    let pass = std::fs::read_to_string(src.join(PASS)).unwrap();
    let gate_at = pass.find("write_scope::gate_root").unwrap();
    let first_pass = pass.find("forge::reconcile_workspace(").unwrap();
    assert!(gate_at < first_pass, "the write-scope gate must precede the passes");
}

/// The shell counterpart of [`daemon_write_paths_are_scoped`] (#9548): a
/// script in `defaults/scripts` (or its `lib/`) that writes to the forge on a
/// code line must vet its target with `loom_write_repo`, or be reviewed into
/// `EXEMPT` with the reason it needs no vetting. Stale exemptions fail too.
#[test]
fn shell_write_paths_are_vetted() {
    const EXEMPT: &[(&str, &str)] = &[
        ("create-issue.sh", "files through forge_gh_create_issue_rl_safe, which vets"),
        ("lib/github-app-token.sh", "POSTs to the App token endpoint, not to a repo"),
        ("check-duplicate.sh", "`gh issue create` appears only in its usage text"),
        ("land-resync-commit.sh", "`gh pr create` appears only in a printed hint"),
        ("worktree.sh", "`gh pr create` appears only in a printed hint"),
        ("docs-guide-lock.sh", "`gh pr create` appears only in its usage text"),
    ];
    let scripts = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/scripts");
    let writes = regex::Regex::new(
        r#"\bgh (issue|pr) (comment|edit|close|reopen|merge|create|ready|review|lock)\b|\bgh label (create|edit|delete)\b|(-X|--method) *"?(POST|PATCH|PUT|DELETE)\b|\bforge_gh_(comment|swap_label|remove_label|reopen_issue|create_issue)_rl_safe\b|\bforge_merge_pr\b"#,
    )
    .unwrap();
    let writes_on_code = |text: &str| {
        text.lines()
            .map(str::trim_start)
            .any(|l| !l.starts_with('#') && writes.is_match(l))
    };
    let mut offenders = Vec::new();
    for dir in [scripts.clone(), scripts.join("lib")] {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("sh") {
                continue;
            }
            let rel = path
                .strip_prefix(&scripts)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let exempt = EXEMPT.iter().find(|(f, _)| *f == rel);
            if let Some((f, why)) = exempt {
                assert!(writes_on_code(&text), "stale exemption: {f} ({why}) no longer matches");
                continue;
            }
            if writes_on_code(&text) && !text.contains("loom_write_repo") {
                offenders.push(rel);
            }
        }
    }
    offenders.sort();
    assert!(
        offenders.is_empty(),
        "shell scripts that write to the forge must vet the repo with loom_write_repo \
         (lib/forge-helpers.sh) and name it on the write, or be listed here with a reason \
         (#9548): {offenders:?}"
    );
}
