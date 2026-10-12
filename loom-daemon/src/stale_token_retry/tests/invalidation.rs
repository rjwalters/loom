//! #9247: a fresh installation token drops what the write-scope gate cached
//! under the old one, so the gate re-reads instead of serving a pre-mint
//! answer for up to two hours (snapshot TTL + probe TTL).

use std::cell::{Cell, RefCell};
use std::path::Path;

use crate::forge_repo_facts::installation::tests::{page, Listing};
use crate::forge_repo_facts::test_support::Env;
use crate::stale_token_retry::invalidate_caches;
use crate::write_scope::probe::{self, CacheScope, Permission, PermissionProbe};

/// Points the probe's disk cache at a scratch directory for one test.
struct ScratchCache;

impl ScratchCache {
    fn new(dir: &Path) -> Self {
        std::env::set_var("LOOM_WRITE_SCOPE_CACHE_DIR", dir.join("write-scope"));
        probe::clear_memory();
        Self
    }
}

impl Drop for ScratchCache {
    fn drop(&mut self) {
        std::env::remove_var("LOOM_WRITE_SCOPE_CACHE_DIR");
        probe::clear_memory();
    }
}

/// The repository joined the installation after the token was minted: the
/// gate refused it from the old listing, and keeps refusing from its two
/// caches until the re-mint's invalidation makes it re-probe.
#[test]
#[serial_test::serial(write_scope_cache)]
fn the_gate_reprobes_after_invalidation_instead_of_serving_the_pre_mint_answer() {
    the_gate_reprobes_body();
}

/// The gate reads the process-global `LOOM_REPO` / `GH_REPO`, whose test
/// writers hold the default key (nested inside the cache key, the order
/// `write_scope_test_support` documents).
#[serial_test::serial]
fn the_gate_reprobes_body() {
    let env = Env::new(&[]);
    let _cache = ScratchCache::new(env.tmp.path());
    let fake = Listing::new(env.tmp.path());
    let id = uuid::Uuid::new_v4().simple().to_string();
    let repo = format!("loom-fixture/s{}", &id[..12]);
    let root = env.repo("ws", &[("origin", &format!("https://github.com/{repo}.git"))]);
    std::fs::create_dir_all(root.join(".loom")).unwrap();

    // Minted before the repo joined: the old token's listing omits it.
    fake.set("page1", &page(1, &[(1, "loom-fixture/elsewhere", false)]));
    let gate = || crate::write_scope::gate_root_with(&root, &fake.gh, "stale-token test");
    assert!(!gate(), "not in the installation the old token lists");
    assert_eq!(fake.listing_calls(), 1);

    // The repo is in the installation now, but both caches still answer.
    fake.set("page1", &page(2, &[(1, "loom-fixture/elsewhere", false), (2, &repo, false)]));
    assert!(!gate(), "the cached pre-mint answer");
    assert_eq!(fake.listing_calls(), 1, "no re-probe yet: {:?}", fake.calls());

    invalidate_caches(&root, None);
    assert!(gate(), "re-probed under the fresh token");
    assert_eq!(fake.listing_calls(), 2, "{:?}", fake.calls());
}

struct Counting {
    answer: RefCell<Permission>,
    calls: Cell<usize>,
}

impl Counting {
    fn cached(dir: &Path, answer: Permission) -> probe::Cached<Self> {
        probe::Cached {
            inner: Self {
                answer: RefCell::new(answer),
                calls: Cell::new(0),
            },
            key_dir: Some(dir.to_path_buf()),
        }
    }
}

impl PermissionProbe for Counting {
    fn permission(&self, _repo: &str) -> Permission {
        self.calls.set(self.calls.get() + 1);
        self.answer.borrow().clone()
    }
    fn cache_scope(&self) -> CacheScope {
        CacheScope::GitHub
    }
}

/// Expiry is per credential and covers every repository probed under it;
/// another credential's answers are untouched, in memory and on disk.
#[test]
#[serial_test::serial(write_scope_cache)]
fn expiry_covers_every_repo_of_one_credential_and_no_other() {
    let dir = tempfile::tempdir().unwrap();
    let _cache = ScratchCache::new(dir.path());
    let (a, b) = (dir.path().join("cred-a"), dir.path().join("cred-b"));
    let refused = || Permission::Insufficient("not in this App installation".into());
    let (ours, theirs) = (Counting::cached(&a, refused()), Counting::cached(&b, refused()));
    for repo in ["acme/one", "acme/two"] {
        assert_eq!(ours.permission(repo), refused());
        assert_eq!(theirs.permission(repo), refused());
    }
    assert_eq!((ours.inner.calls.get(), theirs.inner.calls.get()), (2, 2));

    *ours.inner.answer.borrow_mut() = Permission::Write;
    *theirs.inner.answer.borrow_mut() = Permission::Write;
    probe::expire_credential(Some(&a), &CacheScope::GitHub);
    for repo in ["acme/one", "acme/two"] {
        assert_eq!(ours.permission(repo), Permission::Write, "re-probed");
        assert_eq!(theirs.permission(repo), refused(), "still cached");
    }
    assert_eq!((ours.inner.calls.get(), theirs.inner.calls.get()), (4, 2));

    // The disk layer alone: with memory gone, our entries are deleted and
    // the other credential's still answer.
    probe::clear_memory();
    probe::expire_credential(Some(&a), &CacheScope::GitHub);
    assert_eq!(ours.permission("acme/one"), Permission::Write);
    assert_eq!(theirs.permission("acme/one"), refused());
    assert_eq!((ours.inner.calls.get(), theirs.inner.calls.get()), (5, 2));
}

/// An expired WRITE keeps its stale-while-unverifiable grace: a forge blip
/// at the re-probe does not hold every write under the credential.
#[test]
#[serial_test::serial(write_scope_cache)]
fn an_expired_write_keeps_its_grace_when_the_reprobe_cannot_answer() {
    let dir = tempfile::tempdir().unwrap();
    let _cache = ScratchCache::new(dir.path());
    let cred = dir.path().join("cred");
    let cached = Counting::cached(&cred, Permission::Write);
    assert_eq!(cached.permission("acme/app"), Permission::Write);
    *cached.inner.answer.borrow_mut() = Permission::Unknown("HTTP 502".into());
    probe::expire_credential(Some(&cred), &CacheScope::GitHub);
    assert_eq!(cached.permission("acme/app"), Permission::Write, "graced");
    assert_eq!(cached.inner.calls.get(), 2, "it did re-probe");
}
