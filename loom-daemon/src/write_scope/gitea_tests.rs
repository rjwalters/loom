//! Gitea write-scope tests (#9699), split out of `tests.rs` to keep that file
//! under the file-size budget. Shared fixtures (`FakeProbe`, `seed_disk`,
//! `seed_disk_for`) stay in `tests.rs`.

use super::probe::{Permission, PermissionProbe};
use super::tests::{seed_disk, seed_disk_for, FakeProbe};
use super::*;

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
