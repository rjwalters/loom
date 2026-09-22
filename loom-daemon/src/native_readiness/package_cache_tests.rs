//! Cache tests: the key must carry the full identity, and no worker state may
//! ever become shareable through it.
use super::*;

/// A home-shaped fixture: `home/` acts as `$HOME`, so the under-home rule is
/// exercised for real without writing into the operator's actual home.
fn fixture() -> (tempfile::TempDir, PathBuf, PackageCache) {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let cache = PackageCache::open_under(&home.join("cache"), &home).unwrap();
    (tmp, home, cache)
}

fn identity() -> CacheIdentity {
    CacheIdentity::new(
        "1.18.31",
        "@opencode-ai/plugin@1.18.31",
        br#"{"private":true,"dependencies":{"@opencode-ai/plugin":"1.18.31"}}"#,
    )
}

/// A built tree with a realistic package layout.
fn built(root: &Path) -> PathBuf {
    let built = root.join("built");
    std::fs::create_dir_all(built.join("node_modules/@opencode-ai/plugin")).unwrap();
    std::fs::write(
        built.join("node_modules/@opencode-ai/plugin/index.js"),
        "export const plugin = 1;\n",
    )
    .unwrap();
    std::fs::write(built.join("node_modules/.package-lock.json"), r#"{"lockfileVersion":3}"#)
        .unwrap();
    built
}

#[test]
fn key_includes_version_pin_platform_arch_and_manifest_integrity() {
    let base = identity();
    let key = base.key();
    assert_eq!(key.len(), 64, "a sha256 hex digest");
    assert_eq!(key, base.key(), "the key must be stable");

    /// Mutate exactly one identity field, so the assertion below proves that
    /// field is load-bearing in the key rather than merely stored alongside it.
    type Mutation = (&'static str, fn(&mut CacheIdentity));
    let mutations: [Mutation; 5] = [
        ("cli_version", |i| i.cli_version = "1.18.32".into()),
        ("plugin_pin", |i| {
            i.plugin_pin = "@opencode-ai/plugin@1.19.0".into();
        }),
        ("platform", |i| i.platform = "macos".into()),
        ("arch", |i| i.arch = "aarch64".into()),
        ("manifest_digest", |i| i.manifest_digest = "deadbeef".into()),
    ];
    for (field, apply) in mutations {
        let mut changed = base.clone();
        apply(&mut changed);
        assert_ne!(changed.key(), key, "{field} must be part of the cache key");
    }

    // The manifest digest really is a digest of the bytes, not of a label.
    let other = CacheIdentity::new("1.18.31", "@opencode-ai/plugin@1.18.31", b"{}");
    assert_ne!(other.key(), key, "manifest bytes must change the key");
    // Length-prefixing: no field-boundary collision between distinct identities.
    let a = CacheIdentity::new("1.1", "8.31", b"x");
    let b = CacheIdentity::new("1.18", ".31", b"x");
    assert_ne!(a.key(), b.key());
}

#[test]
fn publish_then_lookup_hits_and_restores_only_package_artifacts() {
    let (tmp, _home, cache) = fixture();
    let built = built(tmp.path());
    let identity = identity();
    assert_eq!(cache.lookup(&identity).unwrap().1, CacheOutcome::Miss);

    let entry = cache.publish(&identity, &built, &["node_modules"]).unwrap();
    assert!(entry.starts_with(cache.base()));
    let (found, outcome) = cache.lookup(&identity).unwrap();
    assert_eq!(outcome, CacheOutcome::Hit);
    assert_eq!(found.as_deref(), Some(entry.as_path()));

    let restored = tmp.path().join("restored");
    std::fs::create_dir(&restored).unwrap();
    cache.restore(&entry, &restored).unwrap();
    assert_eq!(
        std::fs::read_to_string(restored.join("node_modules/@opencode-ai/plugin/index.js"))
            .unwrap(),
        "export const plugin = 1;\n"
    );
    // The integrity record describes the entry; it is not handed to a worker.
    assert!(!restored.join(MANIFEST).exists());

    // A different identity never sees this entry.
    let mut other = identity.clone();
    other.cli_version = "2.0.10".into();
    assert_eq!(cache.lookup(&other).unwrap().1, CacheOutcome::Miss);
}

#[test]
fn auth_session_db_and_config_state_cannot_cross_into_a_shared_entry() {
    let (tmp, _home, cache) = fixture();
    let built = built(tmp.path());
    let identity = identity();

    // Named directly: each of these must be refused outright.
    for name in [
        "auth.json",
        "config.json",
        "credentials.json",
        ".env",
        "opencode.db",
        "sessions.sqlite",
        "server.pem",
        "id.key",
    ] {
        std::fs::write(built.join(name), "worker state").unwrap();
        let error = cache
            .publish(&identity, &built, &[name])
            .expect_err(name)
            .to_string();
        assert!(error.contains("refusing to share"), "{name} must be refused: {error}");
        std::fs::remove_file(built.join(name)).unwrap();
    }

    // Hidden inside an allowlisted directory: publication FAILS rather than
    // silently dropping the file, so a widened allowlist cannot leak state.
    std::fs::write(built.join("node_modules/auth.json"), "{}").unwrap();
    let error = cache
        .publish(&identity, &built, &["node_modules"])
        .expect_err("nested auth state")
        .to_string();
    assert!(error.contains("refusing to share"), "{error}");
    std::fs::remove_file(built.join("node_modules/auth.json")).unwrap();

    // A per-worker state directory component is refused by name.
    std::fs::create_dir(built.join("node_modules/sessions")).unwrap();
    std::fs::write(built.join("node_modules/sessions/a.json"), "{}").unwrap();
    let error = cache
        .publish(&identity, &built, &["node_modules"])
        .expect_err("session tree")
        .to_string();
    assert!(error.contains("mutable worker state"), "{error}");
    std::fs::remove_dir_all(built.join("node_modules/sessions")).unwrap();

    // Nothing partial survived any of those failures.
    assert_eq!(cache.lookup(&identity).unwrap().1, CacheOutcome::Miss);
    let staging = cache.base().join(".staging");
    if staging.exists() {
        assert_eq!(std::fs::read_dir(&staging).unwrap().count(), 0);
    }
    // And the good path still works afterwards.
    cache.publish(&identity, &built, &["node_modules"]).unwrap();
}

#[test]
fn traversal_absolute_paths_and_symlinks_are_refused() {
    let (tmp, _home, cache) = fixture();
    let built = built(tmp.path());
    let identity = identity();
    for name in ["../escape", "/etc/passwd", "node_modules/../../escape"] {
        assert!(cache.publish(&identity, &built, &[name]).is_err(), "{name}");
    }
    assert!(
        cache.publish(&identity, &built, &[]).is_err(),
        "publishing nothing must fail rather than create an empty entry"
    );
    #[cfg(unix)]
    {
        let secret = tmp.path().join("secret");
        std::fs::write(&secret, "token").unwrap();
        std::os::unix::fs::symlink(&secret, built.join("node_modules/link.js")).unwrap();
        let error = cache
            .publish(&identity, &built, &["node_modules"])
            .expect_err("symlink")
            .to_string();
        assert!(error.contains("symlink"), "{error}");
    }
}

#[test]
fn cache_base_must_be_under_home_and_outside_every_repository() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir(&home).unwrap();

    // Outside the home boundary.
    let outside = tmp.path().join("elsewhere");
    let error = PackageCache::open_under(&outside, &home)
        .expect_err("outside home")
        .to_string();
    assert!(error.contains("user home"), "{error}");

    // Inside a repository checkout under home.
    let repo = home.join("repo");
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(repo.join(".git"), "gitdir: fixture").unwrap();
    let error = PackageCache::open_under(&repo.join("cache"), &home)
        .expect_err("inside a checkout")
        .to_string();
    assert!(error.contains("outside every repository"), "{error}");

    // The permitted shape.
    assert!(PackageCache::open_under(&home.join("ok/cache"), &home).is_ok());
}

#[test]
fn a_corrupt_or_stale_entry_is_quarantined_and_rebuilt_in_isolation() {
    let (tmp, _home, cache) = fixture();
    let built = built(tmp.path());
    let identity = identity();
    let entry = cache.publish(&identity, &built, &["node_modules"]).unwrap();

    // Tampered content: the recorded digest no longer matches.
    std::fs::write(
        entry.join("node_modules/@opencode-ai/plugin/index.js"),
        "export const plugin = 2;\n",
    )
    .unwrap();
    let (found, outcome) = cache.lookup(&identity).unwrap();
    assert_eq!(outcome, CacheOutcome::Invalidated);
    assert!(found.is_none(), "an invalid entry is never handed back");
    assert!(!entry.exists(), "the bad entry was moved aside");
    assert_eq!(
        std::fs::read_dir(cache.base().join(".quarantine"))
            .unwrap()
            .count(),
        1,
        "evidence is quarantined, not deleted"
    );
    // A rebuild lands cleanly on the same key.
    let rebuilt = cache.publish(&identity, &built, &["node_modules"]).unwrap();
    assert_eq!(rebuilt, entry);
    assert_eq!(cache.lookup(&identity).unwrap().1, CacheOutcome::Hit);

    // A missing file is equally invalid.
    std::fs::remove_file(entry.join("node_modules/.package-lock.json")).unwrap();
    assert_eq!(cache.lookup(&identity).unwrap().1, CacheOutcome::Invalidated);

    // A manifest recorded under a different identity is refused, not adopted.
    let mut wrong = identity.clone();
    wrong.arch = "sparc64".into();
    let stolen = cache.publish(&wrong, &built, &["node_modules"]).unwrap();
    std::fs::rename(&stolen, cache.base().join(identity.key())).unwrap();
    assert_eq!(cache.lookup(&identity).unwrap().1, CacheOutcome::Invalidated);

    // An entry with no manifest at all is invalid rather than trusted.
    let bare = cache.base().join(identity.key());
    std::fs::create_dir_all(bare.join("node_modules")).unwrap();
    assert_eq!(cache.lookup(&identity).unwrap().1, CacheOutcome::Invalidated);
}

#[test]
fn concurrent_publishers_converge_on_one_entry_without_corrupting_it() {
    let (tmp, home, cache) = fixture();
    let built = built(tmp.path());
    let identity = identity();
    let base = cache.base().to_path_buf();
    drop(cache);

    let threads: Vec<_> = (0..6)
        .map(|_| {
            let (base, home, built, identity) =
                (base.clone(), home.clone(), built.clone(), identity.clone());
            std::thread::spawn(move || {
                let cache = PackageCache::open_under(&base, &home).unwrap();
                cache.publish(&identity, &built, &["node_modules"])
            })
        })
        .collect();
    let published: Vec<PathBuf> = threads
        .into_iter()
        .map(|t| t.join().unwrap().expect("every publisher must converge"))
        .collect();
    assert!(published.iter().all(|p| *p == published[0]));

    let cache = PackageCache::open_under(&base, &home).unwrap();
    assert_eq!(
        cache.lookup(&identity).unwrap().1,
        CacheOutcome::Hit,
        "the surviving entry must validate after a concurrent race"
    );
    // Exactly one entry key, and no staging leftovers.
    let keys = std::fs::read_dir(&base)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().len() == 64)
        .count();
    assert_eq!(keys, 1);
    let staging = base.join(".staging");
    if staging.exists() {
        assert_eq!(std::fs::read_dir(&staging).unwrap().count(), 0);
    }
}

#[test]
fn a_held_lock_never_blocks_correctness_only_duplicates_work() {
    let (tmp, _home, cache) = fixture();
    let built = built(tmp.path());
    let identity = identity();
    // Simulate a peer holding the advisory lock for this key.
    let held =
        MkdirLock::try_acquire(&cache.base().join(format!("{}.lock", identity.key())), LOCK_STALE)
            .unwrap()
            .expect("lock is free");
    let entry = cache
        .publish(&identity, &built, &["node_modules"])
        .expect("publication must not depend on the advisory lock");
    assert_eq!(cache.lookup(&identity).unwrap().0.as_deref(), Some(entry.as_path()));
    drop(held);
}
